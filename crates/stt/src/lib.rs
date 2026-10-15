//! Speech-to-text engines behind [`agt_core::stt::SttEngine`], and their model downloads.
//!
//! Engines are compiled in and picked by `stt.engine` in the config. Each engine is a module
//! here; heavy engine-specific dependencies can be put behind cargo features as more arrive.

pub mod models;
pub mod parakeet;
pub mod tdt;

use agt_core::{
    config::{Engine, SttConfig},
    stt::{Result, SttEngine},
};
pub use models::Progress;

/// Builds the configured engine, downloading its model first if needed (blocking; call it
/// off the UI thread). `progress` reports download progress.
pub fn load(cfg: &SttConfig, progress: &mut dyn FnMut(Progress)) -> Result<Box<dyn SttEngine>> {
    Ok(match cfg.engine {
        Engine::Parakeet => Box::new(parakeet::Parakeet::load(cfg, progress)?),
    })
}
