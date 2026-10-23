//! Settings, read from `config.toml` and written back to it ([`agt_core::config::set`]); the
//! file stays the only place settings live. [`schema`] describes them; the settings window
//! ([`window`], its own process: `agt settings`, with a [`metrics`] page) shows them.

pub mod metrics;
pub mod schema;
pub mod window;

/// What a settings window button does.
#[derive(Clone)]
pub enum Action {
    /// Open `config.toml` in a text editor.
    EditFile,
    /// Open `dictionary.toml` in a text editor, created from its template if missing.
    EditDictionary,
    /// Open the saved dictations' folder (created if needed).
    OpenHistory,
    /// Show a hotkey row that listens for the keys of a new hotkey.
    AddHotkey,
    /// Add the replacement typed on the Replacements page ([`schema::Draft`]) to
    /// `dictionary.toml`.
    AddReplacement,
}
