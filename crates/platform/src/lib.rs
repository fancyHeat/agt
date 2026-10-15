//! OS integration: the hold-to-talk global hotkey and clipboard paste.
//!
//! Windows is implemented (low-level keyboard hook, Win32 clipboard, `SendInput`).
//! macOS (and anything else) currently returns [`Error`]s saying so; Phase 4 fills it in.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

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
    /// The hotkey went down (auto-repeat is filtered out). Can also arrive while a hold is
    /// still believed active, if its release was never seen (e.g. it happened on the secure
    /// desktop): the receiver should then drop the stale hold and start over.
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

/// Supported hotkeys (names are case-insensitive) and their Windows virtual-key codes.
///
/// - Ctrl and Shift are passed through to apps (holding them alone does nothing), and their
///   state can be polled to catch a release the hook missed.
/// - F13-F24 (typically mapped to spare mouse/keyboard buttons) are swallowed: apps never
///   see them while AGT runs.
///
/// Deliberately absent: Alt and Win (releasing them alone opens the menu bar / Start menu),
/// and CapsLock, F1-F12, Insert, Pause (they would have to be swallowed, taking them away
/// from apps).
pub const KEYS: &[(&str, u16)] = &[
    ("RightCtrl", 0xA3),
    ("LeftCtrl", 0xA2),
    ("RightShift", 0xA1),
    ("LeftShift", 0xA0),
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

    /// Whether the hook swallows this key: it's not a modifier, so passing it through would
    /// act in the focused app.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn swallowed(&self) -> bool {
        !is_modifier(self.vk as u32)
    }
}

/// Ctrl, Shift, Alt and Win (generic and left/right virtual-key codes).
#[cfg_attr(not(windows), allow(dead_code))]
fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

/// Keeps the global hotkey alive; dropping it removes the hook.
pub struct HotkeyHandle(imp::HotkeyHandle);

impl HotkeyHandle {
    /// A handle other threads can use to reset the hook.
    pub fn resetter(&self) -> HotkeyReset {
        HotkeyReset(self.0.resetter())
    }
}

/// Re-installs the keyboard hook and forgets any hold in progress. Use it after detecting a
/// missed release: Windows may also have silently removed a hook that was too slow once.
#[derive(Clone)]
pub struct HotkeyReset(imp::HotkeyReset);

impl HotkeyReset {
    pub fn reset(&self) {
        self.0.reset()
    }
}

/// Starts watching `key` on a dedicated, time-critical-priority thread. `on_event` runs on
/// that thread and must return quickly (e.g. just send on a channel): a slow handler delays
/// every keystroke on the system. Ctrl/Shift hotkeys are passed through; F13-F24 are
/// swallowed.
pub fn start_hotkey(
    key: Key,
    on_event: impl Fn(HotkeyEvent) + Send + 'static,
) -> Result<HotkeyHandle, Error> {
    imp::start_hotkey(key, Box::new(on_event)).map(HotkeyHandle)
}

/// Whether `key` is physically down right now, if that can be known: `None` for swallowed
/// keys (their state never reaches the system) and on unsupported platforms.
pub fn key_down(key: Key) -> Option<bool> {
    imp::key_down(key)
}

/// Identifies the window that has keyboard focus (where a paste would go), if known. Only
/// meant for comparing: "is it still the same window?".
pub fn foreground_window() -> Option<usize> {
    imp::foreground_window()
}

/// A saved copy of the clipboard, to put back after pasting.
pub struct ClipboardSnapshot(imp::ClipboardSnapshot);

impl ClipboardSnapshot {
    /// Whether the clipboard changed since this snapshot was taken.
    pub fn is_stale(&self) -> bool {
        imp::is_stale(&self.0)
    }
}

/// Copies the current clipboard contents (every format that can be copied as plain memory:
/// text, HTML, RTF, images as DIB, file lists...). May be slow, or even hang, if the source
/// app renders formats lazily: call it on a thread you can abandon.
pub fn snapshot_clipboard() -> Result<ClipboardSnapshot, Error> {
    imp::snapshot_clipboard().map(ClipboardSnapshot)
}

/// Puts `text` on the clipboard (excluded from clipboard history).
pub fn set_clipboard_text(text: &str) -> Result<PasteToken, Error> {
    imp::set_clipboard_text(text).map(PasteToken)
}

/// Sends Ctrl+V to the focused window (releasing held Shift/Alt/Win first). Returns once the
/// keystrokes are queued; the target app reads the clipboard asynchronously, so wait a bit
/// before restoring it. Fails e.g. when the focused app runs elevated.
pub fn send_paste() -> Result<(), Error> {
    PASTES.fetch_add(1, Ordering::SeqCst);
    imp::send_paste()
}

static PASTES: AtomicU64 = AtomicU64::new(0);

/// How many times [`send_paste`] has been called. With [`paste_may_release`], lets a caller
/// tell a real release of the hotkey from our own injected key-ups.
pub fn paste_count() -> u64 {
    PASTES.load(Ordering::SeqCst)
}

