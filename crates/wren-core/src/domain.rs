//! Domain entities (doc 02): AudioClip, Transcript, DictationSession,
//! Provider/ProviderConfig, Settings. No I/O, no OS, no concrete provider.

use serde::{Deserialize, Serialize};

/// Normalized audio, independent of how it was captured.
/// Phase 1 canonical form: PCM i16 **mono** (16 kHz in the standard pipeline).
#[derive(Clone)]
pub struct AudioClip {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub duration_ms: u64,
}

impl AudioClip {
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty() || self.duration_ms == 0
    }
}

impl std::fmt::Debug for AudioClip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioClip")
            .field("samples", &self.samples.len())
            .field("sample_rate", &self.sample_rate)
            .field("duration_ms", &self.duration_ms)
            .finish()
    }
}

/// The VAD's result over a clip (doc 08 §3).
pub enum VadOutcome {
    /// No speech detected — nothing to transcribe.
    NoSpeech,
    /// Speech detected: clip with the silence at the edges trimmed off.
    Speech { clip: AudioClip, speech_ms: u64 },
}

/// The result of a transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub audio_duration_ms: u64,
    pub latency_ms: u64,
    /// Epoch millis (the core does not format dates; whoever displays decides).
    pub created_at_ms: u64,
    /// Duration actually dictated by the user, BEFORE the VAD's pause
    /// compression — `audio_duration_ms` is what was sent to the provider.
    /// `None` in old records (predating the compression).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_duration_ms: Option<u64>,
}

/// A stage of the dictation pipeline, for performance telemetry (100% local
/// diagnostics — never leaves the machine). The variants follow the real order
/// of the flow in `usecase::transcribe_and_deliver`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `audio.stop()` — ends the capture and materializes the clip.
    CaptureStop,
    /// `vad.gate_and_trim()` — gate + trim + pause compression.
    Vad,
    /// `recordings.save()` — writes the WAV as a safety net before the network.
    PersistAudio,
    /// `transcriber.transcribe()` — the call to the provider (usually dominates).
    Transcribe,
    /// `sink.deliver()` — paste/type into the focused app.
    Deliver,
}

/// Duration (and data volume) of a stage. `audio_bytes` is the size of the clip
/// that entered the stage (`samples.len() * 2`, i16) — "honest" per-stage
/// memory, without faking the allocator's heap accounting. `None` when the
/// stage does not handle audio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: Stage,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bytes: Option<u64>,
}

/// How a session ended, from the telemetry's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcome {
    /// Text transcribed and delivered to the focused app.
    Delivered,
    /// Discarded by the VAD gate (silence/short speech) — nothing sent.
    DiscardedNoSpeech,
    /// Provider refused/network dropped — audio preserved for retry.
    Failed,
    /// Cancelled by the user (Escape) during the session.
    Cancelled,
}

/// Metrics of a dictation session — how long each stage took and the process's
/// peak memory. Persisted locally (`telemetry.jsonl`) and shown in the
/// Diagnostics tab. They NEVER leave the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetrics {
    /// Epoch millis of the session's end (whoever displays formats the date).
    pub created_at_ms: u64,
    pub outcome: SessionOutcome,
    pub provider_id: String,
    pub model: String,
    /// Duration actually dictated, before the VAD shortened it.
    pub recorded_duration_ms: u64,
    /// Duration sent to the provider (after trim/compression). 0 if nothing was sent.
    pub sent_audio_duration_ms: u64,
    /// Wall-clock sum of the measured stages.
    pub total_ms: u64,
    pub stages: Vec<StageTiming>,
    /// The process's resident RSS at the start of processing (baseline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_start_bytes: Option<u64>,
    /// Largest RSS observed during processing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_peak_bytes: Option<u64>,
}

/// Outcome of a history entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    #[default]
    Done,
    Failed,
}

