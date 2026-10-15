//! `agt-bench polish`: run the LLM clean-up over a file of transcripts, one per line.
//!
//! ```text
//! agt-bench polish [options] <transcripts.txt>
//!   --config <file>      start from this config.toml's [polish] section (default: built-in)
//!   --provider <p>       openai | anthropic
//!   --base-url <url>     override polish.base_url
//!   --model <name>       override polish.model
//!   --style <s>          light | formal
//!   --timeout-ms <n>     override polish.timeout_ms
//!   --runs <n>           polish each line n times (default 3); latency = median
//! ```
//!
//! Always polishes (`polish.enabled` is ignored) and warms up first. Output per line: median
//! latency, the guard outcome (polished / skipped / raw: reason) and the text that would be
//! pasted. Lines starting with `#` are comments.

use agt_core::{
    Config,
    config::{PolishConfig, PolishProvider, PolishStyle},
};
use std::time::Instant;

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut cfg = PolishConfig::default();
    let (mut runs, mut file) = (3usize, None);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--config" => {
                let p = val()?;
                let text = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
                cfg = Config::parse(&text).map_err(|e| e.to_string())?.polish;
            }
            "--provider" => {
                cfg.provider = match val()?.as_str() {
                    "openai" => PolishProvider::OpenAi,
                    "anthropic" => PolishProvider::Anthropic,
                    p => return Err(format!("unknown provider {p}")),
                }
            }
            "--base-url" => cfg.base_url = val()?,
            "--model" => cfg.model = val()?,
            "--style" => {
                cfg.style = match val()?.as_str() {
                    "light" => PolishStyle::Light,
                    "formal" => PolishStyle::Formal,
                    s => return Err(format!("unknown style {s}")),
                }
            }
            "--timeout-ms" => cfg.timeout_ms = val()?.parse().map_err(|_| "bad --timeout-ms")?,
            "--runs" => runs = val()?.parse::<usize>().map_err(|_| "bad --runs")?.max(1),
            "-h" | "--help" => {
                println!(
                    "usage: agt-bench polish [--config f] [--provider openai|anthropic] [--base-url u] [--model m] [--style light|formal] [--timeout-ms n] [--runs n] <transcripts.txt>"
                );
                return Ok(());
            }
            _ if file.is_none() && !a.starts_with("--") => file = Some(a),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    let file = file.ok_or("usage: agt-bench polish [options] <transcripts.txt>")?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let polisher = agt_polish::load(&cfg)?;
    println!("polisher: {}", polisher.name());
    let t = Instant::now();
    match polisher.warmup() {
        Ok(()) => println!("warmup: {:.0} ms", ms(t)),
        Err(e) => println!("warmup failed ({:.0} ms): {e}", ms(t)),
    }

    // Latency of every request that was actually sent (skipped lines excluded).
    let mut all = Vec::new();
    for line in &lines {
        let mut times = Vec::with_capacity(runs);
        let mut results = Vec::with_capacity(runs);
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
    Ok(())
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}
