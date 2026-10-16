use super::*;
use crate::{Sink, Update, open_read_only};
use std::sync::atomic::{AtomicU32, Ordering};

fn memory() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    conn
}

/// A fresh directory under the system temp dir (removed by the caller when done).
fn temp_dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agt-telemetry-test-{}-{}-{}",
        std::process::id(),
        now_ms(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn dictation(conn: &Connection, at: i64, release_to_paste: f64, outcome: Outcome) {
    let d = Dictation {
        started_at: at,
        release_to_paste_ms: Some(release_to_paste),
        stt_ms: Some(release_to_paste / 2.0),
        audio_secs: Some(2.0),
        ..Default::default()
    };
    insert_dictation(conn, 1, &d, outcome).unwrap();
}

#[test]
fn migrates_an_empty_database_once() {
    let mut conn = memory();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version as usize, MIGRATIONS.len());
    for table in [
        "app_sessions",
        "dictations",
        "events",
        "updates",
        "metric_rollups",
    ] {
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM {table}")), 0);
    }
    migrate(&mut conn).unwrap(); // already current: no-op, no "table exists" error
}

#[test]
fn version_1_database_gets_the_new_columns() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute(
        "INSERT INTO dictations (started_at, outcome) VALUES (1000, 'pasted')",
        [],
    )
    .unwrap();
    migrate(&mut conn).unwrap();
    let s = SessionStart {
        polish_min_words: Some(4),
        polish_custom_prompt: Some(false),
        overlay_position: Some("bottom".into()),
        audio_device: "Headset".into(),
        history_save_audio: true,
        history_keep_days: 30,
        ..Default::default()
    };
    let id = insert_session(&conn, &s, 2000).unwrap();
    let row: (i64, bool, bool, i64, String, String) = conn
        .query_row(
            "SELECT polish_min_words, polish_custom_prompt, history_save_audio,
                history_keep_days, overlay_position, audio_device FROM app_sessions WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, (4, false, true, 30, "bottom".into(), "Headset".into()));
    let d = Dictation {
        started_at: 3000,
        words_raw: Some(7),
        history_saved: true,
        ..Default::default()
    };
    insert_dictation(&conn, id, &d, Outcome::Pasted).unwrap();
    let row: (i64, bool) = conn
        .query_row(
            "SELECT words_raw, history_saved FROM dictations WHERE started_at = 3000",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (7, true));
    let d = Dictation {
        started_at: 4000,
        audio_secs: Some(100.0),
        stt_ms: Some(300.0),
        release_to_text_ms: Some(320.0),
        live_pause_cuts: Some(5),
        live_limit_cuts: Some(1),
        live_end_cuts: Some(1),
        live_skipped: Some(0),
        live_tail_joined: Some(false),
        live_trimmed_secs: Some(30.5),
        live_tail_secs: Some(9.5),
        live_stt_ms: Some(2100.0),
        live_busy_at_release: Some(false),
        live_idle_ms: Some(4000.0),
        live_max_queue: Some(1),
        history_saved: true,
        ..Default::default()
    };
    insert_dictation(&conn, id, &d, Outcome::Pasted).unwrap();
    let row: (f64, i64, bool, f64, Option<f64>, Option<f64>) = conn
        .query_row(
            "SELECT release_to_text_ms, live_pause_cuts, live_tail_joined, live_stt_ms, rtf,
                live_discarded_ms FROM dictations WHERE started_at = 4000",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    // No RTF: `stt_ms` is only the tail's.
    assert_eq!(row, (320.0, 5, false, 2100.0, None, None));
    // Rows from before the migration read as "not saved".
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM dictations WHERE history_saved = 0"
        ),
        1
    );
}

