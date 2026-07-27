//! Composition layer (see `docs/architecture/overview.md`): builds the adapters,
//! injects them into the core, and handles the Tauri lifecycle — tray, global
//! shortcut, and **on-demand** windows (created when used, destroyed when closed;
//! at idle only this process remains — see docs/reference/resource-budget.md).

mod feedback;
mod overlay_native;
mod windows;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use wren_adapters::{
    init_logging, list_input_devices as adapter_list_input_devices,
    list_models as adapter_list_models, set_log_level, storage::default_data_dir,
    ClipboardPasteSink, CpalAudioSource, EarshotVad, JsonSettingsStore, JsonlHistoryStore,
    JsonlTelemetryStore, LogBuffer, LogRecord, RemoteApiTranscriber, SystemClock, ToneFeedback,
    WavRecordingStore,
};
use wren_core::{
    egress_is_external, factory_presets, ActivationMode, CompositeFeedback, DictationService,
    EntryStatus, Feedback as FeedbackPort, HistoryEntry, HistoryStore, LogLevel, PortError,
    ProviderConfig, ProviderKind, SessionMetrics, SessionState, Settings, SettingsStore,
    ToggleOutcome, TranscriptionOptions,
};

use feedback::TauriFeedback;

pub struct AppState {
    service: RwLock<Arc<DictationService>>,
    options: RwLock<TranscriptionOptions>,
    settings_store: Arc<JsonSettingsStore>,
    history: Arc<JsonlHistoryStore>,
    recordings: Arc<WavRecordingStore>,
    /// Performance telemetry (local). Injected into the service and queried by
    /// the Diagnostics tab.
    telemetry: Arc<JsonlTelemetryStore>,
    /// Ring buffer of recent logs (the Diagnostics tab queries it by command).
    logs: Arc<LogBuffer>,
    /// Log file directory — for the `log_file_path` command.
    log_dir: PathBuf,
    /// The active cancel shortcut — registered only DURING the session (see
    /// `register_cancel_shortcut`); kept here so unregistration can find it.
    cancel_shortcut: RwLock<String>,
    /// Serializes every shortcut-triggered toggle (see `trigger_toggle_if`).
    /// Without this, two near-simultaneous shortcut events — e.g. X11
    /// auto-repeat firing several synthetic Pressed events during a single
    /// physical hold (see `register_shortcut`'s doc comment) — can race
    /// between reading `service.state()` and applying the transition, both
    /// observing the same state and both deciding to register/unregister the
    /// cancel shortcut. That race is what can leave the cancel shortcut
    /// (Escape) registered with the OS after Wren believes the session ended:
    /// two overlapping "session started" registrations collide in the
    /// underlying `global-hotkey` X11 backend (same shortcut ⇒ same content-derived
    /// id), whose failure rollback ungrabs while leaving stale bookkeeping
    /// behind. This lock keeps register/unregister ordering consistent with
    /// the actual state transition.
    shortcut_lock: Mutex<()>,
}

