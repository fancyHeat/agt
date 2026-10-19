//! The meeting folders: `<root>/<id>/` holding `meeting.json`, `transcript.jsonl` and
//! `meeting.md`.
//!
//! - `meeting.json` is replaced whole (temp file, then rename), so a crash leaves the old one
//!   or the new one, never half of one. Every change goes through [`Store::update`], which
//!   re-reads it under one process-wide lock: notes saved from the window while a summary
//!   finishes both land, and neither reverts the other.
//! - `transcript.jsonl` is only appended to, one [`Line`] per line in one write. A line a
//!   crash cut short is skipped when read, and the next one still starts on its own line.
//! - `meeting.md` is derived (see [`markdown`]); rewrite it whenever the meeting changes.
//! - A meeting AGT never got to stop (killed, crashed, the power went) is put right at the next
//!   launch ([`Store::recover`]).

use crate::{
    Line, Meeting, MeetingInfo,
    time::{self, LocalTime},
};
use std::{
    fs,
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Component, Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::UNIX_EPOCH,
};

const MEETING_FILE: &str = "meeting.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const MARKDOWN_FILE: &str = "meeting.md";

/// Held across each read-modify-write of `meeting.json`, `meeting.md`'s rewrite, and each
/// read of or append to `transcript.jsonl`.
static LOCK: Mutex<()> = Mutex::new(());

