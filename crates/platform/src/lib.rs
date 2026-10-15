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
    /// The hotkey (every key of a chord) went down (auto-repeat is filtered out). Can also
    /// arrive while a hold is still believed active, if its release was never seen (e.g. it
    /// happened on the secure desktop): the receiver should then drop the stale hold and
    /// start over.
    Down(Instant),
    /// A hotkey key was released after a `Down` (and no `Cancel`).
    Up(Instant),
    /// Another key was pressed while holding the hotkey (Esc, or a shortcut such as Ctrl+C),
    /// or the hold turned out to be stale (a chord key was pressed again long after its
    /// release was missed): the hold is not a dictation. No `Up` follows.
    Cancel,
}

/// A single key, by name. See [`KEYS`]. The configured hotkey is a [`Hotkey`]: one key or a
/// chord of several.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub name: &'static str,
    /// Windows virtual-key code.
    #[cfg_attr(not(windows), allow(dead_code))]
    vk: u16,
}

/// Supported hotkey keys (names are case-insensitive) and their Windows virtual-key codes.
///
/// - Ctrl and Shift are passed through to apps (holding them alone does nothing), and their
///   state can be polled to catch a release the hook missed.
/// - Win is passed through too, but only allowed in a chord with Ctrl or Shift (see
///   [`Hotkey::parse`]).
/// - F13-F24 (typically mapped to spare mouse/keyboard buttons) are swallowed: apps never
///   see them while AGT runs.
///
/// Deliberately absent: Alt (releasing it alone opens the menu bar), and CapsLock, F1-F12,
/// Insert, Pause (they would have to be swallowed, taking them away from apps).
pub const KEYS: &[(&str, u16)] = &[
    ("RightCtrl", 0xA3),
    ("LeftCtrl", 0xA2),
    ("RightShift", 0xA1),
    ("LeftShift", 0xA0),
    ("LeftWin", 0x5B),
    ("RightWin", 0x5C),
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

fn key_names() -> String {
    KEYS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
}

impl Key {
    pub fn parse(name: &str) -> Result<Self, String> {
        KEYS.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
            .map(|&(name, vk)| Key { name, vk })
            .ok_or_else(|| {
                format!(
                    "unknown key {:?}; expected one of {}",
                    name.trim(),
                    key_names()
                )
            })
    }

    /// Whether the hook swallows this key: it's not a modifier, so passing it through would
    /// act in the focused app.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn swallowed(&self) -> bool {
        !is_modifier(self.vk as u32)
    }

    fn is_win(&self) -> bool {
        matches!(self.vk, 0x5B | 0x5C)
    }

    fn is_ctrl_or_shift(&self) -> bool {
        matches!(self.vk, 0xA0..=0xA3)
    }
}

/// The most keys a chord may have.
pub const MAX_CHORD: usize = 3;

/// The configured hotkey: one key, or a chord of up to [`MAX_CHORD`] keys (e.g.
/// `LeftCtrl+LeftWin`) that must all be held together. A dictation starts once every key is
/// down (in any order) and ends as soon as any of them is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    keys: Vec<Key>,
}

