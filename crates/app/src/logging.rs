//! File logging: `<app dir>/logs/agt.YYYY-MM-DD.log`, daily rotation, last 7 files kept.

use std::path::Path;
use tracing::level_filters::LevelFilter;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub fn init(log_dir: &Path, level: &str) {
    let level: LevelFilter = level.parse().unwrap_or(LevelFilter::INFO);
    let _ = std::fs::create_dir_all(log_dir);
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("agt")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir);

    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false);
    let result = match appender {
        // Debug builds also log to stderr; release builds on Windows have no console.
        Ok(file) if cfg!(debug_assertions) => {
            builder.with_writer(file.and(std::io::stderr)).try_init()
        }
        Ok(file) => builder.with_writer(file).try_init(),
        Err(e) => {
            eprintln!("agt: cannot open log dir {}: {e}", log_dir.display());
            builder.with_writer(std::io::stderr).try_init()
        }
    };
    if let Err(e) = result {
        eprintln!("agt: logging init failed: {e}");
    }

    std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
}
