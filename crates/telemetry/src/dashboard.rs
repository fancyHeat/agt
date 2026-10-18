//! Everything the database holds, summarized for a window of time: what the settings
//! window's Metrics page shows. Read-only, like [`crate::stats::report`].

use crate::stats::Summary;
use rusqlite::{Connection, OptionalExtension, params};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Numeric `dictations` columns, summarized over `pasted` dictations (where set). Grouped
/// the way the Metrics page shows them; every column the app records is here.
pub const DICTATION_METRICS: &[&str] = &[
    // Latency
    "latency_ms",
    "release_to_paste_ms",
    "release_to_text_ms",
    "queue_wait_ms",
    "stt_ms",
    "rtf",
    "polish_wait_ms",
    "polish_ms",
    "hotkey_wait_ms",
    "paste_ms",
    // Recording
    "hold_ms",
    "audio_secs",
    "rms",
    "key_down_to_mic_ms",
    "first_audio_ms",
    "clipboard_save_ms",
    // Output
    "words_raw",
    "words_out",
    "chars_out",
    // Transcribed while recording
    "live_pause_cuts",
    "live_limit_cuts",
    "live_end_cuts",
    "live_skipped",
    "live_trimmed_secs",
    "live_tail_secs",
    "live_stt_ms",
    "live_idle_ms",
    "live_max_queue",
    "live_discarded_ms",
    // Polished in pieces
    "polish_ahead_pieces",
    "polish_ahead_ms",
    "polish_release_words",
    "polish_fallbacks",
];

/// `app_sessions` start-up timings, summarized over the sessions started in the window.
pub const SESSION_METRICS: &[&str] = &[
    "model_download_ms",
    "model_load_ms",
    "warmup_ms",
    "polish_warmup_ms",
];

