//! The database: schema and migrations, the writer thread, retention and daily rollups.
//!
//! Times are unix milliseconds (`INTEGER`); days are local dates (`YYYY-MM-DD`), computed by
//! SQLite's `localtime`. Readable form: `datetime(ts / 1000, 'unixepoch', 'localtime')`.

use crate::{Dictation, Msg, Outcome, Queue, SessionStart, SessionUpdate, now_ms, stats::Summary};
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

/// Raw `dictations` and `updates` rows.
pub const RETENTION_DAYS_RAW: i64 = 180;
pub const RETENTION_DAYS_EVENTS: i64 = 30;
/// DEBUG and TRACE events: verbose, only useful while chasing a recent problem.
pub const RETENTION_DAYS_DEBUG_EVENTS: i64 = 7;
pub const RETENTION_DAYS_SESSIONS: i64 = 365;
// `metric_rollups` are kept forever (one row per metric per day).

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How often the writer checks for a new local day (rollups + pruning) while running.
const DAY_CHECK: Duration = Duration::from_secs(60);

/// Longest wait between retries while another program holds the database's write lock.
const BUSY_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Upper bound on records per transaction, so one huge backlog doesn't hold a write lock long.
const MAX_BATCH: usize = 1000;

/// `dictations` columns rolled up into daily percentiles.
pub const ROLLUP_METRICS: &[&str] = &[
    "latency_ms",
    "release_to_paste_ms",
    "stt_ms",
    "rtf",
    "polish_ms",
    "queue_wait_ms",
    "paste_ms",
    "hotkey_wait_ms",
    "clipboard_save_ms",
    "key_down_to_mic_ms",
    "first_audio_ms",
    "hold_ms",
    "audio_secs",
    "chars_out",
    "words_out",
];

/// Schema migrations; `PRAGMA user_version` = how many have been applied. Append only.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema.
    r#"
CREATE TABLE app_sessions (
    id INTEGER PRIMARY KEY,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    -- quit | update_restart | settings_restart | error | crash (panicked) | unknown (killed, power loss, ...)
    exit_reason TEXT,
    app_version TEXT NOT NULL,
    os TEXT NOT NULL,
    arch TEXT NOT NULL,
    hotkey TEXT,
    min_hold_ms INTEGER,
    keep_mic_open INTEGER,
    stt_engine TEXT,
    stt_model TEXT,
    stt_precision TEXT,
    stt_device TEXT,
    stt_threads INTEGER,
    stt_backend TEXT,
    stt_notice TEXT,
    polish_enabled INTEGER,
    polish_provider TEXT,
    polish_model TEXT,
    polish_style TEXT,
    s1_styling TEXT,
    s1_structure TEXT,
    s1_context TEXT,
    s1_device TEXT,
    restore_clipboard_ms INTEGER,
    trailing_space INTEGER,
    overlay_enabled INTEGER,
    updates_enabled INTEGER,
    log_level TEXT,
    config_error INTEGER,
    model_download_ms REAL,
    model_download_bytes INTEGER,
    model_load_ms REAL,
    warmup_ms REAL,
    polish_warmup_ms REAL,
    polish_warmup_ok INTEGER
);
CREATE INDEX app_sessions_started_at ON app_sessions(started_at);

CREATE TABLE dictations (
    id INTEGER PRIMARY KEY,
    session_id INTEGER,
    started_at INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    hold_ms REAL,
    audio_secs REAL,
    rms REAL,
    key_down_to_mic_ms REAL,
    first_audio_ms REAL,
    missed_release INTEGER NOT NULL DEFAULT 0,
    queue_wait_ms REAL,
    stt_ms REAL,
    rtf REAL,
    polish_ms REAL,
    polish_outcome TEXT,
    hotkey_wait_ms REAL,
    clipboard_save_ms REAL,
    paste_ms REAL,
    release_to_paste_ms REAL,
    latency_ms REAL,
    chars_out INTEGER,
    words_out INTEGER,
    stt_backend TEXT
);
CREATE INDEX dictations_started_at ON dictations(started_at);

CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    session_id INTEGER,
    level TEXT NOT NULL,
    target TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE INDEX events_ts ON events(ts);

CREATE TABLE updates (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    session_id INTEGER,
    kind TEXT NOT NULL,
    from_version TEXT,
    to_version TEXT,
    result TEXT NOT NULL,
    error TEXT,
    duration_ms REAL
);
CREATE INDEX updates_ts ON updates(ts);

CREATE TABLE metric_rollups (
    day TEXT NOT NULL,
    metric TEXT NOT NULL,
    count INTEGER NOT NULL,
    min REAL,
    p50 REAL,
    p90 REAL,
    p99 REAL,
    max REAL,
    mean REAL,
    PRIMARY KEY (day, metric)
) WITHOUT ROWID;
"#,
    // 2: the remaining non-secret settings (microphone, [history], polish limits, custom
    // prompts: whether, never what), the raw transcript's word count, and whether a history
    // record was saved.
    r#"
ALTER TABLE app_sessions ADD COLUMN audio_device TEXT;
ALTER TABLE app_sessions ADD COLUMN overlay_position TEXT;
ALTER TABLE app_sessions ADD COLUMN polish_timeout_ms INTEGER;
ALTER TABLE app_sessions ADD COLUMN polish_timeout_per_word_ms INTEGER;
ALTER TABLE app_sessions ADD COLUMN polish_min_words INTEGER;
ALTER TABLE app_sessions ADD COLUMN polish_custom_prompt INTEGER;
ALTER TABLE app_sessions ADD COLUMN polish_custom_user_message INTEGER;
ALTER TABLE app_sessions ADD COLUMN history_save_text INTEGER;
ALTER TABLE app_sessions ADD COLUMN history_save_audio INTEGER;
ALTER TABLE app_sessions ADD COLUMN history_keep_days INTEGER;
ALTER TABLE dictations ADD COLUMN words_raw INTEGER;
ALTER TABLE dictations ADD COLUMN history_saved INTEGER NOT NULL DEFAULT 0;
"#,
    // 3: audio.duck and audio.duck_level.
    r#"
ALTER TABLE app_sessions ADD COLUMN duck INTEGER;
ALTER TABLE app_sessions ADD COLUMN duck_level REAL;
"#,
    // 4: release -> raw text, and long dictations transcribed while recording.
    r#"
ALTER TABLE dictations ADD COLUMN release_to_text_ms REAL;
ALTER TABLE dictations ADD COLUMN live_pause_cuts INTEGER;
ALTER TABLE dictations ADD COLUMN live_limit_cuts INTEGER;
ALTER TABLE dictations ADD COLUMN live_end_cuts INTEGER;
ALTER TABLE dictations ADD COLUMN live_skipped INTEGER;
ALTER TABLE dictations ADD COLUMN live_tail_joined INTEGER;
ALTER TABLE dictations ADD COLUMN live_trimmed_secs REAL;
ALTER TABLE dictations ADD COLUMN live_tail_secs REAL;
ALTER TABLE dictations ADD COLUMN live_stt_ms REAL;
ALTER TABLE dictations ADD COLUMN live_busy_at_release INTEGER;
ALTER TABLE dictations ADD COLUMN live_idle_ms REAL;
ALTER TABLE dictations ADD COLUMN live_max_queue INTEGER;
ALTER TABLE dictations ADD COLUMN live_discarded_ms REAL;
"#,
    // 5: polish.s1-mini.fast_drafting.
    r#"
ALTER TABLE app_sessions ADD COLUMN s1_fast_drafting INTEGER;
"#,
    // 6: long dictations polished in pieces while recording.
    r#"
ALTER TABLE dictations ADD COLUMN polish_ahead_pieces INTEGER;
ALTER TABLE dictations ADD COLUMN polish_ahead_ms REAL;
ALTER TABLE dictations ADD COLUMN polish_release_words INTEGER;
ALTER TABLE dictations ADD COLUMN polish_fallbacks INTEGER;
ALTER TABLE dictations ADD COLUMN polish_wait_ms REAL;
"#,
    // 7: Windows paste waits for clipboard listeners.
    r#"
