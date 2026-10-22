//! `agt-bench export-history [--days n] [--out dir]`: turns the dictation history into a
//! personal eval set, the ground truth the dictionary work is measured against.
//!
//! Every saved dictation with audio (`[history]` `save_audio`, see `agt-history`) is copied
//! to `dir` as `<YYYY-MM-DD>-<HHMMSS-mmm>.wav`, with a `.txt` reference next to it holding the
//! engine's transcript (`raw`; empty if text wasn't saved) for the user to correct by hand.
//! A `.txt` that is already there is never overwritten, so corrections survive exporting
//! again; until one is filled in, the bench skips its clip (an empty reference isn't one).
//! A WAV is copied under a temporary name and renamed when complete, so an interrupted
//! export never leaves a truncated one that later exports would skip.
//!
//! The app that had focus (`dictations.app_id` in agt.db, joined on `started_at`; the history
//! records don't hold it) goes in a `<name>.app` next to them, one line, when known: the bench
//! applies the dictionary's profiles for it, as AGT did. Written if missing, never changed.
//!
//! Default `dir`: [`paths::eval_dir`], `eval` next to `dictations`: on Windows outside the
//! install folder, which re-running Setup wipes, and kept when AGT is uninstalled (the rest
//! of the history goes). It holds what the user said: it stays on this machine and never
//! belongs in the repository.

