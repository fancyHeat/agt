//! Speech-to-text engines behind [`agt_core::stt::SttEngine`], and their model downloads.
//!
//! Engines are compiled in and picked by `stt.engine` in the config. Each engine is a module
//! here; heavy engine-specific dependencies can be put behind cargo features as more arrive.

pub mod chunk;
pub mod models;
pub mod parakeet;
pub mod tdt;
#[cfg(feature = "whisper")]
pub mod whisper;

use agt_core::{
    config::{Engine, SttConfig},
    stt::{Result, SttEngine},
};
pub use models::Progress;

/// Longest audio transcribed in one engine call; longer audio is split (see [`chunk`]).
pub const CHUNK_SECS: f32 = 25.0;
/// Window before each chunk limit searched for the quietest split point.
pub const CHUNK_SEARCH_SECS: f32 = 5.0;

/// The `stt.model` names `engine` knows (none if this build doesn't have it), for the tray's
/// Settings menu.
pub fn model_names(engine: Engine) -> Vec<&'static str> {
    match engine {
        Engine::Parakeet => parakeet::MODELS.to_vec(),
        #[cfg(feature = "whisper")]
        Engine::Whisper => whisper::model_names().collect(),
        #[cfg(not(feature = "whisper"))]
        Engine::Whisper => Vec::new(),
    }
}

/// Builds the configured engine, downloading its model first if needed (blocking; call it
/// off the UI thread). `progress` reports download progress.
pub fn load(cfg: &SttConfig, progress: &mut dyn FnMut(Progress)) -> Result<Box<dyn SttEngine>> {
    let engine: Box<dyn SttEngine> = match cfg.engine {
        Engine::Parakeet => Box::new(parakeet::Parakeet::load(cfg, progress)?),
        #[cfg(feature = "whisper")]
        Engine::Whisper => Box::new(whisper::Whisper::load(cfg, progress)?),
        #[cfg(not(feature = "whisper"))]
        Engine::Whisper => {
            return Err("this build has no whisper engine (cargo feature `whisper`)".into());
        }
    };
    // Parakeet: 20-25 s chunks keep full attention cheap (RTF stays ~0.05 at any length).
    // An engine with a fixed input window (a static-shape GPU encoder) gets chunks that fit
    // it: see `SttEngine::max_input_secs`.
    Ok(Box::new(chunk::Chunked::new(
        engine,
        CHUNK_SECS,
        CHUNK_SEARCH_SECS,
    )))
}
