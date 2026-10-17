//! The settings window's Metrics page: everything the telemetry database (`agt.db`) holds,
//! summarized for a chosen period (see [`agt_telemetry::dashboard`]). Read-only; it follows
//! the database as AGT writes to it.

use agt_settings_ui::{Control, Row, Section, Stat};
use agt_telemetry::{
    dashboard::{self, Dashboard, Reader},
    stats::Summary,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// The page's title: `agt settings metrics` (the tray's "Metrics...") opens on it.
pub const TITLE: &str = "Metrics";
pub const SYMBOL: &str = "chart.bar.xaxis";

/// The periods to choose from (label, days; `None`: everything kept).
const PERIODS: &[(&str, Option<i64>)] = &[
    ("Last 24 hours", Some(1)),
    ("Last 7 days", Some(7)),
    ("Last 30 days", Some(30)),
    ("Everything kept", None),
];
const DEFAULT_PERIOD: usize = 1;

/// A period ending now moves on: reloaded at least this often while the window is open.
const RELOAD: Duration = Duration::from_secs(60);

/// What a row of the page does (the window's model keeps it by row id).
pub enum Act {
    /// The period chosen (by option index).
    Period,
    OpenFolder,
}

pub struct Metrics {
    path: PathBuf,
    conn: Option<Reader>,
    period: usize,
    /// The database's `data_version` when last loaded, and when that was.
    loaded: Option<(i64, Instant)>,
    data: Result<Dashboard, String>,
}

impl Metrics {
    pub fn new(path: PathBuf) -> Self {
        let mut m = Self {
            path,
            conn: None,
            period: DEFAULT_PERIOD,
            loaded: None,
            data: Ok(Dashboard::default()),
        };
        m.refresh();
        m
    }

    /// Reloads if the database changed (or the period moved on). Whether the figures did.
    pub fn refresh(&mut self) -> bool {
        let mut open_error = None;
        if self.conn.is_none() && self.path.exists() {
            match Reader::open(&self.path) {
                Ok(r) => self.conn = Some(r),
                Err(e) => open_error = Some(e),
            }
        }
        let Some(conn) = &self.conn else {
            let none = Err(match open_error {
                Some(e) => format!("Cannot open {}: {e}", self.path.display()),
                None => format!(
                    "No usage recorded yet: {} doesn't exist.",
                    self.path.display()
                ),
            });
            let changed = self.data != none;
            self.data = none;
            return changed;
        };
        let version = conn.version().ok();
        let fresh = matches!(
            (self.loaded, version),
            (Some((v, at)), Some(now)) if v == now && at.elapsed() < RELOAD
        );
        if fresh {
            return false;
        }
        let days = PERIODS[self.period].1;
        let data = conn
            .load(agt_telemetry::now_ms(), days)
            .map_err(|e| format!("Cannot read {}: {e}", self.path.display()));
        if data.is_err() {
            // Reopened next time (the file may have been replaced).
            self.conn = None;
        }
        self.loaded = version.map(|v| (v, Instant::now()));
        let changed = data != self.data;
        self.data = data;
        changed
    }

    pub fn set_period(&mut self, period: usize) {
        if period < PERIODS.len() && period != self.period {
            self.period = period;
            self.loaded = None;
            self.refresh();
        }
    }

    /// The page's sections; `row` gives a row's id for what it does (`None`: nothing).
    pub fn sections(&self, row: &mut dyn FnMut(Option<Act>) -> usize) -> Vec<Section> {
        let mut out = Page {
            row,
            sections: Vec::new(),
        };
        out.section("");
        out.push(
            "Period",
            Control::Choice {
                options: PERIODS.iter().map(|(l, _)| l.to_string()).collect(),
                selected: self.period,
            },
            "",
            Some(Act::Period),
        );
        match &self.data {
            Ok(d) => dashboard_sections(&mut out, d),
            Err(e) => out.value("", e.clone()),
        }
        out.section("");
        out.push(
            "",
            Control::Button("Open data folder".into()),
            "Kept on this computer only, in agt.db: counts, timings and settings, never \
             audio or text. `agt stats` prints a summary.",
            Some(Act::OpenFolder),
        );
        out.sections
    }

    pub fn folder(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| self.path.clone(), |p| p.to_owned())
    }
}

/// Builds a page's sections, asking for row ids.
struct Page<'a> {
    row: &'a mut dyn FnMut(Option<Act>) -> usize,
    sections: Vec<Section>,
}

