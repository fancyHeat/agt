//! Speech-to-text engines behind [`agt_core::stt::SttEngine`], and their model downloads.
//!
//! Engines are compiled in and picked by `stt.engine` in the config. Each engine is a module
//! here; heavy engine-specific dependencies can be put behind cargo features as more arrive.

pub mod chunk;
pub mod models;
pub mod parakeet;
pub mod tdt;

use agt_core::{
    config::{Device, Engine, SttConfig},
    stt::{Result, SttEngine},
};
pub use models::Progress;

/// Longest audio transcribed in one engine call; longer audio is split (see [`chunk`]).
pub const CHUNK_SECS: f32 = 25.0;
/// Window before each chunk limit searched for the quietest split point.
pub const CHUNK_SEARCH_SECS: f32 = 5.0;

/// Builds the configured engine, downloading its model first if needed (blocking; call it
/// off the UI thread). `progress` reports download progress.
pub fn load(cfg: &SttConfig, progress: &mut dyn FnMut(Progress)) -> Result<Box<dyn SttEngine>> {
    let engine: Box<dyn SttEngine> = match cfg.engine {
        Engine::Parakeet => Box::new(parakeet::Parakeet::load(cfg, progress)?),
    };
    // Parakeet: 20-25 s chunks keep full attention cheap (RTF stays ~0.05 at any length). On
    // Core ML every chunk is padded to a fixed window, so chunks match that window.
    let chunk = match cfg.device {
        Device::CoreMl => parakeet::COREML_WINDOW_SECS,
        _ => CHUNK_SECS,
    };
    Ok(Box::new(chunk::Chunked::new(
        engine,
        chunk,
        CHUNK_SEARCH_SECS.min(chunk / 2.0),
    )))
}