fn build_service(
    app: &AppHandle,
    settings: &Settings,
    history: Arc<JsonlHistoryStore>,
    recordings: Arc<WavRecordingStore>,
    telemetry: Arc<JsonlTelemetryStore>,
) -> Result<Arc<DictationService>, PortError> {
    let provider = settings
        .active_provider()
        .or_else(|| settings.providers.first())
        .ok_or_else(|| PortError::Other("no provider configured".into()))?
        .clone();

    // The visual feedback (bubble) is always assembled. The level closure animates
    // the waveform pointing DIRECTLY at it — audio_level does not go through the
    // composite (it fires every frame; nothing beyond the overlay needs it).
    let tauri_feedback: Arc<TauriFeedback> = Arc::new(TauriFeedback::new(app.clone()));
    let level_feedback = tauri_feedback.clone();
    let audio = Arc::new(CpalAudioSource::new(
        Arc::new(move |level: f32| {
            level_feedback.audio_level(level);
        }),
        settings.input_device.clone(),
    ));

    // Fan-out: visual overlay + audio tones (when enabled). The `play_sounds`
    // toggle is orthogonal to egress; the service is rebuilt in save_settings,
    // so changing it takes effect immediately.
    let mut feedbacks: Vec<Arc<dyn FeedbackPort>> = vec![tauri_feedback];
    if settings.play_sounds {
        feedbacks.push(Arc::new(ToneFeedback::new()));
    }
    let feedback = Arc::new(CompositeFeedback::new(feedbacks));

    // The active provider decides WHICH Transcriber adapter is used — cloud/local
    // server (HTTP) or embedded engine (subprocess). Provider parity: the core
    // only knows "the active Transcriber" (doc 02, doc 03).
    let transcriber = build_transcriber(&provider)?;

    Ok(Arc::new(DictationService::new(
        audio,
        // The pause-compression threshold is a construction-time config: the service
        // is rebuilt in save_settings, so changes take effect immediately.
        Arc::new(EarshotVad::new(settings.compress_pauses_over_ms)),
        transcriber,
        Arc::new(ClipboardPasteSink::with_config(
            settings.paste_method,
            settings.restore_clipboard,
        )),
        history,
        recordings,
        feedback,
        Arc::new(SystemClock),
        telemetry,
    )))
}

/// Picks the `Transcriber` adapter based on the provider's `kind`. `RemoteApi`
/// (cloud or third-party local server) uses the HTTP adapter; `Embedded` uses the
/// local engine — only available in the offline edition (feature `embedded`).
fn build_transcriber(
    provider: &ProviderConfig,
) -> Result<Arc<dyn wren_core::Transcriber>, PortError> {
    match provider.kind {
        ProviderKind::RemoteApi => Ok(Arc::new(RemoteApiTranscriber::new(provider.clone())?)),
        ProviderKind::Embedded => build_embedded_transcriber(&provider.model),
    }
}

/// Embedded engine: the `EmbeddedTranscriber` spawns the worker (the binary
/// itself) pointing at the downloaded model's directory. See
/// `docs/architecture/embedded-engine.md`. It
/// only fails if the model wasn't downloaded — the adapter reports that when it
/// tries to transcribe.
fn build_embedded_transcriber(
    model_id: &str,
) -> Result<Arc<dyn wren_core::Transcriber>, PortError> {
    let dir = wren_embedded::model_dir(&models_cache_root(), model_id);
    Ok(Arc::new(wren_embedded::EmbeddedTranscriber::new(dir)))
}

/// On X11, holding the key generates auto-repeat: synthetic Pressed/Released
/// pairs while the finger hasn't even moved. A Released in push-to-talk only ends
/// the session after this grace period — if a new Pressed arrives first, the
/// Released was synthetic and is ignored (see `register_shortcut`).
const PTT_AUTO_REPEAT_GRACE: Duration = Duration::from_millis(80);

