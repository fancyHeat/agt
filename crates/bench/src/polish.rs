//! `agt-bench polish`: run the LLM clean-up over a file of transcripts, one per line.
//!
//! ```text
//! agt-bench polish [options] <transcripts.txt>
//!   --config <file>      start from this config.toml's [polish] section (default: built-in)
//!   --provider <p>       s1-mini | openai | anthropic
//!   --base-url <url>     override polish.base_url
//!   --model <name>       override polish.model
//!   --style <s>          light | formal
//!   --styling <s>        s1-mini: casual | semi-casual | semi-formal | formal
//!   --context <c>        s1-mini: general | email
//!   --s1-device <d>      s1-mini: auto | cpu
//!   --models-dir <dir>   s1-mini: where it's downloaded (default: AGT's models folder)
//!   --server-log <file>  s1-mini: llama-server's output (default: in the temp folder)
//!   --timeout-ms <n>     override polish.timeout_ms
//!   --runs <n>           polish each line n times (default 3); latency = median
//!   --hold <secs>        keep running (and the s1-mini server up) this long afterwards
//!   --pause-ms <n>       wait this long before each line (e.g. to let the server go idle)
//!   --segments           the file is `agt-bench live --segments` output: compare polishing
//!                        each dictation whole with polishing its segments one by one
//!   --refs <dir>         with --segments: `<name>.txt` references there, for WER
//! ```
//!
//! Always polishes (`polish.enabled` is ignored) and warms up first; for s1-mini the warmup
//! includes the download and server start. Output per line: median latency, the guard
//! outcome (polished / skipped / raw: reason) and the text that would be pasted. Lines
//! starting with `#` are comments.
//!
//! **Segments** (`--segments`): the pipeline could polish each finished segment of a long
//! dictation in the background while the user is still talking, leaving only the tail for
//! release. For each dictation this polishes the whole text (today), each segment on its own,
//! and the pieces of `agt_polish::ahead`, then reports release latency (whole vs what is left
//! at release), how far the joined output is from the whole-text output (word edits, case and
//! punctuation count), the WER of raw, whole and joined against the reference, and the text
//! around every piece boundary where a sentence break differs.
//!
//! Release latency here is the best case: every background piece is done (and the GPU idle)
//! before release. In the app the last background piece may still be running at release,
//! and STT may share the GPU, so release can also wait for that piece.

use agt_core::{Config, config::PolishConfig, paths, polish::Polisher};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

