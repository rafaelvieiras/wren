//! # wren-adapters
//!
//! Concrete implementations of the `wren-core` ports. Each module is a
//! swappable adapter; none leaks into the core.

pub mod audio_cpal;
pub mod feedback_sound;
pub mod logging;
pub mod preprocess;
pub mod storage;
pub mod telemetry;
pub mod textsink_paste;
pub mod transcriber_remote;
pub mod vad_earshot;

pub use audio_cpal::{list_input_devices, CpalAudioSource, LevelCallback};
pub use feedback_sound::ToneFeedback;
pub use logging::{init_logging, set_log_level, LogBuffer, LogRecord};
pub use storage::{JsonSettingsStore, JsonlHistoryStore, SystemClock, WavRecordingStore};
pub use telemetry::JsonlTelemetryStore;
pub use textsink_paste::ClipboardPasteSink;
pub use transcriber_remote::{list_models, RemoteApiTranscriber};
pub use vad_earshot::EarshotVad;