ALTER TABLE dictations ADD COLUMN clipboard_settle_ms REAL;
"#,
    // 8: the dictionary (`dictionary.toml`): its size (as last loaded in the session) and the
    // replacements made in each dictation. Counts only, never its words.
    r#"
ALTER TABLE app_sessions ADD COLUMN dictionary_terms INTEGER;
ALTER TABLE app_sessions ADD COLUMN dictionary_rules INTEGER;
ALTER TABLE dictations ADD COLUMN replacements INTEGER;
"#,
];

/// Opens (creating if needed) and migrates the database. A file that isn't a database or is
/// corrupt is renamed aside and replaced by a fresh one.
pub(crate) fn open(path: &Path) -> rusqlite::Result<Connection> {
    match open_once(path) {
        Err(e) if is_corrupt(&e) => {
            let aside = move_aside(path);
            eprintln!(
                "agt: telemetry database {} is corrupt ({e}); moved to {}",
                path.display(),
                aside.display()
            );
            open_once(path)
        }
        result => result,
    }
}

fn open_once(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    // Must come before the first table is created to take effect.
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&mut conn)?;
    Ok(conn)
}

pub(crate) fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in (0..).zip(MIGRATIONS).skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i + 1_i64)?;
        tx.commit()?;
    }
    Ok(())
}

fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Renames `path` (and its WAL files) to `<path>.corrupt-<unix ms>`; returns the new path.
fn move_aside(path: &Path) -> PathBuf {
    let suffix = format!("corrupt-{}", now_ms());
    let with = |ext: &str| {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        PathBuf::from(p)
    };
    let aside = with(&format!(".{suffix}"));
    if let Err(e) = std::fs::rename(path, &aside) {
        eprintln!("agt: cannot move {} aside: {e}", path.display());
        let _ = std::fs::remove_file(path);
    }
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::rename(with(ext), with(&format!("{ext}.{suffix}")));
    }
    aside
}

/// Closes sessions left open by a previous run that didn't exit cleanly: `crash` if its
/// last event was a panic (`panic at ...`, see the app's panic hook), else `unknown`
/// (killed, logged off, power loss; also a worker-thread panic the app outlived). `ended_at`
/// is its last event.
pub(crate) fn close_stale_sessions(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE app_sessions SET
            exit_reason = CASE WHEN (
                SELECT e.level = 'ERROR' AND e.message GLOB 'panic at *' FROM events e
                WHERE e.session_id = app_sessions.id ORDER BY e.ts DESC, e.id DESC LIMIT 1
            ) THEN 'crash' ELSE 'unknown' END,
            ended_at = max(started_at, coalesce(
                (SELECT max(ts) FROM events e WHERE e.session_id = app_sessions.id), 0))
        WHERE ended_at IS NULL",
        [],
    )
}