/// The meetings under one folder (`history_dir()/meetings`).
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }

    /// Makes the folder of a meeting that started at `started_at_ms` (unix) and its
    /// `meeting.json`, untitled (see [`Meeting::shown_title`]). A second meeting started in
    /// the same second gets `-2` (then `-3`, ...) after its id: a folder is never reused.
    pub fn create(&self, started_at_ms: i64) -> io::Result<Meeting> {
        let t = LocalTime::at(started_at_ms);
        fs::create_dir_all(&self.root).map_err(at(&self.root))?;
        let base = t.id();
        let mut id = base.clone();
        let mut n = 1;
        loop {
            let dir = self.dir(&id);
            match fs::create_dir(&dir) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 100 => {
                    n += 1;
                    id = format!("{base}-{n}");
                }
                Err(e) => return Err(at(&dir)(e)),
            }
        }
        let m = Meeting {
            id,
            started_at: started_at_ms,
            ..Meeting::default()
        };
        write_json(&self.dir(&m.id), &m)?;
        Ok(m)
    }

    /// The folder of meeting `id`.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Every meeting, newest first. Folders without a readable `meeting.json` are skipped.
    pub fn list(&self) -> Vec<MeetingInfo> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new(); // no meeting yet
        };
        let mut list: Vec<MeetingInfo> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let id = e.file_name().into_string().ok()?;
                match self.load(&id) {
                    Ok(m) => Some(MeetingInfo {
                        has_summary: m.summary.as_deref().is_some_and(|s| !s.trim().is_empty()),
                        default_title: m.default_title(),
                        id: m.id,
                        title: m.title,
                        started_at: m.started_at,
                        ended_at: m.ended_at,
                    }),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                    Err(err) => {
                        tracing::warn!("skipping meeting {id}: {err}");
                        None
                    }
                }
            })
            .collect();
        list.sort_by(|a, b| (b.started_at, &b.id).cmp(&(a.started_at, &a.id)));
        list
    }

    /// Reads `meeting.json`. The folder's name is the id, whatever the file says.
    pub fn load(&self, id: &str) -> io::Result<Meeting> {
        let path = self.file(id, MEETING_FILE)?;
        let bytes = fs::read(&path).map_err(at(&path))?;
        let mut m: Meeting = serde_json::from_slice(&bytes)
            .map_err(|e| at(&path)(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        id.clone_into(&mut m.id);
        Ok(m)
    }

    /// The transcript so far, in order; empty before the first line. Lines that don't parse
    /// (one a crash cut short, even in the middle of a character) are skipped.
    pub fn lines(&self, id: &str) -> io::Result<Vec<Line>> {
        let path = self.file(id, TRANSCRIPT_FILE)?;
        let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        read_lines(&path)
    }

    /// Appends `line` to `transcript.jsonl`.
    pub fn append_line(&self, id: &str, line: &Line) -> io::Result<()> {
        let path = self.file(id, TRANSCRIPT_FILE)?;
        let mut json = serde_json::to_string(line).map_err(io::Error::other)?;
        json.push('\n');
        // Under the lock: a reader never sees the line half written.
        let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        append(&path, &json).map_err(at(&path))
    }

    /// Re-reads `meeting.json`, applies `f` and writes it back, all under the store's lock, so
    /// changes made from different threads never undo each other. Returns the result.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Meeting)) -> io::Result<Meeting> {
        let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let mut m = self.load(id)?;
        f(&mut m);
        id.clone_into(&mut m.id);
        write_json(&self.dir(id), &m)?;
        Ok(m)
    }

    /// Writes `meeting.md` (see [`markdown`]) from what's on disk now; returns its path.
    pub fn write_markdown(&self, id: &str) -> io::Result<PathBuf> {
        let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let md = markdown(
            &self.load(id)?,
            &read_lines(&self.file(id, TRANSCRIPT_FILE)?)?,
        );
        let path = self.file(id, MARKDOWN_FILE)?;
        write_atomic(&path, md.as_bytes())?;
        Ok(path)
    }

    /// Puts right the meetings AGT never got to stop (it was killed or crashed, or the Mac
    /// lost power): `ended_at` still null, and started before `started_before` (unix ms: the
    /// app's launch, so the meeting recording now is never one of them). Each gets `ended_at`
    /// from the last time its transcript or audio was written (its start if they never
    /// were), its WAV files' sizes patched so they read (they're zero until the writer
    /// finishes, see `agt_audio::wav::repair_sizes`), and its `meeting.md`. Returns their ids.
    /// Problems are logged; one meeting's never stops the others.
    pub fn recover(&self, started_before: i64) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new(); // no meeting yet
        };
        let mut recovered: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|id| {
                // Unreadable ones are skipped (and logged by `list`).
                self.load(id)
                    .is_ok_and(|m| m.ended_at.is_none() && m.started_at < started_before)
            })
            .filter(|id| match self.recover_one(id) {
                Ok(ended_at) => {
                    tracing::info!("meeting {id} never stopped; saved it as ended at {ended_at}");
                    true
                }
                Err(e) => {
                    tracing::warn!("cannot put right meeting {id}, which never stopped: {e}");
                    false
                }
            })
            .collect();
        recovered.sort();
        recovered
    }

    /// [`Store::recover`] for one meeting. Returns the `ended_at` it got.
    fn recover_one(&self, id: &str) -> io::Result<i64> {
        let dir = self.dir(id);
        let wavs: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(at(&dir))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
            .collect();
        // Before the WAV files are patched, which changes their times.
        let written = wavs
            .iter()
            .chain([&self.file(id, TRANSCRIPT_FILE)?])
            .filter_map(|p| modified_ms(p))
            .max();
        for wav in &wavs {
            match agt_audio::wav::repair_sizes(wav) {
                Ok(true) => tracing::info!("patched the sizes of {}", wav.display()),
                Ok(false) => {}
                // The audio is still there; only players can't read it. Not worth more.
                Err(e) => tracing::warn!("cannot patch {}: {e}", wav.display()),
            }
        }
        let m = self.update(id, |m| {
            let end = written.unwrap_or(m.started_at).max(m.started_at);
            m.ended_at.get_or_insert(end);
        })?;
        self.write_markdown(id)?;
        Ok(m.ended_at.unwrap_or(m.started_at))
    }

    /// `name` in meeting `id`'s folder. Ids also come from the notes window: only a plain
    /// folder name directly under the root is one.
    fn file(&self, id: &str, name: &str) -> io::Result<PathBuf> {
        let mut parts = Path::new(id).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(_)), None) => Ok(self.dir(id).join(name)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a meeting id: {id:?}"),
            )),
        }
    }
}

