//! Placeholder for platforms without an implementation yet (macOS until Phase 4).
//! Everything compiles and fails with a clear error instead of crashing.

use crate::{Error, HotkeyEvent, Key};

const MSG: &str = "not yet supported on this platform (macOS support is planned)";

pub struct HotkeyHandle;
pub struct ClipboardSnapshot;
pub struct PasteToken;

pub fn start_hotkey(
    _key: Key,
    _on_event: Box<dyn Fn(HotkeyEvent) + Send>,
) -> Result<HotkeyHandle, Error> {
    Err(format!("global hotkey {MSG}").into())
}

pub fn snapshot_clipboard() -> Result<ClipboardSnapshot, Error> {
    Err(format!("clipboard access {MSG}").into())
}

pub fn paste_text(_text: &str) -> Result<PasteToken, Error> {
    Err(format!("pasting {MSG}").into())
}

pub fn restore_clipboard(_s: &ClipboardSnapshot, _t: &PasteToken) -> Result<bool, Error> {
    Err(format!("clipboard access {MSG}").into())
}