pub(crate) fn insert_session(
    conn: &Connection,
    s: &SessionStart,
    at: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO app_sessions (started_at, app_version, os, arch, hotkey, min_hold_ms,
            keep_mic_open, stt_engine, stt_model, stt_precision, stt_device, stt_threads,
            polish_enabled, polish_provider, polish_model, polish_style, restore_clipboard_ms,
            trailing_space, overlay_enabled, updates_enabled, log_level, config_error,
            s1_styling, s1_structure, s1_context, s1_device, audio_device, overlay_position,
            polish_timeout_ms,
            polish_timeout_per_word_ms, polish_min_words, polish_custom_prompt,
            polish_custom_user_message, history_save_text, history_save_audio, history_keep_days,
            duck, duck_level, s1_fast_drafting)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
            ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36,
            ?37, ?38, ?39)",
        params![
            at,
            s.app_version,
            std::env::consts::OS,
            std::env::consts::ARCH,
            s.hotkey,
            s.min_hold_ms,
            s.keep_mic_open,
            s.stt_engine,
            s.stt_model,
            s.stt_precision,
            s.stt_device,
            s.stt_threads,
            s.polish_enabled,
            s.polish_provider,
            s.polish_model,
            s.polish_style,
            s.restore_clipboard_ms,
            s.trailing_space,
            s.overlay_enabled,
            s.updates_enabled,
            s.log_level,
            s.config_error,
            s.s1_styling,
            s.s1_structure,
            s.s1_context,
            s.s1_device,
            s.audio_device,
            s.overlay_position,
            s.polish_timeout_ms,
            s.polish_timeout_per_word_ms,
            s.polish_min_words,
            s.polish_custom_prompt,
            s.polish_custom_user_message,
            s.history_save_text,
            s.history_save_audio,
            s.history_keep_days,
            s.duck,
            s.duck_level,
            s.s1_fast_drafting,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn update_session(tx: &Transaction, id: i64, u: &SessionUpdate) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE app_sessions SET
            stt_backend = coalesce(?2, stt_backend),
            stt_notice = coalesce(?3, stt_notice),
            model_download_ms = coalesce(?4, model_download_ms),
            model_download_bytes = coalesce(?5, model_download_bytes),
            model_load_ms = coalesce(?6, model_load_ms),
            warmup_ms = coalesce(?7, warmup_ms),
            polish_warmup_ms = coalesce(?8, polish_warmup_ms),
            polish_warmup_ok = coalesce(?9, polish_warmup_ok),
            dictionary_terms = coalesce(?10, dictionary_terms),
            dictionary_rules = coalesce(?11, dictionary_rules)
        WHERE id = ?1",
        params![
            id,
            u.stt_backend,
            u.stt_notice,
            u.model_download_ms,
            u.model_download_bytes,
            u.model_load_ms,
            u.warmup_ms,
            u.polish_warmup_ms,
            u.polish_warmup_ok,
            u.dictionary_terms,
            u.dictionary_rules,
        ],
    )?;
    Ok(())
}

pub(crate) fn insert_dictation(
    conn: &Connection,
    session: i64,
    d: &Dictation,
    outcome: Outcome,
) -> rusqlite::Result<()> {
    // Not for a dictation transcribed while recording: `stt_ms` is only its tail's.
    let rtf = match (d.stt_ms, d.audio_secs) {
        (Some(stt), Some(secs)) if secs > 0.0 && d.live_stt_ms.is_none() => Some(stt / 1e3 / secs),
        _ => None,
    };
    // The pipeline's latency: time the user still held a hotkey key isn't part of it.
    let latency = d
        .release_to_paste_ms
        .map(|t| t - d.hotkey_wait_ms.unwrap_or(0.0));
    conn.execute(
        "INSERT INTO dictations (session_id, started_at, outcome, hold_ms, audio_secs, rms,
            key_down_to_mic_ms, first_audio_ms, missed_release, queue_wait_ms, stt_ms, rtf,
            polish_ms, polish_outcome, hotkey_wait_ms, clipboard_save_ms, paste_ms,
            release_to_paste_ms, chars_out, words_out, stt_backend, latency_ms, words_raw,
            history_saved, release_to_text_ms, live_pause_cuts, live_limit_cuts, live_end_cuts,
            live_skipped, live_tail_joined, live_trimmed_secs, live_tail_secs, live_stt_ms,
            live_busy_at_release, live_idle_ms, live_max_queue, live_discarded_ms,
            polish_ahead_pieces, polish_ahead_ms, polish_release_words, polish_fallbacks,
            polish_wait_ms, clipboard_settle_ms, replacements)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
            ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35,
            ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44)",
        params![
            session,
            d.started_at,
            outcome.as_str(),
            d.hold_ms,
            d.audio_secs,
            d.rms,
            d.key_down_to_mic_ms,
            d.first_audio_ms,
            d.missed_release,
            d.queue_wait_ms,
            d.stt_ms,
            rtf,
            d.polish_ms,
            d.polish_outcome,
            d.hotkey_wait_ms,
            d.clipboard_save_ms,
            d.paste_ms,
            d.release_to_paste_ms,
            d.chars_out,
            d.words_out,
            d.stt_backend,
            latency,
            d.words_raw,
            d.history_saved,
            d.release_to_text_ms,
            d.live_pause_cuts,
            d.live_limit_cuts,
            d.live_end_cuts,
            d.live_skipped,
            d.live_tail_joined,
            d.live_trimmed_secs,
            d.live_tail_secs,
            d.live_stt_ms,
            d.live_busy_at_release,
            d.live_idle_ms,
            d.live_max_queue,
            d.live_discarded_ms,
            d.polish_ahead_pieces,
            d.polish_ahead_ms,
            d.polish_release_words,
            d.polish_fallbacks,
            d.polish_wait_ms,
            d.clipboard_settle_ms,
            d.replacements,
        ],
    )?;
    Ok(())
}

