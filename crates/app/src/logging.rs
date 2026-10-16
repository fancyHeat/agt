//! Events (what used to be the log file) go to the `events` table of the telemetry database
//! (see `agt-telemetry`), filtered by `log_level`. Debug builds also print them to stderr.
//!
//! Events must never contain dictated text, clipboard contents or secrets: they are kept
//! for 30 days.

use std::{fmt::Write, time::Duration};
use tracing::{Event, Subscriber, field::Field, level_filters::LevelFilter};
use tracing_log::NormalizeEvent;
use tracing_subscriber::{Layer, filter::Targets, fmt, layer::Context, prelude::*};

fn filter(level: LevelFilter) -> Targets {
    Targets::new()
        .with_default(level)
        // Velopack logs ~10 lines per update check at INFO; keep only its warnings.
        .with_target("velopack", LevelFilter::WARN.min(level))
        // ONNX Runtime logs device discovery and graph details at INFO.
        .with_target("ort", LevelFilter::WARN.min(level))
        // SECURITY: at TRACE, ureq hex-dumps raw request bytes, unredacted, including the
        // `x-api-key` / `authorization` headers of LLM polish requests (and the transcript).
        // Never let that into the database, whatever `log_level` says (its DEBUG output
        // redacts headers).
        .with_target("ureq", LevelFilter::DEBUG.min(level))
        .with_target("ureq_proto", LevelFilter::DEBUG.min(level))
}

/// Forwards each event, formatted, to the telemetry writer thread (a channel send: never
/// blocks on the disk, unlike the log file it replaces).
struct DbLayer;

impl<S: Subscriber> Layer<S> for DbLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut v = Message(String::new());
        event.record(&mut v);
        // `log` records (Velopack's) carry their real target in fields, not the metadata.
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        agt_telemetry::global().event(meta.level().as_str(), meta.target(), v.0);
    }
}

/// The `message` field, then any other fields as ` name=value`.
struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `log` records bridged by tracing-log carry their metadata as fields; skip those.
        match field.name() {
            "message" => {
                let _ = write!(self.0, "{value:?}");
            }
            name if name.starts_with("log.") => {}
            name => {
                let sep = if self.0.is_empty() { "" } else { " " };
                let _ = write!(self.0, "{sep}{name}={value:?}");
            }
        }
    }
}

pub fn init(level: &str) {
    let level: LevelFilter = level.parse().unwrap_or(LevelFilter::INFO);
    // Release builds on Windows have no console.
    let stderr =
        cfg!(debug_assertions).then(|| fmt::layer().with_ansi(false).with_writer(std::io::stderr));
    let result = tracing_subscriber::registry()
        .with(filter(level))
        .with(DbLayer)
        .with(stderr)
        .try_init();
    if let Err(e) = result {
        eprintln!("agt: logging init failed: {e}");
    }

    std::panic::set_hook(Box::new(|info| {
        if cfg!(debug_assertions) {
            eprintln!("{info}");
        }
        tracing::error!("{}", panic_message(info));
        // Best effort: the process may be about to die; don't hang it if the writer is stuck
        // (or is the thread that panicked).
        agt_telemetry::global().flush(Duration::from_millis(500));
    }));
}

/// "panic at <file:line:col> on thread <name>: <message>". The message is only kept when
/// it's a literal (`panic!("...")`, `unreachable!()`): a formatted one may contain anything,
/// e.g. the string a bad slice index was taken from, which could be dictated text.
/// (A message starting with "panic" marks the session as a crash; see `agt-telemetry`.)
fn panic_message(info: &std::panic::PanicHookInfo) -> String {
    let location = info
        .location()
        .map_or_else(|| "unknown location".into(), |l| l.to_string());
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    let message = info
        .payload()
        .downcast_ref::<&'static str>()
        .copied()
        .unwrap_or("<formatted message withheld>");
    format!("panic at {location} on thread {thread}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn http_wire_dumps_never_recorded() {
        let f = filter(LevelFilter::TRACE);
        assert!(f.would_enable("agt::pipeline", &Level::TRACE));
        for target in [
            "ureq",
            "ureq::unversioned",
            "ureq_proto",
            "ureq_proto::util",
        ] {
            assert!(!f.would_enable(target, &Level::TRACE), "{target}");
            assert!(f.would_enable(target, &Level::DEBUG), "{target}");
        }
        assert!(!filter(LevelFilter::INFO).would_enable("ureq_proto::util", &Level::DEBUG));
    }
}
