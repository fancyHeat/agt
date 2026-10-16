//! Dictation history: optional copies of each dictation on this machine (`[history]` in the
//! config; off by default). For the user to look back at what they said, and as data for
//! labelling with bigger models, fine-tuning the small ones, or a custom dictionary.
//!
//! **Privacy:** plain files in [`agt_core::paths::dictations_dir`], never uploaded. Deleted
//! by whole local days once older than `keep_days` (so up to `keep_days` + 1 days are kept).
//! Telemetry (`agt.db`) still never holds text or audio; a record joins its metrics and
//! settings there through `started_at`, for as long as agt.db keeps the row (180 days, see
//! `agt_telemetry::RETENTION_DAYS_RAW`).
//!
//! **Never on the dictation path.** The pipeline hands each finished dictation (after the
//! paste) to one `history` writer thread over a channel; the audio is moved, not copied.
//! Disk errors are logged as warnings and change nothing else. With saving off, nothing is
//! built or sent.
//!
//! # Layout
//!
//! One folder per local day, `dictations/YYYY-MM-DD/`, holding `dictations.jsonl` (one JSON
//! object per line, in order) and, with `save_audio`, `HHMMSS-mmm.wav` per dictation (16 kHz
//! mono 16-bit PCM, named by the key-down time). Pruning deletes whole day folders.
//!
//! Saved: dictations that were transcribed, pasted or not (focus moved, clipboard busy) and
//! those with no speech in them. Not: taps, cancelled holds, failed transcriptions.
//!
//! # Record
//!
//! ```json
//! {"started_at":1792153385123,"time":"2026-10-16T14:23:05.123+02:00","audio":"142305-123.wav","audio_secs":3.52,
//!  "raw":"um so lets meet on tuesday","polished":"So, let's meet on Tuesday.","polish_outcome":"polished",
//!  "pasted":"So, let's meet on Tuesday. "}
//! ```
//!
//! - `started_at`: key down, unix ms: the same value as `started_at` in agt.db's
//!   `dictations` table (timings, outcome, the engine and device that ran, and through
//!   `session_id` the session's settings: model, polish provider and style, ...).
//! - `time`: the same moment as local time.
//! - `audio`: the WAV file in the same folder (only with `save_audio`); `audio_secs` its
//!   length.
//! - With `save_text` only: `raw`, the transcript before polish (whitespace collapsed);
//!   `polished`, the polish output, or `null` when polish was off, skipped (short) or failed;
//!   `polish_outcome`: `"polished"`, `"skipped"`, `"fallback"`, or `null` (polish off);
//!   `pasted`, exactly what was pasted (trailing space included), or `null` if nothing was
//!   (no speech, or the paste failed).

