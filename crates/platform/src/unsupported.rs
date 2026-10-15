//! Placeholder for platforms without an implementation yet (macOS until Phase 4).
//! Everything compiles and fails with a clear error instead of crashing.

use crate::{Error, Hotkey, HotkeyEvent, Key};

const MSG: &str = "not yet supported on this platform (macOS support is planned)";

pub struct HotkeyHandle;
#[derive(Clone)]
pub struct HotkeyReset;
pub struct ClipboardSnapshot;
pub struct PasteToken;

impl HotkeyHandle {
    pub fn resetter(&self) -> HotkeyReset {
        HotkeyReset
    }
}

impl HotkeyReset {
    pub fn reset(&self) {}
}

pub fn start_hotkey(
    _hotkey: Hotkey,
    _on_event: Box<dyn Fn(HotkeyEvent) + Send>,
) -> Result<HotkeyHandle, Error> {
    Err(format!("global hotkey {MSG}").into())
}

pub fn paste_may_release(_key: Key) -> bool {
    false
}

pub fn key_down(_key: Key) -> Option<bool> {
    None
}

pub fn foreground_window() -> Option<usize> {
    None
}

pub fn snapshot_clipboard() -> Result<ClipboardSnapshot, Error> {
    Err(format!("clipboard access {MSG}").into())
}

pub fn is_stale(_s: &ClipboardSnapshot) -> bool {
    true
}

pub fn set_clipboard_text(_text: &str) -> Result<PasteToken, Error> {
    Err(format!("clipboard access {MSG}").into())
}

pub fn send_paste(_before_releasing: impl FnOnce()) -> Result<(), Error> {
    Err(format!("pasting {MSG}").into())
}

pub fn restore_clipboard(_s: &ClipboardSnapshot, _t: &PasteToken) -> Result<bool, Error> {
    Err(format!("clipboard access {MSG}").into())
}
