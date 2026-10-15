//! Telemetry: app sessions, per-dictation metrics, events (what used to be the log file),
//! update attempts and daily percentile rollups, in one local SQLite database (`agt.db`).
//!
//! **Privacy:** only counts, durations, enums, versions and non-secret settings are stored.
//! Never audio, transcripts, polished text, clipboard contents, window titles or API keys;
//! nothing in this crate takes text from a dictation.
//!
//! **Never blocks the caller.** Every record is a small struct sent over an unbounded
//! `mpsc` channel to one `telemetry` writer thread, which drains whatever is queued and
//! commits it in one transaction. A send is a heap allocation and a lock-free push; it never
//! waits for the disk. Unbounded rather than bounded with `try_send`: a bounded channel would
//! either block a sender or silently drop records during a burst (start-up, a debug-level
//! log storm), and the volume is tiny (a handful of records per dictation), so the queue only
//! grows while the disk is stalled. The database is opened on the writer thread too, so
//! migrations, pruning and rollups never delay start-up.
//!
//! If the database can't be opened (locked, no disk space, ...), the writer gives up and every
//! send becomes a no-op: the app runs as before. A corrupt file is renamed aside
//! (`agt.db.corrupt-<unix ms>`) and a fresh one created.

mod db;
pub mod stats;

use std::{
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub use db::{
    RETENTION_DAYS_DEBUG_EVENTS, RETENTION_DAYS_EVENTS, RETENTION_DAYS_RAW, RETENTION_DAYS_SESSIONS,
};
pub use stats::percentile;

/// How a dictation ended. Stored as text (see [`Outcome::as_str`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pasted,
    /// Transcribed to nothing.
    NoSpeech,
    /// Held for less than `hotkey.min_hold_ms`.
    TooShort,
    /// Another key was pressed during the hold.
    Cancelled,
    /// The release was never seen; the recording was discarded at the next press.
    StaleDiscarded,
    MicFailed,
    /// The hotkey was pressed while the model was loading (or had failed to load).
    NotReady,
    TranscribeFailed,
    /// The clipboard couldn't be set (another app held it).
    ClipboardBusy,
    /// Ctrl+V couldn't be sent (e.g. an admin window).
    PasteBlocked,
    /// Focus moved to another window before the paste.
    FocusChanged,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pasted => "pasted",
            Outcome::NoSpeech => "no_speech",
            Outcome::TooShort => "too_short",
            Outcome::Cancelled => "cancelled",
            Outcome::StaleDiscarded => "stale_discarded",
            Outcome::MicFailed => "mic_failed",
            Outcome::NotReady => "not_ready",
            Outcome::TranscribeFailed => "transcribe_failed",
            Outcome::ClipboardBusy => "clipboard_busy",
            Outcome::PasteBlocked => "paste_blocked",
            Outcome::FocusChanged => "focus_changed",
        }
    }
}

/// One dictation's measurements, filled in as it moves through the pipeline threads. All
/// durations in milliseconds; `None` where a stage didn't run.
#[derive(Debug, Clone, Default)]
pub struct Dictation {
    /// Wall clock at key down (unix ms).
    pub started_at: i64,
    pub hold_ms: Option<f64>,
    /// Length of the audio that was transcribed.
    pub audio_secs: Option<f64>,
    /// Loudness of the recording (RMS, 0..1): tells silence/mic problems from STT problems.
    pub rms: Option<f64>,
    pub key_down_to_mic_ms: Option<f64>,
    /// Recording start → first audio callback.
    pub first_audio_ms: Option<f64>,
    /// The release was synthesized after the hook missed it (see `RELEASE_POLL` in the app).
    pub missed_release: bool,
    /// Release → transcription start (non-zero when dictations queue up).
    pub queue_wait_ms: Option<f64>,
    pub stt_ms: Option<f64>,
    pub polish_ms: Option<f64>,
    /// "polished", "skipped" or "fallback" (the reason is only in the events).
    pub polish_outcome: Option<&'static str>,
    /// Paste waited for paste-sensitive hotkey keys to be released.
    pub hotkey_wait_ms: Option<f64>,
    /// How long the key-down clipboard save took (on its helper thread).
    pub clipboard_save_ms: Option<f64>,
    /// Paste start → Ctrl+V sent (snapshot collect, set clipboard incl. busy waits, Ctrl+V).
    pub paste_ms: Option<f64>,
    /// Release → paste done: the latency the user feels.
    pub release_to_paste_ms: Option<f64>,
    pub chars_out: Option<i64>,
    pub words_out: Option<i64>,
    /// The engine as it ran, e.g. "parakeet-tdt-0.6b-v2 int8 (directml)".
    pub stt_backend: Option<String>,
}