impl Hotkey {
    /// Parses `"RightCtrl"` or a `+`-separated chord such as `"LeftCtrl+LeftWin"` (names are
    /// case-insensitive; left and right keys are distinct). A chord has 1-3 distinct keys, at
    /// most one of which is not a modifier (F13-F24); Win is only allowed together with Ctrl
    /// or Shift.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let names: Vec<&str> = spec.split('+').map(str::trim).collect();
        if names.iter().any(|n| n.is_empty()) {
            return Err(format!(
                "invalid hotkey {spec:?}: expected a key name or a chord like \"LeftCtrl+LeftWin\" (keys: {})",
                key_names()
            ));
        }
        if names.len() > MAX_CHORD {
            return Err(format!(
                "invalid hotkey {spec:?}: a chord can have at most {MAX_CHORD} keys"
            ));
        }
        let mut keys = Vec::with_capacity(names.len());
        for name in names {
            let key = Key::parse(name).map_err(|e| format!("invalid hotkey {spec:?}: {e}"))?;
            if keys.contains(&key) {
                return Err(format!(
                    "invalid hotkey {spec:?}: {} is listed twice",
                    key.name
                ));
            }
            keys.push(key);
        }
        if keys.iter().filter(|k| k.swallowed()).count() > 1 {
            return Err(format!(
                "invalid hotkey {spec:?}: a chord can have only one non-modifier key (F13-F24), combined with Ctrl/Shift"
            ));
        }
        if keys.iter().any(Key::is_win) && !keys.iter().any(Key::is_ctrl_or_shift) {
            return Err(format!(
                "invalid hotkey {spec:?}: Win only works in a chord with Ctrl or Shift (e.g. \"LeftCtrl+LeftWin\"); on its own, a tap of Win could no longer open the Start menu and every key typed while holding it would trigger a Windows shortcut"
            ));
        }
        Ok(Hotkey { keys })
    }

    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// Which chord key `vk` is, if any.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn index_of(&self, vk: u32) -> Option<usize> {
        self.keys.iter().position(|k| k.vk as u32 == vk)
    }

    /// Whether the chord includes a Win key.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn has_win(&self) -> bool {
        self.keys.iter().any(Key::is_win)
    }
}

impl std::fmt::Display for Hotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, k) in self.keys.iter().enumerate() {
            if i > 0 {
                f.write_str("+")?;
            }
            f.write_str(k.name)?;
        }
        Ok(())
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

/// Starts watching `hotkey` on a dedicated, time-critical-priority thread. `on_event` runs on
/// that thread and must return quickly (e.g. just send on a channel): a slow handler delays
/// every keystroke on the system. Ctrl/Shift/Win keys are passed through; F13-F24 are
/// swallowed.
pub fn start_hotkey(
    hotkey: &Hotkey,
    on_event: impl Fn(HotkeyEvent) + Send + 'static,
) -> Result<HotkeyHandle, Error> {
    imp::start_hotkey(hotkey.clone(), Box::new(on_event)).map(HotkeyHandle)
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

/// Sends Ctrl+V to the focused window (releasing held Shift/Alt/Win first, so e.g. a Win key
/// still held from a `LeftCtrl+LeftWin` chord doesn't turn it into Win+Ctrl+V). Returns once
/// the keystrokes are queued; the target app reads the clipboard asynchronously, so wait a
/// bit before restoring it. Fails e.g. when the focused app runs elevated.
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
/// physically held: it injects key-ups for held Shift and Win keys, and a Ctrl-up that maps
/// to Left Ctrl. Right Ctrl is never touched.
pub fn paste_may_release(key: Key) -> bool {
    // LeftShift, RightShift, LeftCtrl, LeftWin, RightWin
    matches!(key.vk, 0xA0..=0xA2 | 0x5B | 0x5C)
}

/// Identifies the clipboard state [`set_clipboard_text`] created.
pub struct PasteToken(imp::PasteToken);

/// Restores `snapshot`, unless the clipboard changed since `token` (the user copied something
/// new; we must not clobber it). Returns whether it restored.
pub fn restore_clipboard(snapshot: &ClipboardSnapshot, token: &PasteToken) -> Result<bool, Error> {
    imp::restore_clipboard(&snapshot.0, &token.0)
}

/// Auto-repeat arrives at most `keyboard delay` (<= 1 s) after the previous event; a press of
/// a key that is believed to be already down, after a longer silence, means its release was
/// missed.
const STALE_GAP_MS: u32 = 1500;

/// Turns raw key transitions into hold events for a hotkey of one or more keys:
///
/// - The hold starts when every chord key is down, in any order (auto-repeat is ignored).
/// - It ends (`Up`) when any chord key is released. Then *all* chord keys must be released
///   before a new hold can start: letting go of one key and pressing it again while still
///   holding the others does not start a second dictation.
/// - Pressing any other non-modifier key while the hold is active cancels it (`Cancel`);
///   pressing one while the chord is only partly down (e.g. Ctrl+C, then Win) disarms the
///   chord until all its keys are released, so shortcuts never turn into dictations.
/// - A chord key pressed again long after it was last seen ([`STALE_GAP_MS`]) means its
///   release was missed (e.g. on the secure desktop): the stale state is dropped.
///
/// The caller filters out modifiers that aren't part of the chord (they neither cancel nor
/// disarm: they only matter together with a non-modifier key, which does). Platform-
/// independent so it can be unit-tested.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct HoldDetector {
    /// Number of chord keys.
    len: usize,
    state: Hold,
    /// Bit `i`: chord key `i` is believed to be down.
    down: u8,
    /// Event time (ms, wrapping) of each chord key's last transition.
    last_ms: [u32; MAX_CHORD],
}

