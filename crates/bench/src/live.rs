//! `agt-bench live`: long dictations transcribed while they are being recorded (issue #6).
//!
//! ```text
//! agt-bench live [options] <dir-with-wavs>
//!   --config, --engine, --model, --precision, --device, --threads, --models-dir   as above
//!   --pause-ms <list>   silence that ends a segment; comma-separated values are compared in
//!                       one run; 0 = no pause cuts, only at the limit (default 1000)
//!   --min-secs <list>   shortest segment a pause may end (default 8; with the defaults above
//!                       this is `CutConfig::live`)
//!   --pad-ms <n>        silence kept at a cut and either side of a trimmed pause (default 250)
//!   --no-trim           transcribe the pauses instead of dropping them
//!   --max-secs <s>      segment limit (default 25, the app's chunk length; 10 for Core ML)
//!   --idle-ms <n>       longest real sleep for an idle gap on the timeline (default 4000;
//!                       0 = back to back, for WER sweeps)
//!   --baseline          also time today's path: the whole recording after release
//!   --realtime          replay in real time on threads instead (see below); slow
//!   --cuts-only         only run the cutter and compare its pause cuts with the pauses the
//!                       eval set inserted (`<name>.pauses`); no engine
//!   --only <text>       only files whose name contains this
//!   --texts <file>      append every transcript to this file (tab-separated)
//!   --quiet             no per-file lines
//! ```
//!
//! **Timeline** (default): each recording is replayed on a simulated clock. The audio goes
//! into an [`agt_stt::chunk::Cutter`] in 20 ms pieces, like audio callbacks; a segment is
//! ready when the cutter hands it out. One worker transcribes the segments in order, each when
//! it is ready and the previous one is done, then the tail at release. The transcriptions are
//! real and so is the idle time between them: it is slept (up to `--idle-ms`; an idle GPU pays
//! the same after 3 s as after 30 s), so the engine sees the gaps it would in the app. Release
//! -> text is when the last segment is done minus the end of the audio.
//!
//! **Real time** (`--realtime`): the audio is fed at its real pace on the main thread, a
//! worker thread transcribes, and a third thread runs a fixed slice of CPU work in a loop (a
//! stand-in for the app in the foreground). Reports how late the 20 ms feeds were (a stand-in
//! for capture) and how much slower the foreground work got while a segment was transcribed.

use crate::{mem, pct, percentile, wer};
use agt_core::{
    config::SttConfig,
    stt::{SAMPLE_RATE, SttEngine, TranscribeOpts},
};
use agt_stt::chunk::{Cut, CutConfig, Cutter, PauseCut, Segment};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const USAGE: &str = "usage: agt-bench live [stt flags] [--pause-ms list] [--min-secs list] [--pad-ms n] [--no-trim] [--max-secs s] [--idle-ms n] [--baseline] [--realtime] [--cuts-only] [--only text] [--texts file] [--quiet] <dir>";

/// Audio callback size.
const PIECE: usize = SAMPLE_RATE as usize / 50;

struct Setup {
    label: String,
    cut: CutConfig,
}

/// One dictation, one setup.
#[derive(Default)]
struct Run {
    text: String,
    latency_ms: f64,
    /// Wall time and process CPU time inside `transcribe`, number of calls.
    compute_ms: f64,
    cpu_ms: f64,
    calls: usize,
    /// Segments by [`Cut`]: pause, limit, end.
    cuts: [usize; 3],
    trimmed_secs: f64,
    tail_secs: f64,
    /// The worker was still busy at release; else how long it had been idle (simulated ms).
    busy_at_release: bool,
    idle_at_release_ms: f64,
    /// Real time only: feed lateness (ms) and foreground work-slice times while a segment
    /// was / wasn't being transcribed.
    late_ms: Vec<f64>,
    probe_busy: Vec<f64>,
    probe_idle: Vec<f64>,
}

struct Clip {
    name: String,
    secs: f64,
    reference: Option<String>,
    audio: Vec<f32>,
    /// The inserted pauses (start, end in s) from `<name>.pauses` (`prepare_long_eval.py`).
    pauses: Vec<(f64, f64)>,
}

fn secs(s: f64) -> usize {
    (s * SAMPLE_RATE as f64) as usize
}

