//! AGT core: configuration, the user's dictionary, well-known paths, and the
//! [`stt::SttEngine`] and [`polish::Polisher`] traits.

pub mod config;
pub mod dictionary;
pub mod paths;
pub mod polish;
pub mod stt;

pub use config::Config;
pub use dictionary::{Dictionary, Scope};

/// Human-readable app name.
pub const APP_NAME: &str = "AGT";