const USAGE: &str = "usage: agt-bench polish [--config f] [--provider s1-mini|openai|anthropic] [--base-url u] [--model m] [--style light|formal] [--styling s] [--context c] [--s1-device auto|cpu] [--models-dir d] [--server-log f] [--timeout-ms n] [--runs n] [--hold secs] [--pause-ms n] [--segments [--refs dir]] <transcripts.txt>";

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut cfg = PolishConfig::default();
    let mut models_dir = paths::models_dir();
    let mut server_log = std::env::temp_dir().join("agt-bench-llama-server.log");
    let (mut runs, mut hold, mut pause, mut file) = (3usize, 0u64, 0u64, None);
    let (mut segments, mut refs) = (false, None);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--config" => {
                let p = val()?;
                let text = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
                let c = Config::parse(&text).map_err(|e| e.to_string())?;
                models_dir = c.stt.models_path();
                cfg = c.polish;
            }
            "--provider" => cfg.provider = crate::enum_value(&a, val()?)?,
            "--base-url" => cfg.base_url = val()?,
            "--model" => cfg.model = val()?,
            "--style" => cfg.style = crate::enum_value(&a, val()?)?,
            "--styling" => cfg.s1_mini.styling = crate::enum_value(&a, val()?)?,
            "--context" => cfg.s1_mini.context = crate::enum_value(&a, val()?)?,
            "--s1-device" => cfg.s1_mini.device = crate::enum_value(&a, val()?)?,
            "--models-dir" => models_dir = PathBuf::from(val()?),
            "--server-log" => server_log = PathBuf::from(val()?),
            "--timeout-ms" => cfg.timeout_ms = val()?.parse().map_err(|_| "bad --timeout-ms")?,
            "--runs" => runs = val()?.parse::<usize>().map_err(|_| "bad --runs")?.max(1),
            "--hold" => hold = val()?.parse().map_err(|_| "bad --hold")?,
            "--pause-ms" => pause = val()?.parse().map_err(|_| "bad --pause-ms")?,
            "--segments" => segments = true,
            "--refs" => refs = Some(PathBuf::from(val()?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if file.is_none() && !a.starts_with("--") => file = Some(a),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    let file = file.ok_or(USAGE)?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let t0 = Instant::now();
    let env = agt_polish::Env {
        models_dir,
        log_file: Some(server_log),
        on_status: Arc::new(move |s: &agt_polish::Status| {
            eprintln!("[{:6.1} s] {s}", t0.elapsed().as_secs_f64());
            if let agt_polish::Status::Failed(why) = s {
                eprintln!("           {why}");
            }
        }),
    };
    let polisher = agt_polish::load(&cfg, &env)?;
    println!("polisher: {}", polisher.name());
    let t = Instant::now();
    match polisher.warmup() {
        Ok(()) => println!("warmup: {:.0} ms", ms(t)),
        Err(e) => println!("warmup failed ({:.0} ms): {e}", ms(t)),
    }
    if segments {
        return by_segment(polisher.as_ref(), &cfg, &text, refs.as_deref(), pause);
    }

    // Latency of every request that was actually sent (skipped lines excluded).
    let mut all = Vec::new();
    for line in &lines {
        let mut times = Vec::with_capacity(runs);
        let mut results = Vec::with_capacity(runs);
        std::thread::sleep(std::time::Duration::from_millis(pause));
        for _ in 0..runs {
            let t = Instant::now();
            let (out, outcome) = agt_polish::polish_or_raw(polisher.as_ref(), line, &cfg);
            let took = ms(t);
            if !matches!(outcome, agt_polish::Outcome::Skipped(_)) {
                all.push(took);
            }
            times.push(took);
            results.push((out, outcome));
        }
        times.sort_by(f64::total_cmp);
        println!();
        println!(
            "[median {:.0} ms, {} words]",
            times[times.len() / 2],
            line.split_whitespace().count()
        );
        println!("  in:  {line}");
        // Distinct results only (a model at temperature 0 usually repeats itself).
        results.dedup();
        for (out, outcome) in &results {
            println!("  out: ({outcome}) {out}");
        }
    }
    if !all.is_empty() {
        all.sort_by(f64::total_cmp);
        let pct = |p: f64| all[((all.len() - 1) as f64 * p).round() as usize];
        println!();
        println!(
            "requests: {} | p50 {:.0} ms | p90 {:.0} ms | max {:.0} ms",
            all.len(),
            pct(0.5),
            pct(0.9),
            all[all.len() - 1]
        );
    }
    if hold > 0 {
        eprintln!("holding for {hold} s");
        std::thread::sleep(std::time::Duration::from_secs(hold));
    }
    Ok(())
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// `--segments`: whole-text polish vs polishing ahead of release (see the module docs).
fn by_segment(
    polisher: &dyn Polisher,
    cfg: &PolishConfig,
    text: &str,
    refs: Option<&Path>,
    pause: u64,
) -> Result<(), String> {
    let polish = |s: &str| {
        std::thread::sleep(std::time::Duration::from_millis(pause));
        let t = Instant::now();
        let (out, outcome) = agt_polish::polish_or_raw(polisher, s, cfg);
        (out, outcome == agt_polish::Outcome::Polished, ms(t))
    };
    let mut whole_ms = Vec::new();
    let mut ways = [Way::new("each segment"), Way::new("ahead")];
    // Reference words, and word errors of raw and whole.
    let (mut ref_words, mut raw_errors, mut whole_errors) = (0, 0, 0);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut cols = line.split('\t');
        let name = cols.next().unwrap_or_default();
        let segs: Vec<&str> = cols.map(str::trim).filter(|s| !s.is_empty()).collect();
        let Some((&tail, before)) = segs.split_last() else {
            continue;
        };
        let raw = segs.join(" ");
        let (whole, ok, took) = polish(&raw);
        whole_ms.push(took);
        println!();
        println!(
            "[{name}] {} segments, {} words, tail {} words | whole: {took:.0} ms{}",
            segs.len(),
            raw.split_whitespace().count(),
            tail.split_whitespace().count(),
            if ok { "" } else { " (not polished)" }
        );
        let reference =
            refs.and_then(|d| std::fs::read_to_string(d.join(format!("{name}.txt"))).ok());
        if let Some(r) = &reference {
            let (e, n) = crate::wer::word_errors(r, &raw);
            (raw_errors, ref_words) = (raw_errors + e, ref_words + n);
            whole_errors += crate::wer::word_errors(r, &whole).0;
        }
        let w: Vec<String> = whole.split_whitespace().map(str::to_owned).collect();
        // Each segment on its own; the tail at release.
        let pieces: Vec<String> = before.iter().map(|s| s.to_string()).collect();
        ways[0].run(
            &polish,
            &pieces,
            &[tail.to_owned()],
            &w,
            reference.as_deref(),
        );
        // `agt_polish::ahead`.
        let mut ahead = agt_polish::ahead::Ahead::new(cfg)
            .ok_or("--segments: these polish settings need the whole text")?;
        let pieces: Vec<String> = before.iter().filter_map(|s| ahead.push(s)).collect();
        let last = ahead.finish(tail);
        ways[1].run(&polish, &pieces, &last, &w, reference.as_deref());
    }
    whole_ms.sort_by(f64::total_cmp);
    println!();
    println!("{}", stats("whole text (today)", &whole_ms));
    for way in &mut ways {
        way.report(ref_words);
    }
    if ref_words > 0 {
        println!(
            "WER vs reference ({ref_words} words): raw {:.2}%, whole {:.2}%",
            crate::pct(raw_errors, ref_words),
            crate::pct(whole_errors, ref_words)
        );
    }
    Ok(())
}

/// `n requests | p50 | p90 | max` of sorted latencies.
fn stats(label: &str, v: &[f64]) -> String {
    match v.last() {
        None => format!("{label}: none"),
        Some(max) => format!(
            "{label}: {} requests | p50 {:.0} ms | p90 {:.0} ms | max {max:.0} ms",
            v.len(),
            crate::percentile(v, 0.5),
            crate::percentile(v, 0.9),
        ),
    }
}

/// One way of polishing a dictation in pieces: some in the background, the rest at release.
struct Way {
    label: &'static str,
    release_ms: Vec<f64>,
    background_ms: Vec<f64>,
    release_words: Vec<f64>,
    /// Word edits vs the whole-text output, its words.
    edits: usize,
    words: usize,
    /// Piece boundaries, and those where only this way / only the whole text ends a sentence.
    boundaries: usize,
    extra_breaks: usize,
    lost_breaks: usize,
    not_polished: usize,
    errors: usize,
}

impl Way {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            release_ms: Vec::new(),
            background_ms: Vec::new(),
            release_words: Vec::new(),
            edits: 0,
            words: 0,
            boundaries: 0,
            extra_breaks: 0,
            lost_breaks: 0,
            not_polished: 0,
            errors: 0,
        }
    }

    fn run(
        &mut self,
        polish: &dyn Fn(&str) -> (String, bool, f64),
        background: &[String],
        at_release: &[String],
        whole: &[String],
        reference: Option<&str>,
    ) {
        let mut joined: Vec<String> = Vec::new();
        let mut starts = Vec::new();
        let (mut release_ms, mut release_words) = (0.0, 0);
        for (k, piece) in background.iter().chain(at_release).enumerate() {
            let (out, ok, took) = polish(piece);
            self.not_polished += usize::from(!ok);
            if k < background.len() {
                self.background_ms.push(took);
            } else {
                // One server slot: the pieces left at release go one after the other.
                release_ms += took;
                release_words += piece.split_whitespace().count();
            }
            starts.push(joined.len());
            joined.extend(out.split_whitespace().map(str::to_owned));
        }
        self.release_ms.push(release_ms);
        self.release_words.push(release_words as f64);
        let pieces = background.len() + at_release.len();
        let map = align(&joined, whole);
        self.edits += map.edits;
        self.words += whole.len();
        if let Some(r) = reference {
            self.errors += crate::wer::word_errors(r, &joined.join(" ")).0;
        }
        println!(
            "  {}: {} pieces, release {:.0} ms ({} words), {} word edits vs whole",
            self.label, pieces, release_ms, release_words, map.edits
        );
        for &b in starts.iter().skip(1) {
            self.boundaries += 1;
            let at = map.to_b[b];
            let ends = |t: &[String], i: usize| i > 0 && ends_sentence(&t[i - 1]);
            let mark = match (ends(&joined, b), ends(whole, at)) {
                (true, false) => {
                    self.extra_breaks += 1;
                    "extra sentence break"
                }
                (false, true) => {
                    self.lost_breaks += 1;
                    "lost sentence break"
                }
                _ => continue,
            };
            let around = |t: &[String], i: usize| {
                let (lo, hi) = (i.saturating_sub(6), (i + 6).min(t.len()));
                format!("{} | {}", t[lo..i].join(" "), t[i..hi].join(" "))
            };
            println!("    {mark}: ...{}...", around(&joined, b));
            println!("      whole text: ...{}...", around(whole, at));
        }
    }

    fn report(&mut self, ref_words: usize) {
        self.release_ms.sort_by(f64::total_cmp);
        self.background_ms.sort_by(f64::total_cmp);
        self.release_words.sort_by(f64::total_cmp);
        println!("{}:", self.label);
        println!("  {}", stats("at release", &self.release_ms));
        println!("  {}", stats("in the background", &self.background_ms));
        if !self.release_words.is_empty() {
            println!(
                "  words left at release: p50 {:.0}, p90 {:.0}",
                crate::percentile(&self.release_words, 0.5),
                crate::percentile(&self.release_words, 0.9)
            );
        }
        println!(
            "  vs whole text: {} word edits in {} words ({:.2}%); {} boundaries: {} extra sentence breaks, {} lost; {} requests not polished (guard){}",
            self.edits,
            self.words,
            crate::pct(self.edits, self.words),
            self.boundaries,
            self.extra_breaks,
            self.lost_breaks,
            self.not_polished,
            if ref_words > 0 {
                format!("; WER {:.2}%", crate::pct(self.errors, ref_words))
            } else {
                String::new()
            }
        );
    }
}