fn list(a: &str, v: String) -> Result<Vec<f64>, String> {
    v.split(',')
        .map(|x| x.trim().parse().map_err(|_| format!("bad {a}")))
        .collect()
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut cfg = SttConfig::default();
    let (mut pauses, mut mins, mut pad_ms, mut trim) = (vec![1000.0], vec![8.0], 250.0, true);
    let (mut max_secs, mut idle_ms) = (f64::from(agt_stt::CHUNK_SECS), 4000u64);
    let (mut baseline, mut realtime, mut cuts_only, mut quiet) = (false, false, false, false);
    let (mut only, mut texts, mut dir) = (None, None, None);
    while let Some(a) = args.next() {
        if crate::stt_flag(&mut cfg, &a, &mut args)? {
            continue;
        }
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--pause-ms" => pauses = list(&a, val()?)?,
            "--min-secs" => mins = list(&a, val()?)?,
            "--pad-ms" => pad_ms = val()?.parse().map_err(|_| "bad --pad-ms")?,
            "--no-trim" => trim = false,
            "--max-secs" => max_secs = val()?.parse().map_err(|_| "bad --max-secs")?,
            "--idle-ms" => idle_ms = val()?.parse().map_err(|_| "bad --idle-ms")?,
            "--baseline" => baseline = true,
            "--realtime" => realtime = true,
            "--cuts-only" => cuts_only = true,
            "--only" => only = Some(val()?),
            "--texts" => texts = Some(PathBuf::from(val()?)),
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if dir.is_none() && !a.starts_with("--") => dir = Some(PathBuf::from(a)),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    let dir = dir.ok_or(format!("{USAGE}  (--help for options)"))?;
    let idle_cap = Duration::from_millis(idle_ms);

    let search = f64::from(agt_stt::CHUNK_SEARCH_SECS).min(max_secs / 2.0);
    let mut setups = Vec::new();
    for &p in &pauses {
        for &m in &mins {
            if p <= 0.0 && m != mins[0] {
                continue; // no pause cuts: the minimum doesn't matter
            }
            let pause = (p > 0.0).then(|| PauseCut {
                len: secs(p / 1000.0),
                min_segment: secs(m),
                pad: secs(pad_ms / 1000.0),
                trim,
            });
            let label = match &pause {
                None => "limit cuts only".to_owned(),
                Some(_) => format!(
                    "pause {p:.0} ms, min {m} s, pad {pad_ms:.0} ms, {}",
                    if trim { "trim" } else { "no trim" }
                ),
            };
            setups.push(Setup {
                label,
                cut: CutConfig {
                    max_len: secs(max_secs),
                    search_len: secs(search),
                    pause,
                },
            });
        }
    }

    let mut wavs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .filter(|p| only.as_ref().is_none_or(|o| crate::name(p).contains(o)))
        .collect();
    wavs.sort();
    if wavs.is_empty() {
        return Err(format!("no matching .wav files in {}", dir.display()));
    }
    let categories = crate::read_categories(&dir);
    let clips: Vec<Clip> = wavs
        .iter()
        .map(|w| {
            let audio = agt_audio::wav::read_16k_mono(w)?;
            Ok(Clip {
                name: crate::name(w),
                secs: audio.len() as f64 / SAMPLE_RATE as f64,
                reference: crate::read_reference(w),
                audio,
                pauses: std::fs::read_to_string(w.with_extension("pauses"))
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| {
                        let (a, b) = l.split_once('\t')?;
                        Some((a.parse().ok()?, b.parse().ok()?))
                    })
                    .collect(),
            })
        })
        .collect::<Result<_, String>>()?;
    if cuts_only {
        for s in &setups {
            cut_report(&s.label, &s.cut, &clips, quiet);
        }
        return Ok(());
    }

    let rss_before = mem::peak_rss();
    let t = Instant::now();
    let mut engine = agt_stt::load(&cfg, &mut |_| {}).map_err(|e| e.to_string())?;
    println!("engine: {}", engine.name());
    println!("load: {:.0} ms", crate::ms(t));
    engine.warmup().map_err(|e| e.to_string())?;
    let mut texts = match texts {
        Some(p) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .map_err(|e| format!("{}: {e}", p.display()))?,
        ),
        None => None,
    };
    let mut save = |label: &str, name: &str, text: &str| {
        if let Some(f) = &mut texts {
            let _ = writeln!(
                f,
                "{}\t{label}\t{name}\t{}",
                engine_label(&cfg),
                text.trim()
            );
        }
    };

    // Today's path: everything after release, through `Chunked`.
    let mut base = Vec::new();
    if baseline {
        for c in &clips {
            std::thread::sleep(idle_cap);
            let (t, cpu) = (Instant::now(), mem::cpu_time());
            let text = transcribe(&mut engine, &c.audio)?;
            let ms = crate::ms(t);
            save("after release", &c.name, &text);
            base.push(Run {
                latency_ms: ms,
                compute_ms: ms,
                cpu_ms: (mem::cpu_time() - cpu).as_secs_f64() * 1e3,
                text,
                ..Run::default()
            });
        }
        report(
            "after release (today)",
            &clips,
            &base,
            &categories,
            quiet,
            None,
        );
    }

    for s in &setups {
        let mut runs = Vec::new();
        for c in &clips {
            let r = if realtime {
                replay(&mut engine, &c.audio, &s.cut)?
            } else {
                simulate(&mut engine, &c.audio, &s.cut, idle_cap)?
            };
            save(&s.label, &c.name, &r.text);
            runs.push(r);
        }
        let base = (!base.is_empty()).then_some(&base[..]);
        report(&s.label, &clips, &runs, &categories, quiet, base);
    }

    println!();
    println!(
        "memory: peak RSS {} MB (before load {} MB), GPU dedicated {}",
        crate::mb(mem::peak_rss()),
        crate::mb(rss_before),
        mem::gpu_dedicated_bytes().map_or("n/a".into(), |b| format!("{} MB", b / (1 << 20)))
    );
    Ok(())
}