/// Acquires `AppState::shortcut_lock`, recovering from poison instead of
/// panicking. A `std::sync::Mutex` poisons if the thread holding it panics
/// while the guard is live — and this lock's critical sections call into
/// `DictationService::toggle`/`cancel` and the shortcut register/unregister
/// pair, any of which panicking would otherwise wedge the lock (and thus the
/// shortcut) permanently for every subsequent press, with the process itself
/// still alive and no crash to point at. The `()` payload carries no
/// invariant that a panic mid-section could leave inconsistent, so recovering
/// is safe: the point of this lock is ordering, not protecting data.
fn lock_shortcut(lock: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Registers the main shortcut according to the activation mode. Calls
/// `unregister_all()` first — which also clears any leftover cancel shortcut (in
/// practice this only runs outside a session: setup and save_settings).
fn register_shortcut(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let gs = app.global_shortcut();
    gs.unregister_all().map_err(|e| e.to_string())?;
    let shortcut = settings.shortcut.as_str();

    match settings.activation_mode {
        ActivationMode::Toggle => {
            gs.on_shortcut(shortcut, |app, _shortcut, event| {
                let st = event.state();
                let app = app.clone();
                // Reading `service.state()` (and everything downstream of a
                // Pressed event) must never run ON the global-hotkey
                // dispatch thread: that same thread also serves
                // register/unregister for every shortcut, so a hang here
                // (e.g. a core lock wedged behind a stuck audio teardown)
                // would silently kill shortcut detection AND re-registration
                // together, surviving even a shortcut change in Settings —
                // only an app restart would bring it back.
                std::thread::spawn(move || {
                    // DIAGNOSTIC (intermittent "won't stop" bug): logs EVERY raw
                    // shortcut event, with the session state. Distinguishes the two
                    // hypotheses: a burst of Pressed/Released from a single press =
                    // auto-repeat multi-toggle; Pressed with session=Transcribing =
                    // Busy; no log on press = event swallowed in global-hotkey
                    // (`pressed` flag stuck). Remove after diagnosing.
                    let session = {
                        let state: State<AppState> = app.state();
                        let service = state.service.read().unwrap().clone();
                        service.state()
                    };
                    log::debug!(target: "wren::shortcut", "main shortcut: {st:?} (session={session:?})");
                    if st == ShortcutState::Pressed {
                        trigger_toggle(app);
                    }
                });
            })
            .map_err(|e| format!("invalid shortcut '{shortcut}': {e}"))?;
        }
        ActivationMode::PushToTalk => {
            // Auto-repeat mitigation: `generation` grows with each Pressed.
            // The Released memorizes the generation it saw, waits out the grace
            // period, and only ends if NO new Pressed arrived in the interval — a
            // new Pressed reveals the Released was auto-repeat, not the finger lifting.
            let generation = Arc::new(AtomicU64::new(0));
            gs.on_shortcut(shortcut, move |app, _shortcut, event| match event.state() {
                ShortcutState::Pressed => {
                    generation.fetch_add(1, Ordering::SeqCst);
                    // Only start from Idle: an auto-repeat Pressed during
                    // recording MUST NOT stop the session.
                    trigger_toggle_if(app.clone(), Some(SessionState::Idle));
                }
                ShortcutState::Released => {
                    let seen = generation.load(Ordering::SeqCst);
                    let generation = generation.clone();
                    let app = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(PTT_AUTO_REPEAT_GRACE);
                        if generation.load(Ordering::SeqCst) != seen {
                            return; // auto-repeat: the key is still held
                        }
                        // Only end if still recording (a cancel mid-session
                        // must not turn into the start of another).
                        trigger_toggle_if(app, Some(SessionState::Recording));
                    });
                }
            })
            .map_err(|e| format!("invalid shortcut '{shortcut}': {e}"))?;
        }
    }
    log::info!(
        target: "wren::shortcut",
        "global shortcut registered: {shortcut} ({:?})",
        settings.activation_mode
    );
    Ok(())
}

/// Registers the cancel shortcut — ONLY called when a session starts and
/// unregistered when it ends, so it never swallows the key (e.g. Esc) from other
/// apps outside dictation. Invalid/empty config doesn't block dictation: it logs
/// and the session continues without cancellation (fail-open, like the VAD).
fn register_cancel_shortcut(app: &AppHandle) {
    let state: State<AppState> = app.state();
    let shortcut = state.cancel_shortcut.read().unwrap().clone();
    if shortcut.trim().is_empty() {
        log::warn!(target: "wren::shortcut", "cancel shortcut empty; session without cancellation");
        return;
    }
    let gs = app.global_shortcut();
    // Defensive: if our own bookkeeping still thinks this shortcut is
    // registered, some earlier cycle never reached its matching
    // `unregister_cancel_shortcut` (or a race let two "session started"
    // registrations collide — see `AppState::shortcut_lock`). Clear it before
    // registering fresh instead of letting `on_shortcut` fail with
    // AlreadyRegistered: on X11, `global-hotkey`'s failure rollback ungrabs
    // the key for ALL ignored-modifier variants while leaving the ORIGINAL
    // (still "active" per our bookkeeping) entry untouched in its internal
    // map — exactly the kind of inconsistency that can wedge Escape.
    if gs.is_registered(shortcut.as_str()) {
        log::warn!(
            target: "wren::shortcut",
            "cancel shortcut ({shortcut}) already registered at session start (leaked from an earlier cycle?); clearing before re-registering"
        );
        let _ = gs.unregister(shortcut.as_str());
    }
    let result = gs.on_shortcut(shortcut.as_str(), |app, _shortcut, event| {
        // DIAGNOSTIC: logs every Escape event. If, during a "won't stop", the
        // ctrl+shift+space is silent but THIS logs on pressing Escape, it
        // confirms the loss is SPECIFIC to the main combination (grab/flag of
        // that key), not a general stall of global-hotkey.
        log::debug!(target: "wren::shortcut", "cancel shortcut: {:?}", event.state());
        if event.state() == ShortcutState::Pressed {
            let app = app.clone();
            std::thread::spawn(move || {
                let state: State<AppState> = app.state();
                // Same serialization as `trigger_toggle_if`: a cancel
                // racing against a near-simultaneous main-shortcut toggle
                // must not interleave its state mutation + unregister
                // with the other thread's register/unregister.
                let _shortcut_guard = lock_shortcut(&state.shortcut_lock);
                let service = state.service.read().unwrap().clone();
                service.cancel();
                unregister_cancel_shortcut(&app);
            });
        }
    });
    match result {
        Ok(()) => log::info!(
            target: "wren::shortcut",
            "session started: cancel shortcut ({shortcut}) active only during recording"
        ),
        Err(e) => log::warn!(
            target: "wren::shortcut",
            "invalid cancel shortcut '{shortcut}' ({e}); session without cancellation"
        ),
    }
}

