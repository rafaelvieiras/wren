//! The `PerformDictation` use case (doc 02). Orchestrates the flow by calling
//! only ports — it does not know whether the Transcriber is Groq, localhost or
//! embedded.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::domain::{
    HistoryEntry, SessionMetrics, SessionOutcome, SessionState, Stage, StageTiming, Transcript,
    TranscriptionOptions, VadOutcome,
};
use crate::ports::{
    AudioSource, Clock, Feedback, HistoryStore, PortError, RecordingStore, Telemetry, TextSink,
    Transcriber, Vad,
};

/// Minimum speech to be worth sending (doc 08 §3): below this it is almost
/// always a shortcut pressed by mistake.
const MIN_SPEECH_MS: u64 = 300;

pub struct DictationService {
    audio: Arc<dyn AudioSource>,
    vad: Arc<dyn Vad>,
    transcriber: Arc<dyn Transcriber>,
    sink: Arc<dyn TextSink>,
    history: Arc<dyn HistoryStore>,
    recordings: Arc<dyn RecordingStore>,
    feedback: Arc<dyn Feedback>,
    clock: Arc<dyn Clock>,
    telemetry: Arc<dyn Telemetry>,
    state: Mutex<SessionState>,
    /// The session's epoch. Increments when a cancellation happens during
    /// transcription: the thread hung on the network compares the epoch it
    /// captured when it returns and, if it diverged, discards the result (see
    /// `finish`/`cancel`). This is what makes Escape return to Idle immediately,
    /// without waiting for the network timeout.
    epoch: AtomicU64,
}

/// What happened in a `toggle()` — the caller (global shortcut) does not need
/// to know about the internal state.
#[derive(Debug, PartialEq, Eq)]
pub enum ToggleOutcome {
    Started,
    Finished,
    /// Toggle received while transcribing — ignored.
    Busy,
}

