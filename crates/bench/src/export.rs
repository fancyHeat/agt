//! `agt-bench export-history [--days n] [--out dir]`: turns the dictation history into a
//! personal eval set, the ground truth the dictionary work is measured against.
//!
//! Every saved dictation with audio (`[history]` `save_audio`, see `agt-history`) is copied
//! to `dir` as `<YYYY-MM-DD>-<HHMMSS-mmm>.wav`, with a `.txt` reference next to it holding the
//! engine's transcript (`raw`; empty if text wasn't saved) for the user to correct by hand.
//! A `.txt` that is already there is never overwritten, so corrections survive exporting
//! again. Default `dir`: `eval` in the history folder (next to `dictations`; on Windows
//! outside the install folder, which a reinstall wipes). It holds what the user said: it
//! stays on this machine and never belongs in the repository.

use agt_core::paths;
use std::{
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
    let out = out.unwrap_or_else(|| paths::history_dir().join("eval"));
    let since = days.map(|d| now_ms() - (d as i64).saturating_mul(86_400_000));
    let n = export(&paths::dictations_dir(), &out, since)?;
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
    no_audio: usize,
    missing: usize,
}

/// Exports the records in `dictations` started at or after `since` (unix ms) to `out`.
fn export(dictations: &Path, out: &Path, since: Option<i64>) -> Result<Counts, String> {
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
            fs::copy(wav, &to).map_err(|e| format!("{}: {e}", to.display()))?;
            n.new_wavs += 1;
        }
        let txt = to.with_extension("txt");
        if !txt.exists() {
            let text = s.raw.as_deref().unwrap_or("");
            fs::write(&txt, format!("{text}\n")).map_err(|e| format!("{}: {e}", txt.display()))?;
            n.new_txts += 1;
        }
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
        let n = export(&history, &out, None).unwrap();
        assert_eq!(
            n,
            Counts {
                clips: 2,
                new_wavs: 2,
                new_txts: 2,
                no_audio: 1,
                missing: 1
            }
        );
        let first = out.join("2026-10-16-090000-001");
        assert_eq!(fs::read(first.with_extension("wav")).unwrap(), b"RIFF one");
        let txt = first.with_extension("txt");
        assert_eq!(fs::read_to_string(&txt).unwrap(), "merge into Maine\n");
        assert_eq!(
            fs::read_to_string(out.join("2026-10-16-090500-002.txt")).unwrap(),
            "\n"
        );
        // The user's correction survives exporting again.
        fs::write(&txt, "merge into main\n").unwrap();
        let n = export(&history, &out, None).unwrap();
        assert_eq!((n.clips, n.new_wavs, n.new_txts), (2, 0, 0));
        assert_eq!(fs::read_to_string(&txt).unwrap(), "merge into main\n");
        // Only the recent ones.
        let n = export(&history, &dir.join("recent"), Some(2000)).unwrap();
        assert_eq!((n.clips, n.no_audio, n.missing), (1, 1, 1));
        fs::remove_dir_all(&dir).unwrap();
    }
}
