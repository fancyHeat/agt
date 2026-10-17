//! Telemetry: app sessions, per-dictation metrics, events (what used to be the log file),
//! update attempts and daily percentile rollups, in one local SQLite database (`agt.db`).
//!
//! **Privacy:** only counts, durations, enums, versions and non-secret settings are stored.
//! Never audio, transcripts, polished text, clipboard contents, window titles or API keys;
//! nothing in this crate takes text from a dictation. The user can opt in to keeping a
//! dictation's audio and text: that's the dictation history (`agt-history`), separate files,
//! joined to the `dictations` table on `started_at`.
//!
//! **Never blocks the caller.** Every record is a small struct sent over an `mpsc` channel
//! to one `telemetry` writer thread, which drains whatever is queued and commits it in one
//! transaction. A send is a heap allocation and a lock-free push; it never waits for the
//! disk. The channel is unbounded (a bounded one would block senders), but the queue is
//! capped by count (see [`QUEUE_SOFT_CAP`]): while the database is stalled, e.g. another
//! program holds its write lock, the writer keeps retrying the same batch, and past the cap
//! events are dropped (debug/trace first) and their number recorded later. Dictations,
//! sessions and updates are never dropped (a handful per dictation). The database is opened
//! on the writer thread too, so migrations, pruning and rollups never delay start-up.
//!
//! If the database can't be opened (no disk space, a directory in its place, ...), the
//! writer gives up and every send becomes a no-op: the app runs as before. A corrupt file is
//! renamed aside (`agt.db.corrupt-<unix ms>`) and a fresh one created.
//!
//! # Schema
//!
//! Times are unix milliseconds (`datetime(ts / 1000, 'unixepoch', 'localtime')` makes them
//! readable); rollup days are local dates. Retention: see the `RETENTION_DAYS_*` constants.
//!
//! | Table | One row per | Kept |
//! |---|---|---|
//! | `app_sessions` | AGT run: version, OS, non-secret settings (the chosen microphone, `audio_device`; polish: provider, model, style, S1-mini options, timeouts, `min_words`, whether a custom prompt is set; never its key, URL or prompt text; `[history]`: `history_save_text`, `history_save_audio`, `history_keep_days`), the engine and device that actually ran (`stt_backend`, `stt_notice` on a GPU → CPU fallback), model download / load / warmup times, `exit_reason`: `quit`, `update_restart`, `settings_restart`, `error`, `crash` (its last event was a panic) or `unknown` (killed, logoff, power loss) | 365 days |
//! | `dictations` | hotkey press: `outcome` (see [`Outcome`]), `latency_ms` (release → paste, minus `hotkey_wait_ms`: the headline), `release_to_paste_ms` (wall time), `hold_ms`, `audio_secs`, `rms`, `key_down_to_mic_ms`, `first_audio_ms`, `queue_wait_ms`, `stt_ms`, `rtf`, `polish_ms` + `polish_outcome`, `polish_wait_ms`, `hotkey_wait_ms`, `clipboard_save_ms`, `paste_ms`, `chars_out`, `words_out`, `words_raw` (before polish), `missed_release`, `stt_backend`, `history_saved` (handed to the dictation history; a disk error there is only logged), `release_to_text_ms`; for a long dictation transcribed while recording: `live_pause_cuts` / `live_limit_cuts` / `live_end_cuts`, `live_skipped` (no speech), `live_tail_joined`, `live_trimmed_secs`, `live_tail_secs`, `live_stt_ms`, `live_busy_at_release`, `live_idle_ms`, `live_max_queue`, `live_discarded_ms` (cancelled mid-call); polished in pieces in the background: `polish_ahead_pieces`, `polish_ahead_ms`, `polish_release_words`, `polish_fallbacks` | 180 days |
//! | `events` | event (what the log file had): `level`, `target`, `message`; the app's `log_level` sets which are recorded; the home directory is replaced by `~` | 30 days (DEBUG/TRACE 7) |
//! | `updates` | update check, download or apply: versions, `result`, `error`, `duration_ms` | 180 days |
//! | `metric_rollups` | local day and metric: `count`, `min`, `p50`, `p90`, `p99`, `max`, `mean` of each duration over that day's `pasted` dictations, plus `outcome.<x>` and `polish_outcome.<x>` counts | forever |
//!
//! Examples (any SQLite tool; reading while AGT runs is fine, it's in WAL mode):
//!
//! ```sql
//! -- Latency of the last 50 pasted dictations
//! SELECT datetime(started_at / 1000, 'unixepoch', 'localtime') AS at, audio_secs,
//!        stt_ms, polish_ms, latency_ms
//! FROM dictations WHERE outcome = 'pasted' ORDER BY started_at DESC LIMIT 50;
//!
//! -- Daily p50 / p90 / p99 latency
//! SELECT day, count, p50, p90, p99 FROM metric_rollups
//! WHERE metric = 'latency_ms' ORDER BY day DESC;
//!
//! -- Recent warnings and errors (the old log)
//! SELECT datetime(ts / 1000, 'unixepoch', 'localtime'), level, target, message
//! FROM events WHERE level IN ('WARN', 'ERROR') ORDER BY ts DESC LIMIT 50;
//! ```