/// Writes `batch` in one transaction, taking the write lock once up front (`IMMEDIATE`, so
/// a lock held by another program is waited for once per batch, not once per record).
/// `Err` if nothing was committed: busy (retry the whole batch), corrupt, or a failed
/// commit. A record that fails on its own is skipped (reported on stderr) and the rest
/// committed.
fn write_batch(
    conn: &mut Connection,
    session: i64,
    batch: &[Msg],
    home: &Home,
) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for msg in batch {
        let r = match msg {
            Msg::Event {
                ts,
                level,
                target,
                message,
            } => tx
                .execute(
                    "INSERT INTO events (ts, session_id, level, target, message)
                    VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ts, session, level, target, home.redact(message)],
                )
                .map(drop),
            Msg::Dictation(d, outcome) => insert_dictation(&tx, session, d, *outcome),
            Msg::Session(u) => update_session(&tx, session, u),
            Msg::Update(u, ts) => tx
                .execute(
                    "INSERT INTO updates (ts, session_id, kind, from_version, to_version,
                        result, error, duration_ms)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        ts,
                        session,
                        u.kind,
                        u.from_version,
                        u.to_version,
                        u.result,
                        u.error.as_deref().map(|e| home.redact(e)),
                        u.duration_ms
                    ],
                )
                .map(drop),
            Msg::Exit(reason, ts) => tx
                .execute(
                    "UPDATE app_sessions SET ended_at = ?2, exit_reason = ?3 WHERE id = ?1",
                    params![session, ts, reason.as_str()],
                )
                .map(drop),
            Msg::Flush(_) => Ok(()),
        };
        match r {
            Err(e) if is_busy(&e) || is_corrupt(&e) => return Err(e),
            // Keep going: one bad record must not lose the rest of the batch.
            Err(e) => eprintln!("agt: telemetry record skipped: {e}"),
            Ok(()) => {}
        }
    }
    tx.commit()
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Runs `f` until the database isn't busy (another program, e.g. an SQLite browser with
/// unsaved changes, holds the write lock), backing off up to [`BUSY_BACKOFF_MAX`]. Never
/// gives up: records wait in the channel meanwhile (see `Sink`'s queue cap).
fn while_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let mut wait = Duration::from_millis(50);
    let mut warned = false;
    loop {
        match f() {
            Err(e) if is_busy(&e) => {
                if !warned {
                    eprintln!("agt: telemetry database busy ({e}); retrying");
                    warned = true;
                }
                std::thread::sleep(wait);
                wait = (wait * 2).min(BUSY_BACKOFF_MAX);
            }
            r => return r,
        }
    }
}

/// The user's home directory, replaced by `~` in stored messages: paths in events would
/// otherwise carry the user name.
pub(crate) struct Home(Vec<String>);

impl Home {
    pub(crate) fn from_env() -> Self {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        Self::new(std::env::var(var).ok().as_deref())
    }

    pub(crate) fn new(home: Option<&str>) -> Self {
        let home = home.map(|h| h.trim_end_matches(['/', '\\'])).unwrap_or("");
        // Too short to be a real home (e.g. "/"): replacing it would mangle every path.
        if home.len() < 4 {
            return Self(Vec::new());
        }
        // Also with the other separator: config values are often written with `/`.
        let mut forms = vec![home.to_owned(), home.replace('\\', "/")];
        forms.dedup();
        Self(forms)
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for h in &self.0 {
            if text.contains(h.as_str()) {
                text = text.replace(h.as_str(), "~");
            }
        }
        text
    }
}