/// A history entry: a completed transcription **or** a failure with the audio
/// preserved for retry — the user's dictation is never lost to a
/// network/provider error. Old history lines (just `Transcript`) remain
/// readable: `status` defaults to `done` and the failure fields stay `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(default)]
    pub status: EntryStatus,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub audio_duration_ms: u64,
    #[serde(default)]
    pub latency_ms: u64,
    pub created_at_ms: u64,
    /// Why the transcription failed (`status = failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Key of the audio preserved in the `RecordingStore` (`status = failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_path: Option<String>,
    /// Duration actually dictated, before the pause compression (see
    /// `Transcript::recorded_duration_ms`). `None` on old lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_duration_ms: Option<u64>,
}

impl HistoryEntry {
    pub fn done(transcript: &Transcript) -> Self {
        HistoryEntry {
            status: EntryStatus::Done,
            text: transcript.text.clone(),
            language: transcript.language.clone(),
            provider_id: transcript.provider_id.clone(),
            model: transcript.model.clone(),
            audio_duration_ms: transcript.audio_duration_ms,
            latency_ms: transcript.latency_ms,
            created_at_ms: transcript.created_at_ms,
            error: None,
            audio_path: None,
            recorded_duration_ms: transcript.recorded_duration_ms,
        }
    }

    pub fn failed(
        error: String,
        audio_path: Option<String>,
        audio_duration_ms: u64,
        created_at_ms: u64,
    ) -> Self {
        HistoryEntry {
            status: EntryStatus::Failed,
            text: String::new(),
            language: None,
            provider_id: String::new(),
            model: String::new(),
            audio_duration_ms,
            latency_ms: 0,
            created_at_ms,
            error: Some(error),
            audio_path,
            recorded_duration_ms: None,
        }
    }
}

/// Which kind of `Transcriber` adapter serves this provider (doc 03, doc 10).
/// `RemoteApi` covers cloud AND third-party local server (same HTTP adapter);
/// `Embedded` is Phase 2's embedded local engine (a disposable subprocess).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible HTTP adapter (cloud or third-party `localhost`).
    #[default]
    RemoteApi,
    /// Embedded engine (Phase 2): `model` is the local model id; no `base_url`
    /// or credential; `sends_audio_externally=false`.
    Embedded,
}

/// Description of a transcription provider — data, not code (doc 03).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub label: String,
    /// Which adapter serves it. Absent in old settings.json ⇒ `RemoteApi`.
    #[serde(default)]
    pub kind: ProviderKind,
    /// E.g.: "https://api.groq.com/openai/v1" or "http://localhost:8555/v1".
    /// Empty when `kind = Embedded`.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Endpoint URL (RemoteApi) or local model id (Embedded).
    pub model: String,
    /// Explicit egress (doc 02, invariant 4): the UI warns when the audio
    /// leaves the machine. `localhost` and `Embedded` ⇒ false.
    pub sends_audio_externally: bool,
}

/// Options for an individual transcription.
#[derive(Debug, Clone, Default)]
pub struct TranscriptionOptions {
    /// ISO 639-1 (e.g.: "pt"). None = auto-detect.
    pub language: Option<String>,
    /// Context/vocabulary hint, when the provider supports it.
    pub prompt: Option<String>,
}

/// How the global shortcut starts/ends a dictation session (doc 01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    /// Press to start, press again to stop.
    #[default]
    Toggle,
    /// Records while the shortcut is held down (push-to-talk).
    PushToTalk,
}

/// How the transcribed text is delivered to the focused app (doc 05). The
/// default (`Paste`) preserves accents/ç — the well-known pain of synthetic
/// typing on X11 — by pasting via the clipboard. The others cover specific
/// cases: terminals (which intercept Ctrl+V), apps without paste and Wayland
/// sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteMethod {
    /// Clipboard + Ctrl+V. Universal and with no accent problem.
    #[default]
    Paste,
    /// Clipboard + Ctrl+Shift+V. For terminals (GNOME Terminal, etc.), where
    /// Ctrl+V does not paste.
    CtrlShiftV,
    /// Synthetic key-by-key typing (does not use the clipboard). Works where
    /// there is no paste, but suffers with accents/layout on X11 (doc 05).
    Type,
    /// Types the text via the `wtype` binary — the path for Wayland sessions,
    /// where X11's synthetic injection (enigo) does not reach. Requires `wtype`
    /// installed.
    Wtype,
}