impl DictationService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        audio: Arc<dyn AudioSource>,
        vad: Arc<dyn Vad>,
        transcriber: Arc<dyn Transcriber>,
        sink: Arc<dyn TextSink>,
        history: Arc<dyn HistoryStore>,
        recordings: Arc<dyn RecordingStore>,
        feedback: Arc<dyn Feedback>,
        clock: Arc<dyn Clock>,
        telemetry: Arc<dyn Telemetry>,
    ) -> Self {
        DictationService {
            audio,
            vad,
            transcriber,
            sink,
            history,
            recordings,
            feedback,
            clock,
            telemetry,
            state: Mutex::new(SessionState::Idle),
            epoch: AtomicU64::new(0),
        }
    }

    pub fn state(&self) -> SessionState {
        *self.state.lock().unwrap()
    }

    /// A single press of the global shortcut: starts recording if idle,
    /// finishes (transcribes and delivers) if recording.
    pub fn toggle(&self, options: &TranscriptionOptions) -> Result<ToggleOutcome, PortError> {
        let current = self.state();
        match current {
            SessionState::Idle => {
                self.start_recording().inspect_err(|e| {
                    self.feedback.failed(&e.to_string());
                })?;
                Ok(ToggleOutcome::Started)
            }
            SessionState::Recording => {
                self.finish(options)?;
                Ok(ToggleOutcome::Finished)
            }
            SessionState::Transcribing => Ok(ToggleOutcome::Busy),
        }
    }

    /// Cancels the ongoing session, discarding the audio. Works both during
    /// recording and during transcription: in the latter case the transcription
    /// may be hung on a slow/dead network call — the cancellation increments the
    /// epoch (the transcription thread discards the result when it returns) and
    /// returns to Idle IMMEDIATELY, without waiting for the timeout. It is
    /// Escape's emergency exit.
    pub fn cancel(&self) {
        // Transition to Idle under the lock, then do the (possibly slow)
        // teardown work AFTER releasing it — same discipline as `finish`:
        // never hold `state` across a blocking call into the audio adapter,
        // or a wedged device teardown (e.g. cpal stream drop stuck on a
        // sleep/resume or a yanked USB mic) would deadlock every future
        // `state()`/`toggle()`/`cancel()` call, including the one driving the
        // global shortcut handler itself.
        let previous = {
            let mut state = self.state.lock().unwrap();
            let previous = *state;
            match previous {
                SessionState::Recording => *state = SessionState::Idle,
                SessionState::Transcribing => {
                    self.epoch.fetch_add(1, Ordering::SeqCst);
                    *state = SessionState::Idle;
                }
                SessionState::Idle => {}
            }
            previous
        };
        match previous {
            SessionState::Recording => {
                self.audio.abort();
                self.feedback.cancelled();
            }
            SessionState::Transcribing => self.feedback.cancelled(),
            SessionState::Idle => {}
        }
    }

    fn start_recording(&self) -> Result<(), PortError> {
        // Capture starts BEFORE the feedback: the overlay is never on the
        // audio's critical path (doc 07, latency invariant).
        self.audio.start()?;
        *self.state.lock().unwrap() = SessionState::Recording;
        self.feedback.recording_started();
        Ok(())
    }

    fn finish(&self, options: &TranscriptionOptions) -> Result<(), PortError> {
        // Capture the epoch under the same lock as the transition to
        // Transcribing: if a cancellation races during the network call, it
        // increments the epoch and this thread detects the divergence when it
        // returns (see `transcribe_and_deliver` and the state resumption below).
        // Capturing it here closes the race between toggle and finish (cancelled
        // midway ⇒ nothing to finish).
        let my_epoch = {
            let mut state = self.state.lock().unwrap();
            if *state != SessionState::Recording {
                return Ok(());
            }
            *state = SessionState::Transcribing;
            self.epoch.load(Ordering::SeqCst)
        };
        // The overlay enters "processing" RIGHT at the stop — before stop() and
        // the VAD, which on long audio take perceptible wall-clock time. Without
        // this the bubble stays stuck on the recording waveform (frozen, since
        // no more levels arrive) until the VAD finishes, and only then shows the
        // processing. Covers the whole end-of-recording → delivery span.
        self.feedback.transcribing();

        let result = self.transcribe_and_deliver(options, my_epoch);

        // Only resume the state if nobody cancelled/restarted midway: a cancel
        // during transcription already returned to Idle and emitted the feedback
        // — this thread must not overwrite that (nor a new session started
        // afterwards). The epoch divergence is the signal.
        let mut state = self.state.lock().unwrap();
        if self.epoch.load(Ordering::SeqCst) == my_epoch {
            *state = SessionState::Idle;
            if let Err(err) = &result {
                // A provider error does not crash the app: it becomes feedback (doc 02, inv. 7).
                self.feedback.failed(&err.to_string());
            }
        }
        result
    }

    fn transcribe_and_deliver(
        &self,
        options: &TranscriptionOptions,
        my_epoch: u64,
    ) -> Result<(), PortError> {
        // Local telemetry: measures each stage's wall-clock (via Clock) and
        // samples the process's RSS at the heavy points. Never leaves the machine.
        let rss_start = self.telemetry.sample_rss();
        let mut rss_peak = rss_start;
        let mut stages: Vec<StageTiming> = Vec::new();

        // Stage: end the capture and materialize the clip.
        let t = self.clock.now_ms();
        let clip = self.audio.stop()?;
        let clip_bytes = (clip.samples.len() * 2) as u64;
        stages.push(StageTiming {
            stage: Stage::CaptureStop,
            duration_ms: self.clock.now_ms().saturating_sub(t),
            audio_bytes: Some(clip_bytes),
        });
        if clip.is_empty() {
            return Err(PortError::Other("empty recording".into()));
        }
        // Duration actually dictated, before the VAD shortened it (trim + pause
        // compression) — the history records both so as not to lie to the user.
        let recorded_duration_ms = clip.duration_ms;

        // Stage: VAD gate (doc 08 §3). Silence/an accident does not pay the
        // provider's 10 s minimum nor turn into a hallucination. A discard is
        // not a session error — the feedback explains and the flow ends cleanly.
        let t = self.clock.now_ms();
        let vad_outcome = self.vad.gate_and_trim(&clip);
        let vad_ms = self.clock.now_ms().saturating_sub(t);
        let clip = match vad_outcome {
            VadOutcome::Speech { clip, speech_ms } if speech_ms >= MIN_SPEECH_MS => clip,
            _ => {
                stages.push(StageTiming {
                    stage: Stage::Vad,
                    duration_ms: vad_ms,
                    audio_bytes: Some(clip_bytes),
                });
                self.emit_metrics(
                    SessionOutcome::DiscardedNoSpeech,
                    String::new(),
                    String::new(),
                    recorded_duration_ms,
                    0,
                    stages,
                    rss_start,
                    rss_peak,
                );
                self.feedback.failed("no speech detected — nothing sent");
                return Ok(());
            }
        };
        let sent_bytes = (clip.samples.len() * 2) as u64;
        stages.push(StageTiming {
            stage: Stage::Vad,
            duration_ms: vad_ms,
            audio_bytes: Some(sent_bytes),
        });

        // Stage: safety net — the audio goes to disk BEFORE the API. If the
        // transcription fails, the recording survives in the history for retry.
        let t = self.clock.now_ms();
        let preserved = self.recordings.save(&clip).ok();
        stages.push(StageTiming {
            stage: Stage::PersistAudio,
            duration_ms: self.clock.now_ms().saturating_sub(t),
            audio_bytes: Some(sent_bytes),
        });

        // Stage: transcription (the network call — usually dominates the wall-clock).
        let started = self.clock.now_ms();
        let result = self.transcriber.transcribe(&clip, options).and_then(|t| {
            if t.text.trim().is_empty() {
                Err(PortError::Other("empty transcription".into()))
            } else {
                Ok(t)
            }
        });
        let transcribe_ms = self.clock.now_ms().saturating_sub(started);
        stages.push(StageTiming {
            stage: Stage::Transcribe,
            duration_ms: transcribe_ms,
            audio_bytes: Some(sent_bytes),
        });
        rss_peak = max_rss(rss_peak, self.telemetry.sample_rss());

        // Cancellation point: if the user aborted (Escape) during the network
        // call, the epoch diverged. Discard everything — no delivery, no history
        // record — and clean up the preserved audio. `cancel()` already returned
        // the session to Idle and emitted the feedback; delivering here would
        // paste a text the user already gave up on receiving.
        if self.epoch.load(Ordering::SeqCst) != my_epoch {
            if let Some(path) = preserved {
                self.recordings.delete(&path);
            }
            self.emit_metrics(
                SessionOutcome::Cancelled,
                String::new(),
                String::new(),
                recorded_duration_ms,
                clip.duration_ms,
                stages,
                rss_start,
                rss_peak,
            );
            return Ok(());
        }

        let mut transcript = match result {
            Ok(t) => t,
            Err(err) => {
                let mut entry = HistoryEntry::failed(
                    err.to_string(),
                    preserved,
                    clip.duration_ms,
                    self.clock.now_ms(),
                );
                entry.recorded_duration_ms = Some(recorded_duration_ms);
                // Even without history the audio is already on disk; propagate
                // the original error, which is what the user needs to see.
                let _ = self.history.save(&entry);
                self.emit_metrics(
                    SessionOutcome::Failed,
                    String::new(),
                    String::new(),
                    recorded_duration_ms,
                    clip.duration_ms,
                    stages,
                    rss_start,
                    rss_peak,
                );
                return Err(err);
            }
        };

        // The transcription span IS the provider's latency (same measurement).
        transcript.latency_ms = transcribe_ms;
        transcript.audio_duration_ms = clip.duration_ms;
        transcript.recorded_duration_ms = Some(recorded_duration_ms);
        transcript.created_at_ms = self.clock.now_ms();

        // History BEFORE delivery: if the paste fails, the text is already saved.
        if let Err(err) = self.history.save(&HistoryEntry::done(&transcript)) {
            self.feedback.failed(&format!("history not saved: {err}"));
        }
        if let Some(path) = preserved {
            self.recordings.delete(&path);
        }

        // Stage: delivery to the focused app.
        let t = self.clock.now_ms();
        let deliver_result = self.sink.deliver(&transcript.text);
        stages.push(StageTiming {
            stage: Stage::Deliver,
            duration_ms: self.clock.now_ms().saturating_sub(t),
            audio_bytes: None,
        });
        rss_peak = max_rss(rss_peak, self.telemetry.sample_rss());

        // Record BEFORE propagating a delivery error: it already transcribed and
        // saved; the metric must not be lost just because the paste failed.
        let outcome = if deliver_result.is_ok() {
            SessionOutcome::Delivered
        } else {
            SessionOutcome::Failed
        };
        self.emit_metrics(
            outcome,
            transcript.provider_id.clone(),
            transcript.model.clone(),
            recorded_duration_ms,
            clip.duration_ms,
            stages,
            rss_start,
            rss_peak,
        );

        deliver_result?;
        self.feedback.finished(&transcript);
        Ok(())
    }

    /// Builds and reports the session's metrics to the telemetry port.
    /// `total_ms` is the wall-clock sum of the measured stages.
    #[allow(clippy::too_many_arguments)]
    fn emit_metrics(
        &self,
        outcome: SessionOutcome,
        provider_id: String,
        model: String,
        recorded_duration_ms: u64,
        sent_audio_duration_ms: u64,
        stages: Vec<StageTiming>,
        rss_start: Option<u64>,
        rss_peak: Option<u64>,
    ) {
        let total_ms = stages.iter().map(|s| s.duration_ms).sum();
        self.telemetry.record(&SessionMetrics {
            created_at_ms: self.clock.now_ms(),
            outcome,
            provider_id,
            model,
            recorded_duration_ms,
            sent_audio_duration_ms,
            total_ms,
            stages,
            rss_start_bytes: rss_start,
            rss_peak_bytes: rss_peak,
        });
    }

    /// Resends a failed entry to the active provider. It does not inject the
    /// text — the focused app is no longer the original destination; the caller
    /// decides what to do (e.g.: clipboard). On success the entry becomes `done`
    /// (keeping the original instant) and the preserved audio is deleted.
    pub fn retry(
        &self,
        entry: &HistoryEntry,
        options: &TranscriptionOptions,
    ) -> Result<Transcript, PortError> {
        let path = entry
            .audio_path
            .as_deref()
            .ok_or_else(|| PortError::Other("entry with no preserved audio".into()))?;
        let clip = self.recordings.load(path)?;

        let rss_start = self.telemetry.sample_rss();
        let sent_bytes = (clip.samples.len() * 2) as u64;
        let recorded_ms = entry.recorded_duration_ms.unwrap_or(clip.duration_ms);

        let started = self.clock.now_ms();
        let result = self.transcriber.transcribe(&clip, options).and_then(|t| {
            if t.text.trim().is_empty() {
                Err(PortError::Other("empty transcription".into()))
            } else {
                Ok(t)
            }
        });
        let transcribe_ms = self.clock.now_ms().saturating_sub(started);
        let rss_peak = max_rss(rss_start, self.telemetry.sample_rss());
        let stages = vec![StageTiming {
            stage: Stage::Transcribe,
            duration_ms: transcribe_ms,
            audio_bytes: Some(sent_bytes),
        }];

        let mut transcript = match result {
            Ok(t) => t,
            Err(err) => {
                self.emit_metrics(
                    SessionOutcome::Failed,
                    String::new(),
                    String::new(),
                    recorded_ms,
                    clip.duration_ms,
                    stages,
                    rss_start,
                    rss_peak,
                );
                return Err(err);
            }
        };
        transcript.latency_ms = transcribe_ms;
        transcript.audio_duration_ms = clip.duration_ms;
        // The retry uses the clip already processed by the VAD; the original
        // duration only exists in the failure record — propagate it so as not
        // to lose the honesty.
        transcript.recorded_duration_ms = entry.recorded_duration_ms;
        transcript.created_at_ms = entry.created_at_ms;

        self.history.resolve(entry.created_at_ms, &transcript)?;
        self.recordings.delete(path);
        self.emit_metrics(
            SessionOutcome::Delivered,
            transcript.provider_id.clone(),
            transcript.model.clone(),
            recorded_ms,
            clip.duration_ms,
            stages,
            rss_start,
            rss_peak,
        );
        Ok(transcript)
    }
}