use agt_core::config::HistoryConfig;
use serde::Serialize;
use std::{
    fs,
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How often the writer checks whether the local date changed, to prune once per day (also
/// once at start-up). A timer, unlike a 24 h sleep, keeps up with sleep/suspend.
const PRUNE_CHECK: Duration = Duration::from_secs(3600);

/// Entries handed to the writer and not written yet (see [`flush`]).
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// The lines of each day folder.
const RECORDS_FILE: &str = "dictations.jsonl";

/// One finished dictation, as the pipeline hands it over.
#[derive(Debug, Default)]
pub struct Entry {
    /// Key down, unix ms (agt.db's `dictations.started_at`).
    pub started_at: i64,
    /// The recording; empty when audio isn't saved.
    pub audio: Vec<f32>,
    pub audio_secs: f64,
    /// `None` when text isn't saved.
    pub text: Option<Text>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Text {
    /// The transcript before polish.
    pub raw: String,
    /// `None`: polish off, skipped or failed.
    pub polished: Option<String>,
    pub polish_outcome: Option<&'static str>,
    /// `None`: nothing pasted.
    pub pasted: Option<String>,
}

/// Sends dictations to the writer thread. Only exists while saving is on.
#[derive(Clone)]
pub struct History {
    tx: Sender<Box<Entry>>,
    pub save_text: bool,
    pub save_audio: bool,
}

impl History {
    /// Hands `entry` to the writer (a channel send, never a disk write).
    pub fn save(&self, entry: Box<Entry>) {
        PENDING.fetch_add(1, SeqCst);
        if self.tx.send(entry).is_err() {
            PENDING.fetch_sub(1, SeqCst);
        }
    }
}

/// Waits (up to `timeout`) until everything handed to the writer is on disk; call right before
/// the process exits. Returns whether it all was.
pub fn flush(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while PENDING.load(SeqCst) > 0 {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Starts the writer thread for `dir` ([`agt_core::paths::dictations_dir`]). It prunes days
/// older than `keep_days` at start-up and when the local date changes, also with saving off
/// (once, then it exits). Returns the sender while saving is on.
pub fn start(config: &HistoryConfig, dir: PathBuf) -> Option<History> {
    let (tx, rx) = mpsc::channel();
    let keep_days = config.keep_days;
    if config.enabled() || keep_days > 0 {
        let spawned = thread::Builder::new()
            .name("history".into())
            .spawn(move || writer(&dir, keep_days, rx));
        if let Err(e) = spawned {
            tracing::warn!("dictation history off, cannot start its thread: {e}");
            return None;
        }
    }
    config.enabled().then_some(History {
        tx,
        save_text: config.save_text,
        save_audio: config.save_audio,
    })
}

fn writer(dir: &Path, keep_days: u64, rx: Receiver<Box<Entry>>) {
    // The local day of the last prune.
    let mut pruned: Option<i64> = None;
    loop {
        let today = local_day(now_ms());
        if keep_days > 0 && pruned != Some(today) {
            prune(dir, today, keep_days);
            pruned = Some(today);
        }
        match rx.recv_timeout(PRUNE_CHECK) {
            Ok(e) => {
                if let Err(err) = write(dir, &e) {
                    tracing::warn!("cannot save dictation to history: {err}");
                }
                PENDING.fetch_sub(1, SeqCst);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// One line of `dictations.jsonl`.
#[derive(Serialize)]
struct Record<'a> {
    started_at: i64,
    time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<String>,
    audio_secs: f64,
    #[serde(flatten)]
    text: Option<&'a Text>,
}

/// The record's line (with its newline), and the WAV file name if there is audio. `t`:
/// `e.started_at` in local time.
fn record(e: &Entry, t: &LocalTime) -> (String, Option<String>) {
    let wav = (!e.audio.is_empty()).then(|| t.file_stem() + ".wav");
    let record = Record {
        started_at: e.started_at,
        time: t.iso(),
        audio: wav.clone(),
        audio_secs: (e.audio_secs * 1e3).round() / 1e3,
        text: e.text.as_ref(),
    };
    let mut line = serde_json::to_string(&record).expect("a record always serializes");
    line.push('\n');
    (line, wav)
}

/// Writes the WAV first, then the line: a line never points at a file that isn't there. The
/// WAV is written under a temporary name and renamed once complete, so a crash never leaves a
/// truncated `.wav`.
fn write(dir: &Path, e: &Entry) -> Result<(), String> {
    let t = LocalTime::at(e.started_at);
    let (line, wav) = record(e, &t);
    let day = dir.join(t.day());
    fs::create_dir_all(&day).map_err(|err| format!("{}: {err}", day.display()))?;
    if let Some(wav) = wav {
        let path = day.join(wav);
        let tmp = path.with_extension("wav.tmp");
        agt_audio::wav::write_16k_mono(&tmp, &e.audio)?;
        fs::rename(&tmp, &path).map_err(|err| format!("{}: {err}", path.display()))?;
    }
    let path = day.join(RECORDS_FILE);
    append_line(&path, &line).map_err(|err| format!("{}: {err}", path.display()))
}

/// Appends `line` in one write, so a reader never sees half of one. If the file doesn't end
/// with a newline (a crash cut its last line short), one goes first: the broken line stays
/// on its own, and this one is intact.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    let mut last = *b"\n";
    if f.metadata()?.len() > 0 {
        f.seek(SeekFrom::End(-1))?;
        f.read_exact(&mut last)?;
    }
    if last[0] == b'\n' {
        f.write_all(line.as_bytes())
    } else {
        f.write_all(format!("\n{line}").as_bytes())
    }
}

/// Deletes the day folders in `dir` more than `keep_days` before `today` (days since the
/// epoch, local). Anything that isn't a day folder is left alone.
fn prune(dir: &Path, today: i64, keep_days: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return; // nothing saved yet
    };
    let oldest = today.saturating_sub(keep_days.min(i64::MAX as u64) as i64);
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(day) = name.to_str().and_then(parse_day) else {
            continue;
        };
        if day >= oldest || !entry.path().is_dir() {
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(
                "cannot delete old dictations {}: {e}",
                entry.path().display()
            ),
        }
    }
    if removed > 0 {
        tracing::info!("deleted {removed} day(s) of dictation history older than {keep_days} days");
    }
}

/// `YYYY-MM-DD` → days since the epoch.
fn parse_day(name: &str) -> Option<i64> {
    let mut parts = name.splitn(3, '-');
    let mut next = |len: usize| {
        let p = parts
            .next()
            .filter(|p| p.len() == len && p.bytes().all(|b| b.is_ascii_digit()))?;
        p.parse::<i64>().ok()
    };
    let (y, m, d) = (next(4)?, next(2)?, next(2)?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Today (local) as days since the epoch, at `unix_ms`.
fn local_day(unix_ms: i64) -> i64 {
    LocalTime::at(unix_ms).days
}

/// A moment in local time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LocalTime {
    /// Days since the epoch (local).
    days: i64,
    /// Milliseconds into the (local) day.
    ms: i64,
    /// Seconds east of UTC.
    offset: i64,
}

impl LocalTime {
    fn at(unix_ms: i64) -> Self {
        Self::with_offset(unix_ms, utc_offset(unix_ms.div_euclid(1000)))
    }

    fn with_offset(unix_ms: i64, offset: i64) -> Self {
        let local = unix_ms + offset * 1000;
        Self {
            days: local.div_euclid(86_400_000),
            ms: local.rem_euclid(86_400_000),
            offset,
        }
    }

    /// `YYYY-MM-DD`.
    fn day(&self) -> String {
        let (y, m, d) = civil_from_days(self.days);
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// `HHMMSS-mmm`.
    fn file_stem(&self) -> String {
        let (h, m, s, ms) = self.clock();
        format!("{h:02}{m:02}{s:02}-{ms:03}")
    }

    /// ISO 8601 with the offset, e.g. `2026-10-16T14:23:05.123+02:00`.
    fn iso(&self) -> String {
        let (h, m, s, ms) = self.clock();
        let sign = if self.offset < 0 { '-' } else { '+' };
        let off = self.offset.abs() / 60;
        format!(
            "{}T{h:02}:{m:02}:{s:02}.{ms:03}{sign}{:02}:{:02}",
            self.day(),
            off / 60,
            off % 60
        )
    }

    fn clock(&self) -> (i64, i64, i64, i64) {
        let secs = self.ms / 1000;
        (secs / 3600, secs / 60 % 60, secs % 60, self.ms % 1000)
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

/// The local time zone's offset from UTC at `unix_secs`, in seconds (DST included).
#[cfg(windows)]
fn utc_offset(unix_secs: i64) -> i64 {
    use windows_sys::Win32::{
        Foundation::SYSTEMTIME, System::Time::SystemTimeToTzSpecificLocalTime,
    };
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    let sod = unix_secs.rem_euclid(86_400);
    let utc = SYSTEMTIME {
        wYear: y as u16,
        wMonth: m as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: (sod / 3600) as u16,
        wMinute: (sod / 60 % 60) as u16,
        wSecond: (sod % 60) as u16,
        wMilliseconds: 0,
    };
    let mut local = utc;
    // SAFETY: both pointers are to valid SYSTEMTIMEs; null means the current time zone.
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return 0;
    }
    let days = days_from_civil(local.wYear.into(), local.wMonth.into(), local.wDay.into());
    let secs = i64::from(local.wHour) * 3600 + i64::from(local.wMinute) * 60;
    days * 86_400 + secs + i64::from(local.wSecond) - unix_secs
}

#[cfg(unix)]
fn utc_offset(unix_secs: i64) -> i64 {
    let t = unix_secs as libc::time_t;
    // SAFETY: `tm` is plain data, filled in by localtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

#[cfg(not(any(windows, unix)))]
fn utc_offset(_: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests;
