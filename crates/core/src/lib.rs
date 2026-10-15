//! AGT core: configuration and well-known paths.
//!
//! Later phases add the dictation pipeline and the `SttEngine` / `Polisher` traits here.

pub mod config;
pub mod paths;

pub use config::Config;

/// Human-readable app name.
pub const APP_NAME: &str = "AGT";