/// The meeting as Markdown: `# <title>` (the date one while untitled), the date, `## Notes` (the AI notes, their headings
/// moved down under it, `nested_headings`), `## My notes` (what the user typed) and
/// `## Transcript` (`[mm:ss] Me: ...` / `[mm:ss] Them: ...`, one paragraph each, in the order
/// they were said, not the order they were written). Empty sections are left out. The notes
/// window's Copy builds the same (`meetingMarkdown` in `notes.html`).
pub fn markdown(m: &Meeting, lines: &[Line]) -> String {
    let lines = in_order(lines);
    let mut md = format!("# {}\n\n{}\n", m.shown_title(), when(m));
    let summary = nested_headings(m.summary.as_deref().map(str::trim).unwrap_or_default());
    for (heading, body) in [("Notes", summary.as_str()), ("My notes", m.notes.trim())] {
        if !body.is_empty() {
            md += &format!("\n## {heading}\n\n{body}\n");
        }
    }
    if !lines.is_empty() {
        md += "\n## Transcript\n";
        for line in lines {
            md += &format!("\n{}\n", transcript_line(line));
        }
    }
    md
}

/// The AI notes' Markdown with its headings moved down so the highest is `###`: they sit
/// under the export's `## Notes`, and their own `## Summary` would otherwise read as a section
/// of the export, level with `## Transcript`. Claude writes `##` sections
/// ([`SYSTEM_PROMPT`](crate::summary::SYSTEM_PROMPT)), so that is one level: `## Summary`
/// becomes `### Summary`. Every heading moves the same, so they keep their order, and none
/// goes past `######`. Only `#` headings, and not in fenced code blocks.
pub(crate) fn nested_headings(md: &str) -> String {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut fence = None;
    let levels: Vec<Option<usize>> = lines
        .iter()
        .map(|line| {
            let (indent, text) = indented(line);
            let marker = if indent <= 3 {
                fence_marker(text)
            } else {
                None
            };
            match (fence, marker) {
                (Some(open), Some(marker)) if marker == open => {
                    fence = None;
                    None
                }
                (Some(_), _) => None,
                (None, Some(marker)) => {
                    fence = Some(marker);
                    None
                }
                (None, None) if indent <= 3 => heading_level(text),
                (None, None) => None,
            }
        })
        .collect();
    let Some(&top) = levels.iter().flatten().min() else {
        return md.to_owned();
    };
    let down = 3usize.saturating_sub(top);
    if down == 0 {
        return md.to_owned();
    }
    let moved: Vec<String> = lines
        .iter()
        .zip(&levels)
        .map(|(line, level)| match *level {
            Some(level) => {
                let (indent, text) = indented(line);
                let more = "#".repeat((level + down).min(6) - level);
                format!("{}{more}{text}", &line[..indent])
            }
            None => (*line).to_owned(),
        })
        .collect();
    moved.join("\n")
}

/// The leading spaces' length, and the rest of `line`.
fn indented(line: &str) -> (usize, &str) {
    let text = line.trim_start_matches(' ');
    (line.len() - text.len(), text)
}

/// The marker of a line opening or closing a fenced code block.
fn fence_marker(text: &str) -> Option<&'static str> {
    ["```", "~~~"].into_iter().find(|f| text.starts_with(f))
}

/// An ATX heading's level: 1 to 6 `#`, then a space, a tab or the end of the line.
fn heading_level(text: &str) -> Option<usize> {
    let rest = text.trim_start_matches('#');
    let level = text.len() - rest.len();
    let ends = rest.is_empty() || rest.starts_with([' ', '\t', '\r']);
    ((1..=6).contains(&level) && ends).then_some(level)
}

/// When the meeting was: `Oct 16, 2026, 2:30 PM (45 min)`.
pub(crate) fn when(m: &Meeting) -> String {
    let start = LocalTime::at(m.started_at).long();
    match m.ended_at {
        Some(end) => format!("{start} ({})", time::duration(end - m.started_at)),
        None => start,
    }
}