/// Verbosity of the local diagnostics logger (file + ring buffer + stderr).
/// Ships defaulting to `Info` so a production install doesn't accumulate
/// DEBUG/TRACE noise on disk; the user can raise it from Settings › System
/// when troubleshooting (doc: Diagnostics tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

/// The user's persisted preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub active_provider_id: String,
    pub providers: Vec<ProviderConfig>,
    /// Global shortcut in the trigger adapter's format (e.g.: "ctrl+shift+space").
    pub shortcut: String,
    /// How the shortcut operates. Absent in old settings.json ⇒ Toggle.
    #[serde(default)]
    pub activation_mode: ActivationMode,
    /// Shortcut that cancels the ongoing session, discarding the audio. It is
    /// only registered WHILE a session is active — outside it the key stays
    /// free for other apps. Absent in old settings.json ⇒ "Escape".
    #[serde(default = "default_cancel_shortcut")]
    pub cancel_shortcut: String,
    pub language: Option<String>,
    /// Internal silence pauses longer than this (ms) are compressed by the VAD
    /// to a short residue before sending (doc 08 §3).
    /// `None` = off. Absent in old settings.json ⇒ default 2000.
    #[serde(default = "default_compress_pauses_over_ms")]
    pub compress_pauses_over_ms: Option<u64>,
    /// Name of the input device (microphone) chosen by the user.
    /// `None` = the OS's default microphone. Absent in old settings.json ⇒
    /// loads as `None` (keeps using the default microphone).
    #[serde(default)]
    pub input_device: Option<String>,
    /// Plays synthesized feedback tones (start/end/error) as a complement to
    /// the visual overlay — also covers the cases where the overlay/GPU fails.
    /// Absent in old settings.json ⇒ loads ON (sound by default).
    #[serde(default = "default_play_sounds")]
    pub play_sounds: bool,
    /// How the text is delivered to the focused app (paste, Ctrl+Shift+V, type,
    /// wtype). Absent in old settings.json ⇒ `Paste` (the long-standing
    /// behavior).
    #[serde(default)]
    pub paste_method: PasteMethod,
    /// Restore the clipboard's previous content after pasting? The Phase 0
    /// decision was NOT to restore (the dictation stays on the clipboard as a
    /// safety net); this makes the restore optional. Only applies to methods
    /// that use the clipboard (paste / Ctrl+Shift+V). Absent in old
    /// settings.json ⇒ `false` (keeps the previous behavior).
    #[serde(default)]
    pub restore_clipboard: bool,
    /// Start Wren together with the system (autostart). Absent in old
    /// settings.json ⇒ `false`. The composition layer reconciles the OS's state
    /// with this value at boot and on save (the autostart plugin is the
    /// authority).
    #[serde(default)]
    pub launch_at_login: bool,
    /// Minimum severity captured by the local logger. Absent in old
    /// settings.json ⇒ `Info`.
    #[serde(default)]
    pub log_level: LogLevel,
}

fn default_compress_pauses_over_ms() -> Option<u64> {
    Some(2000)
}

fn default_play_sounds() -> bool {
    true
}

fn default_cancel_shortcut() -> String {
    "Escape".into()
}

/// Catalog of presets the UI offers on "add provider". The single source of
/// the configuration literals — `Settings`'s `Default` derives from here, so as
/// not to duplicate base_url/model. `custom` is the blank template for the user
/// to point at their own provider.
pub fn factory_presets() -> Vec<ProviderConfig> {
    vec![
        ProviderConfig {
            id: "groq".into(),
            label: "Groq Whisper".into(),
            kind: ProviderKind::RemoteApi,
            base_url: "https://api.groq.com/openai/v1".into(),
            api_key: None,
            model: "whisper-large-v3-turbo".into(),
            sends_audio_externally: true,
        },
        ProviderConfig {
            id: "openai".into(),
            label: "OpenAI".into(),
            kind: ProviderKind::RemoteApi,
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            model: "whisper-1".into(),
            sends_audio_externally: true,
        },
        ProviderConfig {
            id: "local-server".into(),
            label: "Local server".into(),
            kind: ProviderKind::RemoteApi,
            base_url: "http://localhost:8555/v1".into(),
            api_key: None,
            model: "whisper-1".into(),
            sends_audio_externally: false,
        },
        ProviderConfig {
            id: "custom".into(),
            label: "Custom".into(),
            kind: ProviderKind::RemoteApi,
            base_url: "".into(),
            api_key: None,
            model: "".into(),
            sends_audio_externally: true,
        },
    ]
}

