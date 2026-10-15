//! `agt-bench`: run an STT engine over a directory of WAV files and report latency,
//! real-time factor and (when a `<name>.txt` reference sits next to `<name>.wav`) WER.
//!
//! ```text
//! agt-bench [options] <dir-with-wavs>
//!   --config <file>     start from this config.toml's [stt] section (default: built-in defaults)
//!   --model <name>      override stt.model
//!   --precision <p>     override stt.precision (int8 | fp32)
//!   --device <d>        override stt.device (cpu | directml)
//!   --threads <n>       override stt.threads
//!   --models-dir <dir>  override stt.models_dir
//!   --runs <n>          transcribe each file n times (default 3); latency = median
//!   --no-warmup         skip warmup, so the first run shows the cold-start cost
//! ```
//!
//! Engine-agnostic: it only uses `agt_stt::load` and the `SttEngine` trait.

use agt_core::{
    Config,
    config::{Device, Precision, SttConfig},
    stt::{SAMPLE_RATE, TranscribeOpts},
};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agt-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // Engine logs (per-stage timings at debug) go to stderr; level from AGT_LOG (default warn).
    let level = std::env::var("AGT_LOG").ok().and_then(|l| l.parse().ok());
    tracing_subscriber::fmt()
        .with_max_level(level.unwrap_or(tracing::Level::WARN))
        .with_writer(std::io::stderr)
        .init();
    let mut cfg = SttConfig::default();
    let (mut runs, mut warmup, mut dir) = (3usize, true, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--config" => {
                let p = val()?;
                let text = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
                cfg = Config::parse(&text).map_err(|e| e.to_string())?.stt;
            }
            "--model" => cfg.model = val()?,
            "--precision" => {
                cfg.precision = match val()?.as_str() {
                    "int8" => Precision::Int8,
                    "fp32" => Precision::Fp32,
                    p => return Err(format!("unknown precision {p}")),
                }
            }
            "--device" => {
                cfg.device = match val()?.as_str() {
                    "cpu" => Device::Cpu,
                    "directml" => Device::DirectMl,
                    d => return Err(format!("unknown device {d}")),
                }
            }
            "--threads" => cfg.threads = val()?.parse().map_err(|_| "bad --threads")?,
            "--models-dir" => cfg.models_dir = val()?,
            "--runs" => runs = val()?.parse::<usize>().map_err(|_| "bad --runs")?.max(1),
            "--no-warmup" => warmup = false,
            "-h" | "--help" => {
                println!(
                    "usage: agt-bench [--config f] [--model m] [--precision int8|fp32] [--device cpu|directml] [--threads n] [--models-dir d] [--runs n] [--no-warmup] <dir>"
                );
                return Ok(());
            }
            _ if dir.is_none() && !a.starts_with("--") => dir = Some(PathBuf::from(a)),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    let dir = dir.ok_or("usage: agt-bench [options] <dir-with-wavs>  (--help for options)")?;

    let mut wavs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .collect();
    wavs.sort();
    if wavs.is_empty() {
        return Err(format!("no .wav files in {}", dir.display()));
    }

    let t = Instant::now();
    let mut engine = agt_stt::load(&cfg, &mut |p| {
        eprintln!(
            "downloading model: {:.0}%",
            p.done as f64 * 100.0 / p.total.max(1) as f64
        )
    })
    .map_err(|e| e.to_string())?;
    let load_ms = ms(t);
    println!("engine: {}", engine.name());
    println!("load: {load_ms:.0} ms");
    if warmup {
        let t = Instant::now();
        engine.warmup().map_err(|e| e.to_string())?;
        println!("warmup: {:.0} ms", ms(t));
    }

    let opts = TranscribeOpts::default();
    let (mut total_audio, mut total_ms, mut total_err, mut total_words) = (0.0, 0.0, 0, 0);
    println!();
    println!(
        "{:<28} {:>7} {:>9} {:>9} {:>7} {:>6}  transcript",
        "file", "audio s", "first ms", "median ms", "RTF", "WER"
    );
    for wav in &wavs {
        let audio = agt_audio::wav::read_16k_mono(wav)?;
        let secs = audio.len() as f64 / SAMPLE_RATE as f64;
        let mut times = Vec::with_capacity(runs);
        let mut text = String::new();
        for _ in 0..runs {
            let t = Instant::now();
            text = engine
                .transcribe(&audio, &opts)
                .map_err(|e| e.to_string())?
                .text;
            times.push(ms(t));
        }
        let first = times[0];
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        total_audio += secs;
        total_ms += median;
        let wer = read_reference(wav).map(|r| {
            let (errs, words) = word_errors(&r, &text);
            total_err += errs;
            total_words += words;
            format!("{:5.1}%", errs as f64 * 100.0 / words.max(1) as f64)
        });
        println!(
            "{:<28} {secs:>7.2} {first:>9.1} {median:>9.1} {:>7.4} {:>6}  {text}",
            name(wav),
            median / 1000.0 / secs,
            wer.as_deref().unwrap_or("-"),
        );
    }
    println!();
    print!(
        "total: {total_audio:.1} s audio in {total_ms:.0} ms (RTF {:.4})",
        total_ms / 1000.0 / total_audio
    );
    if total_words > 0 {
        print!(
            ", WER {:.2}% ({total_err}/{total_words} words)",
            total_err as f64 * 100.0 / total_words as f64
        );
    }
    println!();
    Ok(())
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_reference(wav: &Path) -> Option<String> {
    std::fs::read_to_string(wav.with_extension("txt")).ok()
}

/// Lowercased words with punctuation stripped (apostrophes inside words kept); hyphens split
/// words, so "double-check" and "double check" compare equal.
fn normalize(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == '-')
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(char::to_lowercase)
                .collect::<String>()
                .trim_matches('\'')
                .to_owned()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// (substitutions + deletions + insertions, reference word count).
fn word_errors(reference: &str, hypothesis: &str) -> (usize, usize) {
    let (r, h) = (normalize(reference), normalize(hypothesis));
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(rw != hw))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    (prev[h.len()], r.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_counts_edits_ignoring_case_and_punctuation() {
        assert_eq!(word_errors("Hello, world.", "hello world"), (0, 2));
        assert_eq!(word_errors("the cat sat", "the bat sat down"), (2, 3));
        assert_eq!(word_errors("a b c", ""), (3, 3));
        assert_eq!(word_errors("Don't stop", "don't stop"), (0, 2));
        assert_eq!(word_errors("double check", "double-check"), (0, 2));
    }
}