#[derive(Debug, Default, PartialEq)]
enum Hold {
    /// Armed: the hold starts as soon as every chord key is down.
    #[default]
    Idle,
    Held,
    /// The hold ended or was cancelled, or the chord was disarmed: wait until every chord key
    /// is released.
    Rearm,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl HoldDetector {
    pub(crate) fn new(len: usize) -> Self {
        assert!((1..=MAX_CHORD).contains(&len));
        Self {
            len,
            state: Hold::Idle,
            down: 0,
            last_ms: [0; MAX_CHORD],
        }
    }

    fn complete(&self) -> bool {
        self.down == (1u8 << self.len) - 1
    }

    /// `chord_key`: which chord key this transition is for (`None`: another key); `down`:
    /// press (incl. auto-repeat) or release; `time_ms`: the event's timestamp
    /// (`KBDLLHOOKSTRUCT::time`).
    pub(crate) fn on_key(
        &mut self,
        chord_key: Option<usize>,
        down: bool,
        time_ms: u32,
        now: Instant,
    ) -> Option<HotkeyEvent> {
        let Some(i) = chord_key else {
            if !down {
                return None; // releases of other keys don't matter
            }
            return match self.state {
                Hold::Held => {
                    self.state = Hold::Rearm;
                    Some(HotkeyEvent::Cancel)
                }
                Hold::Idle if self.down != 0 => {
                    self.state = Hold::Rearm; // a shortcut with part of the chord
                    None
                }
                _ => None,
            };
        };
        let bit = 1u8 << i;
        let gap = time_ms.wrapping_sub(self.last_ms[i]);
        self.last_ms[i] = time_ms;
        if !down {
            self.down &= !bit;
            let ev = if self.state == Hold::Held {
                self.state = Hold::Rearm;
                Some(HotkeyEvent::Up(now))
            } else {
                None
            };
            if self.down == 0 {
                self.state = Hold::Idle;
            }
            return ev;
        }
        if self.down & bit == 0 {
            self.down |= bit;
            if self.state == Hold::Idle && self.complete() {
                self.state = Hold::Held;
                return Some(HotkeyEvent::Down(now));
            }
            return None;
        }
        if gap <= STALE_GAP_MS {
            return None; // auto-repeat
        }
        // A fresh press of a key we thought was still down: its release was missed. Other
        // keys not heard from for as long are presumably stale too.
        for j in 0..self.len {
            if j != i && time_ms.wrapping_sub(self.last_ms[j]) > STALE_GAP_MS {
                self.down &= !(1u8 << j);
            }
        }
        let was_held = self.state == Hold::Held;
        self.state = Hold::Idle;
        if self.complete() {
            self.state = Hold::Held;
            Some(HotkeyEvent::Down(now))
        } else {
            was_held.then_some(HotkeyEvent::Cancel)
        }
    }

    /// Forget any hold in progress.
    pub(crate) fn reset(&mut self) {
        self.state = Hold::Idle;
        self.down = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOT: Option<usize> = Some(0);
    const OTHER: Option<usize> = None;

    #[test]
    fn parse_keys() {
        assert_eq!(Key::parse("rightctrl").unwrap().name, "RightCtrl");
        assert_eq!(Key::parse(" F13 ").unwrap().vk, 0x7C);
        assert!(Key::parse("Hyper").is_err());
        for gone in ["RightAlt", "CapsLock", "F5", "Insert", "Pause"] {
            assert!(Key::parse(gone).is_err(), "{gone} should not be allowed");
        }
        assert!(!Key::parse("LeftShift").unwrap().swallowed());
        assert!(!Key::parse("LeftWin").unwrap().swallowed());
        assert!(Key::parse("F24").unwrap().swallowed());
        assert!(!paste_may_release(Key::parse("RightCtrl").unwrap()));
        assert!(paste_may_release(Key::parse("LeftCtrl").unwrap()));
        assert!(paste_may_release(Key::parse("RightShift").unwrap()));
        assert!(paste_may_release(Key::parse("LeftWin").unwrap()));
        assert!(paste_may_release(Key::parse("RightWin").unwrap()));
    }

    #[test]
    fn parse_hotkeys() {
        let names = |s: &str| {
            Hotkey::parse(s)
                .unwrap()
                .keys()
                .iter()
                .map(|k| k.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("RightCtrl"), ["RightCtrl"]);
        assert_eq!(names("leftctrl + LEFTWIN"), ["LeftCtrl", "LeftWin"]);
        assert_eq!(names("LeftWin+LeftShift"), ["LeftWin", "LeftShift"]);
        assert_eq!(names("RightCtrl+F13"), ["RightCtrl", "F13"]);
        assert_eq!(
            names("LeftCtrl+LeftShift+LeftWin"),
            ["LeftCtrl", "LeftShift", "LeftWin"]
        );
        assert_eq!(names("LeftCtrl+RightCtrl"), ["LeftCtrl", "RightCtrl"]);
        assert_eq!(
            Hotkey::parse("leftctrl+leftwin").unwrap().to_string(),
            "LeftCtrl+LeftWin"
        );
        let err = |s: &str| Hotkey::parse(s).unwrap_err();
        assert!(err("Hyper").contains("LeftWin"), "lists valid names");
        assert!(err("LeftCtrl+Hyper").contains("unknown key \"Hyper\""));
        assert!(err("").contains("chord like"));
        assert!(err("LeftCtrl+").contains("chord like"));
        assert!(err("LeftCtrl++LeftWin").contains("chord like"));
        assert!(err("LeftCtrl+leftctrl").contains("twice"));
        assert!(err("LeftCtrl+LeftShift+LeftWin+RightCtrl").contains("at most 3"));
        assert!(err("F13+F14").contains("only one non-modifier"));
        assert!(err("LeftCtrl+F13+F14").contains("only one non-modifier"));
        for win_only in ["LeftWin", "RightWin", "LeftWin+RightWin", "LeftWin+F13"] {
            assert!(err(win_only).contains("Start menu"), "{win_only}");
        }
        assert!(Hotkey::parse("LeftCtrl+LeftWin").unwrap().has_win());
        assert!(!Hotkey::parse("LeftCtrl+F13").unwrap().has_win());
    }

    #[test]
    fn hold_ignores_repeat_and_reports_release() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        assert_eq!(d.on_key(HOT, true, 1000, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(HOT, true, 1500, t), None); // first auto-repeat
        assert_eq!(d.on_key(HOT, true, 1533, t), None);
        assert_eq!(d.on_key(OTHER, false, 1600, t), None); // release of some other key
        assert_eq!(d.on_key(HOT, false, 9000, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(HOT, false, 9001, t), None);
        assert_eq!(d.on_key(HOT, true, 9100, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn other_key_cancels_and_swallows_release() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        assert_eq!(d.on_key(OTHER, true, 0, t), None); // typing while idle
        assert_eq!(d.on_key(HOT, true, 10, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(OTHER, true, 20, t), Some(HotkeyEvent::Cancel)); // e.g. Esc
        assert_eq!(d.on_key(OTHER, true, 30, t), None);
        assert_eq!(d.on_key(HOT, false, 40, t), None);
        // Next hold works normally.
        assert_eq!(d.on_key(HOT, true, 50, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn missed_release_does_not_turn_next_press_into_repeat() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        assert_eq!(d.on_key(HOT, true, 1_000, t), Some(HotkeyEvent::Down(t)));
        // The release happened on the secure desktop; the hook never saw it. Much later:
        assert_eq!(d.on_key(HOT, true, 60_000, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(HOT, false, 61_000, t), Some(HotkeyEvent::Up(t)));
        // Same after a cancelled hold whose release was missed, across tick wrap-around.
        let mut d = HoldDetector::new(1);
        d.on_key(HOT, true, u32::MAX - 10, t);
        d.on_key(OTHER, true, u32::MAX - 5, t);
        assert_eq!(d.on_key(HOT, true, 5_000, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn reset_forgets_the_hold() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        d.on_key(HOT, true, 0, t);
        d.reset();
        assert_eq!(d.on_key(HOT, false, 100, t), None);
        assert_eq!(d.on_key(HOT, true, 200, t), Some(HotkeyEvent::Down(t)));
    }

    // Chords: key 0 = Ctrl, key 1 = Win.
    const CTRL: Option<usize> = Some(0);
    const WIN: Option<usize> = Some(1);

    #[test]
    fn chord_starts_when_all_keys_are_down_in_any_order() {
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_millis(300);
        for (first, second) in [(CTRL, WIN), (WIN, CTRL)] {
            let mut d = HoldDetector::new(2);
            assert_eq!(d.on_key(first, true, 0, t0), None);
            assert_eq!(d.on_key(first, true, 500, t0), None); // auto-repeat
            // The hold (and the short-hold threshold) starts when the chord completes.
            assert_eq!(d.on_key(second, true, 600, t1), Some(HotkeyEvent::Down(t1)));
            assert_eq!(d.on_key(second, true, 1100, t1), None); // auto-repeat
            assert_eq!(d.on_key(first, true, 1150, t1), None); // late repeat of the first
            // Releasing either key ends it.
            assert_eq!(d.on_key(first, false, 2000, t1), Some(HotkeyEvent::Up(t1)));
            assert_eq!(d.on_key(second, false, 2010, t1), None);
            // Ready again.
            assert_eq!(d.on_key(second, true, 3000, t0), None);
            assert_eq!(d.on_key(first, true, 3010, t1), Some(HotkeyEvent::Down(t1)));
            assert_eq!(d.on_key(second, false, 3500, t1), Some(HotkeyEvent::Up(t1)));
        }
    }

    #[test]
    fn chord_short_tap_is_a_normal_down_up() {
        // The receiver measures Up - Down against min_hold_ms; both are reported.
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(WIN, false, 40, t), Some(HotkeyEvent::Up(t)));
    }

    #[test]
    fn partial_release_and_repress_does_not_start_a_second_hold() {
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(WIN, false, 1000, t), Some(HotkeyEvent::Up(t)));
        // Ctrl still held; Win again: no new dictation...
        assert_eq!(d.on_key(WIN, true, 1100, t), None);
        assert_eq!(d.on_key(WIN, false, 1200, t), None);
        // ...until everything was released.
        assert_eq!(d.on_key(CTRL, false, 1300, t), None);
        d.on_key(WIN, true, 1400, t);
        assert_eq!(d.on_key(CTRL, true, 1410, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn other_key_during_chord_cancels() {
        // Ctrl+Win+Right (switch desktop), Ctrl+Win+D, ...: cancel, then wait for release.
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(OTHER, true, 50, t), Some(HotkeyEvent::Cancel));
        assert_eq!(d.on_key(OTHER, false, 60, t), None);
        assert_eq!(d.on_key(OTHER, true, 70, t), None); // again: Ctrl+Win+Right twice
        assert_eq!(d.on_key(WIN, false, 80, t), None);
        assert_eq!(d.on_key(WIN, true, 90, t), None); // Ctrl never released
        assert_eq!(d.on_key(WIN, false, 95, t), None);
        assert_eq!(d.on_key(CTRL, false, 100, t), None);
        d.on_key(CTRL, true, 200, t);
        assert_eq!(d.on_key(WIN, true, 210, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn shortcut_with_part_of_the_chord_disarms_it() {
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(OTHER, true, 10, t), None); // Ctrl+C
        assert_eq!(d.on_key(OTHER, false, 20, t), None);
        assert_eq!(d.on_key(WIN, true, 30, t), None); // not a dictation
        assert_eq!(d.on_key(WIN, false, 40, t), None);
        assert_eq!(d.on_key(WIN, true, 50, t), None);
        d.on_key(WIN, false, 60, t);
        d.on_key(CTRL, false, 70, t);
        // Typing with nothing held doesn't disarm anything.
        assert_eq!(d.on_key(OTHER, true, 80, t), None);
        d.on_key(WIN, true, 90, t);
        assert_eq!(d.on_key(CTRL, true, 100, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn chord_recovers_from_missed_releases() {
        let t = Instant::now();
        // Win+L locks the screen: both releases happen on the secure desktop.
        let mut d = HoldDetector::new(2);
        d.on_key(WIN, true, 0, t);
        assert_eq!(d.on_key(OTHER, true, 10, t), None); // L
        // Back at the desktop a minute later: Ctrl, then Win.
        assert_eq!(d.on_key(CTRL, true, 60_000, t), None);
        assert_eq!(d.on_key(WIN, true, 60_100, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(CTRL, false, 61_000, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(WIN, false, 61_010, t), None);

        // During a hold, both releases are missed; a much later press of one key drops the
        // stale hold (Cancel), the other key's press then starts a new one.
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(CTRL, true, 30_000, t), Some(HotkeyEvent::Cancel));
        assert_eq!(d.on_key(WIN, true, 30_050, t), Some(HotkeyEvent::Down(t)));

        // Only one release missed (Win), Ctrl's was seen: re-pressing completes the chord.
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        d.on_key(WIN, true, 10, t);
        assert_eq!(d.on_key(OTHER, true, 20, t), Some(HotkeyEvent::Cancel));
        assert_eq!(d.on_key(CTRL, false, 5_000, t), None);
        assert_eq!(d.on_key(CTRL, true, 9_000, t), None); // Win still believed down
        assert_eq!(d.on_key(WIN, true, 9_100, t), Some(HotkeyEvent::Down(t)));
    }

    #[test]
    fn three_key_chord() {
        let t = Instant::now();
        let mut d = HoldDetector::new(3);
        assert_eq!(d.on_key(Some(2), true, 0, t), None);
        assert_eq!(d.on_key(Some(0), true, 10, t), None);
        assert_eq!(d.on_key(Some(1), true, 20, t), Some(HotkeyEvent::Down(t)));
        assert_eq!(d.on_key(Some(0), false, 500, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(Some(0), true, 600, t), None);
        d.reset();
        // After a reset (hook re-installed), keys must be pressed again.
        assert_eq!(d.on_key(Some(0), true, 700, t), None);
        assert_eq!(d.on_key(Some(1), true, 710, t), None);
        assert_eq!(d.on_key(Some(2), true, 720, t), Some(HotkeyEvent::Down(t)));
    }
}