/// `agt stats` opens read-only and can't migrate: a newer `agt stats` must still read a
/// database an older running AGT created (it only queries version-1 columns).
#[test]
fn stats_read_a_never_migrated_v1_database() {
    let dir = temp_dir();
    let path = dir.join("agt.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO dictations (started_at, outcome, stt_ms, audio_secs, release_to_paste_ms,
                chars_out, words_out) VALUES (?1, 'pasted', 100, 2, 150, 10, 2)",
            [now_ms()],
        )
        .unwrap();
    }
    let report = crate::stats::report(&path).unwrap_or_else(|e| panic!("{e}"));
    assert!(report.contains("1 dictation"), "{report}");
    let version: i64 = open_read_only(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1, "read-only: not migrated");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version_2_database_gains_the_duck_columns() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.execute_batch(MIGRATIONS[1]).unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    conn.execute(
        "INSERT INTO app_sessions (started_at, app_version, os, arch) VALUES (1, 'old', 'x', 'y')",
        [],
    )
    .unwrap();
    migrate(&mut conn).unwrap();
    let s = SessionStart {
        duck: true,
        duck_level: 0.25,
        ..Default::default()
    };
    insert_session(&conn, &s, 2).unwrap();
    let rows: Vec<(Option<bool>, Option<f64>)> = conn
        .prepare("SELECT duck, duck_level FROM app_sessions ORDER BY started_at")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, [(None, None), (Some(true), Some(0.25))]);
}

#[test]
fn rtf_is_derived() {
    let conn = memory();
    dictation(&conn, now_ms(), 200.0, Outcome::Pasted);
    let rtf: f64 = conn
        .query_row("SELECT rtf FROM dictations", [], |r| r.get(0))
        .unwrap();
    assert!((rtf - 0.05).abs() < 1e-9, "100 ms for 2 s of audio");
}

