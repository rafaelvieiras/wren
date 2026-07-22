//! Centralized logging (**fully local** diagnostics — nothing leaves the machine).
//!
//! A single `log::Log` implementation (`WrenLogger`) fans each record out to
//! three destinations:
//! 1. **stderr** — preserves the previous behavior (`eprintln!`) in the dev server;
//! 2. **rotating file** at `<data>/logs/wren.log` — DEBUG+ always, with size-based
//!    rotation (a marker on rollover, never truncates silently);
//! 3. **in-memory ring buffer** — queried by the Diagnostics tab via a command.
//!
//! We chose the (lightweight) `log` facade over `tracing`: performance telemetry
//! is measured per port + `Clock` in the core, so we do not need spans — and we
//! avoid the weight of `tracing-appender`.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
use serde::Serialize;

/// File limit before rotating (~5 MB).
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// How many rotated files to keep (`wren.log.1` … `wren.log.N`).
const KEEP_ROTATED: usize = 3;
/// Capacity of the in-memory ring buffer (lines).
const BUFFER_CAP: usize = 5000;

/// A structured log record — what the UI consumes.
#[derive(Debug, Clone, Serialize)]
pub struct LogRecord {
    /// Epoch millis (the display side formats the time).
    pub ts_ms: u64,
    /// Level in uppercase: ERROR, WARN, INFO, DEBUG, TRACE.
    pub level: String,
    /// Record target (e.g. `wren::shortcut`) — useful for filtering.
    pub target: String,
    pub message: String,
}

/// Thread-safe ring buffer of recent records, with a fixed limit.
pub struct LogBuffer {
    inner: Mutex<VecDeque<LogRecord>>,
    cap: usize,
}

impl LogBuffer {
    fn new(cap: usize) -> Self {
        LogBuffer {
            inner: Mutex::new(VecDeque::with_capacity(cap.min(1024))),
            cap,
        }
    }

    fn push(&self, record: LogRecord) {
        let mut q = self.inner.lock().unwrap();
        if q.len() == self.cap {
            q.pop_front();
        }
        q.push_back(record);
    }

