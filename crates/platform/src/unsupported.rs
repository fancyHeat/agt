//! Placeholder for platforms without an implementation (anything but Windows and macOS).
//! Everything compiles and fails with a clear error instead of crashing.

use crate::{Error, Hotkey, HotkeyEvent, Key, Permission};

const MSG: &str = "not supported on this platform";

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

pub fn accessibility_name() -> &'static str {
    "Accessibility"
}

pub fn missing_permissions() -> Vec<Permission> {
    Vec::new()
}

pub fn request_permissions(_missing: &[Permission]) {}

pub fn open_permission_settings(_permission: Permission) {}

/// No volume control here: ducking does nothing.
pub struct OutputVolume;

pub fn init_volume_thread() {}

impl OutputVolume {
    pub fn open_default() -> Result<Option<Self>, Error> {
        Ok(None)
    }

    pub fn open(_id: &str) -> Result<Option<Self>, Error> {
        Ok(None)
    }

    pub fn id(&self) -> &str {
        ""
    }

    pub fn volume(&self) -> Result<f32, Error> {
        Err(format!("volume control {MSG}").into())
    }

    pub fn set_volume(&self, _v: f32) -> Result<(), Error> {
        Err(format!("volume control {MSG}").into())
    }

    pub fn muted(&self) -> Result<bool, Error> {
        Ok(false)
    }
}