/// Is the `base_url`'s host external (does the audio leave the machine)?
/// Returns `false` for loopback (localhost, 127.0.0.1, ::1, [::1]) and `true`
/// for any other destination. It is the backend's authority over egress (doc
/// 02, invariant 4): the UI only reflects what this decides.
pub fn egress_is_external(base_url: &str) -> bool {
    match extract_host(base_url) {
        Some(host) => !is_local_host(&host),
        // No recognizable host (e.g.: empty base_url): assume external out of
        // caution — better to warn needlessly than to leak audio silently.
        None => true,
    }
}

/// Extracts the host from a URL without depending on a parsing crate: discards
/// the scheme (`http://`), cuts at `/`, `?` or `#`, removes credentials before
/// `@` and the port after `:`. Preserves the IPv6 brackets (`[::1]`).
fn extract_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    // Authority = everything before the first path/query/fragment delimiter.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Discards "user:password@" if present.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host_port = host_port.trim();
    if host_port.is_empty() {
        return None;
    }
    // IPv6 comes in brackets: "[::1]:8555" ⇒ keeps "[::1]".
    if let Some(end) = host_port.strip_prefix('[').and_then(|_| host_port.find(']')) {
        return Some(host_port[..=end].to_string());
    }
    // Common case: cut the port at ":".
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn is_local_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

impl Settings {
    pub fn active_provider(&self) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == self.active_provider_id)
    }
}

impl Default for Settings {
    fn default() -> Self {
        // Reuses the catalog as the single source: the factory default is the
        // groq + local-server subset (active = groq), without repeating literals.
        let providers = factory_presets()
            .into_iter()
            .filter(|p| p.id == "groq" || p.id == "local-server")
            .collect();
        Settings {
            active_provider_id: "groq".into(),
            providers,
            shortcut: "ctrl+shift+space".into(),
            activation_mode: ActivationMode::default(),
            cancel_shortcut: default_cancel_shortcut(),
            language: Some("pt".into()),
            compress_pauses_over_ms: default_compress_pauses_over_ms(),
            input_device: None,
            play_sounds: default_play_sounds(),
            paste_method: PasteMethod::default(),
            restore_clipboard: false,
            launch_at_login: false,
            log_level: LogLevel::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backward compatibility: settings.json predating the pause compression
    /// (without `compress_pauses_over_ms`) loads with the default on (2000 ms).
    #[test]
    fn settings_missing_compress_pauses_loads_default() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": "pt"
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.compress_pauses_over_ms, Some(2000));
    }

