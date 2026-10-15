//! The optional LLM clean-up ("polish") interface. Backends live in `agt-polish`; this crate
//! only defines the contract, like [`crate::stt::SttEngine`].
//!
//! A polisher turns a raw transcript into cleaned-up text (punctuation, capitalization, filler
//! words). It is constructed once from config and kept for the app's lifetime. It must be
//! bounded in time: the pipeline waits for it before pasting, and falls back to the raw
//! transcript on any error.

pub use crate::stt::{Error, Result};

/// Per-call context. Empty for now; a place for things like the focused app's name later,
/// without changing the trait.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PolishCtx {}

pub trait Polisher: Send + Sync {
    /// For logs and the tray, e.g. `"anthropic claude-haiku-5-5"`.
    fn name(&self) -> String;

    /// Prepare for the first call (e.g. make a local server load its model, or open the
    /// connection). May take long; never called on the dictation path.
    fn warmup(&self) -> Result<()> {
        Ok(())
    }

    /// Returns the model's cleaned-up version of `text`, as-is. Callers validate it
    /// (`agt_polish::polish_or_raw`) before using it.
    fn polish(&self, text: &str, ctx: &PolishCtx) -> Result<String>;
}