fn engine_label(cfg: &SttConfig) -> String {
    format!("{:?} {:?}", cfg.device, cfg.precision).to_lowercase()
}

fn transcribe(engine: &mut Box<dyn SttEngine>, audio: &[f32]) -> Result<String, String> {
    engine
        .transcribe(audio, &TranscribeOpts::default())
        .map(|t| t.text)
        .map_err(|e| e.to_string())
}

/// Appends a segment's text (trimmed, space-separated), like the app would join them.
fn join(text: &mut String, piece: &str) {
    let piece = piece.trim();
    if !piece.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(piece);
    }
}

fn count(r: &mut Run, seg: &Segment) {
    r.cuts[seg.cut as usize] += 1;
}

/// The timeline replay (see the module docs).
fn simulate(
    engine: &mut Box<dyn SttEngine>,
    audio: &[f32],
    cut: &CutConfig,
    idle_cap: Duration,
) -> Result<Run, String> {
    let mut r = Run::default();
    let mut cutter = Cutter::new(cut.clone());
    // Simulated seconds: when the worker is next free. It has been idle for long at the start.
    let mut free_at = f64::NEG_INFINITY;
    let mut kept = 0;
    let mut job = |r: &mut Run, free_at: &mut f64, seg: Segment, ready: f64| {
        count(r, &seg);
        kept += seg.audio.len();
        let start = ready.max(*free_at);
        std::thread::sleep(Duration::from_secs_f64(
            (start - *free_at).min(idle_cap.as_secs_f64()),
        ));
        let (t, cpu) = (Instant::now(), mem::cpu_time());
        let text = transcribe(engine, &seg.audio)?;
        let took = t.elapsed().as_secs_f64();
        r.cpu_ms += (mem::cpu_time() - cpu).as_secs_f64() * 1e3;
        r.compute_ms += took * 1e3;
        r.calls += 1;
        *free_at = start + took;
        join(&mut r.text, &text);
        Ok::<_, String>(())
    };
    for piece in audio.chunks(PIECE) {
        for seg in cutter.push(piece) {
            let ready = seg.at as f64 / SAMPLE_RATE as f64;
            job(&mut r, &mut free_at, seg, ready)?;
        }
    }
    let release = audio.len() as f64 / SAMPLE_RATE as f64;
    r.busy_at_release = free_at > release;
    r.idle_at_release_ms = ((release - free_at.max(0.0)) * 1e3).max(0.0);
    for seg in cutter.finish() {
        r.tail_secs += seg.audio.len() as f64 / SAMPLE_RATE as f64;
        job(&mut r, &mut free_at, seg, release)?;
    }
    r.trimmed_secs = (audio.len() - kept) as f64 / SAMPLE_RATE as f64;
    r.latency_ms = (free_at.max(release) - release) * 1e3;
    Ok(r)
}

