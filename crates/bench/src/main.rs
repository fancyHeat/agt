//! `agt-bench`: run an STT engine over a directory of WAV files and report latency,
//! real-time factor, memory and (when a `<name>.txt` reference sits next to `<name>.wav`) WER.
//!
//! ```text
//! agt-bench [options] <dir-with-wavs>
//!   --config <file>     start from this config.toml's [stt] section (default: built-in defaults)
//!   --engine <e>        override stt.engine
//!   --model <name>      override stt.model
//!   --precision <p>     override stt.precision
//!   --device <d>        override stt.device
//!   --threads <n>       override stt.threads
//!   --models-dir <dir>  override stt.models_dir
//!   --runs <n>          transcribe each file n times (default 3); latency = median
//!   --gap-ms <n>        sleep n ms before every transcription (default 0: back to back), to see
//!                       what an idle GPU/CPU costs the next call (clock-down, wake-up)
//!   --no-warmup         skip warmup, so the first run shows the cold-start cost
//!   --quiet             no per-file lines, only the summary
//!   --dictionary <file> use this dictionary.toml (default: AGT's, if there is one)
//!   --no-dictionary     don't apply it (its terms are still counted, for a before/after)
//! ```
//!
//! With a dictionary, its terms go to the engine and its replacements are applied to each
//! transcript, exactly as AGT does before polish, and a per-term table (see [`terms`]) counts
//! each term (and each replacement's `to`) in the references and transcripts, case-sensitive.
//!
//! `agt-bench export-history [--days n] [--out dir]` turns the dictation history into an
//! eval set of `.wav` + `.txt` reference pairs (see [`export`]).
//!
//! Clips are grouped for the summary by duration (short < 6 s, medium 6-20 s, long > 20 s)
//! and, for WER, by "set": the file name up to `-short-`/`-medium-`/`-long-` (the naming used
//! by `scripts/prepare_eval.py`). If `manifest.tsv` from that script is in the WAV directory
//! or its parent, sets are also rolled up by its `category` column (read vs real speech).
//!
//! Engine-agnostic: it only uses `agt_stt::load` and the `SttEngine` trait.
//!
//! `agt-bench live [options] <dir>` simulates recording long dictations and transcribing them
//! while they are being recorded (see [`live`]).
//!
//! `agt-bench polish [options] <transcripts.txt>` instead runs the LLM clean-up (`[polish]`)
//! over each non-empty line of a text file and prints latency, the guard outcome and the
//! result (see [`polish`]).

mod export;
mod live;
mod polish;

mod mem;
mod terms;
mod wer;