/// Whether [`send_paste`] can make `key` *look* released to [`key_down`] while it is still
/// physically held: it injects a Shift-up for held Shift keys and a Ctrl-up that maps to Left
/// Ctrl. Right Ctrl is never touched.
pub fn paste_may_release(key: Key) -> bool {
    matches!(key.vk, 0xA0..=0xA2) // LeftShift, RightShift, LeftCtrl
}

/// Identifies the clipboard state [`set_clipboard_text`] created.
pub struct PasteToken(imp::PasteToken);

/// Restores `snapshot`, unless the clipboard changed since `token` (the user copied something
/// new; we must not clobber it). Returns whether it restored.
pub fn restore_clipboard(snapshot: &ClipboardSnapshot, token: &PasteToken) -> Result<bool, Error> {
    imp::restore_clipboard(&snapshot.0, &token.0)
}

/// Auto-repeat arrives at most `keyboard delay` (<= 1 s) after the previous event; a hotkey
/// press after a longer silence while "held" means the release was missed.
const STALE_GAP_MS: u32 = 1500;

/// Turns raw key transitions into hold events: ignores auto-repeat, cancels when any other
/// key is pressed during the hold, and recovers from a missed release. Platform-independent
/// so it can be unit-tested.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct HoldDetector {
    state: Hold,
    /// Event time (ms, wrapping) of the last hotkey transition.
    last_hotkey_ms: u32,
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
    /// or release; `time_ms`: the event's timestamp (`KBDLLHOOKSTRUCT::time`).
    pub(crate) fn on_key(
        &mut self,
        is_hotkey: bool,
        down: bool,
        time_ms: u32,
        now: Instant,
    ) -> Option<HotkeyEvent> {
        let stale = is_hotkey
            && down
            && self.state != Hold::Idle
            && time_ms.wrapping_sub(self.last_hotkey_ms) > STALE_GAP_MS;
        if is_hotkey {
            self.last_hotkey_ms = time_ms;
        }
        if stale {
            // A fresh press, not auto-repeat: the previous release was never seen.
            self.state = Hold::Held;
            return Some(HotkeyEvent::Down(now));
        }
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

    /// Forget any hold in progress.
    pub(crate) fn reset(&mut self) {
        self.state = Hold::Idle;
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
        for gone in ["RightAlt", "RightWin", "CapsLock", "F5", "Insert", "Pause"] {
            assert!(Key::parse(gone).is_err(), "{gone} should not be allowed");
        }
        assert!(!Key::parse("LeftShift").unwrap().swallowed());
        assert!(Key::parse("F24").unwrap().swallowed());
        assert!(!paste_may_release(Key::parse("RightCtrl").unwrap()));
        assert!(paste_may_release(Key::parse("LeftCtrl").unwrap()));
        assert!(paste_may_release(Key::parse("RightShift").unwrap()));
    }

    #[test]
    fn hold_ignores_repeat_and_reports_release() {
        let t = Instant::now();
        let mut d = HoldDetector::default();
        assert_eq!(d.on_key(true, true, 1000, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(true, true, 1500, t), None); // first auto-repeat
        assert_eq!(d.on_key(true, true, 1533, t), None);
        assert_eq!(d.on_key(false, false, 1600, t), None); // release of some other key
        assert_eq!(d.on_key(true, false, 9000, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(true, false, 9001, t), None);
    }

    #[test]
    fn other_key_cancels_and_swallows_release() {
        let t = Instant::now();
        let mut d = HoldDetector::default();
        assert_eq!(d.on_key(false, true, 0, t), None); // typing while idle
        assert_eq!(d.on_key(true, true, 10, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(false, true, 20, t), Some(HotkeyEvent::Cancel)); // e.g. Esc
        assert_eq!(d.on_key(false, true, 30, t), None);
        assert_eq!(d.on_key(true, false, 40, t), None);
        // Next hold works normally.
        assert_eq!(d.on_key(true, true, 50, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn missed_release_does_not_turn_next_press_into_repeat() {
        let t = Instant::now();
        let mut d = HoldDetector::default();
        assert_eq!(d.on_key(true, true, 1_000, t), Some(HotkeyEvent::Down(t)));
        // The release happened on the secure desktop; the hook never saw it. Much later:
        assert_eq!(d.on_key(true, true, 60_000, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(true, false, 61_000, t), Some(HotkeyEvent::Up(t)));
        // Same after a cancelled hold whose release was missed, across tick wrap-around.
        let mut d = HoldDetector::default();
        d.on_key(true, true, u32::MAX - 10, t);
        d.on_key(false, true, u32::MAX - 5, t);
        assert_eq!(d.on_key(true, true, 5_000, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn reset_forgets_the_hold() {
        let t = Instant::now();
        let mut d = HoldDetector::default();
        d.on_key(true, true, 0, t);
        d.reset();
        assert_eq!(d.on_key(true, false, 100, t), None);
        assert_eq!(d.on_key(true, true, 200, t), Some(HotkeyEvent::Down(t)));
    }
}