use agt_core::paths;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const USAGE: &str = "usage: agt-bench export-history [--days n] [--out dir]";

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut days, mut out) = (None, None);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--days" => days = Some(val()?.parse::<u64>().map_err(|_| "bad --days")?),
            "--out" => out = Some(PathBuf::from(val()?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(format!("unexpected argument {a}\n{USAGE}")),
        }
    }
    let out = out.unwrap_or_else(paths::eval_dir);
    let since = days.map(|d| now_ms() - (d as i64).saturating_mul(86_400_000));
    // Not having the database (or an old one) only means no app context.
    let apps = agt_telemetry::app_ids(&paths::db_file()).unwrap_or_else(|e| {
        eprintln!("no app ids ({e}); only the dictionary's global terms will apply");
        Default::default()
    });
    let n = export(&paths::dictations_dir(), &out, since, &apps)?;
    println!(
        "{} clips in {}: {} new, {} already there; {} references written, {} kept (never overwritten)",
        n.clips,
        out.display(),
        n.new_wavs,
        n.clips - n.new_wavs,
        n.new_txts,
        n.clips - n.new_txts
    );
    if n.no_audio + n.missing > 0 {
        println!(
            "skipped {} dictations without audio ([history] save_audio off then) and {} whose WAV is gone",
            n.no_audio, n.missing
        );
    }
    if n.apps > 0 {
        println!(
            "{} clips have the app they were dictated in (.app files)",
            n.apps
        );
    }
    if n.empty_txts > 0 {
        println!(
            "{} references are empty (transcripts weren't saved, or nothing was recognized): write what was said in them, or the bench skips those clips",
            n.empty_txts
        );
    }
    if n.clips > 0 {
        println!(
            "Next: fix each .txt by hand to exactly what you said (case matters for the term counts), then run\n  agt-bench {}\n  agt-bench --no-dictionary {}",
            out.display(),
            out.display()
        );
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct Counts {
    /// Exported (WAV in `out`, new or not).
    clips: usize,
    new_wavs: usize,
    new_txts: usize,
    /// Exported with an empty `.txt` (new or not): still to be written by hand.
    empty_txts: usize,
    /// Exported with a `.app` (new or not).
    apps: usize,
    no_audio: usize,
    missing: usize,
}

/// Exports the records in `dictations` started at or after `since` (unix ms) to `out`, with
/// the app of each in `apps` (by `started_at`).
fn export(
    dictations: &Path,
    out: &Path,
    since: Option<i64>,
    apps: &HashMap<i64, String>,
) -> Result<Counts, String> {
    let mut n = Counts::default();
    let saved = agt_history::read(dictations);
    for s in saved
        .iter()
        .filter(|s| since.is_none_or(|t| s.started_at >= t))
    {
        let Some(wav) = &s.audio else {
            n.no_audio += 1;
            continue;
        };
        let Some(stem) = wav
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|_| wav.is_file())
        else {
            n.missing += 1;
            continue;
        };
        if n.clips == 0 {
            fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
        }
        let name = format!("{}-{stem}", s.day);
        let to = out.join(format!("{name}.wav"));
        if !to.exists() {
            // Whole or not at all (see the module docs).
            let tmp = to.with_extension("wav.tmp");
            fs::copy(wav, &tmp)
                .and_then(|_| fs::rename(&tmp, &to))
                .map_err(|e| {
                    let _ = fs::remove_file(&tmp);
                    format!("{}: {e}", to.display())
                })?;
            n.new_wavs += 1;
        }
        let txt = to.with_extension("txt");
        if !txt.exists() {
            let text = s.raw.as_deref().unwrap_or("");
            fs::write(&txt, format!("{text}\n")).map_err(|e| format!("{}: {e}", txt.display()))?;
            n.new_txts += 1;
        }
        let app = to.with_extension("app");
        if let Some(id) = apps.get(&s.started_at).filter(|_| !app.exists()) {
            fs::write(&app, format!("{id}\n")).map_err(|e| format!("{}: {e}", app.display()))?;
        }
        n.apps += usize::from(app.exists());
        let reference = fs::read_to_string(&txt).unwrap_or_default();
        n.empty_txts += usize::from(reference.trim().is_empty());
        n.clips += 1;
    }
    Ok(n)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_audio_with_references_and_keeps_corrections() {
        let dir = std::env::temp_dir().join(format!("agt-export-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (history, out) = (dir.join("dictations"), dir.join("eval"));
        let day = history.join("2026-10-16");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("090000-001.wav"), b"RIFF one").unwrap();
        fs::write(day.join("090500-002.wav"), b"RIFF two").unwrap();
        fs::write(
            day.join("dictations.jsonl"),
            [
                r#"{"started_at":1000,"audio":"090000-001.wav","audio_secs":1,"raw":"merge into Maine"}"#,
                r#"{"started_at":2000,"audio":"090500-002.wav","audio_secs":1}"#,
                r#"{"started_at":3000,"audio_secs":1,"raw":"no audio"}"#,
                r#"{"started_at":4000,"audio":"gone.wav","audio_secs":1}"#,
                "",
            ]
            .join("\n"),
        )
        .unwrap();
        // The first one's app is known (agt.db); the second's isn't.
        let apps = HashMap::from([(1000, "Code.exe".to_owned()), (3000, "x.exe".to_owned())]);
        let n = export(&history, &out, None, &apps).unwrap();
        assert_eq!(
            n,
            Counts {
                clips: 2,
                new_wavs: 2,
                new_txts: 2,
                empty_txts: 1,
                apps: 1,
                no_audio: 1,
                missing: 1
            }
        );
        // No temporary file left behind.
        assert!(
            fs::read_dir(&out).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let first = out.join("2026-10-16-090000-001");
        assert_eq!(fs::read(first.with_extension("wav")).unwrap(), b"RIFF one");
        let txt = first.with_extension("txt");
        assert_eq!(fs::read_to_string(&txt).unwrap(), "merge into Maine\n");
        assert_eq!(
            fs::read_to_string(first.with_extension("app")).unwrap(),
            "Code.exe\n"
        );
        assert!(!out.join("2026-10-16-090500-002.app").exists());
        assert_eq!(
            fs::read_to_string(out.join("2026-10-16-090500-002.txt")).unwrap(),
            "\n"
        );
        // The user's corrections survive exporting again.
        fs::write(&txt, "merge into main\n").unwrap();
        fs::write(out.join("2026-10-16-090500-002.txt"), "hello\n").unwrap();
        let n = export(&history, &out, None, &HashMap::new()).unwrap();
        assert_eq!(
            (n.clips, n.new_wavs, n.new_txts, n.empty_txts, n.apps),
            (2, 0, 0, 0, 1)
        );
        assert_eq!(fs::read_to_string(&txt).unwrap(), "merge into main\n");
        // Only the recent ones.
        let n = export(&history, &dir.join("recent"), Some(2000), &apps).unwrap();
        assert_eq!((n.clips, n.no_audio, n.missing), (1, 1, 1));
        fs::remove_dir_all(&dir).unwrap();
    }
}