/// Deletes rows past their retention (see the constants above). `now`: unix ms.
pub(crate) fn prune(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    let before = |days: i64| now - days * DAY_MS;
    let mut n = 0;
    n += conn.execute(
        "DELETE FROM dictations WHERE started_at < ?1",
        [before(RETENTION_DAYS_RAW)],
    )?;
    n += conn.execute(
        "DELETE FROM updates WHERE ts < ?1",
        [before(RETENTION_DAYS_RAW)],
    )?;
    n += conn.execute(
        "DELETE FROM events WHERE ts < ?1",
        [before(RETENTION_DAYS_EVENTS)],
    )?;
    n += conn.execute(
        "DELETE FROM events WHERE ts < ?1 AND level IN ('DEBUG', 'TRACE')",
        [before(RETENTION_DAYS_DEBUG_EVENTS)],
    )?;
    n += conn.execute(
        "DELETE FROM app_sessions WHERE started_at < ?1",
        [before(RETENTION_DAYS_SESSIONS)],
    )?;
    if n > 0 {
        // Give freed pages back to the OS (auto_vacuum = INCREMENTAL; cheap).
        conn.execute_batch("PRAGMA incremental_vacuum")?;
    }
    Ok(n)
}

/// Days before this local date are complete, for rollups: the date 10 minutes ago, so a
/// dictation that started before midnight but was recorded just after it is still counted.
const COMPLETED_BEFORE: &str = "date('now', '-10 minutes', 'localtime')";

/// Computes `metric_rollups` for every completed local day (see [`COMPLETED_BEFORE`]) from
/// the last rolled-up one on. That last day is rolled up again while it's within the last 2
/// days, which picks up rows recorded late (e.g. held back by a long external lock); an
/// older one is final, so a rollup is never recomputed from raw rows that retention has
/// started to prune. Idempotent: a day's rows are replaced as a whole. Returns the number
/// of days (re)computed.
///
/// Durations are over `pasted` dictations only: a failed one has partial timings (e.g. no
/// paste), and the `outcome.<x>` counts cover failures.
pub(crate) fn rollup(conn: &mut Connection) -> rusqlite::Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let days: Vec<(String, i64, i64)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT day, strftime('%s', day, 'utc') * 1000, strftime('%s', day, '+1 day', 'utc') * 1000
            FROM (
                SELECT DISTINCT date(started_at / 1000, 'unixepoch', 'localtime') AS day
                FROM dictations
                WHERE started_at >= coalesce((
                    SELECT strftime('%s', max(day),
                        CASE WHEN max(day) >= date('now', '-2 days', 'localtime')
                            THEN '+0 days' ELSE '+1 day' END,
                        'utc') * 1000
                    FROM metric_rollups), 0)
            )
            WHERE day < {COMPLETED_BEFORE}
            ORDER BY day"
        ))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (day, start, end) in &days {
        tx.execute("DELETE FROM metric_rollups WHERE day = ?1", [day])?;
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO metric_rollups (day, metric, count, min, p50, p90, p99, max, mean)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for metric in ROLLUP_METRICS {
            let values = metric_values(&tx, metric, *start, *end)?;
            if let Some(s) = Summary::of(values) {
                insert.execute(params![
                    day, metric, s.count, s.min, s.p50, s.p90, s.p99, s.max, s.mean
                ])?;
            }
        }
        for column in ["outcome", "polish_outcome"] {
            let mut stmt = tx.prepare(&format!(
                "SELECT {column}, count(*) FROM dictations
                WHERE started_at >= ?1 AND started_at < ?2 AND {column} IS NOT NULL
                GROUP BY {column}"
            ))?;
            let counts = stmt
                .query_map([start, end], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (value, count) in counts {
                let metric = format!("{column}.{value}");
                insert.execute(params![
                    day,
                    metric,
                    count,
                    None::<f64>,
                    None::<f64>,
                    None::<f64>,
                    None::<f64>,
                    None::<f64>,
                    None::<f64>
                ])?;
            }
        }
    }
    tx.commit()?;
    Ok(days.len())
}

