//! AGT core: configuration, well-known paths, and the [`stt::SttEngine`] and
//! [`polish::Polisher`] traits.

pub mod config;
pub mod paths;
pub mod polish;
pub mod stt;

pub use config::Config;

/// Human-readable app name.
pub const APP_NAME: &str = "AGT";