/// `lines` in the order they were said: by start, then id. `transcript.jsonl` has them in the
/// order they were final, and a Me line is final after the Them lines said around it (it waits
/// for the echo check).
pub(crate) fn in_order(lines: &[Line]) -> Vec<&Line> {
    let mut lines: Vec<&Line> = lines.iter().collect();
    lines.sort_by_key(|l| (l.start_ms, l.id));
    lines
}

/// `[00:05] Them: hello`.
pub(crate) fn transcript_line(line: &Line) -> String {
    format!(
        "[{}] {}: {}",
        time::clock(line.start_ms),
        line.speaker.label(),
        line.text.trim()
    )
}

/// Reads `transcript.jsonl` at `path` as bytes, line by line, so that one broken line (cut
/// short by a crash, maybe inside a multibyte character) costs only itself.
fn read_lines(path: &Path) -> io::Result<Vec<Line>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(at(path)(e)),
    };
    let mut skipped = 0;
    let lines = bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
        .filter_map(|l| {
            let line = serde_json::from_slice(l).ok();
            skipped += usize::from(line.is_none());
            line
        })
        .collect();
    if skipped > 0 {
        tracing::warn!("skipped {skipped} unreadable line(s) in {}", path.display());
    }
    Ok(lines)
}

/// When `path` was last written, in unix ms; `None` if it isn't there (or the system doesn't
/// say).
fn modified_ms(path: &Path) -> Option<i64> {
    let at = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let ms = at.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(ms).ok()
}

/// Adds the path to an I/O error, for the log.
fn at(path: &Path) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |e| io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

fn write_json(dir: &Path, m: &Meeting) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(m).map_err(io::Error::other)?;
    write_atomic(&dir.join(MEETING_FILE), &json)
}

/// Writes `path.tmp`, then renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes).map_err(at(&tmp))?;
    fs::rename(&tmp, path).map_err(at(path))
}