/// The non-null values of `metric` (a `dictations` column) for `pasted` dictations started
/// in [start, end) (see [`rollup`] for why only those).
pub(crate) fn metric_values(
    conn: &Connection,
    metric: &str,
    start: i64,
    end: i64,
) -> rusqlite::Result<Vec<f64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {metric} FROM dictations
        WHERE started_at >= ?1 AND started_at < ?2 AND outcome = 'pasted'
            AND {metric} IS NOT NULL"
    ))?;
    stmt.query_map([start, end], |r| r.get::<_, f64>(0))?
        .collect()
}

/// The latest completed day (see [`COMPLETED_BEFORE`]): maintenance runs when it changes.
fn completed_day(conn: &Connection) -> Option<String> {
    conn.query_row(&format!("SELECT {COMPLETED_BEFORE}"), [], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

/// Rollups first (so nothing is pruned before it's rolled up), then retention. False if it
/// has to be tried again (the database was busy).
fn maintenance(conn: &mut Connection) -> bool {
    let mut done = true;
    if let Err(e) = rollup(conn) {
        eprintln!("agt: telemetry rollup failed: {e}");
        done &= !is_busy(&e);
    }
    if let Err(e) = prune(conn, now_ms()) {
        eprintln!("agt: telemetry pruning failed: {e}");
        done &= !is_busy(&e);
    }
    done
}

/// Opens the database, closes stale sessions and starts a new one (waiting while the
/// database is busy). `None` if the database can't be used.
fn connect(path: &Path, session: &SessionStart) -> Option<(Connection, i64)> {
    let conn = match open(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agt: telemetry off, cannot open {}: {e}", path.display());
            return None;
        }
    };
    let id = while_busy(|| {
        close_stale_sessions(&conn)?;
        insert_session(&conn, session, now_ms())
    });
    match id {
        Ok(id) => Some((conn, id)),
        Err(e) => {
            eprintln!("agt: telemetry off, cannot start a session: {e}");
            None
        }
    }
}

/// The writer thread (see the crate docs). Returns, dropping `rx` (which turns every send
/// into a no-op), if the database can't be used.
pub(crate) fn writer(path: &Path, session: &SessionStart, rx: Receiver<Msg>, queue: &Queue) {
    let Some((mut conn, mut session_id)) = connect(path, session) else {
        return;
    };
    let home = Home::from_env();
    let mut maintenance_due = true;
    let mut day = completed_day(&conn);
    let mut day_checked = Instant::now();
    loop {
        if maintenance_due {
            maintenance_due = !maintenance(&mut conn);
        }
        let first = match rx.recv_timeout(DAY_CHECK) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(first) = first {
            let mut batch: Vec<Msg> = std::iter::once(first)
                .chain(rx.try_iter().take(MAX_BATCH))
                .collect();
            queue.received(batch.len());
            if let Some(event) = queue.dropped_event() {
                batch.push(event);
            }
            // The whole batch is kept and retried while another program holds the lock.
            match while_busy(|| write_batch(&mut conn, session_id, &batch, &home)) {
                Ok(()) => {
                    // Flushes are answered only after their records were committed.
                    for msg in batch {
                        if let Msg::Flush(ack) = msg {
                            let _ = ack.send(());
                        }
                    }
                }
                Err(e) if is_corrupt(&e) => {
                    // Corrupted while running: start over with a fresh file and session.
                    drop(conn);
                    let aside = move_aside(path);
                    eprintln!(
                        "agt: telemetry database corrupt ({e}); moved to {}",
                        aside.display()
                    );
                    let Some(c) = connect(path, session) else {
                        return;
                    };
                    (conn, session_id) = c;
                }
                // Lost (e.g. disk full); pending flushes see their ack dropped.
                Err(e) => eprintln!("agt: telemetry write failed: {e}"),
            }
        }
        if day_checked.elapsed() >= DAY_CHECK {
            day_checked = Instant::now();
            let now = completed_day(&conn);
            if now != day {
                day = now;
                maintenance_due = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