/// The real-time replay (see the module docs).
fn replay(engine: &mut Box<dyn SttEngine>, audio: &[f32], cut: &CutConfig) -> Result<Run, String> {
    let busy = AtomicBool::new(false);
    let done_at = Mutex::new(None::<Instant>);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Segment>();
    let mut r = Run::default();
    std::thread::scope(|sc| {
        let worker = sc.spawn(|| -> Result<(String, f64, f64, usize, Instant), String> {
            let (mut text, mut compute, mut cpu_ms, mut calls) = (String::new(), 0.0, 0.0, 0);
            for seg in rx {
                busy.store(true, Ordering::SeqCst);
                let (t, cpu) = (Instant::now(), mem::cpu_time());
                let piece = transcribe(engine, &seg.audio);
                *done_at.lock().unwrap() = Some(Instant::now());
                busy.store(false, Ordering::SeqCst);
                compute += crate::ms(t);
                cpu_ms += (mem::cpu_time() - cpu).as_secs_f64() * 1e3;
                calls += 1;
                join(&mut text, &piece?);
            }
            Ok((text, compute, cpu_ms, calls, Instant::now()))
        });
        let probe = sc.spawn(|| {
            let (mut on, mut off) = (Vec::new(), Vec::new());
            while !stop.load(Ordering::Relaxed) {
                let was = busy.load(Ordering::Relaxed);
                let t = Instant::now();
                work_slice();
                let ms = crate::ms(t);
                if was && busy.load(Ordering::Relaxed) {
                    on.push(ms);
                } else if !was && !busy.load(Ordering::Relaxed) {
                    off.push(ms);
                }
            }
            (on, off)
        });

        let mut cutter = Cutter::new(cut.clone());
        let mut kept = 0;
        let t0 = Instant::now();
        for (k, piece) in audio.chunks(PIECE).enumerate() {
            // A callback delivers a piece once it has been recorded.
            let due = t0 + Duration::from_secs_f64((k + 1) as f64 * PIECE as f64 / 16_000.0);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            r.late_ms.push(crate::ms(due));
            for seg in cutter.push(piece) {
                count(&mut r, &seg);
                kept += seg.audio.len();
                let _ = tx.send(seg);
            }
        }
        let release = Instant::now();
        r.busy_at_release = busy.load(Ordering::SeqCst);
        r.idle_at_release_ms = done_at.lock().unwrap().map_or(crate::ms(t0), crate::ms);
        for seg in cutter.finish() {
            count(&mut r, &seg);
            kept += seg.audio.len();
            r.tail_secs += seg.audio.len() as f64 / SAMPLE_RATE as f64;
            let _ = tx.send(seg);
        }
        drop(tx);
        r.trimmed_secs = (audio.len() - kept) as f64 / SAMPLE_RATE as f64;
        let result = worker.join().expect("worker panicked");
        stop.store(true, Ordering::Relaxed);
        (r.probe_busy, r.probe_idle) = probe.join().expect("probe panicked");
        let (text, compute, cpu_ms, calls, done) = result?;
        (r.text, r.compute_ms, r.cpu_ms, r.calls) = (text, compute, cpu_ms, calls);
        r.latency_ms = done.duration_since(release).as_secs_f64() * 1e3;
        Ok(r)
    })
}

/// About a millisecond of single-threaded integer work on a desktop CPU.
fn work_slice() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..200_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    std::hint::black_box(x);
}