/// The larger of two RSS samples, tolerating `None` (an OS that does not expose memory).
fn max_rss(current: Option<u64>, sample: Option<u64>) -> Option<u64> {
    match (current, sample) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Builds a partial `Transcript` — a helper for adapters to fill in only what
/// they know (text/language/provider); the use case completes the rest.
pub fn partial_transcript(
    text: String,
    language: Option<String>,
    provider_id: &str,
    model: &str,
) -> Transcript {
    Transcript {
        text,
        language,
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        audio_duration_ms: 0,
        latency_ms: 0,
        created_at_ms: 0,
        recorded_duration_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AudioClip, EntryStatus};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct FakeAudio {
        aborted: AtomicBool,
        fail_on_start: bool,
    }
    impl FakeAudio {
        fn new() -> Self {
            FakeAudio { aborted: AtomicBool::new(false), fail_on_start: false }
        }
    }
    impl AudioSource for FakeAudio {
        fn start(&self) -> Result<(), PortError> {
            if self.fail_on_start {
                return Err(PortError::AudioUnavailable("no mic".into()));
            }
            Ok(())
        }
        fn stop(&self) -> Result<AudioClip, PortError> {
            // 24000 samples @ 16 kHz = 1500 ms.
            Ok(AudioClip {
                samples: vec![0i16; 24_000],
                sample_rate: 16_000,
                duration_ms: 1500,
            })
        }
        fn abort(&self) {
            self.aborted.store(true, Ordering::SeqCst);
        }
    }

    /// Configurable VAD: in passthrough it returns the whole clip as speech;
    /// otherwise it simulates the gate (NoSpeech or short speech).
    struct FakeVad {
        mode: VadMode,
    }
    enum VadMode {
        Passthrough,
        NoSpeech,
        ShortSpeech(u64),
        /// Simulates trim + pause compression: returns the clip halved.
        Halved,
    }
    impl Vad for FakeVad {
        fn gate_and_trim(&self, clip: &AudioClip) -> VadOutcome {
            match self.mode {
                VadMode::Passthrough => VadOutcome::Speech {
                    clip: clip.clone(),
                    speech_ms: clip.duration_ms,
                },
                VadMode::NoSpeech => VadOutcome::NoSpeech,
                VadMode::ShortSpeech(ms) => {
                    VadOutcome::Speech { clip: clip.clone(), speech_ms: ms }
                }
                VadMode::Halved => {
                    let samples = clip.samples[..clip.samples.len() / 2].to_vec();
                    let duration_ms = clip.duration_ms / 2;
                    VadOutcome::Speech {
                        clip: AudioClip { samples, sample_rate: clip.sample_rate, duration_ms },
                        speech_ms: duration_ms,
                    }
                }
            }
        }
    }

    /// Probe VAD: at the moment it runs, it records whether the feedback has
    /// already emitted "transcribing". Proves that the overlay enters processing
    /// BEFORE the VAD's heavy work — the frozen-bubble regression on long audio.
    struct ProbeVad {
        feedback: Arc<FakeFeedback>,
        transcribing_before_vad: Arc<AtomicBool>,
    }
    impl Vad for ProbeVad {
        fn gate_and_trim(&self, clip: &AudioClip) -> VadOutcome {
            let seen = self
                .feedback
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e == "transcribing");
            self.transcribing_before_vad.store(seen, Ordering::SeqCst);
            VadOutcome::Speech { clip: clip.clone(), speech_ms: clip.duration_ms }
        }
    }

    struct FakeTranscriber {
        text: String,
        fail: AtomicBool,
    }
    impl Transcriber for FakeTranscriber {
        fn transcribe(
            &self,
            _clip: &AudioClip,
            _options: &TranscriptionOptions,
        ) -> Result<Transcript, PortError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(PortError::ProviderRejected { status: 401, message: "bad key".into() });
            }
            Ok(partial_transcript(self.text.clone(), Some("pt".into()), "fake", "fake-1"))
        }
    }

    /// Transcriber that blocks inside `transcribe` until the test releases it —
    /// simulates the hung network call, to exercise cancellation in the middle
    /// of the transcription. Signals its entry through one channel and waits on
    /// another.
    struct GatedTranscriber {
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        text: String,
    }
    impl Transcriber for GatedTranscriber {
        fn transcribe(
            &self,
            _clip: &AudioClip,
            _options: &TranscriptionOptions,
        ) -> Result<Transcript, PortError> {
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = self.release.lock().unwrap().recv();
            Ok(partial_transcript(self.text.clone(), Some("pt".into()), "fake", "fake-1"))
        }
    }

    #[derive(Default)]
    struct FakeSink {
        delivered: Mutex<Vec<String>>,
    }
    impl TextSink for FakeSink {
        fn deliver(&self, text: &str) -> Result<(), PortError> {
            self.delivered.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeHistory {
        saved: Mutex<Vec<HistoryEntry>>,
    }
    impl HistoryStore for FakeHistory {
        fn save(&self, entry: &HistoryEntry) -> Result<(), PortError> {
            self.saved.lock().unwrap().push(entry.clone());
            Ok(())
        }
        fn recent(&self, _limit: usize) -> Result<Vec<HistoryEntry>, PortError> {
            Ok(self.saved.lock().unwrap().clone())
        }
        fn resolve(&self, created_at_ms: u64, transcript: &Transcript) -> Result<(), PortError> {
            let mut saved = self.saved.lock().unwrap();
            let entry = saved
                .iter_mut()
                .find(|e| e.created_at_ms == created_at_ms)
                .ok_or_else(|| PortError::Storage("entry not found".into()))?;
            *entry = HistoryEntry::done(transcript);
            Ok(())
        }
    }

    /// Keeps clips in memory, indexed by a sequential key.
    #[derive(Default)]
    struct FakeRecordings {
        clips: Mutex<Vec<(String, AudioClip)>>,
        next: AtomicU64,
    }
    impl RecordingStore for FakeRecordings {
        fn save(&self, clip: &AudioClip) -> Result<String, PortError> {
            let key = format!("rec-{}", self.next.fetch_add(1, Ordering::SeqCst));
            self.clips.lock().unwrap().push((key.clone(), clip.clone()));
            Ok(key)
        }
        fn load(&self, key: &str) -> Result<AudioClip, PortError> {
            self.clips
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, c)| c.clone())
                .ok_or_else(|| PortError::Storage("recording not found".into()))
        }
        fn delete(&self, key: &str) {
            self.clips.lock().unwrap().retain(|(k, _)| k != key);
        }
    }

    #[derive(Default)]
    struct FakeFeedback {
        events: Mutex<Vec<String>>,
    }
    impl Feedback for FakeFeedback {
        fn recording_started(&self) {
            self.events.lock().unwrap().push("recording".into());
        }
        fn audio_level(&self, _level: f32) {}
        fn transcribing(&self) {
            self.events.lock().unwrap().push("transcribing".into());
        }
        fn finished(&self, _t: &Transcript) {
            self.events.lock().unwrap().push("finished".into());
        }
        fn failed(&self, msg: &str) {
            self.events.lock().unwrap().push(format!("failed: {msg}"));
        }
        fn cancelled(&self) {
            self.events.lock().unwrap().push("cancelled".into());
        }
    }

    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            // Advances 100ms on each query, so latency_ms is observable.
            self.0.fetch_add(100, Ordering::SeqCst)
        }
    }

    /// Captures the reported metrics, so the tests can inspect stages and
    /// outcome. `sample_rss` stays `None` (the core does not depend on the OS).
    #[derive(Default)]
    struct FakeTelemetry {
        records: Mutex<Vec<SessionMetrics>>,
    }
    impl Telemetry for FakeTelemetry {
        fn record(&self, metrics: &SessionMetrics) {
            self.records.lock().unwrap().push(metrics.clone());
        }
    }

    struct Harness {
        svc: Arc<DictationService>,
        transcriber: Arc<FakeTranscriber>,
        sink: Arc<FakeSink>,
        history: Arc<FakeHistory>,
        recordings: Arc<FakeRecordings>,
        feedback: Arc<FakeFeedback>,
        telemetry: Arc<FakeTelemetry>,
    }

    fn service(text: &str, fail: bool) -> Harness {
        service_with_vad(text, fail, VadMode::Passthrough)
    }

    fn service_with_vad(text: &str, fail: bool, vad: VadMode) -> Harness {
        let transcriber =
            Arc::new(FakeTranscriber { text: text.into(), fail: AtomicBool::new(fail) });
        let sink = Arc::new(FakeSink::default());
        let history = Arc::new(FakeHistory::default());
        let recordings = Arc::new(FakeRecordings::default());
        let feedback = Arc::new(FakeFeedback::default());
        let telemetry = Arc::new(FakeTelemetry::default());
        let svc = Arc::new(DictationService::new(
            Arc::new(FakeAudio::new()),
            Arc::new(FakeVad { mode: vad }),
            transcriber.clone(),
            sink.clone(),
            history.clone(),
            recordings.clone(),
            feedback.clone(),
            Arc::new(FakeClock(AtomicU64::new(1_000))),
            telemetry.clone(),
        ));
        Harness { svc, transcriber, sink, history, recordings, feedback, telemetry }
    }

    #[test]
    fn overlay_enters_processing_before_vad() {
        // Regression: feedback.transcribing() must be emitted at the moment of
        // the stop, BEFORE audio.stop()/vad.gate_and_trim(). On long audio the
        // VAD takes real wall-clock time; if the "transcribing" came afterwards
        // (as in the bug), the bubble would be stuck on the recording waveform,
        // frozen during that interval, with no sign that something is happening.
        let feedback = Arc::new(FakeFeedback::default());
        let transcribing_before_vad = Arc::new(AtomicBool::new(false));
        let svc = Arc::new(DictationService::new(
            Arc::new(FakeAudio::new()),
            Arc::new(ProbeVad {
                feedback: feedback.clone(),
                transcribing_before_vad: transcribing_before_vad.clone(),
            }),
            Arc::new(FakeTranscriber { text: "hi".into(), fail: AtomicBool::new(false) }),
            Arc::new(FakeSink::default()),
            Arc::new(FakeHistory::default()),
            Arc::new(FakeRecordings::default()),
            feedback.clone(),
            Arc::new(FakeClock(AtomicU64::new(1_000))),
            Arc::new(FakeTelemetry::default()),
        ));
        let opts = TranscriptionOptions::default();

        svc.toggle(&opts).unwrap(); // starts recording
        svc.toggle(&opts).unwrap(); // finishes → triggers the processing

        assert!(
            transcribing_before_vad.load(Ordering::SeqCst),
            "feedback.transcribing() should precede the VAD's gate_and_trim",
        );
    }

    #[test]
    fn full_flow_delivers_text_and_saves_history() {
        let h = service("hello world", false);
        let opts = TranscriptionOptions::default();

        assert_eq!(h.svc.toggle(&opts).unwrap(), ToggleOutcome::Started);
        assert_eq!(h.svc.state(), SessionState::Recording);

        assert_eq!(h.svc.toggle(&opts).unwrap(), ToggleOutcome::Finished);
        assert_eq!(h.svc.state(), SessionState::Idle);

        assert_eq!(h.sink.delivered.lock().unwrap().as_slice(), ["hello world"]);
        let saved = h.history.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].status, EntryStatus::Done);
        assert_eq!(saved[0].latency_ms, 100);
        assert_eq!(saved[0].audio_duration_ms, 1500);
        // Success leaves no audio behind.
        assert!(h.recordings.clips.lock().unwrap().is_empty());
        assert_eq!(
            h.feedback.events.lock().unwrap().as_slice(),
            ["recording", "transcribing", "finished"]
        );
    }

    #[test]
    fn vad_that_shortens_clip_records_both_durations() {
        // FakeAudio records 1500 ms; the VAD (Halved) sends only 750 ms. The
        // history must be honest: it keeps the sent duration AND the dictated
        // duration.
        let h = service_with_vad("compressed", false, VadMode::Halved);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        h.svc.toggle(&opts).unwrap();

        let saved = h.history.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].audio_duration_ms, 750);
        assert_eq!(saved[0].recorded_duration_ms, Some(1500));
    }

    #[test]
    fn provider_failure_also_records_original_duration() {
        let h = service_with_vad("", true, VadMode::Halved);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        assert!(h.svc.toggle(&opts).is_err());

        let saved = h.history.saved.lock().unwrap();
        assert_eq!(saved[0].audio_duration_ms, 750);
        assert_eq!(saved[0].recorded_duration_ms, Some(1500));
    }

    #[test]
    fn vad_no_speech_discards_without_error_or_side_effects() {
        let h = service_with_vad("never", false, VadMode::NoSpeech);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        // A discard is not a session error: the toggle ends cleanly.
        assert_eq!(h.svc.toggle(&opts).unwrap(), ToggleOutcome::Finished);
        assert_eq!(h.svc.state(), SessionState::Idle);

        assert!(h.sink.delivered.lock().unwrap().is_empty());
        assert!(h.history.saved.lock().unwrap().is_empty());
        assert!(h.recordings.clips.lock().unwrap().is_empty());
        let events = h.feedback.events.lock().unwrap();
        assert!(events.iter().any(|e| e.contains("failed: no speech")));
    }

    #[test]
    fn vad_with_short_speech_discards_as_silence() {
        // 200 ms of speech < MIN_SPEECH_MS: same outcome as NoSpeech.
        let h = service_with_vad("never", false, VadMode::ShortSpeech(200));
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        assert_eq!(h.svc.toggle(&opts).unwrap(), ToggleOutcome::Finished);

        assert!(h.sink.delivered.lock().unwrap().is_empty());
        assert!(h.history.saved.lock().unwrap().is_empty());
        assert!(h.recordings.clips.lock().unwrap().is_empty());
        let events = h.feedback.events.lock().unwrap();
        assert!(events.iter().any(|e| e.contains("failed: no speech")));
    }

    #[test]
    fn provider_error_preserves_audio_and_records_failure() {
        let h = service("", true);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        assert!(h.svc.toggle(&opts).is_err());

        assert_eq!(h.svc.state(), SessionState::Idle);
        assert!(h.sink.delivered.lock().unwrap().is_empty());

        // The dictation was NOT lost: the failed entry points to the saved audio.
        let saved = h.history.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].status, EntryStatus::Failed);
        assert_eq!(saved[0].audio_duration_ms, 1500);
        assert!(saved[0].error.as_deref().unwrap().contains("401"));
        let audio_path = saved[0].audio_path.clone().unwrap();
        assert!(h.recordings.load(&audio_path).is_ok());

        let events = h.feedback.events.lock().unwrap();
        assert!(events.last().unwrap().starts_with("failed:"));
    }

    #[test]
    fn retry_resolves_entry_and_deletes_audio() {
        let h = service("second chance", true);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        assert!(h.svc.toggle(&opts).is_err());
        let entry = h.history.recent(10).unwrap().pop().unwrap();

        // Provider comes back online; the retry uses the preserved audio.
        h.transcriber.fail.store(false, Ordering::SeqCst);
        let transcript = h.svc.retry(&entry, &opts).unwrap();

        assert_eq!(transcript.text, "second chance");
        assert_eq!(transcript.created_at_ms, entry.created_at_ms);
        // The retry does not inject text — the caller decides the destination.
        assert!(h.sink.delivered.lock().unwrap().is_empty());

        let saved = h.history.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].status, EntryStatus::Done);
        assert_eq!(saved[0].text, "second chance");
        assert!(h.recordings.clips.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_retry_keeps_entry_and_audio() {
        let h = service("", true);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        assert!(h.svc.toggle(&opts).is_err());
        let entry = h.history.recent(10).unwrap().pop().unwrap();

        assert!(h.svc.retry(&entry, &opts).is_err());

        let saved = h.history.saved.lock().unwrap();
        assert_eq!(saved[0].status, EntryStatus::Failed);
        assert_eq!(h.recordings.clips.lock().unwrap().len(), 1);
    }

    #[test]
    fn cancel_discards_the_recording() {
        let h = service("never", false);
        let opts = TranscriptionOptions::default();

        h.svc.toggle(&opts).unwrap();
        h.svc.cancel();

        assert_eq!(h.svc.state(), SessionState::Idle);
        assert!(h.sink.delivered.lock().unwrap().is_empty());
        assert_eq!(h.feedback.events.lock().unwrap().last().unwrap(), "cancelled");
    }

    #[test]
    fn cancel_while_idle_is_noop() {
        let h = service("x", false);
        h.svc.cancel();
        assert!(h.feedback.events.lock().unwrap().is_empty());
    }

    #[test]
    fn cancel_during_transcription_returns_to_idle_and_discards_result() {
        // Simulates the hung-provider bug: the stop enters Transcribing and
        // blocks on the network; Escape (cancel) must take the session out of
        // Transcribing IMMEDIATELY, and the orphaned transcription, when it
        // returns, must not deliver anything.
        let (enter_tx, enter_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let transcriber = Arc::new(GatedTranscriber {
            entered: Mutex::new(Some(enter_tx)),
            release: Mutex::new(release_rx),
            text: "should not be delivered".into(),
        });
        let sink = Arc::new(FakeSink::default());
        let history = Arc::new(FakeHistory::default());
        let recordings = Arc::new(FakeRecordings::default());
        let feedback = Arc::new(FakeFeedback::default());
        let telemetry = Arc::new(FakeTelemetry::default());
        let svc = Arc::new(DictationService::new(
            Arc::new(FakeAudio::new()),
            Arc::new(FakeVad { mode: VadMode::Passthrough }),
            transcriber,
            sink.clone(),
            history.clone(),
            recordings.clone(),
            feedback.clone(),
            Arc::new(FakeClock(AtomicU64::new(1_000))),
            telemetry.clone(),
        ));
        let opts = TranscriptionOptions::default();

        svc.toggle(&opts).unwrap(); // starts recording

        // The stop runs on a thread and blocks inside transcribe.
        let svc_stop = svc.clone();
        let opts_stop = opts.clone();
        let stop = std::thread::spawn(move || svc_stop.toggle(&opts_stop).unwrap());

        // Wait for the transcription to start and cancel midway.
        enter_rx.recv().unwrap();
        assert_eq!(svc.state(), SessionState::Transcribing);
        svc.cancel();
        assert_eq!(svc.state(), SessionState::Idle, "cancel must return to Idle immediately");

        // Release the hung transcription; it must DISCARD the result.
        release_tx.send(()).unwrap();
        stop.join().unwrap();

        assert!(
            sink.delivered.lock().unwrap().is_empty(),
            "nothing may be delivered after the cancellation",
        );
        assert!(
            history.saved.lock().unwrap().iter().all(|e| e.status != EntryStatus::Done),
            "no 'done' entry after the cancellation",
        );
        assert!(
            recordings.clips.lock().unwrap().is_empty(),
            "the preserved audio must be cleaned up on discard",
        );
        let events = feedback.events.lock().unwrap();
        assert!(events.iter().any(|e| e == "cancelled"), "must emit cancelled");
        assert!(
            !events.iter().any(|e| e == "finished"),
            "must not emit finished after the cancellation",
        );

        // The telemetry records the cancelled session (with the partial stages).
        let records = telemetry.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, SessionOutcome::Cancelled);
    }

    #[test]
    fn telemetry_records_stages_on_delivery() {
        let h = service("hello world", false);
        let opts = TranscriptionOptions::default();
        h.svc.toggle(&opts).unwrap();
        h.svc.toggle(&opts).unwrap();

        let records = h.telemetry.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        let m = &records[0];
        assert_eq!(m.outcome, SessionOutcome::Delivered);
        assert_eq!(m.provider_id, "fake");
        // The happy path goes through all five stages, in order.
        let stages: Vec<Stage> = m.stages.iter().map(|s| s.stage).collect();
        assert_eq!(
            stages,
            vec![
                Stage::CaptureStop,
                Stage::Vad,
                Stage::PersistAudio,
                Stage::Transcribe,
                Stage::Deliver,
            ]
        );
        // Each stage measured by the FakeClock takes exactly 100 ms.
        assert!(m.stages.iter().all(|s| s.duration_ms == 100));
        assert_eq!(m.total_ms, 500);
        assert_eq!(m.recorded_duration_ms, 1500);
        assert_eq!(m.sent_audio_duration_ms, 1500);
    }

    #[test]
    fn telemetry_records_vad_discard() {
        let h = service_with_vad("never", false, VadMode::NoSpeech);
        let opts = TranscriptionOptions::default();
        h.svc.toggle(&opts).unwrap();
        h.svc.toggle(&opts).unwrap();

        let records = h.telemetry.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, SessionOutcome::DiscardedNoSpeech);
        assert_eq!(records[0].sent_audio_duration_ms, 0);
        // A discard never gets to transcribe or deliver.
        assert!(records[0].stages.iter().all(|s| s.stage != Stage::Transcribe));
    }

    #[test]
    fn telemetry_records_provider_failure() {
        let h = service("", true);
        let opts = TranscriptionOptions::default();
        h.svc.toggle(&opts).unwrap();
        assert!(h.svc.toggle(&opts).is_err());

        let records = h.telemetry.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, SessionOutcome::Failed);
        // The transcription was attempted (stage present), but there was no delivery.
        assert!(records[0].stages.iter().any(|s| s.stage == Stage::Transcribe));
        assert!(records[0].stages.iter().all(|s| s.stage != Stage::Deliver));
    }
}