/// A word that ends a sentence (`end.`, `why?"`, `no!)`).
fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')', '\u{201d}', '\u{2019}'])
        .ends_with(['.', '?', '!'])
}

struct Alignment {
    /// Word edits (substitutions, insertions, deletions; exact match, so case and
    /// punctuation count).
    edits: usize,
    /// For each position in `a` (and one past its end): the matching position in `b`.
    to_b: Vec<usize>,
}

/// Word-level Levenshtein alignment of `a` onto `b`.
fn align(a: &[String], b: &[String]) -> Alignment {
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            d[i][j] = (d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]))
                .min(d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1);
        }
    }
    let mut to_b = vec![0; n + 1];
    to_b[n] = m;
    let (mut i, mut j) = (n, m);
    while i > 0 {
        if j > 0 && d[i][j] == d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]) {
            (i, j) = (i - 1, j - 1);
        } else if j > 0 && d[i][j] == d[i][j - 1] + 1 {
            j -= 1;
            continue;
        } else {
            i -= 1;
        }
        to_b[i] = j;
    }
    Alignment {
        edits: d[n][m],
        to_b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn alignment_maps_positions_and_counts_edits() {
        let a = words("We met. Then we left early.");
        let b = words("We met, then we left.");
        let m = align(&a, &b);
        // met./met,  Then/then  left/left.  early. deleted
        assert_eq!(m.edits, 4);
        assert_eq!(m.to_b[2], 2); // "Then" <-> "then"
        assert_eq!(m.to_b[a.len()], b.len());
        assert_eq!(align(&a, &a).edits, 0);
        assert!(ends_sentence("left.\"") && ends_sentence("why?") && !ends_sentence("met,"));
    }
}