/// Unregisters the cancel shortcut. May race with another unregistration
/// (e.g. cancel pressed at the exact end of the session) — unregistering
/// something already unregistered is NOT a panic: it just logs and continues.
fn unregister_cancel_shortcut(app: &AppHandle) {
    let state: State<AppState> = app.state();
    let shortcut = state.cancel_shortcut.read().unwrap().clone();
    if shortcut.trim().is_empty() {
        return;
    }
    let gs = app.global_shortcut();
    match gs.unregister(shortcut.as_str()) {
        Ok(()) => log::info!(
            target: "wren::shortcut",
            "session ended: cancel shortcut ({shortcut}) released back to other apps"
        ),
        Err(e) => {
            log::warn!(target: "wren::shortcut", "unregistering cancel shortcut ({shortcut}): {e}")
        }
    }
    // Defensive extra ungrab. On X11, `global-hotkey`'s backend can report
    // success here even when the OS-level `ungrab_key` request for one of the
    // ignored-modifier variants (NumLock/CapsLock combinations) silently
    // failed to reach the X server — it only sends the request `if let
    // Ok(...)` and otherwise drops it, yet still returns `Ok(())`
    // unconditionally. That means a "released" log line is not a hard
    // guarantee the OS actually let go of the key. The plugin's own
    // `is_registered` bookkeeping can't detect this (it's updated
    // unconditionally by `unregister()`, independent of what happened at the
    // X11 level), so the best mitigation from this side is to just re-issue
    // the ungrab: it's idempotent and cheap, and gives the modifier variants
    // that may have been dropped the first time another chance to go out.
    let _ = gs.unregister(shortcut.as_str());
}

/// Reconciles the OS autostart with the user's preference. The autostart plugin
/// is the authority over the real state; here we just push it toward `enabled`.
/// Failure interrupts nothing (fail-open): it logs and continues.
fn apply_autostart(app: &AppHandle, enabled: bool) {
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(e) = result {
        log::warn!(target: "wren::autostart", "autostart ({enabled}): {e}");
    }
}

/// Fires the dictation on its own thread: the shortcut handler never blocks
/// (transcription is a synchronous network call).
fn trigger_toggle(app: AppHandle) {
    trigger_toggle_if(app, None);
}