#[test]
fn rollups_cover_completed_days_once() {
    let mut conn = memory();
    let now = now_ms();
    // Same timestamp per day, so rows can't straddle a local midnight.
    for v in [100.0, 200.0, 300.0] {
        dictation(&conn, now - 3 * DAY_MS, v, Outcome::Pasted);
    }
    dictation(&conn, now - 3 * DAY_MS, 0.0, Outcome::NoSpeech);
    dictation(&conn, now - 2 * DAY_MS, 50.0, Outcome::Pasted);
    dictation(&conn, now, 999.0, Outcome::Pasted); // today: not complete yet

    assert_eq!(rollup(&mut conn).unwrap(), 2);
    let (n, p50, max): (i64, f64, f64) = conn
        .query_row(
            "SELECT count, p50, max FROM metric_rollups WHERE metric = 'release_to_paste_ms'
            ORDER BY day LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((n, p50, max), (3, 200.0, 300.0), "pasted dictations only");
    let no_speech: i64 = conn
        .query_row(
            "SELECT count FROM metric_rollups WHERE metric = 'outcome.no_speech'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(no_speech, 1);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM metric_rollups WHERE p50 = 999.0 OR max = 999.0"
        ),
        0,
        "today isn't rolled up"
    );

    let rows = count(&conn, "SELECT count(*) FROM metric_rollups");
    assert_eq!(rollup(&mut conn).unwrap(), 1, "only the last day is redone");
    assert_eq!(count(&conn, "SELECT count(*) FROM metric_rollups"), rows);

    // Recorded late (it ended after midnight, or the database was locked): the last
    // rolled-up day picks it up; days before it are final.
    dictation(&conn, now - 2 * DAY_MS, 70.0, Outcome::Pasted);
    dictation(&conn, now - 3 * DAY_MS, 70.0, Outcome::Pasted);
    assert_eq!(rollup(&mut conn).unwrap(), 1);
    let counts: Vec<i64> = conn
        .prepare(
            "SELECT count FROM metric_rollups WHERE metric = 'release_to_paste_ms' ORDER BY day",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(counts, [3, 2]);
    assert_eq!(count(&conn, "SELECT count(*) FROM metric_rollups"), rows);
}

#[test]
fn old_rollups_are_final() {
    let mut conn = memory();
    let now = now_ms();
    for v in [100.0, 200.0] {
        dictation(&conn, now - 5 * DAY_MS, v, Outcome::Pasted);
    }
    assert_eq!(rollup(&mut conn).unwrap(), 1);
    // Retention pruned part of that day (or it's simply old): its rollup isn't redone.
    conn.execute(
        "DELETE FROM dictations WHERE release_to_paste_ms = 100.0",
        [],
    )
    .unwrap();
    assert_eq!(rollup(&mut conn).unwrap(), 0);
    let n: i64 = conn
        .query_row(
            "SELECT count FROM metric_rollups WHERE metric = 'release_to_paste_ms'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "the complete rollup is kept");
}

#[test]
fn latency_excludes_the_hotkey_wait() {
    let conn = memory();
    let d = Dictation {
        started_at: now_ms(),
        release_to_paste_ms: Some(900.0),
        hotkey_wait_ms: Some(700.0),
        ..Default::default()
    };
    insert_dictation(&conn, 1, &d, Outcome::Pasted).unwrap();
    let latency: f64 = conn
        .query_row("SELECT latency_ms FROM dictations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(latency, 200.0);
}

#[test]
fn home_directory_is_redacted() {
    let home = Home::new(Some(r"C:\Users\Jane Doe"));
    assert_eq!(
        home.redact(r"config at C:\Users\Jane Doe\AppData\Roaming\AGT\config.toml"),
        r"config at ~\AppData\Roaming\AGT\config.toml"
    );
    assert_eq!(
        home.redact("models in C:/Users/Jane Doe/AGT"),
        "models in ~/AGT"
    );
    assert_eq!(home.redact("no path"), "no path");
    let root = Home::new(Some("/"));
    assert_eq!(
        root.redact("/usr/lib"),
        "/usr/lib",
        "too short to be a home"
    );
}

#[test]
fn prune_keeps_each_table_for_its_retention() {
    let conn = memory();
    let now = now_ms();
    let ago = |days: i64| now - days * DAY_MS - 1000;
    for days in [0, RETENTION_DAYS_RAW] {
        dictation(&conn, ago(days), 1.0, Outcome::Pasted);
        conn.execute(
            "INSERT INTO updates (ts, kind, result) VALUES (?1, 'check', 'none')",
            [ago(days)],
        )
        .unwrap();
    }
    for (days, level) in [
        (0, "DEBUG"),
        (RETENTION_DAYS_DEBUG_EVENTS, "DEBUG"),
        (RETENTION_DAYS_DEBUG_EVENTS, "TRACE"),
        (RETENTION_DAYS_DEBUG_EVENTS, "INFO"),
        (RETENTION_DAYS_EVENTS, "WARN"),
    ] {
        conn.execute(
            "INSERT INTO events (ts, level, target, message) VALUES (?1, ?2, 't', 'm')",
            params![ago(days), level],
        )
        .unwrap();
    }
    for days in [0, RETENTION_DAYS_SESSIONS] {
        let id = insert_session(&conn, &SessionStart::default(), ago(days)).unwrap();
        assert!(id > 0);
    }

    assert_eq!(prune(&conn, now).unwrap(), 1 + 1 + 3 + 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM dictations"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM updates"), 1);
    let levels: String = conn
        .query_row(
            "SELECT group_concat(level, ',') FROM (SELECT level FROM events ORDER BY ts DESC)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        levels, "DEBUG,INFO",
        "recent debug kept, a week-old one dropped"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM app_sessions"), 1);
    assert_eq!(prune(&conn, now).unwrap(), 0);
}

#[test]
fn stale_sessions_are_closed_as_crash_or_unknown() {
    let conn = memory();
    let crashed = insert_session(&conn, &SessionStart::default(), 1000).unwrap();
    let killed = insert_session(&conn, &SessionStart::default(), 2000).unwrap();
    // A worker thread panicked, the app kept running, then was killed.
    let survived = insert_session(&conn, &SessionStart::default(), 3000).unwrap();
    let not_a_panic = insert_session(&conn, &SessionStart::default(), 4000).unwrap();
    let event = |ts: i64, session: i64, level: &str, message: &str| {
        conn.execute(
            "INSERT INTO events (ts, session_id, level, target, message)
            VALUES (?1, ?2, ?3, 'agt', ?4)",
            params![ts, session, level, message],
        )
        .unwrap();
    };
    event(4000, crashed, "INFO", "ready");
    event(
        5000,
        crashed,
        "ERROR",
        "panic at src/main.rs:1:1 on thread main: x",
    );
    event(
        5000,
        survived,
        "ERROR",
        "panic at src/pipeline.rs:1:1 on thread stt: x",
    );
    event(6000, survived, "INFO", "status: Ready");
    event(7000, not_a_panic, "ERROR", "Panic at the disco");
    assert_eq!(close_stale_sessions(&conn).unwrap(), 4);
    let get = |id: i64| -> (String, i64) {
        conn.query_row(
            "SELECT exit_reason, ended_at FROM app_sessions WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(
        get(crashed),
        ("crash".into(), 5000),
        "ended at its last event"
    );
    assert_eq!(get(killed), ("unknown".into(), 2000));
    assert_eq!(get(survived), ("unknown".into(), 6000));
    assert_eq!(get(not_a_panic), ("unknown".into(), 7000));
}

#[test]
fn queue_is_capped_while_the_writer_is_stalled() {
    let dir = temp_dir();
    let path = dir.join("agt.db");
    let sink = Sink::default();
    // Not started yet: nothing is drained, like a database locked for a long time.
    for _ in 0..crate::QUEUE_SOFT_CAP + 500 {
        sink.event("DEBUG", "t", "verbose".into());
    }
    sink.event("WARN", "t", "important".into());
    sink.dictation(Dictation::default(), Outcome::Pasted);
    assert!(sink.start(path.clone(), SessionStart::default()));
    assert!(sink.flush(Duration::from_secs(10)));
    let conn = open_read_only(&path).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM events WHERE message = 'verbose'"
        ) as usize,
        crate::QUEUE_SOFT_CAP
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM events WHERE message = 'important'"
        ),
        1,
        "warnings get more room than debug events"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM dictations"), 1);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM events WHERE message = '500 events were dropped while the database was unavailable'"
        ),
        1
    );
    drop(conn);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn corrupt_file_is_moved_aside() {
    let dir = temp_dir();
    let path = dir.join("agt.db");
    std::fs::write(
        &path,
        b"this is not a database, just some bytes long enough..........",
    )
    .unwrap();
    let conn = open(&path).expect("a fresh database");
    drop(conn);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("agt.db.corrupt-")),
        "{names:?}"
    );
    let conn = open_read_only(&path).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM dictations"), 0);
    drop(conn);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unusable_database_turns_telemetry_off() {
    let dir = temp_dir();
    // A directory where the file should be: can't be opened, isn't "corrupt".
    let path = dir.join("agt.db");
    std::fs::create_dir_all(&path).unwrap();
    let sink = Sink::default();
    assert!(sink.start(path.clone(), SessionStart::default()));
    sink.event("INFO", "t", "m".into()); // must not panic or block
    assert!(!sink.flush(Duration::from_secs(5)), "writer gave up");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn senders_never_wait_for_the_disk() {
    let dir = temp_dir();
    let path = dir.join("agt.db");
    let sink = Sink::default();
    // Sent before the writer runs: kept and written once it does.
    sink.event("INFO", "t", "early".into());
    assert!(sink.start(path.clone(), SessionStart::default()));
    assert!(sink.flush(Duration::from_secs(10)));

    // Another connection holds the write lock: the writer is stuck in its busy wait.
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let t = Instant::now();
    for i in 0..10_000 {
        sink.dictation(
            Dictation {
                started_at: now_ms() - i,
                ..Default::default()
            },
            Outcome::Pasted,
        );
    }
    let sending = t.elapsed();
    assert!(
        sending < Duration::from_millis(500),
        "10k sends took {sending:?} while the database was locked"
    );
    sink.event("DEBUG", "t", "during the lock".into());
    // Longer than the connection's busy timeout (2 s): the batch is kept and retried, and
    // a flush only succeeds once it's really committed.
    assert!(
        !sink.flush(Duration::from_secs(3)),
        "nothing can be committed while the lock is held"
    );
    blocker.execute_batch("COMMIT").unwrap();

    sink.update(Update {
        kind: "check",
        from_version: "0.0.1".into(),
        to_version: None,
        result: "none",
        error: None,
        duration_ms: 1.0,
    });
    assert!(sink.end_session(crate::ExitReason::Quit, Duration::from_secs(10)));
    let conn = open_read_only(&path).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM dictations"), 10_000);
    assert_eq!(count(&conn, "SELECT count(*) FROM updates"), 1);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM events WHERE message = 'during the lock'"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM events WHERE message = 'early' AND session_id = 1"
        ),
        1
    );
    let reason: String = conn
        .query_row("SELECT exit_reason FROM app_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reason, "quit");
    let report = crate::stats::report(&path).unwrap();
    assert!(report.contains("10000 dictations"), "{report}");
    drop(conn);
    let _ = std::fs::remove_dir_all(dir);
}
