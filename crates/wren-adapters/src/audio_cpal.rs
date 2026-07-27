//! Adapter for the `AudioSource` port using cpal. `cpal::Stream` is not `Send`,
//! so each recording runs on a dedicated thread that owns the stream and
//! accumulates samples; `stop()` shuts the thread down and normalizes to the
//! domain's canonical form (PCM i16 mono 16 kHz — see `preprocess`).

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use wren_core::{AudioClip, AudioSource, PortError};

/// Level callback (RMS in [0,1]) — feeds the bubble animation.
pub type LevelCallback = Arc<dyn Fn(f32) + Send + Sync>;

/// How long `finish()` waits for the recording thread to tear down (stream
/// drop) before giving up on it. A wedged cpal teardown (sleep/resume
/// transition, a mic yanked mid-stream) must not block the caller forever —
/// callers include `DictationService::cancel`, invoked from the global
/// shortcut handler.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(3);

struct ActiveRecording {
    stop_tx: mpsc::Sender<()>,
    handle: JoinHandle<Result<RecordedAudio, PortError>>,
}

struct RecordedAudio {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

/// Enumerates the available microphones by name. A host/enumeration failure
/// never panics — it returns an empty `Vec` (the UI falls back to "default
/// microphone").
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        // In cpal 0.18 the device name comes from `Display` (`to_string()`),
        // not from `name()`.
        Ok(devices) => devices.map(|d| d.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

pub struct CpalAudioSource {
    on_level: LevelCallback,
    /// Microphone chosen by the user (device name). `None` = OS default.
    device_name: Option<String>,
    active: Mutex<Option<ActiveRecording>>,
}

impl CpalAudioSource {
    pub fn new(on_level: LevelCallback, device_name: Option<String>) -> Self {
        CpalAudioSource { on_level, device_name, active: Mutex::new(None) }
    }

    fn finish(&self) -> Result<Option<RecordedAudio>, PortError> {
        let Some(recording) = self.active.lock().unwrap().take() else {
            return Ok(None);
        };
        let _ = recording.stop_tx.send(());

        // Poll instead of a blocking `join()`: if the thread never reaches
        // the deadline, its `JoinHandle` is just dropped here — the OS thread
        // keeps running detached and gets reaped whenever it does eventually
        // finish. That is preferable to the caller (and anything relying on a
        // lock it holds) blocking indefinitely on a wedged teardown.
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        while !recording.handle.is_finished() {
            if Instant::now() >= deadline {
                return Err(PortError::AudioUnavailable(
                    "audio device teardown timed out".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let recorded = recording
            .handle
            .join()
            .map_err(|_| PortError::AudioUnavailable("audio thread died".into()))??;
        Ok(Some(recorded))
    }
}

impl AudioSource for CpalAudioSource {
    fn start(&self) -> Result<(), PortError> {
        let mut active = self.active.lock().unwrap();
        if active.is_some() {
            return Err(PortError::Other("recording already in progress".into()));
        }

        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        // The handshake guarantees that start() only returns once the stream is
        // open — capture begins here, before any visual feedback (doc 07).
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), PortError>>();
        let on_level = self.on_level.clone();
        let device_name = self.device_name.clone();

        let handle =
            std::thread::spawn(move || record_thread(stop_rx, ready_tx, on_level, device_name));

        ready_rx
            .recv()
            .map_err(|_| PortError::AudioUnavailable("audio thread did not start".into()))??;

        *active = Some(ActiveRecording { stop_tx, handle });
        Ok(())
    }

    fn stop(&self) -> Result<AudioClip, PortError> {
        let recorded = self
            .finish()?
            .ok_or_else(|| PortError::Other("no recording in progress".into()))?;

        // The device delivers whatever it likes (48 kHz stereo is common); the
        // domain always receives the canonical form: 16 kHz mono (doc 08 §1).
        let (samples, sample_rate) = crate::preprocess::normalize(
            &recorded.samples,
            recorded.sample_rate,
            recorded.channels,
        );
        let duration_ms = samples.len() as u64 * 1000 / sample_rate.max(1) as u64;

        Ok(AudioClip { samples, sample_rate, duration_ms })
    }

    fn abort(&self) {
        let _ = self.finish();
    }
}

fn record_thread(
    stop_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::Sender<Result<(), PortError>>,
    on_level: LevelCallback,
    device_name: Option<String>,
) -> Result<RecordedAudio, PortError> {
    let host = cpal::default_host();
    // Resolve the microphone: if the user chose a name, look it up among the
    // input devices. If not found (mic disconnected since the last config) OR
    // if none was chosen, fall back to the OS default — dictation never stops
    // working because of a vanished microphone.
    let selected = device_name.as_deref().and_then(|name| {
        host.input_devices()
            .ok()
            // The name comes from the device's `Display` (cpal 0.18), the same
            // one `list_input_devices` exposed to the UI.
            .and_then(|mut it| it.find(|d| d.to_string() == name))
    });
    let device = match selected.or_else(|| host.default_input_device()) {
        Some(d) => d,
        None => {
            let err = PortError::AudioUnavailable("no microphone found".into());
            let _ = ready_tx.send(Err(PortError::AudioUnavailable(
                "no microphone found".into(),
            )));
            return Err(err);
        }
    };

    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(PortError::AudioUnavailable(e.to_string())));
            return Err(PortError::AudioUnavailable(e.to_string()));
        }
    };

    let sample_rate = config.sample_rate();
    let channels = config.channels();
    let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));

    let stream_result = build_stream(&device, &config, samples.clone(), on_level);
    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(PortError::AudioUnavailable(e.to_string())));
            return Err(PortError::AudioUnavailable(e.to_string()));
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(PortError::AudioUnavailable(e.to_string())));
        return Err(PortError::AudioUnavailable(e.to_string()));
    }

    let _ = ready_tx.send(Ok(()));

    // Blocks until stop()/abort(); the stream lives as long as the thread does.
    let _ = stop_rx.recv();
    drop(stream);

    let samples = std::mem::take(&mut *samples.lock().unwrap());
    Ok(RecordedAudio { samples, sample_rate, channels })
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    samples: Arc<Mutex<Vec<i16>>>,
    on_level: LevelCallback,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let err_fn = |e| log::error!(target: "wren::audio", "audio stream error: {e}");
    let stream_config: cpal::StreamConfig = config.config();

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            stream_config.clone(),
            move |data: &[f32], _| {
                let mut sum = 0.0f32;
                let mut buffer = samples.lock().unwrap();
                for &sample in data {
                    sum += sample * sample;
                    let clamped = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    buffer.push(clamped);
                }
                drop(buffer);
                let rms = (sum / data.len().max(1) as f32).sqrt();
                on_level(rms.min(1.0));
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            stream_config,
            move |data: &[i16], _| {
                let mut sum = 0.0f32;
                let mut buffer = samples.lock().unwrap();
                for &sample in data {
                    let normalized = sample as f32 / i16::MAX as f32;
                    sum += normalized * normalized;
                    buffer.push(sample);
                }
                drop(buffer);
                let rms = (sum / data.len().max(1) as f32).sqrt();
                on_level(rms.min(1.0));
            },
            err_fn,
            None,
        )?,
        other => return Err(format!("unsupported sample format: {other:?}").into()),
    };

    Ok(stream)
}