/// Like `trigger_toggle`, but with a state guard: it only runs the toggle if the
/// session is in `only_if` — that's what stops push-to-talk from inverting the
/// intent (Pressed always starts, Released always ends). The check and the toggle
/// happen on the same thread, back to back; the remaining race window is
/// negligible against the minimum auto-repeat delay (~hundreds of ms).
///
/// It's also the single point for the cancel shortcut's lifecycle: session
/// started (Started) ⇒ register; session ended (Finished or error) ⇒
/// unregister.
fn trigger_toggle_if(app: AppHandle, only_if: Option<SessionState>) {
    std::thread::spawn(move || {
        let state: State<AppState> = app.state();
        // Serialize with any other shortcut-triggered toggle in flight (see
        // `AppState::shortcut_lock`). Must be held across the whole
        // read-state → toggle → register/unregister sequence: that is exactly
        // the section whose non-atomicity can otherwise leave the cancel
        // shortcut registration out of sync with the real session state.
        let _shortcut_guard = lock_shortcut(&state.shortcut_lock);
        let service = state.service.read().unwrap().clone();
        if let Some(expected) = only_if {
            if service.state() != expected {
                return;
            }
        }
        let options = state.options.read().unwrap().clone();
        // Errors already become feedback inside the use case; the log is diagnostic.
        match service.toggle(&options) {
            Ok(ToggleOutcome::Started) => {
                log::info!(target: "wren::session", "toggle: Started");
                register_cancel_shortcut(&app);
            }
            Ok(ToggleOutcome::Finished) => {
                log::info!(target: "wren::session", "toggle: Finished");
                unregister_cancel_shortcut(&app);
            }
            Ok(outcome) => log::info!(target: "wren::session", "toggle: {outcome:?}"),
            Err(err) => {
                log::error!(target: "wren::session", "toggle failed: {err}");
                // The session died on error: the cancel must not outlive it.
                unregister_cancel_shortcut(&app);
            }
        }
    });
}

/// Copies the last successful transcription to the clipboard WITHOUT pasting
/// (the "Copy last transcription" tray item). Runs on its own thread: X11's
/// selection persistence BLOCKS while serving the clipboard, so it can't run in
/// the menu handler. With no `Done` entry with text, it's a no-op with a log —
/// never panics.
fn copy_last_transcription(app: AppHandle) {
    std::thread::spawn(move || {
        let state: State<AppState> = app.state();
        let recent = match state.history.recent(50) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!(target: "wren::tray", "copy last: failed to read history: {err}");
                return;
            }
        };
        // `recent()` returns most-recent-first (see storage.rs): the first
        // `Done` entry with text is the transcription we want.
        let last = recent
            .into_iter()
            .find(|e| e.status == EntryStatus::Done && !e.text.trim().is_empty());
        match last {
            Some(entry) => match ClipboardPasteSink::new().copy(&entry.text) {
                Ok(()) => log::info!(
                    target: "wren::tray",
                    "last transcription copied to clipboard ({} chars)",
                    entry.text.chars().count()
                ),
                Err(err) => log::warn!(target: "wren::tray", "copy last: failed to copy: {err}"),
            },
            None => {
                log::info!(target: "wren::tray", "copy last: no completed transcription in history")
            }
        }
    });
}

