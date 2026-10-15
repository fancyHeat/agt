//! Percentiles (SQLite has none built in) and the `agt stats` report.

use crate::{
    db::{ROLLUP_METRICS, metric_values},
    now_ms, open_read_only,
};
use rusqlite::Connection;
use std::{fmt::Write, path::Path};

/// Nearest-rank percentile (`p` in 0..=100) of ascending `sorted` values: the smallest value
/// with at least `p`% of the values at or below it. Always an observed value, so p99 of a
/// few samples is simply the max.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub count: i64,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

impl Summary {
    /// `None` for no values.
    pub fn of(mut values: Vec<f64>) -> Option<Self> {
        values.retain(|v| v.is_finite());
        values.sort_by(f64::total_cmp);
        let p = |q| percentile(&values, q);
        Some(Self {
            count: values.len() as i64,
            min: *values.first()?,
            p50: p(50.0)?,
            p90: p(90.0)?,
            p99: p(99.0)?,
            max: *values.last()?,
            mean: values.iter().sum::<f64>() / values.len() as f64,
        })
    }
}

/// Windows of raw data shown by `agt stats` (label, days).
const WINDOWS: &[(&str, i64)] = &[
    ("last 24 hours", 1),
    ("last 7 days", 7),
    ("last 30 days", 30),
    ("all retained", crate::RETENTION_DAYS_RAW),
];

/// How many days of rollups `agt stats` lists.
const HISTORY_DAYS: i64 = 14;

/// How many of the latest warnings and errors `agt stats` lists.
const RECENT_PROBLEMS: i64 = 10;

/// The `agt stats` text for the database at `path`.
pub fn report(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Err(format!("no data yet ({} does not exist)", path.display()));
    }
    let conn = open_read_only(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    report_from(&conn, now_ms()).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

pub(crate) fn report_from(conn: &Connection, now: i64) -> rusqlite::Result<String> {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let mut out = String::new();
    let sessions: i64 = conn.query_row("SELECT count(*) FROM app_sessions", [], |r| r.get(0))?;
    let crashes: i64 = conn.query_row(
        "SELECT count(*) FROM app_sessions WHERE exit_reason = 'crash'",
        [],
        |r| r.get(0),
    )?;
    let _ = writeln!(out, "Sessions: {sessions} ({crashes} crashed)");

    for &(label, days) in WINDOWS {
        let start = now - days * DAY;
        let total: i64 = conn.query_row(
            "SELECT count(*) FROM dictations WHERE started_at >= ?1",
            [start],
            |r| r.get(0),
        )?;
        let _ = writeln!(out, "\n{label}: {total} dictations");
        if total == 0 {
            continue;
        }
        let mut stmt = conn.prepare(
            "SELECT outcome, count(*) FROM dictations WHERE started_at >= ?1
            GROUP BY outcome ORDER BY count(*) DESC",
        )?;
        let outcomes = stmt
            .query_map([start], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let list: Vec<String> = outcomes.iter().map(|(o, n)| format!("{o} {n}")).collect();
        let _ = writeln!(out, "  outcomes: {}", list.join(", "));
        let _ = writeln!(
            out,
            "  {:<20} {:>6} {:>9} {:>9} {:>9} {:>9}",
            "metric", "n", "p50", "p90", "p99", "max"
        );
        for metric in ROLLUP_METRICS {
            if let Some(s) = Summary::of(metric_values(conn, metric, start, i64::MAX)?) {
                let _ = writeln!(
                    out,
                    "  {metric:<20} {:>6} {:>9} {:>9} {:>9} {:>9}",
                    s.count,
                    num(s.p50),
                    num(s.p90),
                    num(s.p99),
                    num(s.max)
                );
            }
        }
    }

    let mut stmt = conn.prepare(
        "SELECT day, count, p50, p90, p99 FROM metric_rollups
        WHERE metric = 'release_to_paste_ms' ORDER BY day DESC LIMIT ?1",
    )?;
    let history = stmt
        .query_map([HISTORY_DAYS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !history.is_empty() {
        let _ = writeln!(
            out,
            "\nrelease_to_paste_ms by day (rollups, kept forever):\n  {:<10} {:>6} {:>9} {:>9} {:>9}",
            "day", "n", "p50", "p90", "p99"
        );
        for (day, n, p50, p90, p99) in history {
            let _ = writeln!(
                out,
                "  {day:<10} {n:>6} {:>9} {:>9} {:>9}",
                num(p50),
                num(p90),
                num(p99)
            );
        }
    }

    // What the tray's "see log" points at.
    let mut stmt = conn.prepare(
        "SELECT datetime(ts / 1000, 'unixepoch', 'localtime'), level, message FROM events
        WHERE level IN ('ERROR', 'WARN') ORDER BY ts DESC LIMIT ?1",
    )?;
    let recent = stmt
        .query_map([RECENT_PROBLEMS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !recent.is_empty() {
        let _ = writeln!(out, "\nlatest warnings and errors:");
        for (ts, level, message) in recent.iter().rev() {
            let _ = writeln!(out, "  {ts} {level:<5} {message}");
        }
    }
    Ok(out)
}

/// Three significant-ish digits: `161`, `2.97`, `0.052`.
fn num(v: f64) -> String {
    match v.abs() {
        a if a >= 100.0 => format!("{v:.0}"),
        a if a >= 1.0 => format!("{v:.2}"),
        _ => format!("{v:.3}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7.0], 99.0), Some(7.0));
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), Some(50.0));
        assert_eq!(percentile(&v, 90.0), Some(90.0));
        assert_eq!(percentile(&v, 99.0), Some(99.0));
        assert_eq!(percentile(&v, 100.0), Some(100.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&v, 50.0), Some(2.0));
        assert_eq!(percentile(&v, 90.0), Some(4.0));
    }

    #[test]
    fn summary_sorts_and_ignores_non_finite() {
        let s = Summary::of(vec![3.0, f64::NAN, 1.0, 2.0, f64::INFINITY]).unwrap();
        assert_eq!(s.count, 3);
        assert_eq!((s.min, s.p50, s.max, s.mean), (1.0, 2.0, 3.0, 2.0));
        assert_eq!(Summary::of(vec![]), None);
        assert_eq!(Summary::of(vec![f64::NAN]), None);
    }
}