    /// Records **most-recent-first**, filtered by minimum severity level
    /// (`min_level`: includes that level and the more severe ones) and by a
    /// case-insensitive substring in target/message, limited to `limit`.
    pub fn snapshot(&self, limit: usize, min_level: Option<Level>, query: &str) -> Vec<LogRecord> {
        let q = self.inner.lock().unwrap();
        let needle = query.trim().to_lowercase();
        let max_rank = min_level.map(|l| l as usize);
        q.iter()
            .rev()
            .filter(|r| max_rank.is_none_or(|max| level_rank(&r.level) <= max))
            .filter(|r| {
                needle.is_empty()
                    || r.message.to_lowercase().contains(&needle)
                    || r.target.to_lowercase().contains(&needle)
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// Empties the in-memory buffer (the on-disk file remains).
    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

/// Severity as a number (ERROR=1 … TRACE=5), for the level filter. Mirrors
/// `log::Level as usize`; unknown falls back to INFO.
fn level_rank(level: &str) -> usize {
    match level {
        "ERROR" => 1,
        "WARN" => 2,
        "INFO" => 3,
        "DEBUG" => 4,
        "TRACE" => 5,
        _ => 3,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Log file with size-based rotation. If the file cannot be opened
/// (e.g. disk full/permission), it degrades to a no-op — the log still goes to
/// stderr and to the ring buffer.
struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

impl RotatingFile {
    fn open(dir: &Path) -> Self {
        let _ = fs::create_dir_all(dir);
        let path = dir.join("wren.log");
        let (file, written) = Self::open_append(&path);
        RotatingFile { path, file, written }
    }

    fn open_append(path: &Path) -> (Option<File>, u64) {
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => {
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                (Some(f), len)
            }
            Err(_) => (None, 0),
        }
    }

    fn rotated_path(&self, n: usize) -> PathBuf {
        self.path.with_file_name(format!("wren.log.{n}"))
    }

    fn write_line(&mut self, line: &str) {
        if self.file.is_none() {
            return;
        }
        let need = line.len() as u64 + 1;
        if self.written + need > MAX_FILE_BYTES {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut() {
            if writeln!(f, "{line}").is_ok() {
                self.written += need;
            }
        }
    }

    /// Shifts `wren.log.(N-1) → .N`, …, `wren.log → wren.log.1` and reopens the
    /// live file with a marker — the rollover is always explicit in the log.
    fn rotate(&mut self) {
        self.file = None; // closes the current file before renaming
        for i in (1..KEEP_ROTATED).rev() {
            let _ = fs::rename(self.rotated_path(i), self.rotated_path(i + 1));
        }
        let _ = fs::rename(&self.path, self.rotated_path(1));
        let (file, _) = Self::open_append(&self.path);
        self.file = file;
        self.written = 0;
        if let Some(f) = self.file.as_mut() {
            let marker = format!("--- log rotated (limit {MAX_FILE_BYTES} bytes) ---");
            if writeln!(f, "{marker}").is_ok() {
                self.written += marker.len() as u64 + 1;
            }
        }
    }

    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// Wren's global logger. Installed once by `init_logging`.
struct WrenLogger {
    buffer: Arc<LogBuffer>,
    file: Mutex<RotatingFile>,
}

impl Log for WrenLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        // The effective level is controlled by `set_max_level` (see
        // `init_logging`/`set_log_level`); the `log` macros already filter
        // against it before a record reaches here, so we accept everything.
        true
    }

    fn log(&self, record: &Record) {
        let ts_ms = now_ms();
        let level = record.level();
        let target = record.target().to_string();
        let message = record.args().to_string();

        // stderr — the dev server keeps seeing the logs in the terminal.
        eprintln!("[{level}] {target} — {message}");

        let line = format!("{ts_ms} [{level}] {target} — {message}");
        self.file.lock().unwrap().write_line(&line);

        self.buffer.push(LogRecord {
            ts_ms,
            level: level.to_string(),
            target,
            message,
        });
    }

    fn flush(&self) {
        self.file.lock().unwrap().flush();
    }
}

/// Installs the central logger and returns the ring buffer handle (for the
/// command the UI queries). Call ONCE at boot; on hot-reload the process
/// persists and the second call would be ignored (`log` only accepts one global
/// logger).
///
/// `level` is the initial minimum severity captured (file + ring buffer +
/// stderr) — it comes from the user's persisted `Settings::log_level`, so a
/// production install doesn't accumulate DEBUG/TRACE noise by default. Change
/// it later at runtime with `set_log_level`.
pub fn init_logging(dir: PathBuf, level: LevelFilter) -> Arc<LogBuffer> {
    let buffer = Arc::new(LogBuffer::new(BUFFER_CAP));
    let logger = WrenLogger {
        buffer: buffer.clone(),
        file: Mutex::new(RotatingFile::open(&dir)),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    } else {
        eprintln!("[wren] logger already initialized; ignoring re-initialization");
    }
    buffer
}

/// Changes the captured severity at runtime (e.g. right after `save_settings`),
/// with no restart needed — `log::set_max_level` is a global atomic.
pub fn set_log_level(level: LevelFilter) {
    log::set_max_level(level);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: &str, target: &str, message: &str) -> LogRecord {
        LogRecord {
            ts_ms: 0,
            level: level.into(),
            target: target.into(),
            message: message.into(),
        }
    }

    #[test]
    fn buffer_respects_the_limit_by_dropping_the_oldest() {
        let buf = LogBuffer::new(3);
        for i in 0..5 {
            buf.push(rec("INFO", "t", &format!("msg{i}")));
        }
        let snap = buf.snapshot(10, None, "");
        // Most-recent-first; only the last 3 remain.
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "msg4");
        assert_eq!(snap[2].message, "msg2");
    }

    #[test]
    fn snapshot_filters_by_minimum_severity_level() {
        let buf = LogBuffer::new(10);
        buf.push(rec("DEBUG", "t", "d"));
        buf.push(rec("INFO", "t", "i"));
        buf.push(rec("WARN", "t", "w"));
        buf.push(rec("ERROR", "t", "e"));
        // WARN includes WARN and ERROR (more severe), excludes INFO/DEBUG.
        let snap = buf.snapshot(10, Some(Level::Warn), "");
        let levels: Vec<&str> = snap.iter().map(|r| r.level.as_str()).collect();
        assert_eq!(levels, vec!["ERROR", "WARN"]);
    }

    #[test]
    fn snapshot_filters_by_substring_in_target_and_message() {
        let buf = LogBuffer::new(10);
        buf.push(rec("INFO", "wren::shortcut", "main shortcut"));
        buf.push(rec("INFO", "wren::audio", "level 0.3"));
        assert_eq!(buf.snapshot(10, None, "SHORTCUT").len(), 1);
        assert_eq!(buf.snapshot(10, None, "main")[0].target, "wren::shortcut");
        assert_eq!(buf.snapshot(10, None, "nonexistent").len(), 0);
    }

    #[test]
    fn file_rotates_when_the_limit_is_exceeded() {
        let dir = std::env::temp_dir().join(format!("wren-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut rf = RotatingFile::open(&dir);
        // Force the overflow: write more than MAX_FILE_BYTES in large lines.
        let big = "x".repeat(64 * 1024);
        for _ in 0..90 {
            rf.write_line(&big);
        }
        rf.flush();
        assert!(dir.join("wren.log").exists());
        assert!(dir.join("wren.log.1").exists(), "should have rotated at least once");
        let _ = fs::remove_dir_all(&dir);
    }
}