    /// An explicit `null` in the JSON means off — it does not become the default.
    #[test]
    fn settings_with_compress_pauses_null_stays_off() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": null,
            "compress_pauses_over_ms": null
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.compress_pauses_over_ms, None);
    }

    /// Backward compatibility: settings.json predating push-to-talk (without
    /// `activation_mode` or `cancel_shortcut`) loads with the defaults —
    /// Toggle and "Escape".
    #[test]
    fn settings_missing_activation_loads_toggle_and_escape() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": "pt"
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.activation_mode, ActivationMode::Toggle);
        assert_eq!(settings.cancel_shortcut, "Escape");
    }

    /// The mode's serde values are snake_case ("toggle" / "push_to_talk").
    #[test]
    fn activation_mode_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ActivationMode::PushToTalk).unwrap(),
            r#""push_to_talk""#
        );
        let parsed: ActivationMode = serde_json::from_str(r#""toggle""#).unwrap();
        assert_eq!(parsed, ActivationMode::Toggle);
    }

    /// Backward compatibility: settings.json predating the selectable microphone
    /// (without `input_device`) loads with the field `None` — stays on the OS's
    /// default microphone. Mirrors the `compress_pauses_over_ms` test.
    #[test]
    fn settings_missing_input_device_loads_as_none() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": "pt"
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.input_device, None);
    }

    /// Backward compatibility: settings.json predating the audible feedback
    /// (without `play_sounds`) loads with the sound ON — the default is `true`.
    #[test]
    fn settings_missing_play_sounds_loads_enabled() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": "pt"
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert!(settings.play_sounds);
    }

    /// An explicit `false` in the JSON turns the sound off — it does not become the default.
    #[test]
    fn settings_with_play_sounds_false_stays_off() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": "pt",
            "play_sounds": false
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert!(!settings.play_sounds);
    }

    /// Backward compatibility: settings.json predating the configurable
    /// injection (without `paste_method`, `restore_clipboard`,
    /// `launch_at_login`) loads with the defaults — paste via clipboard, no
    /// clipboard restore, no autostart.
    #[test]
    fn settings_missing_delivery_config_loads_defaults() {
        let raw = r#"{
            "active_provider_id": "groq",
            "providers": [],
            "shortcut": "ctrl+shift+space",
            "language": "pt"
        }"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        assert_eq!(settings.paste_method, PasteMethod::Paste);
        assert!(!settings.restore_clipboard);
        assert!(!settings.launch_at_login);
    }

    /// The paste method's serde values are snake_case, with `CtrlShiftV`
    /// becoming "ctrl_shift_v".
    #[test]
    fn paste_method_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&PasteMethod::CtrlShiftV).unwrap(),
            r#""ctrl_shift_v""#
        );
        assert_eq!(
            serde_json::to_string(&PasteMethod::Wtype).unwrap(),
            r#""wtype""#
        );
        let parsed: PasteMethod = serde_json::from_str(r#""type""#).unwrap();
        assert_eq!(parsed, PasteMethod::Type);
    }

    /// The `Default` derives from the catalog: groq + local-server, active = groq.
    #[test]
    fn default_derives_from_catalog_groq_and_local() {
        let settings = Settings::default();
        assert_eq!(settings.active_provider_id, "groq");
        let ids: Vec<&str> = settings.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["groq", "local-server"]);
    }

    /// `egress_is_external`: loopback is internal; any other host is external.
    #[test]
    fn egress_distinguishes_local_from_remote() {
        // Local ⇒ false.
        assert!(!egress_is_external("http://localhost:8555/v1"));
        assert!(!egress_is_external("http://127.0.0.1:8555/v1"));
        assert!(!egress_is_external("http://[::1]:8555/v1"));
        assert!(!egress_is_external("http://LocalHost/v1"));
        // Remote ⇒ true.
        assert!(egress_is_external("https://api.groq.com/openai/v1"));
        assert!(egress_is_external("https://api.openai.com/v1"));
        assert!(egress_is_external("http://192.168.0.10:8555/v1"));
        assert!(egress_is_external("https://user:pass@example.com/v1"));
        // No recognizable host ⇒ external out of caution.
        assert!(egress_is_external(""));
    }

    /// History backward compatibility: an old JSONL line (without
    /// `recorded_duration_ms`) remains readable, with the field `None`.
    #[test]
    fn old_history_line_missing_recorded_duration_loads() {
        let old_line = r#"{"text":"old","language":"pt","provider_id":"groq","model":"m","audio_duration_ms":1000,"latency_ms":50,"created_at_ms":111}"#;
        let entry: HistoryEntry = serde_json::from_str(old_line).unwrap();
        assert_eq!(entry.recorded_duration_ms, None);
        assert_eq!(entry.audio_duration_ms, 1000);
        assert_eq!(entry.status, EntryStatus::Done);
    }
}

/// States of a dictation session (doc 02: DictationSession).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Recording,
    Transcribing,
}
