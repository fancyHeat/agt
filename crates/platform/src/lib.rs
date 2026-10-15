//! OS integration: the hold-to-talk global hotkey and clipboard paste.
//!
//! Windows is implemented (low-level keyboard hook, Win32 clipboard, `SendInput`).
//! macOS (and anything else) currently returns [`Error`]s saying so; Phase 4 fills it in.

use std::time::Instant;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
use unsupported as imp;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// What the hotkey thread reports. Delivered from the hook thread: handlers must not block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// The hotkey went down (auto-repeat is filtered out).
    Down(Instant),
    /// The hotkey was released after a `Down` (and no `Cancel`).
    Up(Instant),
    /// Another key was pressed while holding the hotkey (Esc, or a shortcut such as
    /// Ctrl+C): the hold is not a dictation. No `Up` follows.
    Cancel,
}

/// A hotkey, by name. See [`KEYS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub name: &'static str,
    /// Windows virtual-key code.
    #[cfg_attr(not(windows), allow(dead_code))]
    vk: u16,
}

/// Supported hotkey names (case-insensitive) and their Windows virtual-key codes.
/// Modifier-only keys work best: holding them doesn't type anything.
pub const KEYS: &[(&str, u16)] = &[
    ("RightCtrl", 0xA3),
    ("LeftCtrl", 0xA2),
    ("RightAlt", 0xA5),
    ("RightShift", 0xA1),
    ("LeftShift", 0xA0),
    ("RightWin", 0x5C),
    ("CapsLock", 0x14),
    ("ScrollLock", 0x91),
    ("Pause", 0x13),
    ("Insert", 0x2D),
    ("F1", 0x70),
    ("F2", 0x71),
    ("F3", 0x72),
    ("F4", 0x73),
    ("F5", 0x74),
    ("F6", 0x75),
    ("F7", 0x76),
    ("F8", 0x77),
    ("F9", 0x78),
    ("F10", 0x79),
    ("F11", 0x7A),
    ("F12", 0x7B),
    ("F13", 0x7C),
    ("F14", 0x7D),
    ("F15", 0x7E),
    ("F16", 0x7F),
    ("F17", 0x80),
    ("F18", 0x81),
    ("F19", 0x82),
    ("F20", 0x83),
    ("F21", 0x84),
    ("F22", 0x85),
    ("F23", 0x86),
    ("F24", 0x87),
];

impl Key {
    pub fn parse(name: &str) -> Result<Self, String> {
        KEYS.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
            .map(|&(name, vk)| Key { name, vk })
            .ok_or_else(|| {
                let names: Vec<_> = KEYS.iter().map(|(n, _)| *n).collect();
                format!(
                    "unknown hotkey {name:?}; expected one of {}",
                    names.join(", ")
                )
            })
    }
}

/// Keeps the global hotkey alive; dropping it removes the hook.
pub struct HotkeyHandle(#[allow(dead_code)] imp::HotkeyHandle);

/// Starts watching `key` on a dedicated thread. `on_event` runs on that thread and must
/// return quickly (e.g. just send on a channel): a slow handler delays every keystroke on
/// the system. The key itself is never swallowed.
pub fn start_hotkey(
    key: Key,
    on_event: impl Fn(HotkeyEvent) + Send + 'static,
) -> Result<HotkeyHandle, Error> {
    imp::start_hotkey(key, Box::new(on_event)).map(HotkeyHandle)
}

/// A saved copy of the clipboard, to put back after pasting.
pub struct ClipboardSnapshot(imp::ClipboardSnapshot);

/// Copies the current clipboard contents (every format that can be copied as plain memory:
/// text, HTML, RTF, images as DIB, file lists...). May be slow if the source app renders
/// formats lazily, so take it before it's needed (e.g. on hotkey down).
pub fn snapshot_clipboard() -> Result<ClipboardSnapshot, Error> {
    imp::snapshot_clipboard().map(ClipboardSnapshot)
}

/// Puts `text` on the clipboard (excluded from clipboard history) and sends Ctrl+V to the
/// focused window. Returns once the keystrokes are queued; the target app reads the
/// clipboard asynchronously, so wait a bit before restoring it.
pub fn paste_text(text: &str) -> Result<PasteToken, Error> {
    imp::paste_text(text).map(PasteToken)
}

/// Identifies the clipboard state our paste created.
pub struct PasteToken(imp::PasteToken);

/// Restores `snapshot`, unless the clipboard changed since `token`'s paste (the user copied
/// something new; we must not clobber it). Returns whether it restored.
pub fn restore_clipboard(snapshot: &ClipboardSnapshot, token: &PasteToken) -> Result<bool, Error> {
    imp::restore_clipboard(&snapshot.0, &token.0)
}

/// Turns raw key transitions into hold events: ignores auto-repeat, cancels when any other
/// key is pressed during the hold. Platform-independent so it can be unit-tested.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct HoldDetector {
    state: Hold,
}

#[derive(Debug, Default, PartialEq)]
enum Hold {
    #[default]
    Idle,
    Held,
    /// Another key was pressed; wait for the hotkey's release silently.
    Cancelled,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl HoldDetector {
    /// `is_hotkey`: whether this transition is for the hotkey; `down`: press (incl. repeat)
    /// or release.
    pub(crate) fn on_key(
        &mut self,
        is_hotkey: bool,
        down: bool,
        now: Instant,
    ) -> Option<HotkeyEvent> {
        match (is_hotkey, down, &self.state) {
            (true, true, Hold::Idle) => {
                self.state = Hold::Held;
                Some(HotkeyEvent::Down(now))
            }
            (true, false, Hold::Held) => {
                self.state = Hold::Idle;
                Some(HotkeyEvent::Up(now))
            }
            (true, false, Hold::Cancelled) => {
                self.state = Hold::Idle;
                None
            }
            (false, true, Hold::Held) => {
                self.state = Hold::Cancelled;
                Some(HotkeyEvent::Cancel)
            }
            // Auto-repeat, other keys while idle, releases of other keys.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys() {
        assert_eq!(Key::parse("rightctrl").unwrap().name, "RightCtrl");
        assert_eq!(Key::parse(" F13 ").unwrap().vk, 0x7C);
        assert!(Key::parse("Hyper").is_err());
    }

    #[test]
    fn hold_ignores_repeat_and_reports_release() {
        let t = Instant::now();
        let mut d = HoldDetector::default();
        assert_eq!(d.on_key(true, true, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(true, true, t), None); // auto-repeat
        assert_eq!(d.on_key(false, false, t), None); // release of some other key
        assert_eq!(d.on_key(true, false, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(true, false, t), None);
    }

    #[test]
    fn other_key_cancels_and_swallows_release() {
        let t = Instant::now();
        let mut d = HoldDetector::default();
        assert_eq!(d.on_key(false, true, t), None); // typing while idle
        assert_eq!(d.on_key(true, true, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(false, true, t), Some(HotkeyEvent::Cancel)); // e.g. Esc
        assert_eq!(d.on_key(false, true, t), None);
        assert_eq!(d.on_key(true, false, t), None);
        // Next hold works normally.
        assert_eq!(d.on_key(true, true, t), Some(HotkeyEvent::Down(t)));
    }
}