/// How many days of latency rollups are listed for everything kept (a period lists all its
/// days); how many updates and problems.
pub const HISTORY_DAYS: i64 = 14;
pub const RECENT_UPDATES: i64 = 8;
pub const RECENT_PROBLEMS: i64 = 12;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dashboard {
    /// All dictations in the window (any outcome), and those pasted.
    pub dictations: i64,
    pub pasted: i64,
    /// Over pasted dictations.
    pub words: i64,
    pub audio_secs: f64,
    /// Dictations whose release the hook missed, and those handed to the history.
    pub missed_release: i64,
    pub history_saved: i64,
    /// Long dictations transcribed while recording (any outcome), and those whose last
    /// segment was a short tail joined to the one before, or the worker was still busy at
    /// release.
    pub live: i64,
    pub live_tail_joined: i64,
    pub live_busy_at_release: i64,
    /// `(value, count)`, most first.
    pub outcomes: Vec<(String, i64)>,
    pub polish_outcomes: Vec<(String, i64)>,
    pub stt_backends: Vec<(String, i64)>,
    /// [`DICTATION_METRICS`] that have values, in that order.
    pub metrics: Vec<(&'static str, Summary)>,
    /// Daily `latency_ms` rollups (`day`, count, p50, p90, p99), newest first.
    pub daily_latency: Vec<(String, i64, f64, f64, f64)>,
    pub sessions: i64,
    pub exit_reasons: Vec<(String, i64)>,
    /// [`SESSION_METRICS`] that have values.
    pub session_metrics: Vec<(&'static str, Summary)>,
    pub latest_session: Option<Session>,
    pub updates: Vec<UpdateRow>,
    /// Latest warnings and errors: (local time, level, message), newest first.
    pub problems: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// Local time.
    pub started: String,
    pub app_version: String,
    pub os: String,
    pub arch: String,
    pub stt_backend: Option<String>,
    pub stt_notice: Option<String>,
    pub polish_provider: Option<String>,
    pub polish_model: Option<String>,
    pub model_load_ms: Option<f64>,
    pub warmup_ms: Option<f64>,
    pub polish_warmup_ms: Option<f64>,
    pub polish_warmup_ok: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateRow {
    /// Local time.
    pub at: String,
    pub kind: String,
    pub to_version: Option<String>,
    pub result: String,
    pub error: Option<String>,
    pub duration_ms: Option<f64>,
}

/// The dashboard for the `days` before `now` (`None`: everything kept).
pub fn load(conn: &Connection, now: i64, days: Option<i64>) -> rusqlite::Result<Dashboard> {
    let start = days.map_or(i64::MIN, |d| now - d * DAY_MS);
    let mut d = Dashboard::default();

    (d.dictations, d.missed_release, d.history_saved, d.live) = conn.query_row(
        "SELECT count(*), coalesce(sum(missed_release), 0), coalesce(sum(history_saved), 0),
            count(live_end_cuts)
        FROM dictations WHERE started_at >= ?1",
        [start],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    (d.live_tail_joined, d.live_busy_at_release) = conn.query_row(
        "SELECT coalesce(sum(live_tail_joined), 0), coalesce(sum(live_busy_at_release), 0)
        FROM dictations WHERE started_at >= ?1",
        [start],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    (d.pasted, d.words, d.audio_secs) = conn.query_row(
        "SELECT count(*), coalesce(sum(words_out), 0), coalesce(sum(audio_secs), 0)
        FROM dictations WHERE started_at >= ?1 AND outcome = 'pasted'",
        [start],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    d.outcomes = counts(conn, "dictations", "outcome", "started_at", start)?;
    d.polish_outcomes = counts(conn, "dictations", "polish_outcome", "started_at", start)?;
    d.stt_backends = counts(conn, "dictations", "stt_backend", "started_at", start)?;
    // One pass over the pasted dictations for every column.
    let mut columns = vec![Vec::new(); DICTATION_METRICS.len()];
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM dictations WHERE started_at >= ?1 AND outcome = 'pasted'",
        DICTATION_METRICS.join(", ")
    ))?;
    let mut rows = stmt.query([start])?;
    while let Some(row) = rows.next()? {
        for (i, column) in columns.iter_mut().enumerate() {
            if let Some(v) = row.get::<_, Option<f64>>(i)? {
                column.push(v);
            }
        }
    }
    for (&metric, values) in DICTATION_METRICS.iter().zip(columns) {
        if let Some(s) = Summary::of(values) {
            d.metrics.push((metric, s));
        }
    }

    // The days the window touches (rollups are of whole local days), all of a longer window's.
    let limit = days.map_or(HISTORY_DAYS, |d| d.max(HISTORY_DAYS));
    let mut stmt = conn.prepare(
        "SELECT day, count, p50, p90, p99 FROM metric_rollups
        WHERE metric = 'latency_ms'
            AND (?2 IS NULL OR day >= date(?2 / 1000, 'unixepoch', 'localtime'))
        ORDER BY day DESC LIMIT ?1",
    )?;
    d.daily_latency = stmt
        .query_map(params![limit, days.map(|_| start)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    d.sessions = conn.query_row(
        "SELECT count(*) FROM app_sessions WHERE started_at >= ?1",
        [start],
        |r| r.get(0),
    )?;
    d.exit_reasons = counts(conn, "app_sessions", "exit_reason", "started_at", start)?;
    for &metric in SESSION_METRICS {
        if let Some(s) = Summary::of(values(conn, "app_sessions", metric, "started_at", start)?) {
            d.session_metrics.push((metric, s));
        }
    }
    d.latest_session = conn
        .query_row(
            "SELECT datetime(started_at / 1000, 'unixepoch', 'localtime'), app_version, os, arch,
                stt_backend, stt_notice, polish_provider, polish_model, model_load_ms, warmup_ms,
                polish_warmup_ms, polish_warmup_ok
            FROM app_sessions ORDER BY started_at DESC, id DESC LIMIT 1",
            [],
            |r| {
                Ok(Session {
                    started: r.get(0)?,
                    app_version: r.get(1)?,
                    os: r.get(2)?,
                    arch: r.get(3)?,
                    stt_backend: r.get(4)?,
                    stt_notice: r.get(5)?,
                    polish_provider: r.get(6)?,
                    polish_model: r.get(7)?,
                    model_load_ms: r.get(8)?,
                    warmup_ms: r.get(9)?,
                    polish_warmup_ms: r.get(10)?,
                    polish_warmup_ok: r.get(11)?,
                })
            },
        )
        .optional()?;

    let mut stmt = conn.prepare(
        "SELECT datetime(ts / 1000, 'unixepoch', 'localtime'), kind, to_version, result, error,
            duration_ms
        FROM updates ORDER BY ts DESC, id DESC LIMIT ?1",
    )?;
    d.updates = stmt
        .query_map([RECENT_UPDATES], |r| {
            Ok(UpdateRow {
                at: r.get(0)?,
                kind: r.get(1)?,
                to_version: r.get(2)?,
                result: r.get(3)?,
                error: r.get(4)?,
                duration_ms: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = conn.prepare(
        "SELECT datetime(ts / 1000, 'unixepoch', 'localtime'), level, message FROM events
        WHERE level IN ('ERROR', 'WARN') AND ts >= ?1 ORDER BY ts DESC, id DESC LIMIT ?2",
    )?;
    d.problems = stmt
        .query_map([start, RECENT_PROBLEMS], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(d)
}

/// `column`'s values and how often each occurs in `table` since `start`, most first
/// (`NULL`s left out).
fn counts(
    conn: &Connection,
    table: &str,
    column: &str,
    time: &str,
    start: i64,
) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {column}, count(*) FROM {table}
        WHERE {time} >= ?1 AND {column} IS NOT NULL
        GROUP BY {column} ORDER BY count(*) DESC, {column}"
    ))?;
    stmt.query_map([start], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// `column`'s non-null values in `table` since `start`.
fn values(
    conn: &Connection,
    table: &str,
    column: &str,
    time: &str,
    start: i64,
) -> rusqlite::Result<Vec<f64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {column} FROM {table} WHERE {time} >= ?1 AND {column} IS NOT NULL"
    ))?;
    stmt.query_map([start], |r| r.get::<_, f64>(0))?.collect()
}

/// A read-only connection kept open to follow the database as AGT writes to it.
pub struct Reader(Connection);

impl Reader {
    /// See [`crate::open_read_only`]: never creates the file.
    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        crate::open_read_only(path).map(Self)
    }

    /// Changes whenever another connection commits: a cheap way to tell whether
    /// [`Self::load`] would give something new.
    pub fn version(&self) -> rusqlite::Result<i64> {
        self.0.query_row("PRAGMA data_version", [], |r| r.get(0))
    }

    pub fn load(&self, now: i64, days: Option<i64>) -> rusqlite::Result<Dashboard> {
        load(&self.0, now, days)
    }
}