/// Maps the domain's `LogLevel` to the `log` crate's filter (composition-layer
/// concern — `wren-core` stays free of the `log` dependency).
fn log_level_filter(level: LogLevel) -> log::LevelFilter {
    match level {
        LogLevel::Error => log::LevelFilter::Error,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Trace => log::LevelFilter::Trace,
    }
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Result<Settings, String> {
    state.settings_store.load().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: State<AppState>,
    mut settings: Settings,
) -> Result<(), String> {
    // The backend is the authority over egress (doc 02, invariant 4): we don't
    // trust the flag that came from the UI — we re-derive it from each base_url
    // before persisting, so the "audio leaves the machine" warning never lies.
    for provider in &mut settings.providers {
        provider.sends_audio_externally = match provider.kind {
            // Embedded engine: audio never leaves the machine (docs/architecture/provider-model.md).
            ProviderKind::Embedded => false,
            ProviderKind::RemoteApi => egress_is_external(&provider.base_url),
        };
    }

    state
        .settings_store
        .save(&settings)
        .map_err(|e| e.to_string())?;

    // Takes effect immediately, no restart needed (log::set_max_level is a
    // global atomic) — so raising the level to debug a live issue works right away.
    set_log_level(log_level_filter(settings.log_level));

    // Rebuild the service with the new provider and re-register the shortcut —
    // switching provider is config, not reinstallation (doc 01).
    let service = build_service(
        &app,
        &settings,
        state.history.clone(),
        state.recordings.clone(),
        state.telemetry.clone(),
    )
    .map_err(|e| e.to_string())?;
    *state.service.write().unwrap() = service;
    *state.options.write().unwrap() = TranscriptionOptions {
        language: settings.language.clone(),
        prompt: None,
    };
    *state.cancel_shortcut.write().unwrap() = settings.cancel_shortcut.clone();
    apply_autostart(&app, settings.launch_at_login);
    register_shortcut(&app, &settings)
}

#[tauri::command]
fn get_history(state: State<AppState>) -> Result<Vec<HistoryEntry>, String> {
    state.history.recent(50).map_err(|e| e.to_string())
}

/// Resends a failed entry (audio preserved) to the active provider and returns
/// the transcribed text — the UI puts it on the clipboard (the settings window
/// is focused; auto-pasting would land in the wrong place). `async` +
/// `spawn_blocking` because transcription is a synchronous network call.
#[tauri::command]
async fn retry_transcription(app: AppHandle, created_at_ms: u64) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state: State<AppState> = app.state();
        let entry = {
            state
                .history
                .recent(500)
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|e| e.created_at_ms == created_at_ms)
                .ok_or_else(|| "entry not found in history".to_string())?
        };
        let service = state.service.read().unwrap().clone();
        let options = state.options.read().unwrap().clone();
        let transcript = service.retry(&entry, &options).map_err(|e| e.to_string())?;
        log::info!(
            target: "wren::session",
            "resend ok: {} ms of audio, {} ms of latency",
            transcript.audio_duration_ms, transcript.latency_ms
        );
        Ok(transcript.text)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Lists a provider's transcription models (`GET {base_url}/models`), so the UI
/// can populate the selector during configuration. `async` + `spawn_blocking`:
/// the adapter uses `reqwest::blocking` (same pattern as `retry_transcription`).
#[tauri::command]
async fn list_models(base_url: String, api_key: Option<String>) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        adapter_list_models(&base_url, api_key.as_deref()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Enumerates available microphones by name (synchronous and cheap). Empty if
/// enumeration fails — the UI falls back to "default microphone".
#[tauri::command]
fn list_input_devices() -> Vec<String> {
    adapter_list_input_devices()
}

/// Catalog of provider presets the UI offers under "add provider".
#[tauri::command]
fn provider_presets() -> Vec<ProviderConfig> {
    factory_presets()
}

/// Checks for and installs an update, if any, and returns a message ready for
/// the UI to display. As long as `plugins.updater` doesn't point at a real
/// endpoint and a production pubkey, it responds gracefully instead of failing —
/// it never panics (see `run()` and the tauri.conf.json comment).
#[tauri::command]
async fn check_for_updates(app: AppHandle) -> Result<String, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(e) => {
            log::warn!(target: "wren::update", "updater unavailable: {e}");
            return Ok("Automatic updates are not configured in this version yet.".into());
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            log::info!(target: "wren::update", "update {version} available; downloading…");
            update
                .download_and_install(|_, _| {}, || {})
                .await
                .map_err(|e| e.to_string())?;
            log::info!(target: "wren::update", "update {version} installed; restarting");
            app.restart();
        }
        Ok(None) => Ok("You're already on the latest version.".into()),
        Err(e) => {
            log::warn!(target: "wren::update", "update check failed: {e}");
            Ok("Couldn't check for updates right now.".into())
        }
    }
}

/// Recent logs from the ring buffer (local diagnostics), filtered by minimum
/// severity level and substring. Feeds the Settings › Diagnostics tab.
#[tauri::command]
fn get_logs(
    state: State<AppState>,
    limit: Option<usize>,
    level: Option<String>,
    query: Option<String>,
) -> Vec<LogRecord> {
    let min_level = level.and_then(|s| s.parse::<log::Level>().ok());
    let query = query.unwrap_or_default();
    state.logs.snapshot(limit.unwrap_or(500), min_level, &query)
}