fn report(
    label: &str,
    clips: &[Clip],
    runs: &[Run],
    categories: &BTreeMap<String, String>,
    quiet: bool,
    base: Option<&[Run]>,
) {
    println!();
    println!("== {label}");
    let errors: Vec<Option<(usize, usize)>> = clips
        .iter()
        .zip(runs)
        .map(|(c, r)| c.reference.as_ref().map(|t| wer::word_errors(t, &r.text)))
        .collect();
    if !quiet {
        println!(
            "{:<18} {:>6} {:>5} {:>5} {:>5} {:>7} {:>6} {:>8} {:>8} {:>6}",
            "file",
            "audio s",
            "pause",
            "limit",
            "end",
            "trim s",
            "tail s",
            "compute",
            "rel->txt",
            "WER"
        );
        for ((c, r), e) in clips.iter().zip(runs).zip(&errors) {
            println!(
                "{:<18} {:>7.1} {:>5} {:>5} {:>5} {:>7.1} {:>6.1} {:>8.0} {:>8.0} {:>6}",
                c.name,
                c.secs,
                r.cuts[0],
                r.cuts[1],
                r.cuts[2],
                r.trimmed_secs,
                r.tail_secs,
                r.compute_ms,
                r.latency_ms,
                e.map_or("-".into(), |(e, w)| format!("{:5.2}%", pct(e, w))),
            );
        }
    }
    let sorted = |f: &dyn Fn(&Run) -> f64| {
        let mut v: Vec<f64> = runs.iter().map(f).collect();
        v.sort_by(f64::total_cmp);
        v
    };
    let lat = sorted(&|r| r.latency_ms);
    let (p50, p90) = (percentile(&lat, 0.5), percentile(&lat, 0.9));
    println!(
        "release -> text: p50 {p50:.0} ms, p90 {p90:.0} ms, max {:.0} ms",
        lat[lat.len() - 1]
    );
    let mut wer_cells = Vec::new();
    if errors.iter().any(Option::is_some) {
        let mut by: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (c, e) in clips.iter().zip(&errors) {
            if let Some((e, w)) = *e {
                let cat = categories.get(&c.name).cloned().unwrap_or("other".into());
                // AMI's references leave out the cross-talk its headset mics pick up, which
                // swings its WER by tens of points: also show the total without it.
                let set = crate::set_of(&c.name);
                let not_ami = (set != "ami").then(|| "all-but-ami".to_owned());
                for k in [Some(cat), Some(set), not_ami, Some("all".into())]
                    .into_iter()
                    .flatten()
                {
                    let acc = by.entry(k).or_default();
                    *acc = (acc.0 + e, acc.1 + w);
                }
            }
        }
        let line: Vec<String> = by
            .iter()
            .map(|(k, (e, w))| format!("{k} {:.2}%", pct(*e, *w)))
            .collect();
        println!("WER: {} ({} words)", line.join(", "), by["all"].1);
        for k in ["read", "real", "all-but-ami", "all"] {
            if let Some((e, w)) = by.get(k) {
                wer_cells.push(format!("{:.2}", pct(*e, *w)));
            }
        }
    }
    let audio: f64 = clips.iter().map(|c| c.secs).sum();
    let compute: f64 = runs.iter().map(|r| r.compute_ms).sum();
    let cpu: f64 = runs.iter().map(|r| r.cpu_ms).sum();
    let calls: usize = runs.iter().map(|r| r.calls).sum();
    let trimmed: f64 = runs.iter().map(|r| r.trimmed_secs).sum();
    println!(
        "compute: {compute:.0} ms for {audio:.0} s (RTF {:.4}) in {calls} calls; {:.1} cores busy while transcribing; {:.0}% of the audio trimmed",
        compute / 1000.0 / audio,
        cpu / compute.max(1.0),
        trimmed * 100.0 / audio
    );
    let mut row_tail = String::new();
    if runs
        .iter()
        .any(|r| r.calls > 0 && r.cuts.iter().sum::<usize>() > 0)
    {
        let cuts = runs.iter().fold([0; 3], |a, r| {
            [a[0] + r.cuts[0], a[1] + r.cuts[1], a[2] + r.cuts[2]]
        });
        let tails = sorted(&|r| r.tail_secs);
        let busy = runs.iter().filter(|r| r.busy_at_release).count();
        let idle = sorted(&|r| r.idle_at_release_ms);
        println!(
            "segments: {} at pauses, {} at the limit, {} tails; tail p50 {:.1} s, p90 {:.1} s; worker busy at release {busy} of {}, else idle p50 {:.1} s",
            cuts[0],
            cuts[1],
            cuts[2],
            percentile(&tails, 0.5),
            percentile(&tails, 0.9),
            runs.len(),
            percentile(&idle, 0.5) / 1000.0,
        );
        row_tail = format!(
            " {}/{}/{} | {:.1}/{:.1} |",
            cuts[0],
            cuts[1],
            cuts[2],
            percentile(&tails, 0.5),
            percentile(&tails, 0.9)
        );
    }
    if runs.iter().any(|r| !r.late_ms.is_empty()) {
        let late = sorted_all(runs.iter().flat_map(|r| r.late_ms.iter().copied()));
        let on = sorted_all(runs.iter().flat_map(|r| r.probe_busy.iter().copied()));
        let off = sorted_all(runs.iter().flat_map(|r| r.probe_idle.iter().copied()));
        println!(
            "real time: 20 ms feeds late by p50 {:.2} / p99 {:.2} / max {:.1} ms; foreground work slice {:.2} ms idle vs {:.2} ms while transcribing (p50; p90 {:.2} vs {:.2})",
            percentile(&late, 0.5),
            percentile(&late, 0.99),
            late.last().copied().unwrap_or(0.0),
            percentile(&off, 0.5),
            percentile(&on, 0.5),
            percentile(&off, 0.9),
            percentile(&on, 0.9),
        );
    }
    if let Some(base) = base {
        let b = {
            let mut v: Vec<f64> = base.iter().map(|r| r.latency_ms).collect();
            v.sort_by(f64::total_cmp);
            v
        };
        println!(
            "after release (today): p50 {:.0} ms, p90 {:.0} ms",
            percentile(&b, 0.5),
            percentile(&b, 0.9)
        );
    }
    // | setup | release->text p50/p90 | WER read | real | all | compute ms | pause/limit/end | tail p50/p90 s |
    println!(
        "row: | {label} | {p50:.0}/{p90:.0} | {} | {compute:.0} |{row_tail}",
        wer_cells.join(" | ")
    );
}