use agt_core::{
    Config, Dictionary,
    config::SttConfig,
    paths,
    stt::{SAMPLE_RATE, TranscribeOpts},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

const USAGE: &str = "usage: agt-bench [--config f] [--engine e] [--model m] [--precision p] [--device d] [--threads n] [--models-dir d] [--runs n] [--gap-ms n] [--no-warmup] [--quiet] [--dictionary f] [--no-dictionary] <dir>\n       agt-bench export-history [--days n] [--out dir]\n       agt-bench live [options] <dir>\n       agt-bench polish [options] <transcripts.txt>";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agt-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parses one config enum value (e.g. `"directml"`) exactly like `config.toml` would.
fn enum_value<T: serde::de::DeserializeOwned>(flag: &str, v: String) -> Result<T, String> {
    toml::Value::String(v.clone())
        .try_into()
        .map_err(|_| format!("{flag}: unknown value {v:?}"))
}

struct Clip {
    name: String,
    set: String,
    secs: f64,
    median_ms: f64,
    errors: Option<(usize, usize)>,
}

fn run() -> Result<(), String> {
    // Engine logs (per-stage timings at debug) go to stderr; level from AGT_LOG (default warn).
    let level = std::env::var("AGT_LOG").ok().and_then(|l| l.parse().ok());
    tracing_subscriber::fmt()
        .with_max_level(level.unwrap_or(tracing::Level::WARN))
        .with_writer(std::io::stderr)
        .init();
    match std::env::args().nth(1).as_deref() {
        Some("polish") => return polish::run(std::env::args().skip(2)),
        Some("live") => return live::run(std::env::args().skip(2)),
        Some("export-history") => return export::run(std::env::args().skip(2)),
        _ => {}
    }
    let mut cfg = SttConfig::default();
    let (mut runs, mut warmup, mut quiet, mut dir) = (3usize, true, false, None);
    // `None`: AGT's own, if there is one.
    let (mut dictionary_file, mut apply_dictionary) = (None, true);
    let mut gap = std::time::Duration::ZERO;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if stt_flag(&mut cfg, &a, &mut args)? {
            continue;
        }
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--runs" => runs = val()?.parse::<usize>().map_err(|_| "bad --runs")?.max(1),
            "--gap-ms" => {
                gap = std::time::Duration::from_millis(val()?.parse().map_err(|_| "bad --gap-ms")?)
            }
            "--no-warmup" => warmup = false,
            "--quiet" => quiet = true,
            "--dictionary" => dictionary_file = Some(PathBuf::from(val()?)),
            "--no-dictionary" => apply_dictionary = false,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if dir.is_none() && !a.starts_with("--") => dir = Some(PathBuf::from(a)),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    let dir = dir.ok_or(format!("{USAGE}  (--help for options)"))?;

    let mut wavs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .collect();
    wavs.sort();
    if wavs.is_empty() {
        return Err(format!("no .wav files in {}", dir.display()));
    }
    let categories = read_categories(&dir);
    let dictionary_file =
        dictionary_file.or_else(|| Some(paths::dictionary_file()).filter(|p| p.is_file()));
    // Its terms are counted either way; it's applied unless --no-dictionary.
    let counted = match &dictionary_file {
        Some(p) => Dictionary::load(p).map_err(|e| format!("{}: {e}", p.display()))?,
        None => Dictionary::default(),
    };
    let dictionary = match apply_dictionary {
        true => counted.clone(),
        false => Dictionary::default(),
    };

    let rss_before = mem::peak_rss();
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
    let mut warmup_ms = None;
    if warmup {
        let t = Instant::now();
        engine.warmup().map_err(|e| e.to_string())?;
        warmup_ms = Some(ms(t));
        println!("warmup: {:.0} ms", ms(t));
    }

    match &dictionary_file {
        Some(p) => println!(
            "dictionary: {} ({} terms, {} replacements){}",
            p.display(),
            counted.terms().len(),
            counted.replacements().len(),
            if apply_dictionary {
                ""
            } else {
                "; not applied, terms only counted"
            }
        ),
        None => println!("dictionary: none"),
    }
    // As the app passes them (see the pipeline's `Hints`).
    let opts = TranscribeOpts {
        vocabulary: dictionary.terms().to_vec(),
        ..Default::default()
    };
    let mut term_counts = terms::Terms::new(terms::of(&counted));
    let mut replacements = 0;
    let mut clips = Vec::new();
    if !quiet {
        println!();
        println!(
            "{:<22} {:>7} {:>9} {:>9} {:>7} {:>6}  transcript",
            "file", "audio s", "first ms", "median ms", "RTF", "WER"
        );
    }
    for wav in &wavs {
        let audio = agt_audio::wav::read_16k_mono(wav)?;
        let secs = audio.len() as f64 / SAMPLE_RATE as f64;
        let mut times = Vec::with_capacity(runs);
        let mut text = String::new();
        for _ in 0..runs {
            std::thread::sleep(gap);
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
        // What AGT does before polish (the pipeline's polish thread).
        let (replaced, n) = dictionary.replace(&text);
        replacements += n;
        let text = replaced.into_owned();
        let reference = read_reference(wav);
        if let Some(r) = &reference {
            term_counts.add(r, &text);
        }
        let errors = reference.map(|r| wer::word_errors(&r, &text));
        let name = name(wav);
        if !quiet {
            println!(
                "{:<22} {secs:>7.2} {first:>9.1} {median:>9.1} {:>7.4} {:>6}  {text}",
                name,
                median / 1000.0 / secs,
                errors.map_or("-".into(), |(e, w)| format!("{:5.1}%", pct(e, w))),
            );
        }
        clips.push(Clip {
            set: set_of(&name),
            name,
            secs,
            median_ms: median,
            errors,
        });
    }

    let (audio, total): (f64, f64) = clips
        .iter()
        .fold((0.0, 0.0), |(a, t), c| (a + c.secs, t + c.median_ms));
    println!();
    println!(
        "total: {audio:.1} s audio in {total:.0} ms (RTF {:.4}), {} runs per clip",
        total / 1000.0 / audio,
        runs
    );

    println!();
    println!(
        "{:<8} {:>5} {:>8} {:>8} {:>8} {:>8}",
        "latency", "clips", "avg s", "p50 ms", "p90 ms", "max ms"
    );
    let mut lat = Vec::new();
    for (bucket, lo, hi) in [
        ("short", 0.0, 6.0),
        ("medium", 6.0, 20.0),
        ("long", 20.0, f64::MAX),
    ] {
        let mut t: Vec<f64> = clips
            .iter()
            .filter(|c| c.secs >= lo && c.secs < hi)
            .map(|c| c.median_ms)
            .collect();
        if t.is_empty() {
            continue;
        }
        let avg_s = clips
            .iter()
            .filter(|c| c.secs >= lo && c.secs < hi)
            .map(|c| c.secs)
            .sum::<f64>()
            / t.len() as f64;
        t.sort_by(f64::total_cmp);
        let (p50, p90) = (percentile(&t, 0.5), percentile(&t, 0.9));
        println!(
            "{bucket:<8} {:>5} {avg_s:>8.1} {p50:>8.0} {p90:>8.0} {:>8.0}",
            t.len(),
            t[t.len() - 1]
        );
        lat.push(format!("{p50:.0}/{p90:.0}"));
    }

    // WER by set, then by category, then overall.
    let mut wers = Vec::new();
    if clips.iter().any(|c| c.errors.is_some()) {
        let mut by_set: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
        let mut by_cat: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
        let mut all = (0, 0, 0);
        for c in &clips {
            if let Some((e, w)) = c.errors {
                for acc in [by_set.entry(c.set.clone()).or_default(), &mut all] {
                    *acc = (acc.0 + 1, acc.1 + e, acc.2 + w);
                }
                if let Some(cat) = categories.get(&c.name) {
                    let acc = by_cat.entry(cat.clone()).or_default();
                    *acc = (acc.0 + 1, acc.1 + e, acc.2 + w);
                }
            }
        }
        println!();
        println!("{:<12} {:>5} {:>6} {:>7}", "WER", "clips", "words", "WER");
        for (k, (n, e, w)) in by_set.iter().chain(by_cat.iter()) {
            println!("{k:<12} {n:>5} {w:>6} {:>6.2}%", pct(*e, *w));
        }
        println!(
            "{:<12} {:>5} {:>6} {:>6.2}%",
            "all",
            all.0,
            all.2,
            pct(all.1, all.2)
        );
        for k in ["read", "real"] {
            if let Some((_, e, w)) = by_cat.get(k) {
                wers.push(format!("{:.2}", pct(*e, *w)));
            }
        }
        wers.push(format!("{:.2}", pct(all.1, all.2)));
    }

    if !dictionary.replacements().is_empty() {
        println!();
        println!("replacements made: {replacements}");
    }
    if !term_counts.is_empty() && clips.iter().any(|c| c.errors.is_some()) {
        println!();
        println!(
            "{:<24} {:>6} {:>6} {:>6} {:>9}",
            "term (case-sensitive)", "in ref", "hits", "misses", "false ins"
        );
        let total = term_counts.total();
        for c in term_counts.counts().iter().chain([&total]) {
            println!(
                "{:<24} {:>6} {:>6} {:>6} {:>9}",
                c.term,
                c.expected(),
                c.hits,
                c.misses,
                c.false_insertions
            );
        }
    }

    println!();
    let rss = mem::peak_rss();
    let vram = mem::gpu_dedicated_bytes();
    println!(
        "memory: peak RSS {} MB (before load {} MB), GPU dedicated {}",
        mb(rss),
        mb(rss_before),
        vram.map_or("n/a".into(), |b| format!("{} MB", b / (1 << 20)))
    );
    // One markdown table row for docs/benchmarks.md:
    // | engine | load ms | warmup ms | short p50/p90 | medium | long | WER read | real | all | RSS MB | VRAM MB |
    println!(
        "row: | {} | {load_ms:.0} | {} | {} | {} | {} | {} |",
        engine.name(),
        warmup_ms.map_or("-".into(), |w| format!("{w:.0}")),
        lat.join(" | "),
        wers.join(" | "),
        mb(rss),
        vram.map_or("-".into(), |b| (b / (1 << 20)).to_string()),
    );
    Ok(())
}

/// Applies one of the `[stt]` flags (`--config`, `--engine`, `--model`, `--precision`,
/// `--device`, `--threads`, `--models-dir`); false if `a` isn't one.
fn stt_flag(
    cfg: &mut SttConfig,
    a: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<bool, String> {
    let mut val = || args.next().ok_or(format!("{a} needs a value"));
    match a {
        "--config" => {
            let p = val()?;
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
            *cfg = Config::parse(&text).map_err(|e| e.to_string())?.stt;
        }
        "--engine" => cfg.engine = enum_value(a, val()?)?,
        "--model" => cfg.model = val()?,
        "--precision" => cfg.precision = enum_value(a, val()?)?,
        "--device" => cfg.device = enum_value(a, val()?)?,
        "--threads" => cfg.threads = val()?.parse().map_err(|_| "bad --threads")?,
        "--models-dir" => cfg.models_dir = val()?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn mb(bytes: Option<u64>) -> String {
    bytes.map_or("?".into(), |b| (b / (1 << 20)).to_string())
}

fn pct(e: usize, w: usize) -> f64 {
    e as f64 * 100.0 / w.max(1) as f64
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn name(p: &Path) -> String {
    p.file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `ted-medium-03` (or `ted-dict-01`) -> `ted`; names without a bucket marker are their own set.
fn set_of(name: &str) -> String {
    ["-short-", "-medium-", "-long-", "-dict-"]
        .iter()
        .find_map(|m| name.find(m).map(|i| name[..i].to_owned()))
        .unwrap_or_else(|| "other".into())
}

/// name -> category from `manifest.tsv` (written by `scripts/prepare_eval.py`), if present.
fn read_categories(dir: &Path) -> BTreeMap<String, String> {
    let candidates = [
        Some(dir.join("manifest.tsv")),
        dir.parent().map(|p| p.join("manifest.tsv")),
    ];
    let Some(text) = candidates
        .into_iter()
        .flatten()
        .find_map(|p| std::fs::read_to_string(p).ok())
    else {
        return BTreeMap::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some((f.next()?.to_owned(), f.next()?.to_owned()))
        })
        .collect()
}

fn read_reference(wav: &Path) -> Option<String> {
    std::fs::read_to_string(wav.with_extension("txt")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_and_percentiles() {
        assert_eq!(set_of("ls-clean-short-01"), "ls-clean");
        assert_eq!(set_of("ami-long-01"), "ami");
        assert_eq!(set_of("hello"), "other");
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&v, 0.5), 5.0);
        assert_eq!(percentile(&v, 0.9), 9.0);
        assert_eq!(percentile(&[3.0], 0.9), 3.0);
    }
}