pub mod dashboard;
mod db;
pub mod stats;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
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
    /// Esc during a hands-free recording.
    EscCancelled,
    /// A hands-free recording reached its time limit: discarded.
    TimedOut,
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
            Outcome::EscCancelled => "esc_cancelled",
            Outcome::TimedOut => "timed_out",
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
    /// Length of the audio that was transcribed (for one dropped after segments of it were,
    /// how much had been recorded).
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
    /// Polishing after the raw transcript was ready (for one polished in pieces, the pieces
    /// left at release), not counting `polish_wait_ms`.
    pub polish_ms: Option<f64>,
    /// Raw transcript ready → polish started: waiting for polish requests still running (a
    /// piece polished in the background, or an earlier dictation).
    pub polish_wait_ms: Option<f64>,
    /// "polished", "skipped", "too_long" (over S1-mini's context, not sent), "fallback"
    /// (the reason is only in the events) or, polished in pieces, "partial" (some pasted
    /// raw: `polish_fallbacks`).
    pub polish_outcome: Option<&'static str>,
    /// Paste waited for paste-sensitive hotkey keys to be released.
    pub hotkey_wait_ms: Option<f64>,
    /// How long the key-down clipboard save took (on its helper thread).
    pub clipboard_save_ms: Option<f64>,
    /// Paste start → Ctrl+V sent (snapshot collect, set clipboard incl. busy waits, Ctrl+V).
    pub paste_ms: Option<f64>,
    /// Release → paste done, wall time. Includes `hotkey_wait_ms`; the stored `latency_ms`
    /// (the headline) doesn't.
    pub release_to_paste_ms: Option<f64>,
    pub chars_out: Option<i64>,
    pub words_out: Option<i64>,
    /// The engine as it ran, e.g. "parakeet-tdt-0.6b-v2 int8 (directml)".
    pub stt_backend: Option<String>,
    /// Words in the transcript before polish (`words_out` is after).
    pub words_raw: Option<i64>,
    /// Handed to the dictation history (`agt-history`) to be saved.
    pub history_saved: bool,
    /// Release → raw transcript ready (STT, plus any wait for earlier work; before polish).
    pub release_to_text_ms: Option<f64>,
    /// Transcribed while recording (something was cut before release; `None` otherwise):
    /// the segments by how they were cut (a pause, the length limit, the release).
    /// `audio_secs` is the whole recording, `queue_wait_ms` + `stt_ms` the work at release.
    pub live_pause_cuts: Option<i64>,
    pub live_limit_cuts: Option<i64>,
    pub live_end_cuts: Option<i64>,
    /// Segments not transcribed because they had no speech in them.
    pub live_skipped: Option<i64>,
    /// The last segment was a short tail, transcribed together with the one before it.
    pub live_tail_joined: Option<bool>,
    /// Silence dropped (pauses trimmed), and the audio left to transcribe at release.
    pub live_trimmed_secs: Option<f64>,
    pub live_tail_secs: Option<f64>,
    /// Time spent transcribing segments before release, in all.
    pub live_stt_ms: Option<f64>,
    /// At release the worker was still busy (earlier segments, or an earlier dictation);
    /// else how long it had been idle when the tail started (a GPU's wake-up penalty).
    pub live_busy_at_release: Option<bool>,
    pub live_idle_ms: Option<f64>,
    /// Most segments waiting for the worker at once.
    pub live_max_queue: Option<i64>,
    /// Cancelled while a segment was being transcribed: the time that call took, wasted.
    pub live_discarded_ms: Option<f64>,
    /// Polished in pieces, some in the background (`agt_polish::ahead`; `None` otherwise):
    /// the pieces polished before the polish thread got to the transcript (most while
    /// recording; one still queued when the transcript was ready also counts in
    /// `polish_wait_ms`) and the time they took, the words left to polish after that, and
    /// the pieces (of all) pasted raw after a failure.
    pub polish_ahead_pieces: Option<i64>,
    pub polish_ahead_ms: Option<f64>,
    pub polish_release_words: Option<i64>,
    pub polish_fallbacks: Option<i64>,
}