impl Page<'_> {
    fn section(&mut self, title: &str) {
        self.sections.push(Section {
            title: title.into(),
            rows: Vec::new(),
        });
    }

    fn push(&mut self, label: &str, control: Control, note: &str, act: Option<Act>) {
        let id = (self.row)(act);
        if let Some(section) = self.sections.last_mut() {
            section.rows.push(Row {
                id,
                label: label.into(),
                control,
                note: note.into(),
                enabled: true,
            });
        }
    }

    fn value(&mut self, label: &str, text: String) {
        self.push(label, Control::Value(text), "", None);
    }

    fn table(&mut self, columns: &[&str], rows: Vec<Vec<String>>) {
        if rows.is_empty() {
            return;
        }
        let columns = columns.iter().map(|c| c.to_string()).collect();
        self.push("", Control::Table { columns, rows }, "", None);
    }

    /// A table of summaries: n, p50, p90, p99, max.
    fn summaries(&mut self, first: &str, metrics: &[(&'static str, Summary)], pick: &[&str]) {
        let rows = pick
            .iter()
            .filter_map(|&name| metrics.iter().find(|(m, _)| *m == name))
            .map(|(m, s)| {
                let unit = unit(m);
                vec![
                    label(m).to_owned(),
                    count(s.count),
                    unit.format(s.p50),
                    unit.format(s.p90),
                    unit.format(s.p99),
                    unit.format(s.max),
                ]
            })
            .collect();
        self.table(&[first, "n", "p50", "p90", "p99", "max"], rows);
    }

    /// A table of values and how often each occurred, with each one's share.
    fn shares(&mut self, first: &str, counts: &[(String, i64)], name: fn(&str) -> &str) {
        let total: i64 = counts.iter().map(|(_, n)| n).sum();
        let rows = counts
            .iter()
            .map(|(v, n)| vec![name(v).to_owned(), count(*n), percent(*n, total)])
            .collect();
        self.table(&[first, "Count", "Share"], rows);
    }
}

const LATENCY: &[&str] = &[
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
];
const RECORDING: &[&str] = &[
    "hold_ms",
    "audio_secs",
    "rms",
    "key_down_to_mic_ms",
    "first_audio_ms",
    "clipboard_save_ms",
];
const OUTPUT: &[&str] = &["words_raw", "words_out", "chars_out"];
const LIVE: &[&str] = &[
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
];
const POLISH_AHEAD: &[&str] = &[
    "polish_ahead_pieces",
    "polish_ahead_ms",
    "polish_release_words",
    "polish_fallbacks",
];

fn dashboard_sections(page: &mut Page, d: &Dashboard) {
    let latency = d.metrics.iter().find(|(m, _)| *m == "latency_ms");
    page.push(
        "",
        Control::Stats(vec![
            stat(count(d.dictations), "dictations"),
            stat(percent(d.pasted, d.dictations), "pasted"),
            stat(count(d.words), "words"),
            stat(Unit::Secs.format(d.audio_secs), "of speech"),
            stat(
                latency.map_or("–".into(), |(_, s)| Unit::Ms.format(s.p50)),
                "typical latency",
            ),
        ]),
        "",
        None,
    );
    if d.dictations == 0 {
        page.value("", "No dictations in this period.".into());
    }

    page.section("Latency (pasted dictations)");
    page.summaries("Stage", &d.metrics, LATENCY);

    page.section("Recording");
    page.summaries("Measure", &d.metrics, RECORDING);
    if d.missed_release > 0 {
        page.value("Missed releases", count(d.missed_release));
    }
    if d.history_saved > 0 {
        page.value("Saved to history", count(d.history_saved));
    }

    page.section("Output");
    page.summaries("Measure", &d.metrics, OUTPUT);

    page.section("Outcomes (all dictations)");
    page.shares("Outcome", &d.outcomes, outcome_name);

    if !d.polish_outcomes.is_empty() {
        page.section("Polish");
        page.shares("Result", &d.polish_outcomes, polish_name);
        page.summaries("Polished in pieces", &d.metrics, POLISH_AHEAD);
    }

    if d.live > 0 {
        page.section("Long dictations (transcribed while recording)");
        page.push(
            "",
            Control::Stats(vec![
                stat(count(d.live), "long dictations"),
                stat(count(d.live_tail_joined), "tails joined"),
                stat(count(d.live_busy_at_release), "busy at release"),
            ]),
            "",
            None,
        );
        page.summaries("Measure", &d.metrics, LIVE);
    }

    if !d.stt_backends.is_empty() {
        page.section("Speech engine");
        page.shares("Engine", &d.stt_backends, |s| s);
    }

    if !d.daily_latency.is_empty() {
        page.section("Latency by day (kept forever)");
        let rows = d
            .daily_latency
            .iter()
            .map(|(day, n, p50, p90, p99)| {
                vec![
                    day.clone(),
                    count(*n),
                    Unit::Ms.format(*p50),
                    Unit::Ms.format(*p90),
                    Unit::Ms.format(*p99),
                ]
            })
            .collect();
        page.table(&["Day", "n", "p50", "p90", "p99"], rows);
    }

    page.section("App");
    let crashes = d
        .exit_reasons
        .iter()
        .find(|(r, _)| r == "crash")
        .map_or(0, |(_, n)| *n);
    page.push(
        "",
        Control::Stats(vec![
            stat(count(d.sessions), "runs"),
            stat(count(crashes), "crashes"),
        ]),
        "",
        None,
    );
    page.shares("How runs ended", &d.exit_reasons, exit_name);
    page.summaries("Start-up", &d.session_metrics, dashboard::SESSION_METRICS);
    if let Some(s) = &d.latest_session {
        page.value(
            "Latest run",
            format!(
                "AGT {} on {} {}, {}",
                s.app_version, s.os, s.arch, s.started
            ),
        );
        if let Some(backend) = &s.stt_backend {
            let notice = s
                .stt_notice
                .as_deref()
                .map_or(String::new(), |n| format!(" ({n})"));
            page.value("Speech engine", format!("{backend}{notice}"));
        }
        if let Some(provider) = &s.polish_provider {
            let model = s
                .polish_model
                .as_deref()
                .map_or(String::new(), |m| format!(", {m}"));
            let warm = match s.polish_warmup_ok {
                Some(false) => ", warm-up failed",
                _ => "",
            };
            page.value("Polish", format!("{provider}{model}{warm}"));
        }
    }

    if !d.updates.is_empty() {
        page.section("Updates");
        let rows = d
            .updates
            .iter()
            .map(|u| {
                vec![
                    u.at.clone(),
                    u.kind.clone(),
                    u.to_version.clone().unwrap_or_default(),
                    u.result.clone(),
                    u.duration_ms.map_or(String::new(), |v| Unit::Ms.format(v)),
                ]
            })
            .collect();
        page.table(&["When", "Step", "Version", "Result", "Took"], rows);
        if let Some(u) = d.updates.iter().find(|u| u.error.is_some()) {
            page.value(
                &format!("Error {}", u.at),
                u.error.clone().unwrap_or_default(),
            );
        }
    }

    if !d.problems.is_empty() {
        page.section("Latest warnings and errors");
        for (at, level, message) in &d.problems {
            let level = if level == "ERROR" { "Error" } else { "Warning" };
            page.value(at, format!("{level}: {message}"));
        }
    }
}

fn stat(value: String, caption: &str) -> Stat {
    Stat {
        value,
        caption: caption.into(),
    }
}

#[derive(Clone, Copy)]
enum Unit {
    Ms,
    Secs,
    /// Real-time factor: processing time per second of audio.
    Factor,
    /// Loudness, 0..1.
    Level,
    Count,
}

impl Unit {
    fn format(self, v: f64) -> String {
        match self {
            Unit::Ms if v.abs() >= 1000.0 => Unit::Secs.format(v / 1000.0),
            Unit::Ms => format!("{v:.0} ms"),
            Unit::Secs if v.abs() >= 600.0 => format!("{:.0} min", v / 60.0),
            Unit::Secs if v.abs() >= 100.0 => format!("{v:.0} s"),
            Unit::Secs if v.abs() >= 10.0 => format!("{v:.1} s"),
            Unit::Secs => format!("{v:.2} s"),
            Unit::Factor => format!("{v:.3}×"),
            Unit::Level => format!("{v:.3}"),
            Unit::Count if v.fract() == 0.0 => count(v as i64),
            Unit::Count => format!("{v:.1}"),
        }
    }
}

fn unit(metric: &str) -> Unit {
    match metric {
        "rtf" => Unit::Factor,
        "rms" => Unit::Level,
        m if m.ends_with("_ms") => Unit::Ms,
        m if m.ends_with("_secs") => Unit::Secs,
        _ => Unit::Count,
    }
}

/// What a metric is, in a few words.
fn label(metric: &str) -> &str {
    match metric {
        "latency_ms" => "Release → paste",
        "release_to_paste_ms" => "… incl. hotkey wait",
        "release_to_text_ms" => "Release → text",
        "queue_wait_ms" => "Queued",
        "stt_ms" => "Transcribing",
        "rtf" => "Real-time factor",
        "polish_wait_ms" => "Waiting for polish",
        "polish_ms" => "Polishing",
        "hotkey_wait_ms" => "Hotkey still held",
        "paste_ms" => "Pasting",
        "hold_ms" => "Hotkey held",
        "audio_secs" => "Audio",
        "rms" => "Loudness (RMS)",
        "key_down_to_mic_ms" => "Key down → mic",
        "first_audio_ms" => "Mic → first audio",
        "clipboard_save_ms" => "Saving clipboard",
        "words_raw" => "Words heard",
        "words_out" => "Words pasted",
        "chars_out" => "Characters pasted",
        "live_pause_cuts" => "Cuts at pauses",
        "live_limit_cuts" => "Cuts at length limit",
        "live_end_cuts" => "Cuts at release",
        "live_skipped" => "Silent segments",
        "live_trimmed_secs" => "Silence trimmed",
        "live_tail_secs" => "Left at release",
        "live_stt_ms" => "Transcribed early",
        "live_idle_ms" => "Worker idle",
        "live_max_queue" => "Most queued",
        "live_discarded_ms" => "Work discarded",
        "polish_ahead_pieces" => "Pieces done early",
        "polish_ahead_ms" => "Time on early pieces",
        "polish_release_words" => "Words left at release",
        "polish_fallbacks" => "Pieces pasted raw",
        "model_download_ms" => "Model download",
        "model_load_ms" => "Model load",
        "warmup_ms" => "Speech warm-up",
        "polish_warmup_ms" => "Polish warm-up",
        other => other,
    }
}

fn outcome_name(outcome: &str) -> &str {
    match outcome {
        "pasted" => "Pasted",
        "no_speech" => "No speech",
        "too_short" => "Too short",
        "cancelled" => "Cancelled (other key)",
        "esc_cancelled" => "Cancelled (Esc)",
        "timed_out" => "Timed out",
        "stale_discarded" => "Release missed",
        "mic_failed" => "Microphone failed",
        "not_ready" => "Not ready yet",
        "transcribe_failed" => "Transcription failed",
        "clipboard_busy" => "Clipboard busy",
        "paste_blocked" => "Paste blocked",
        "focus_changed" => "Focus changed",
        other => other,
    }
}

fn polish_name(result: &str) -> &str {
    match result {
        "polished" => "Polished",
        "skipped" => "Skipped (short)",
        "too_long" => "Too long",
        "fallback" => "Failed, pasted raw",
        "partial" => "Partly polished",
        other => other,
    }
}

fn exit_name(reason: &str) -> &str {
    match reason {
        "quit" => "Quit",
        "update_restart" => "Restarted to update",
        "settings_restart" => "Restarted for settings",
        "error" => "Error",
        "crash" => "Crashed",
        "unknown" => "Killed or shut down",
        other => other,
    }
}

/// `12,345`.
fn count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn percent(n: i64, total: i64) -> String {
    if total == 0 {
        return "–".into();
    }
    let p = n as f64 * 100.0 / total as f64;
    if p >= 10.0 || p == 0.0 {
        format!("{p:.0}%")
    } else {
        format!("{p:.1}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(count(999), "999");
        assert_eq!(percent(1, 3), "33%");
        assert_eq!(percent(1, 200), "0.5%");
        assert_eq!(Unit::Ms.format(161.4), "161 ms");
        assert_eq!(Unit::Ms.format(2970.0), "2.97 s");
        assert_eq!(Unit::Secs.format(4000.0), "67 min");
        assert_eq!(unit("live_trimmed_secs").format(12.34), "12.3 s");
    }

    #[test]
    fn every_dictation_metric_has_a_place_and_a_label() {
        for &m in agt_telemetry::dashboard::DICTATION_METRICS {
            let shown = [LATENCY, RECORDING, OUTPUT, LIVE, POLISH_AHEAD]
                .iter()
                .any(|g| g.contains(&m));
            assert!(shown, "{m} isn't on the Metrics page");
            assert_ne!(label(m), m, "{m} has no label");
        }
    }
}
