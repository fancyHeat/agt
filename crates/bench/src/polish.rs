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
//! ```
//!
//! Always polishes (`polish.enabled` is ignored) and warms up first; for s1-mini the warmup
//! includes the download and server start. Output per line: median latency, the guard
//! outcome (polished / skipped / raw: reason) and the text that would be pasted. Lines
//! starting with `#` are comments.

use agt_core::{Config, config::PolishConfig, paths};
use std::{path::PathBuf, sync::Arc, time::Instant};

const USAGE: &str = "usage: agt-bench polish [--config f] [--provider s1-mini|openai|anthropic] [--base-url u] [--model m] [--style light|formal] [--styling s] [--context c] [--s1-device auto|cpu] [--models-dir d] [--server-log f] [--timeout-ms n] [--runs n] [--hold secs] [--pause-ms n] <transcripts.txt>";

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut cfg = PolishConfig::default();
    let mut models_dir = paths::models_dir();
    let mut server_log = std::env::temp_dir().join("agt-bench-llama-server.log");
    let (mut runs, mut hold, mut pause, mut file) = (3usize, 0u64, 0u64, None);
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