/// What is known about this run at start-up. Only non-secret settings: no API key, no URL.
#[derive(Debug, Clone, Default)]
pub struct SessionStart {
    pub app_version: String,
    pub hotkey: String,
    pub min_hold_ms: i64,
    pub keep_mic_open: bool,
    pub duck: bool,
    pub duck_level: f64,
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
    /// `[polish.s1-mini]` options, when S1-mini is the polisher.
    pub s1_styling: Option<String>,
    pub s1_structure: Option<String>,
    pub s1_context: Option<String>,
    pub s1_device: Option<String>,
    pub s1_fast_drafting: Option<bool>,
    /// `audio.device`: the chosen microphone's name, "" = the system default.
    pub audio_device: String,
    /// `None` with the overlay off.
    pub overlay_position: Option<String>,
    /// `[polish]` limits, when polish is on.
    pub polish_timeout_ms: Option<i64>,
    pub polish_timeout_per_word_ms: Option<i64>,
    pub polish_min_words: Option<i64>,
    /// Whether a custom prompt / user message is set (never the text itself).
    pub polish_custom_prompt: Option<bool>,
    pub polish_custom_user_message: Option<bool>,
    /// `[history]`: what is saved (the files themselves never go into the database).
    pub history_save_text: bool,
    pub history_save_audio: bool,
    pub history_keep_days: i64,
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
    /// Restarting to apply settings changed in config.toml (or the tray's Settings).
    SettingsRestart,
    /// The event loop failed or the tray couldn't be created.
    Error,
}

impl ExitReason {
    fn as_str(self) -> &'static str {
        match self {
            ExitReason::Quit => "quit",
            ExitReason::UpdateRestart => "update_restart",
            ExitReason::SettingsRestart => "settings_restart",
            ExitReason::Error => "error",
        }
    }
}

pub(crate) enum Msg {
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

/// Past this many queued records, DEBUG and TRACE events are dropped (counted).
pub const QUEUE_SOFT_CAP: usize = 10_000;
/// Past this many, all events are dropped (counted); other records are always kept.
pub const QUEUE_HARD_CAP: usize = 50_000;

/// How many records wait in the channel, and how many events were dropped for it.
#[derive(Default)]
pub(crate) struct Queue {
    queued: AtomicUsize,
    dropped: AtomicU64,
}

impl Queue {
    /// Whether an event at `level` may be queued now.
    fn admit(&self, level: &str) -> bool {
        let cap = match level {
            "DEBUG" | "TRACE" => QUEUE_SOFT_CAP,
            _ => QUEUE_HARD_CAP,
        };
        if self.queued.load(Relaxed) >= cap {
            self.dropped.fetch_add(1, Relaxed);
            return false;
        }
        true
    }

    /// The writer took `n` records off the channel.
    pub(crate) fn received(&self, n: usize) {
        self.queued.fetch_sub(n, Relaxed);
    }

    /// An event saying how many were dropped since the last one, if any.
    pub(crate) fn dropped_event(&self) -> Option<Msg> {
        let n = self.dropped.swap(0, Relaxed);
        (n > 0).then(|| Msg::Event {
            ts: now_ms(),
            level: "WARN",
            target: "agt_telemetry".into(),
            message: format!("{n} events were dropped while the database was unavailable"),
        })
    }
}

/// The channel to the writer thread. Records sent before [`Sink::start`] wait in the channel
/// (start-up messages are written once the database is open).
pub struct Sink {
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    queue: Arc<Queue>,
}

impl Default for Sink {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            queue: Arc::default(),
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
        let queue = self.queue.clone();
        thread::Builder::new()
            .name("telemetry".into())
            .spawn(move || db::writer(&path, &session, rx, &queue))
            .is_ok()
    }

    fn send(&self, msg: Msg) {
        self.queue.queued.fetch_add(1, Relaxed);
        // Fails only once the writer gave up (no database): telemetry is then off.
        if self.tx.send(msg).is_err() {
            self.queue.queued.fetch_sub(1, Relaxed);
        }
    }

    /// A formatted log event (`level`: "ERROR" .. "TRACE").
    pub fn event(&self, level: &'static str, target: &str, message: String) {
        if !self.queue.admit(level) {
            return;
        }
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

    /// Waits up to `timeout` for everything sent so far to be committed. False on timeout
    /// (e.g. the database is locked by another program), if that commit failed, or if the
    /// writer isn't running.
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
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // Wait out the app's brief locks (WAL checkpoint, recovery) instead of failing.
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}