/// What is known about this run at start-up. Only non-secret settings: no API key, no URL.
#[derive(Debug, Clone, Default)]
pub struct SessionStart {
    pub app_version: String,
    pub hotkey: String,
    pub min_hold_ms: i64,
    pub keep_mic_open: bool,
    pub stt_engine: String,
    pub stt_model: String,
    pub stt_precision: String,
    pub stt_device: String,
    pub stt_threads: i64,
    pub polish_enabled: bool,
    pub polish_provider: Option<String>,
    pub polish_model: Option<String>,
    pub polish_style: Option<String>,
    pub restore_clipboard_ms: i64,
    pub trailing_space: bool,
    pub overlay_enabled: bool,
    pub updates_enabled: bool,
    pub log_level: String,
    pub config_error: bool,
}

/// Facts learned later in a session; `None` fields leave the stored value as it is.
#[derive(Debug, Clone, Default)]
pub struct SessionUpdate {
    pub stt_backend: Option<String>,
    /// E.g. "on the CPU, DirectML unavailable" (a GPU → CPU fallback).
    pub stt_notice: Option<String>,
    pub model_download_ms: Option<f64>,
    pub model_download_bytes: Option<i64>,
    pub model_load_ms: Option<f64>,
    pub warmup_ms: Option<f64>,
    pub polish_warmup_ms: Option<f64>,
    pub polish_warmup_ok: Option<bool>,
}

/// One update check, download or apply.
#[derive(Debug, Clone)]
pub struct Update {
    /// "check", "download" or "apply".
    pub kind: &'static str,
    pub from_version: String,
    pub to_version: Option<String>,
    /// "none", "available", "ok" or "error".
    pub result: &'static str,
    pub error: Option<String>,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Tray → Quit.
    Quit,
    /// Restarting into an update.
    UpdateRestart,
    /// The event loop failed or the tray couldn't be created.
    Error,
}

impl ExitReason {
    fn as_str(self) -> &'static str {
        match self {
            ExitReason::Quit => "quit",
            ExitReason::UpdateRestart => "update_restart",
            ExitReason::Error => "error",
        }
    }
}

enum Msg {
    Event {
        ts: i64,
        level: &'static str,
        target: String,
        message: String,
    },
    Dictation(Box<Dictation>, Outcome),
    Session(Box<SessionUpdate>),
    Update(Box<Update>, i64),
    Exit(ExitReason, i64),
    /// Commit everything before this, then reply.
    Flush(mpsc::SyncSender<()>),
}

/// The channel to the writer thread. Records sent before [`Sink::start`] wait in the channel
/// (start-up messages are written once the database is open).
pub struct Sink {
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
}

impl Default for Sink {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }
}

impl Sink {
    /// Starts the writer thread on the database at `path`, recording a new session. Returns
    /// false if it was already started (or the thread can't be spawned).
    pub fn start(&self, path: PathBuf, session: SessionStart) -> bool {
        let Some(rx) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return false;
        };
        thread::Builder::new()
            .name("telemetry".into())
            .spawn(move || db::writer(&path, &session, rx))
            .is_ok()
    }

    fn send(&self, msg: Msg) {
        // Fails only once the writer gave up (no database): telemetry is then off.
        let _ = self.tx.send(msg);
    }

    /// A formatted log event (`level`: "ERROR" .. "TRACE").
    pub fn event(&self, level: &'static str, target: &str, message: String) {
        self.send(Msg::Event {
            ts: now_ms(),
            level,
            target: target.to_owned(),
            message,
        });
    }

    /// `d`: a `Dictation` or, to avoid moving it into a new box, a `Box<Dictation>`.
    pub fn dictation(&self, d: impl Into<Box<Dictation>>, outcome: Outcome) {
        self.send(Msg::Dictation(d.into(), outcome));
    }

    pub fn session(&self, u: SessionUpdate) {
        self.send(Msg::Session(Box::new(u)));
    }

    pub fn update(&self, u: Update) {
        self.send(Msg::Update(Box::new(u), now_ms()));
    }

    /// Waits up to `timeout` for everything sent so far to be committed. False on timeout,
    /// or if the writer isn't running.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Msg::Flush(tx));
        rx.recv_timeout(timeout).is_ok()
    }

    /// Records a clean exit and flushes (call right before the process exits).
    pub fn end_session(&self, reason: ExitReason, timeout: Duration) -> bool {
        self.send(Msg::Exit(reason, now_ms()));
        self.flush(timeout)
    }
}

static GLOBAL: LazyLock<Sink> = LazyLock::new(Sink::default);

/// The process-wide sink the app records to.
pub fn global() -> &'static Sink {
    &GLOBAL
}

/// Unix time in milliseconds.
pub fn now_ms() -> i64 {
    to_unix_ms(SystemTime::now())
}

pub fn to_unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Opens `path` read-only, for `agt stats` and external readers. Never creates it.
pub fn open_read_only(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}
