//! The speech-to-text engine interface. Engines live in `agt-stt`; this crate only defines
//! the contract so the pipeline, the app and the bench don't depend on any engine.
//!
//! Engines are batch: the whole utterance goes in after the hotkey is released. A streaming
//! engine can be added later as a separate, optional trait (e.g. `SttEngine::stream()`
//! returning `Option<Box<dyn SttStream>>`) without changing this one.

/// Sample rate of the audio passed to [`SttEngine::transcribe`].
pub const SAMPLE_RATE: u32 = 16_000;

/// Engine errors are reported to the user as text and logged; nothing matches on them.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Default)]
pub struct TranscribeOpts {
    /// BCP-47-ish language hint (`"en"`); `None` = engine default / auto-detect.
    /// Engines that can't use it ignore it.
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    /// Raw engine output (not yet trimmed or otherwise post-processed).
    pub text: String,
}

/// A loaded speech-to-text model. Constructed once from config and kept loaded (warm) for
/// the app's lifetime; `transcribe` is called from one worker thread at a time.
pub trait SttEngine: Send {
    /// For logs, e.g. `"parakeet-tdt-0.6b-v2 int8 (cpu)"`.
    fn name(&self) -> String;

    /// Run a dummy inference so the first real dictation doesn't pay one-time costs
    /// (allocations, kernel/shader compilation).
    fn warmup(&mut self) -> Result<()>;

    /// `audio`: mono f32 samples in [-1, 1] at [`SAMPLE_RATE`].
    fn transcribe(&mut self, audio: &[f32], opts: &TranscribeOpts) -> Result<Transcript>;
}