/// Appends `line` in one write, so a reader never sees half of one. If the file doesn't end
/// with a newline (a crash cut its last line short), one goes first: the broken line stays on
/// its own, and this one is intact.
fn append(path: &Path, line: &str) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Speaker, time::LocalTime};
    use std::fs;

    /// A fresh, empty folder for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agt-meeting-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(id: u64, speaker: Speaker, start_ms: u64, text: &str) -> Line {
        Line {
            id,
            speaker,
            start_ms,
            end_ms: start_ms + 1500,
            text: text.into(),
        }
    }

    /// 2026-10-16 12:30:05 UTC.
    const START: i64 = 1_792_153_805_000;

    #[test]
    fn create_then_load() {
        let store = Store::new(temp_dir("create"));
        let m = store.create(START).unwrap();
        let t = LocalTime::at(START);
        assert_eq!(m.id, t.id());
        // No title until the user or the AI notes give it one; the date stands in for it.
        assert_eq!(m.title, "");
        assert_eq!(m.default_title(), format!("Meeting {}", t.short()));
        assert_eq!(m.started_at, START);
        assert_eq!((m.ended_at, &*m.notes, &m.summary), (None, "", &None));
        assert!(store.dir(&m.id).join("meeting.json").is_file());
        assert_eq!(store.load(&m.id).unwrap(), m);

        // All fields round trip.
        let full = store
            .update(&m.id, |m| {
                m.title = "Weekly sync".into();
                m.ended_at = Some(START + 45 * 60_000);
                m.notes = "- ship friday\n- \"quotes\"".into();
                m.summary = Some("## Summary\n\nShipped.".into());
                m.summary_by = Some("claude sonnet".into());
                m.audio = vec!["me.wav".into(), "them.wav".into()];
            })
            .unwrap();
        assert_eq!(store.load(&m.id).unwrap(), full);
        assert_eq!(full.title, "Weekly sync");

        // A second meeting in the same second gets its own folder.
        let again = store.create(START).unwrap();
        assert_eq!(again.id, format!("{}-2", m.id));
        assert_eq!(store.load(&m.id).unwrap(), full);

        // Only plain folder names.
        for bad in ["", "..", "../x", "a/b"] {
            let err = store.load(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[test]
    fn list_newest_first() {
        let root = temp_dir("list");
        let store = Store::new(root.clone());
        assert_eq!(store.list(), []);
        let old = store.create(START).unwrap();
        let new = store.create(START + 3_600_000).unwrap();
        store
            .update(&old.id, |m| m.summary = Some("notes".into()))
            .unwrap();
        // Not meetings: a stray file, an empty folder, a folder with a broken meeting.json.
        fs::write(root.join(".DS_Store"), "x").unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        fs::create_dir(root.join("broken")).unwrap();
        fs::write(root.join("broken/meeting.json"), "{\"id\":").unwrap();

        let list = store.list();
        let ids: Vec<&str> = list.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, [new.id.as_str(), old.id.as_str()]);
        assert_eq!(
            list[1],
            MeetingInfo {
                default_title: old.default_title(),
                id: old.id,
                title: String::new(),
                started_at: START,
                ended_at: None,
                has_summary: true,
            }
        );
        assert!(!list[0].has_summary);
    }

    #[test]
    fn lines_skip_torn_tail() {
        let store = Store::new(temp_dir("lines"));
        let m = store.create(START).unwrap();
        assert_eq!(store.lines(&m.id).unwrap(), []);
        let a = line(1, Speaker::Them, 5_000, "hello there");
        let b = line(2, Speaker::Me, 7_000, "hi, \"all\"");
        store.append_line(&m.id, &a).unwrap();
        store.append_line(&m.id, &b).unwrap();
        // A crash halfway through a third line.
        let path = store.dir(&m.id).join("transcript.jsonl");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"id":3,"speaker":"me","sta"#).unwrap();
        drop(f);
        assert_eq!(store.lines(&m.id).unwrap(), [a.clone(), b.clone()]);

        // The next line starts on a line of its own: the broken one is still skipped.
        let c = line(4, Speaker::Them, 9_000, "next");
        store.append_line(&m.id, &c).unwrap();
        assert_eq!(store.lines(&m.id).unwrap(), [a, b, c]);
    }

    #[test]
    fn lines_survive_torn_multibyte_char() {
        let store = Store::new(temp_dir("lines-utf8"));
        let m = store.create(START).unwrap();
        let a = line(
            1,
            Speaker::Them,
            5_000,
            "we don\u{2019}t ship caf\u{e9} today",
        );
        store.append_line(&m.id, &a).unwrap();
        // A crash in the middle of the next line's curly apostrophe (3 bytes in UTF-8).
        let torn = serde_json::to_vec(&line(2, Speaker::Me, 7_000, "it\u{2019}s fine")).unwrap();
        let cut = torn
            .windows(3)
            .position(|w| w == "\u{2019}".as_bytes())
            .unwrap()
            + 1;
        let path = store.dir(&m.id).join("transcript.jsonl");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&torn[..cut]).unwrap();
        drop(f);
        assert!(std::str::from_utf8(&fs::read(&path).unwrap()).is_err());
        assert_eq!(store.lines(&m.id).unwrap(), std::slice::from_ref(&a));

        // Appending after the tear: both good lines read back, and the export still works.
        let b = line(3, Speaker::Them, 9_000, "na\u{ef}ve r\u{e9}sum\u{e9}");
        store.append_line(&m.id, &b).unwrap();
        assert_eq!(store.lines(&m.id).unwrap(), [a, b]);
        let md = fs::read_to_string(store.write_markdown(&m.id).unwrap()).unwrap();
        assert!(
            md.contains("[00:09] Them: na\u{ef}ve r\u{e9}sum\u{e9}\n"),
            "{md}"
        );
    }

    #[test]
    fn summary_save_keeps_newer_notes() {
        let store = Store::new(temp_dir("update"));
        let m = store.create(START).unwrap();
        // A summary starts from this copy...
        let stale = store.load(&m.id).unwrap();
        // ...the user keeps typing...
        store.update(&m.id, |m| m.notes = "new".into()).unwrap();
        // ...and the summary lands.
        let saved = store
            .update(&stale.id, |m| m.summary = Some("## Summary".into()))
            .unwrap();
        assert_eq!(saved.notes, "new");
        let m = store.load(&m.id).unwrap();
        assert_eq!(m.notes, "new");
        assert_eq!(m.summary.as_deref(), Some("## Summary"));
        // No temp file left behind.
        let names: Vec<_> = fs::read_dir(store.dir(&m.id))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["meeting.json"]);
    }

    #[test]
    fn markdown_has_sections() {
        let mut m = Meeting {
            id: "2026-10-16-143005".into(),
            title: "Weekly sync".into(),
            started_at: START,
            ended_at: Some(START + 45 * 60_000),
            notes: "- ship friday".into(),
            summary: Some("## Summary\n\nWe ship Friday.\n".into()),
            ..Meeting::default()
        };
        let lines = [
            line(1, Speaker::Them, 5_000, "hello"),
            line(2, Speaker::Me, 65_000, "so we ship friday"),
        ];
        let md = markdown(&m, &lines);
        assert!(md.starts_with("# Weekly sync\n"), "{md}");
        // Untitled: the date stands in.
        let untitled = Meeting {
            title: " ".into(),
            ..m.clone()
        };
        let want = format!("# Meeting {}\n", LocalTime::at(START).short());
        assert!(markdown(&untitled, &lines).starts_with(&want), "{md}");
        assert!(md.contains("45 min"), "{md}");
        // Claude's own `## Summary` goes under `## Notes`, a level down.
        let notes = md
            .find("## Notes\n\n### Summary\n\nWe ship Friday.\n")
            .expect(&md);
        let mine = md.find("## My notes\n\n- ship friday\n").expect(&md);
        let transcript = md.find("## Transcript\n").expect(&md);
        assert!(notes < mine && mine < transcript, "{md}");
        assert!(md.contains("[00:05] Them: hello\n"), "{md}");
        assert!(md.contains("[01:05] Me: so we ship friday\n"), "{md}");

        // Without a summary or notes, those sections are left out.
        m.summary = None;
        m.notes = " \n".into();
        let md = markdown(&m, &lines);
        assert!(
            !md.contains("## Notes") && !md.contains("## My notes"),
            "{md}"
        );
        assert!(md.contains("## Transcript"), "{md}");

        // write_markdown puts it in the folder.
        let store = Store::new(temp_dir("markdown"));
        let created = store.create(START).unwrap();
        store
            .update(&created.id, |c| c.notes = "- ship friday".into())
            .unwrap();
        store.append_line(&created.id, &lines[0]).unwrap();
        let path = store.write_markdown(&created.id).unwrap();
        assert_eq!(path, store.dir(&created.id).join("meeting.md"));
        let md = fs::read_to_string(path).unwrap();
        assert!(md.contains("## My notes\n\n- ship friday\n"), "{md}");
        assert!(md.contains("[00:05] Them: hello\n"), "{md}");
    }

    #[test]
    fn ai_notes_headings_go_under_notes() {
        let notes = "## Summary\nShipped.\n\n## Action items\n- [ ] Me: #42 by Friday\n\n\
                     ```\n## not a heading\n```\n#hashtag\n   ### indented\n    ## code";
        assert_eq!(
            nested_headings(notes),
            "### Summary\nShipped.\n\n### Action items\n- [ ] Me: #42 by Friday\n\n\
             ```\n## not a heading\n```\n#hashtag\n   #### indented\n    ## code"
        );
        // Every level moves the same; none past six.
        assert_eq!(
            nested_headings("# Top\n## Sub\n###### Deep\n#\n####### seven"),
            "### Top\n#### Sub\n###### Deep\n###\n####### seven"
        );
        // Already below `##`, or no headings at all: as it is.
        for md in [
            "### Summary\n#### More",
            "Just text.\n- a\n",
            "",
            "~~~\n# in code\n~~~",
        ] {
            assert_eq!(nested_headings(md), md);
        }
        // An unclosed fence runs to the end.
        assert_eq!(nested_headings("## A\n```\n## B"), "### A\n```\n## B");
    }

    #[test]
    fn recover_puts_right_meetings_that_never_stopped() {
        use agt_audio::wav::{read_16k_mono, write_16k_mono};
        let store = Store::new(temp_dir("recover"));
        let launch = START + 86_400_000;
        // Killed while recording: no ended_at, no meeting.md, a WAV with zero sizes.
        let killed = store.create(START).unwrap();
        store
            .append_line(&killed.id, &line(1, Speaker::Them, 5_000, "hello"))
            .unwrap();
        let wav = store.dir(&killed.id).join("system.wav");
        write_16k_mono(&wav, &[0.25; 1600]).unwrap();
        let mut bytes = fs::read(&wav).unwrap();
        bytes[4..8].fill(0);
        bytes[40..44].fill(0);
        fs::write(&wav, bytes).unwrap();
        store
            .update(&killed.id, |m| m.audio.push("system.wav".into()))
            .unwrap();
        assert_eq!(read_16k_mono(&wav).map(|s| s.len()).unwrap_or(0), 0);
        let written = [store.dir(&killed.id).join("transcript.jsonl"), wav.clone()]
            .iter()
            .filter_map(|p| modified_ms(p))
            .max()
            .unwrap();
        // Killed before it said anything.
        let empty = store.create(START + 60_000).unwrap();
        // Stopped properly, and the one recording now (started after the launch): left alone.
        let done = store.create(START + 120_000).unwrap();
        store
            .update(&done.id, |m| m.ended_at = Some(START + 180_000))
            .unwrap();
        let live = store.create(launch).unwrap();

        assert_eq!(store.recover(launch), [killed.id.clone(), empty.id.clone()]);
        let m = store.load(&killed.id).unwrap();
        assert_eq!(m.ended_at, Some(written.max(START)));
        assert_eq!(read_16k_mono(&wav).unwrap().len(), 1600);
        let md = fs::read_to_string(store.dir(&killed.id).join("meeting.md")).unwrap();
        assert!(md.contains("[00:05] Them: hello\n"), "{md}");
        // Nothing written: it ended when it started.
        assert_eq!(
            store.load(&empty.id).unwrap().ended_at,
            Some(START + 60_000)
        );
        assert!(store.dir(&empty.id).join("meeting.md").is_file());
        assert_eq!(
            store.load(&done.id).unwrap().ended_at,
            Some(START + 180_000)
        );
        assert!(!store.dir(&done.id).join("meeting.md").exists());
        assert_eq!(store.load(&live.id).unwrap().ended_at, None);
        assert!(!store.dir(&live.id).join("meeting.md").exists());
        // Once is enough.
        assert_eq!(store.recover(launch), Vec::<String>::new());
        // No meetings folder yet: nothing to do.
        assert!(
            Store::new(temp_dir("recover-none").join("x"))
                .recover(launch)
                .is_empty()
        );
    }

    #[test]
    fn markdown_sorts_by_start() {
        let m = Meeting {
            title: "Sync".into(),
            started_at: START,
            ..Meeting::default()
        };
        // In the order they were written: the Me line waited for the echo check.
        let lines = [
            line(1, Speaker::Them, 7_000, "them later"),
            line(2, Speaker::Me, 5_000, "me first"),
            line(3, Speaker::Them, 5_000, "them at the same time"),
        ];
        let md = markdown(&m, &lines);
        let at = |text: &str| md.find(text).expect(&md);
        // By start, then by id.
        assert!(at("me first") < at("them at the same time"), "{md}");
        assert!(at("them at the same time") < at("them later"), "{md}");
    }
}