/// Empties the in-memory log buffer (the on-disk file remains).
#[tauri::command]
fn clear_logs(state: State<AppState>) {
    state.logs.clear();
    log::info!(target: "wren", "in-memory logs cleared by the user");
}

/// Path of the persisted log file — the UI displays it and offers "copy".
#[tauri::command]
fn log_file_path(state: State<AppState>) -> String {
    state
        .log_dir
        .join("wren.log")
        .to_string_lossy()
        .into_owned()
}

/// Performance metrics for recent sessions (local), most-recent-first.
#[tauri::command]
fn get_metrics(state: State<AppState>, limit: Option<usize>) -> Vec<SessionMetrics> {
    state.telemetry.recent(limit.unwrap_or(50))
}

// ── Embedded engine — model management commands ──────────────────────────────
// The engine is always embedded; it's a runtime-selectable provider (docs/architecture/provider-model.md).
// The ONNX weight only exists in the worker subprocess, when dictating with the
// embedded provider.

/// Where the embedded engine's models are cached.
fn models_cache_root() -> PathBuf {
    default_data_dir().join("models")
}

/// Catalog DTO for the UI (camelCase for the front-end).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfoDto {
    id: String,
    label: String,
    language: String,
    size_bytes: u64,
}

/// Event emitted on `embedded://download-progress` during the download.
#[derive(Clone, serde::Serialize)]
struct DownloadProgressEvent {
    id: String,
    downloaded: u64,
    total: u64,
    done: bool,
    error: Option<String>,
}

/// Catalog of available offline models.
#[tauri::command]
fn embedded_catalog() -> Vec<ModelInfoDto> {
    wren_embedded::catalog()
        .into_iter()
        .map(|m| ModelInfoDto {
            id: m.id,
            label: m.label,
            language: m.language,
            size_bytes: m.size_bytes,
        })
        .collect()
}

/// Ids of models already downloaded and intact.
#[tauri::command]
fn embedded_local_models() -> Vec<String> {
    wren_embedded::local_models(&models_cache_root())
}

/// Downloads a model, emitting progress on `embedded://download-progress`.
#[tauri::command]
async fn embedded_download_model(app: AppHandle, id: String) -> Result<(), String> {
    let cache = models_cache_root();
    let emit_app = app.clone();
    let emit_id = id.clone();
    // Download is blocking (network + IO) → spawn_blocking; the progress becomes
    // events the UI listens to.
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let progress = |p: wren_embedded::DownloadProgress| {
            let _ = emit_app.emit(
                "embedded://download-progress",
                DownloadProgressEvent {
                    id: emit_id.clone(),
                    downloaded: p.downloaded,
                    total: p.total,
                    done: false,
                    error: None,
                },
            );
        };
        wren_embedded::download_model(&cache, &emit_id, &progress)
    })
    .await;

    let err_msg = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(e) => Some(format!("download task failed: {e}")),
    };
    // Final event: `done` (success) or `error` (the UI clears the bar / warns).
    let _ = app.emit(
        "embedded://download-progress",
        DownloadProgressEvent {
            id,
            downloaded: 0,
            total: 0,
            done: err_msg.is_none(),
            error: err_msg.clone(),
        },
    );
    match err_msg {
        None => Ok(()),
        Some(m) => Err(m),
    }
}

/// Removes a downloaded model from the cache.
#[tauri::command]
fn embedded_delete_model(id: String) -> Result<(), String> {
    wren_embedded::delete_model(&models_cache_root(), &id).map_err(|e| e.to_string())
}

