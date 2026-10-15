//! The database: schema and migrations, the writer thread, retention and daily rollups.
//!
//! Times are unix milliseconds (`INTEGER`); days are local dates (`YYYY-MM-DD`), computed by
//! SQLite's `localtime`. Readable form: `datetime(ts / 1000, 'unixepoch', 'localtime')`.

use crate::{Dictation, Msg, Outcome, SessionStart, SessionUpdate, now_ms, stats::Summary};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Transaction, params};
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

/// Upper bound on records per transaction, so one huge backlog doesn't hold a write lock long.
const MAX_BATCH: usize = 1000;

/// `dictations` columns rolled up into daily percentiles.
pub const ROLLUP_METRICS: &[&str] = &[
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
    -- quit | update_restart | error | crash (panicked) | unknown (killed, power loss, ...)
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

/// Closes sessions left open by a previous run that didn't exit cleanly: `crash` if it
/// recorded a panic, else `unknown` (killed, logged off, power loss). `ended_at` is its last
/// event.
pub(crate) fn close_stale_sessions(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE app_sessions SET
            exit_reason = CASE WHEN EXISTS (
                SELECT 1 FROM events e WHERE e.session_id = app_sessions.id
                    AND e.level = 'ERROR' AND e.message LIKE 'panic%'
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
            trailing_space, overlay_enabled, updates_enabled, log_level, config_error)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
            ?19, ?20, ?21, ?22)",
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
            polish_warmup_ok = coalesce(?9, polish_warmup_ok)
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
    let rtf = match (d.stt_ms, d.audio_secs) {
        (Some(stt), Some(secs)) if secs > 0.0 => Some(stt / 1e3 / secs),
        _ => None,
    };
    conn.execute(
        "INSERT INTO dictations (session_id, started_at, outcome, hold_ms, audio_secs, rms,
            key_down_to_mic_ms, first_audio_ms, missed_release, queue_wait_ms, stt_ms, rtf,
            polish_ms, polish_outcome, hotkey_wait_ms, clipboard_save_ms, paste_ms,
            release_to_paste_ms, chars_out, words_out, stt_backend)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
            ?19, ?20, ?21)",
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
        ],
    )?;
    Ok(())
}

/// Writes `batch` in one transaction. Flush requests are returned, to be answered once it's
/// committed (or failed: a flush must never hang).
fn write_batch(
    conn: &mut Connection,
    session: i64,
    batch: Vec<Msg>,
    acks: &mut Vec<std::sync::mpsc::SyncSender<()>>,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let mut result = Ok(());
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
                    params![ts, session, level, target, message],
                )
                .map(drop),
            Msg::Dictation(d, outcome) => insert_dictation(&tx, session, &d, outcome),
            Msg::Session(u) => update_session(&tx, session, &u),
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
                        u.error,
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
            Msg::Flush(ack) => {
                acks.push(ack);
                Ok(())
            }
        };
        // Keep going: one bad record must not lose the rest of the batch.
        if let Err(e) = r {
            if is_corrupt(&e) {
                return Err(e);
            }
            result = Err(e);
        }
    }
    tx.commit()?;
    result
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

/// Computes `metric_rollups` for every completed local day after the last rolled-up one.
/// Idempotent: a day is rolled up once (and re-running replaces identical rows). Returns the
/// number of days rolled up.
pub(crate) fn rollup(conn: &mut Connection) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    let days: Vec<(String, i64, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT day, strftime('%s', day, 'utc') * 1000, strftime('%s', day, '+1 day', 'utc') * 1000
            FROM (
                SELECT DISTINCT date(started_at / 1000, 'unixepoch', 'localtime') AS day
                FROM dictations
                WHERE started_at >= coalesce(
                    (SELECT strftime('%s', max(day), '+1 day', 'utc') * 1000 FROM metric_rollups), 0)
            )
            WHERE day < date('now', 'localtime')
            ORDER BY day",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (day, start, end) in &days {
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

/// The non-null values of `metric` (a `dictations` column) for rows started in [start, end).
pub(crate) fn metric_values(
    conn: &Connection,
    metric: &str,
    start: i64,
    end: i64,
) -> rusqlite::Result<Vec<f64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {metric} FROM dictations
        WHERE started_at >= ?1 AND started_at < ?2 AND {metric} IS NOT NULL"
    ))?;
    stmt.query_map([start, end], |r| r.get::<_, f64>(0))?
        .collect()
}

fn today(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT date('now', 'localtime')", [], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

/// Rollups first (so nothing is pruned before it's rolled up), then retention.
fn maintenance(conn: &mut Connection) {
    if let Err(e) = rollup(conn) {
        eprintln!("agt: telemetry rollup failed: {e}");
    }
    if let Err(e) = prune(conn, now_ms()) {
        eprintln!("agt: telemetry pruning failed: {e}");
    }
}

/// Opens the database, closes stale sessions, starts a new one and does start-up
/// maintenance. `None` if the database can't be used.
fn connect(path: &Path, session: &SessionStart) -> Option<(Connection, i64)> {
    let mut conn = match open(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agt: telemetry off, cannot open {}: {e}", path.display());
            return None;
        }
    };
    let id = close_stale_sessions(&conn).and_then(|_| insert_session(&conn, session, now_ms()));
    let id = match id {
        Ok(id) => id,
        Err(e) => {
            eprintln!("agt: telemetry off, cannot start a session: {e}");
            return None;
        }
    };
    maintenance(&mut conn);
    Some((conn, id))
}

/// The writer thread (see the crate docs). Returns, dropping `rx` (which turns every send
/// into a no-op), if the database can't be used.
pub(crate) fn writer(path: &Path, session: &SessionStart, rx: Receiver<Msg>) {
    let Some((mut conn, mut session_id)) = connect(path, session) else {
        return;
    };
    let mut day = today(&conn);
    let mut day_checked = Instant::now();
    loop {
        let first = match rx.recv_timeout(DAY_CHECK) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(first) = first {
            let batch: Vec<Msg> = std::iter::once(first)
                .chain(rx.try_iter().take(MAX_BATCH))
                .collect();
            let mut acks = Vec::new();
            let result = write_batch(&mut conn, session_id, batch, &mut acks);
            match result {
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
                Err(e) => eprintln!("agt: telemetry write failed: {e}"),
                Ok(()) => {}
            }
            for ack in acks {
                let _ = ack.send(());
            }
        }
        if day_checked.elapsed() >= DAY_CHECK {
            day_checked = Instant::now();
            let now = today(&conn);
            if now != day {
                day = now;
                maintenance(&mut conn);
            }
        }
    }
}

#[cfg(test)]
mod tests;
