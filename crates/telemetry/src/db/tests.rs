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
    assert_eq!((n, p50, max), (4, 100.0, 300.0));
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
    assert_eq!(rollup(&mut conn).unwrap(), 0, "nothing new");
    assert_eq!(count(&conn, "SELECT count(*) FROM metric_rollups"), rows);
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
    conn.execute(
        "INSERT INTO events (ts, session_id, level, target, message)
        VALUES (5000, ?1, 'ERROR', 'agt', 'panic at src/main.rs:1:1')",
        [crashed],
    )
    .unwrap();
    assert_eq!(close_stale_sessions(&conn).unwrap(), 2);
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
    std::thread::sleep(Duration::from_millis(200));
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