/// The binary itself acts as a transcription worker when spawned with the hidden
/// subcommand (the `EmbeddedTranscriber` adapter does this). Must be called at the
/// start of `main`, BEFORE any Tauri init; returns the exit code when it acted as
/// a worker, `None` in the app's normal flow.
pub fn run_worker_if_invoked() -> Option<std::process::ExitCode> {
    wren_embedded::run_if_worker(&std::env::args().collect::<Vec<_>>())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // Autostart: launch Wren with the system (the toggle lives in Settings ›
        // System; reconciliation with the OS happens in setup/save_settings).
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Auto-update: the plugin stays registered, but only actually updates when
        // tauri.conf.json's `plugins.updater` points at a real endpoint and a
        // production pubkey (see docs/reference/open-questions.md and the config
        // comment). Without that,
        // `check_for_updates` responds with a friendly message — it never panics.
        // `process` enables the post-install restart.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_history,
            retry_transcription,
            list_models,
            list_input_devices,
            provider_presets,
            check_for_updates,
            get_logs,
            clear_logs,
            log_file_path,
            get_metrics,
            embedded_catalog,
            embedded_local_models,
            embedded_download_model,
            embedded_delete_model
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Settings load FIRST so the logger can start at the user's
            // persisted `log_level` (default `Info`) instead of always
            // capturing DEBUG — a production install shouldn't accumulate
            // verbose noise on disk unless the user opts in.
            let settings_store = Arc::new(JsonSettingsStore::at_default_location());
            let settings = settings_store.load().unwrap_or_default();

            // Central logging — from here on every `log::` is captured
            // (stderr + rotating file + ring buffer). 100% local diagnostics.
            let log_dir = default_data_dir().join("logs");
            let logs = init_logging(log_dir.clone(), log_level_filter(settings.log_level));
            log::info!(target: "wren", "Wren starting");

            let history = Arc::new(JsonlHistoryStore::at_default_location());
            let recordings = Arc::new(WavRecordingStore::at_default_location());
            let telemetry = Arc::new(JsonlTelemetryStore::at_default_location());

            let service = build_service(
                &handle,
                &settings,
                history.clone(),
                recordings.clone(),
                telemetry.clone(),
            )?;
            app.manage(AppState {
                service: RwLock::new(service),
                options: RwLock::new(TranscriptionOptions {
                    language: settings.language.clone(),
                    prompt: None,
                }),
                settings_store,
                history,
                recordings,
                telemetry,
                logs,
                log_dir,
                cancel_shortcut: RwLock::new(settings.cancel_shortcut.clone()),
                shortcut_lock: Mutex::new(()),
            });

            if let Err(err) = register_shortcut(&handle, &settings) {
                log::error!(target: "wren::shortcut", "{err}");
            }

            // Align the OS autostart with the persisted preference at boot.
            apply_autostart(&handle, settings.launch_at_login);

            // Diagnostic trigger: WREN_TEST_SETTINGS=1 opens the settings window
            // at startup (useful in CI/tests without a tray).
            if std::env::var("WREN_TEST_SETTINGS").is_ok() {
                windows::open_settings_window(&handle);
            }

            // Diagnostic trigger: WREN_TEST_TOGGLE="2000,8000" simulates the
            // shortcut at the given instants in ms (useful in CI/tests without a keyboard).
            if let Ok(spec) = std::env::var("WREN_TEST_TOGGLE") {
                let mut delays: Vec<u64> = spec
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                delays.sort_unstable();
                let test_handle = handle.clone();
                std::thread::spawn(move || {
                    let start = std::time::Instant::now();
                    for at in delays {
                        let target = std::time::Duration::from_millis(at);
                        if let Some(wait) = target.checked_sub(start.elapsed()) {
                            std::thread::sleep(wait);
                        }
                        trigger_toggle(test_handle.clone());
                    }
                });
            }

            // Tray: the app's only permanent presence. The frequently-used action
            // ("Copy last transcription") sits at the top, with an emoji that
            // highlights it, separated by a line from the settings/quit block —
            // which stays emoji-free to avoid clutter.
            let copy_last = MenuItem::with_id(
                app,
                "copy_last",
                "📋 Copy last transcription",
                true,
                None::<&str>,
            )?;
            let separator = PredefinedMenuItem::separator(app)?;
            let open_settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&copy_last, &separator, &open_settings, &quit])?;
            TrayIconBuilder::with_id("wren-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .tooltip("Wren — voice dictation")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => windows::open_settings_window(app),
                    "copy_last" => copy_last_transcription(app.clone()),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error starting Wren")
        .run(|_app, event| {
            // Closing the last window does NOT quit the app: Wren lives in the tray.
            if let RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