/// `--cuts-only`: where the cutter cuts, without transcribing.
fn cut_report(label: &str, cut: &CutConfig, clips: &[Clip], quiet: bool) {
    println!();
    println!("== {label}");
    let len = cut
        .pause
        .as_ref()
        .map_or(f64::MAX, |p| p.len as f64 / 16_000.0);
    let (mut found, mut long, mut cuts, mut inside) = (0, 0, [0; 3], 0);
    let (mut kept, mut seg_lens, mut tails) = (0, Vec::new(), Vec::new());
    for c in clips {
        let mut cutter = Cutter::new(cut.clone());
        let mut segs = Vec::new();
        for piece in c.audio.chunks(PIECE) {
            segs.extend(cutter.push(piece));
        }
        let tail = cutter.finish();
        tails.push(tail.iter().map(|s| s.audio.len()).sum::<usize>() as f64 / 16_000.0);
        segs.extend(tail);
        // A pause cut is made `len` into the silence, which may start a little before the
        // inserted pause (the utterance's own trailing silence).
        let at: Vec<f64> = segs
            .iter()
            .filter(|s| s.cut == Cut::Pause)
            .map(|s| s.at as f64 / 16_000.0)
            .collect();
        let hit = |&(s, e): &(f64, f64)| at.iter().any(|&a| a >= s && a <= e + 0.1);
        let big: Vec<_> = c
            .pauses
            .iter()
            .filter(|(s, e)| e - s >= len + 0.1)
            .collect();
        let f = big.iter().filter(|p| hit(p)).count();
        let ins = at
            .iter()
            .filter(|&&a| c.pauses.iter().any(|&(s, e)| a >= s && a <= e + 0.1))
            .count();
        let mut n = [0; 3];
        for s in &segs {
            n[s.cut as usize] += 1;
            kept += s.audio.len();
            seg_lens.push(s.audio.len() as f64 / 16_000.0);
        }
        if !quiet {
            println!(
                "{:<18} {:>6.1} s: {} pause / {} limit / {} end cuts; inserted pauses >= {:.1} s cut: {f}/{}; pause cuts elsewhere: {}",
                c.name,
                c.secs,
                n[0],
                n[1],
                n[2],
                len + 0.1,
                big.len(),
                at.len() - ins
            );
        }
        (found, long, inside) = (found + f, long + big.len(), inside + at.len() - ins);
        cuts = [cuts[0] + n[0], cuts[1] + n[1], cuts[2] + n[2]];
    }
    let audio: usize = clips.iter().map(|c| c.audio.len()).sum();
    seg_lens.sort_by(f64::total_cmp);
    tails.sort_by(f64::total_cmp);
    println!(
        "cuts: {} pause, {} limit, {} end; inserted pauses found {found}/{long}, pause cuts elsewhere {inside}; segment p50 {:.1} s, p90 {:.1} s; tail p50 {:.1} s, p90 {:.1} s, max {:.1} s; {:.0}% trimmed",
        cuts[0],
        cuts[1],
        cuts[2],
        percentile(&seg_lens, 0.5),
        percentile(&seg_lens, 0.9),
        percentile(&tails, 0.5),
        percentile(&tails, 0.9),
        tails.last().copied().unwrap_or(0.0),
        (audio - kept) as f64 * 100.0 / audio as f64
    );
}

fn sorted_all(it: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = it.collect();
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        v.push(0.0);
    }
    v
}
