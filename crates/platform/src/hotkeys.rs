//! The configured hotkeys: any of them dictates.

use crate::{HoldDetector, Hotkey, HotkeyEvent, KEYS, Key};
use std::{fmt, time::Instant};

/// The configured hotkeys (`[hotkey] key`, one or a list), at least one. None is the same as
/// another or contains another: holding the longer one would start the shorter one's
/// dictation first. Sharing keys is fine (`LeftShift+A` and `LeftShift+B`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkeys(Vec<Hotkey>);

impl Hotkeys {
    /// Parses the file's names in order (see [`Hotkey::parse`]). An entry that isn't a hotkey
    /// fails with [`Hotkey::parse`]'s error; one that breaks the rules with
    /// [`Self::check_new`]'s.
    pub fn parse(specs: &[String]) -> Result<Hotkeys, String> {
        if specs.is_empty() {
            return Err("At least one hotkey is needed.".into());
        }
        let mut hotkeys = Hotkeys(Vec::with_capacity(specs.len()));
        for spec in specs {
            let new = Hotkey::parse(spec)?;
            hotkeys.check_new(&new, None)?;
            hotkeys.0.push(new);
        }
        Ok(hotkeys)
    }

    pub fn list(&self) -> &[Hotkey] {
        &self.0
    }

    /// Whether `new` can join the list, in place of entry `replacing` if given (re-recording
    /// a row): it must not be the same keys as another entry, contain one or be contained in
    /// one. Errors are plain sentences for the settings window, naming the hotkey already in
    /// the list first.
    pub fn check_new(&self, new: &Hotkey, replacing: Option<usize>) -> Result<(), String> {
        for (i, old) in self.0.iter().enumerate() {
            if Some(i) == replacing {
                continue;
            }
            if old.same_keys(new) {
                return Err("That's already a hotkey.".into());
            }
            if new.contains(old) {
                return Err(format!(
                    "{} is already a hotkey, and {} starts with it.",
                    old.label(),
                    new.label()
                ));
            }
            if old.contains(new) {
                return Err(format!(
                    "{} is already a hotkey, and it starts with {}.",
                    old.label(),
                    new.label()
                ));
            }
        }
        Ok(())
    }
}

/// As the file names them, joined with ", " (`Fn, LeftControl+Space`).
impl fmt::Display for Hotkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, hotkey) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{hotkey}")?;
        }
        Ok(())
    }
}

/// Watches every configured hotkey at once, from every key event the hook sees: one
/// [`HoldDetector`] per hotkey, and one hold at a time.
///
/// - A modifier that isn't in a hotkey never reaches its detector: it neither cancels nor
///   blocks that hotkey's hold (Shift + Fn still dictates).
/// - A combo's key that isn't a modifier counts for it only if its press starts the combo's
///   hold: no other hotkey holds, the combo is armed (not waiting for its keys to be
///   released after a hold or a shortcut), its modifiers went down first, and no other
///   modifier is down. Its auto-repeat and release count only if its press did. Otherwise
///   it's just another key: Space alone, Space then Left Control, Left Control + Left Shift +
///   Space, a Space typed after Left Control + A with Left Control still held, and one
///   pressed while another hotkey holds all reach apps. A key that is a whole hotkey on its
///   own always counts.
/// - One hold at a time, judged by the state before each event: while one hotkey is held,
///   another one completing starts nothing, and waits until all its keys are up (as after a
///   hold of its own). Of two completing on the same key, the first in the list holds.
/// - The modifiers down are tracked from the hook's events, and a hook can miss a release
///   (it happened on the secure desktop, say). When an extra modifier is all that keeps a
///   press from completing a combo ([`Self::press_needs_resync`]), the hook asks the OS which
///   modifiers are down ([`Self::resync_modifiers`]) before passing the press on.
///
/// Allocation-free per key event: it sees every keystroke on the system. Platform-independent
/// so it can be unit-tested.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) struct Detector {
    hotkeys: Hotkeys,
    /// One per hotkey, in [`Hotkeys::list`]'s order.
    watches: Vec<Watch>,
    /// The modifiers down, as [`modifier_bit`]s.
    modifiers: u64,
    /// The hotkey held, if any.
    held: Option<usize>,
    /// The last Esc press was reported as `Escape`: its release is kept from the app too.
    escape_down: bool,
}

/// One hotkey, in a [`Detector`].
struct Watch {
    hold: HoldDetector,
    /// Its modifiers, as [`modifier_bit`]s (none for a lone key that isn't a modifier).
    modifiers: u64,
    /// Which of its keys isn't a modifier, if one isn't.
    other: Option<usize>,
    /// What became of that key's last press.
    press: Press,
}

impl Watch {
    /// Whether a fresh press of its key that isn't a modifier (chord key `j`), with
    /// `modifiers` down, counts for it: always for a lone key; for a combo, if it starts the
    /// combo's hold: it completes the combo with no other modifier down, and no other hotkey
    /// holds (`other_holds`: the combo can't start then, and a key that starts nothing isn't
    /// kept from apps).
    fn counts(&self, j: usize, modifiers: u64, other_holds: bool) -> bool {
        self.modifiers == 0
            || (!other_holds && self.modifiers == modifiers && self.hold.would_complete(j))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Press {
    /// Released (or never pressed).
    Up,
    /// It counts for the hotkey: so do its auto-repeat and release, and apps see none of them.
    Counted,
    /// Pressed without exactly the hotkey's modifiers down: any other key, until released.
    Ignored,
}

/// The bit of the modifier with platform key code `code` in a set of modifiers (its place
/// among [`KEYS`]' modifiers, of which there are well under 64), if it's one. The same order
/// as [`Detector::resync_modifiers`].
fn modifier_bit(code: u16) -> Option<u64> {
    KEYS.iter()
        .map(Key::from_entry)
        .filter(Key::is_modifier)
        .position(|k| k.code() == code)
        .map(|i| 1u64 << i)
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl Detector {
    pub(crate) fn new(hotkeys: &Hotkeys) -> Self {
        let watches = hotkeys
            .list()
            .iter()
            .map(|hotkey| Watch {
                hold: HoldDetector::new(hotkey.keys().len()),
                modifiers: hotkey
                    .keys()
                    .iter()
                    .filter_map(|k| modifier_bit(k.code()))
                    .fold(0, |all, bit| all | bit),
                other: hotkey.keys().iter().position(|k| !k.is_modifier()),
                press: Press::Up,
            })
            .collect();
        Detector {
            hotkeys: hotkeys.clone(),
            watches,
            modifiers: 0,
            held: None,
            escape_down: false,
        }
    }

    /// What the hooks call for every key event: the key with platform key code `code` went
    /// `down` (or auto-repeated) or up; `time_ms` as for [`HoldDetector::on_key`]. `esc`: the
    /// key is Esc; `armed`: [`crate::arm_escape`] is on and no modifier is held. An armed Esc
    /// press with no hotkey key down is [`HotkeyEvent::Escape`] (with a hotkey held, Esc is
    /// its `Cancel`).
    ///
    /// Also returns whether to keep the key from the focused app: a counted key that isn't a
    /// modifier, and an Esc reported as `Escape` (each with its auto-repeat and release).
    /// Modifiers always reach apps.
    pub(crate) fn on_key(
        &mut self,
        code: u16,
        down: bool,
        esc: bool,
        armed: bool,
        time_ms: u32,
        now: Instant,
    ) -> (Option<HotkeyEvent>, bool) {
        let modifier = modifier_bit(code);
        if let Some(bit) = modifier {
            if down {
                self.modifiers |= bit;
            } else {
                self.modifiers &= !bit;
            }
        }
        let held = self.held; // before this event
        let mut ev = None;
        let mut keep = false;
        for (i, (hotkey, w)) in self
            .hotkeys
            .list()
            .iter()
            .zip(&mut self.watches)
            .enumerate()
        {
            let chord_key = match hotkey.index_of(code.into()) {
                Some(j) if Some(j) == w.other => {
                    if down && w.press == Press::Up {
                        let other_holds = held.is_some_and(|h| h != i);
                        w.press = if w.counts(j, self.modifiers, other_holds) {
                            Press::Counted
                        } else {
                            Press::Ignored
                        };
                    }
                    let counted = w.press == Press::Counted;
                    if !down {
                        w.press = Press::Up;
                    }
                    keep |= counted;
                    counted.then_some(j)
                }
                Some(j) => Some(j),
                None if modifier.is_some() => continue, // not one of its modifiers
                None => None,
            };
            match w.hold.on_key(chord_key, down, time_ms, now) {
                Some(HotkeyEvent::Down(t, _)) if ev.is_none() && held.is_none_or(|h| h == i) => {
                    self.held = Some(i);
                    ev = Some(HotkeyEvent::Down(t, i));
                }
                Some(HotkeyEvent::Down(..)) => w.hold.rearm(), // one hold at a time
                // `Up` or `Cancel`: only the holder's detector is ever held.
                Some(end) => {
                    self.held = None;
                    ev = Some(end);
                }
                None => {}
            }
        }
        if !esc {
            return (ev, keep);
        }
        if !down {
            return (ev, std::mem::take(&mut self.escape_down));
        }
        if self.escape_down {
            return (ev, true); // its auto-repeat
        }
        if ev.is_none() && armed && self.watches.iter().all(|w| w.hold.idle()) {
            self.escape_down = true;
            return (Some(HotkeyEvent::Escape), true);
        }
        (ev, false)
    }

    /// Whether a press of `code` (call before [`Self::on_key`]) would count for a combo but
    /// for another modifier believed down: that may be a release the hook missed, so the hook
    /// should [`Self::resync_modifiers`] first. Only for a fresh press (not its auto-repeat),
    /// and not when the OS's answer can't make it count (another hotkey holds, the combo is
    /// waiting for its keys to be released, or one of its own modifiers isn't down): the OS is
    /// asked at most once per press, and only on this rare path.
    pub(crate) fn press_needs_resync(&self, code: u16) -> bool {
        self.hotkeys
            .list()
            .iter()
            .zip(&self.watches)
            .enumerate()
            .any(|(i, (hotkey, w))| {
                let other_holds = self.held.is_some_and(|h| h != i);
                w.press == Press::Up
                    && w.modifiers != 0
                    && w.modifiers != self.modifiers
                    && hotkey.index_of(code.into()).is_some_and(|j| {
                        // It would, with exactly its own modifiers down.
                        Some(j) == w.other && w.counts(j, w.modifiers, other_holds)
                    })
            })
    }

    /// Replaces the modifiers believed down with what the OS says: `down` tells, for each
    /// modifier in [`KEYS`], whether it's down, or `None` if the OS can't tell (that one is
    /// left as the hook saw it). Allocation-free.
    pub(crate) fn resync_modifiers(&mut self, down: impl Fn(Key) -> Option<bool>) {
        let modifiers = KEYS.iter().map(Key::from_entry).filter(Key::is_modifier);
        for (i, key) in modifiers.enumerate() {
            match down(key) {
                Some(true) => self.modifiers |= 1 << i,
                Some(false) => self.modifiers &= !(1 << i),
                None => {}
            }
        }
    }

    /// Whether a press of `code` (call before [`Self::on_key`]) is an auto-repeat in a chord
    /// that is held or being released, for any hotkey with that key (see
    /// [`HoldDetector::repeat_in_chord`]).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn repeat_in_chord(&self, code: u16) -> bool {
        self.hotkeys
            .list()
            .iter()
            .zip(&self.watches)
            .any(|(hotkey, w)| {
                hotkey
                    .index_of(code.into())
                    .is_some_and(|j| w.hold.repeat_in_chord(j))
            })
    }

    /// Forget any hold in progress, and which keys are down.
    pub(crate) fn reset(&mut self) {
        for w in &mut self.watches {
            w.hold.reset();
            w.press = Press::Up;
        }
        self.modifiers = 0;
        self.held = None;
    }

    pub(crate) fn hotkeys(&self) -> &Hotkeys {
        &self.hotkeys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HotkeyEvent::{Cancel, Down, Escape, Up};
    use crate::keys::ESC_CODE;
    use std::sync::LazyLock;

    #[test]
    fn list_rules() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Hotkeys::parse(&[]).unwrap_err(),
            "At least one hotkey is needed."
        );
        assert_eq!(
            Hotkeys::parse(&s(&["F13", "f13"])).unwrap_err(),
            "That's already a hotkey."
        );
        // The brief's F13 and F13+A: F13+A is refused (two keys that aren't modifiers).
        assert_eq!(
            Hotkeys::parse(&s(&["LeftShift", "LeftShift+A"])).unwrap_err(),
            "Left Shift is already a hotkey, and Left Shift + A starts with it."
        );
        let ok = Hotkeys::parse(&s(&["LeftShift+A", "LeftShift+B"])).unwrap();
        assert_eq!(ok.to_string(), "LeftShift+A, LeftShift+B");
        assert!(
            ok.check_new(&Hotkey::parse("LeftShift+A").unwrap(), Some(0))
                .is_ok()
        );
        assert!(
            ok.check_new(&Hotkey::parse("LeftShift+A").unwrap(), Some(1))
                .is_err()
        );
    }

    #[test]
    fn list_order_and_errors() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let one = Hotkeys::parse(&s(&["LeftShift+A"])).unwrap();
        assert_eq!(one.list(), [Hotkey::parse("LeftShift+A").unwrap()]);
        assert_eq!(one.to_string(), "LeftShift+A");
        // The one already in the list comes first, whichever is shorter.
        assert_eq!(
            one.check_new(&Hotkey::parse("LeftShift").unwrap(), None)
                .unwrap_err(),
            "Left Shift + A is already a hotkey, and it starts with Left Shift."
        );
        let short = Hotkeys::parse(&s(&["LeftShift"])).unwrap();
        assert_eq!(
            short
                .check_new(&Hotkey::parse("LeftShift+A").unwrap(), None)
                .unwrap_err(),
            "Left Shift is already a hotkey, and Left Shift + A starts with it."
        );
        assert_eq!(
            Hotkeys::parse(&s(&["LeftShift+A", "LeftShift"])).unwrap_err(),
            "Left Shift + A is already a hotkey, and it starts with Left Shift."
        );
        assert_eq!(
            one.check_new(&Hotkey::parse("a+leftshift").unwrap(), None)
                .unwrap_err(),
            "That's already a hotkey."
        );
        // An entry that isn't a hotkey fails with its own error, unchanged.
        assert_eq!(
            Hotkeys::parse(&s(&["F13", "Hyper"])).unwrap_err(),
            Hotkey::parse("Hyper").unwrap_err()
        );
    }

    /// The event time of every test event (`Down` and `Up` carry it).
    static T: LazyLock<Instant> = LazyLock::new(Instant::now);

    /// A third modifier, named differently on each platform.
    #[cfg(target_os = "macos")]
    const CTRL: &str = "LeftControl";
    #[cfg(not(target_os = "macos"))]
    const CTRL: &str = "LeftCtrl";

    fn detector(names: &[&str]) -> Detector {
        let specs: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        Detector::new(&Hotkeys::parse(&specs).unwrap())
    }

    fn code(name: &str) -> u16 {
        Key::parse(name).unwrap().code()
    }

    /// The key named `name` pressed (or auto-repeated) or released, Esc disarmed.
    fn press(d: &mut Detector, name: &str, down: bool) -> (Option<HotkeyEvent>, bool) {
        d.on_key(code(name), down, false, false, 0, *T)
    }

    fn esc(d: &mut Detector, armed: bool, down: bool) -> (Option<HotkeyEvent>, bool) {
        d.on_key(ESC_CODE, down, true, armed, 0, *T)
    }

    #[test]
    fn second_hotkey_ignored_while_first_held() {
        let mut d = detector(&["LeftShift", "RightShift"]);
        assert_eq!(d.hotkeys().to_string(), "LeftShift, RightShift");
        assert_eq!(press(&mut d, "LeftShift", true), (Some(Down(*T, 0)), false));
        assert_eq!(press(&mut d, "RightShift", true), (None, false));
        assert_eq!(press(&mut d, "LeftShift", false), (Some(Up(*T)), false));
        // Right Shift went down during the other hold: it starts nothing until let go.
        assert_eq!(press(&mut d, "RightShift", false), (None, false));
        assert_eq!(
            press(&mut d, "RightShift", true),
            (Some(Down(*T, 1)), false)
        );
        assert_eq!(press(&mut d, "RightShift", false), (Some(Up(*T)), false));
    }

    #[test]
    fn hotkeys_completing_on_the_same_key_hold_the_first() {
        let both = format!("LeftShift+{CTRL}");
        let mut d = detector(&["LeftShift+RightShift", &both]);
        assert_eq!(press(&mut d, "RightShift", true), (None, false));
        assert_eq!(press(&mut d, CTRL, true), (None, false));
        assert_eq!(press(&mut d, "LeftShift", true), (Some(Down(*T, 0)), false));
        assert_eq!(press(&mut d, "RightShift", false), (Some(Up(*T)), false));
        // The second one waits until its own keys are up.
        assert_eq!(press(&mut d, CTRL, false), (None, false));
        assert_eq!(press(&mut d, CTRL, true), (None, false));
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        assert_eq!(press(&mut d, CTRL, false), (None, false));
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, CTRL, true), (Some(Down(*T, 1)), false));
    }

    #[test]
    fn combo_needs_exact_modifiers_first() {
        let mut d = detector(&["LeftShift+Space"]);
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 0)), true));
        assert_eq!(press(&mut d, "Space", true), (None, true)); // auto-repeat
        assert_eq!(press(&mut d, "Space", false), (Some(Up(*T)), true));
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        // Space first types a space, and neither Left Shift after it nor Space's auto-repeat
        // then starts anything.
        assert_eq!(press(&mut d, "Space", true), (None, false));
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (None, false));
        assert_eq!(press(&mut d, "Space", false), (None, false));
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        // Another modifier down too: a shortcut, not the hotkey.
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "RightShift", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (None, false));
        assert_eq!(press(&mut d, "Space", false), (None, false));
        assert_eq!(press(&mut d, "RightShift", false), (None, false));
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        // Then it works again.
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 0)), true));
    }

    #[test]
    fn disarmed_combo_key_reaches_apps() {
        let mut d = detector(&["LeftShift+Space"]);
        // Shift+A types a capital and disarms the combo until its keys are up: the space typed
        // next, Left Shift still held, is the app's.
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "A", true), (None, false));
        assert_eq!(press(&mut d, "A", false), (None, false));
        assert_eq!(press(&mut d, "Space", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (None, false)); // auto-repeat
        assert_eq!(press(&mut d, "Space", false), (None, false));
        // Once everything is up, the combo works again.
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 0)), true));
    }

    #[test]
    fn resync_drops_a_stale_modifier() {
        let space = code("Space");
        let stale = || {
            // Right Shift's release was missed (it happened on the secure desktop, say).
            let mut d = detector(&["LeftShift+Space"]);
            press(&mut d, "RightShift", true);
            press(&mut d, "LeftShift", true);
            d
        };
        // As an extra modifier, it blocks the combo: Space types.
        let mut d = stale();
        assert!(d.press_needs_resync(space));
        assert_eq!(press(&mut d, "Space", true), (None, false));
        assert!(
            !d.press_needs_resync(space),
            "nor is its auto-repeat judged again"
        );
        // So the hook asks the OS before passing the press on (`None`: what the hook saw
        // stands): only Left Shift is down, and the same press counts.
        let mut d = stale();
        d.resync_modifiers(|k| (k.name != "LeftShift").then_some(false));
        assert!(!d.press_needs_resync(space));
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 0)), true));
        // A modifier that really is down still blocks it.
        let mut d = stale();
        d.resync_modifiers(|k| Some(k.name.ends_with("Shift")));
        assert!(d.press_needs_resync(space));
        assert_eq!(press(&mut d, "Space", true), (None, false));
        // Nothing to ask when the press counts anyway, or isn't a combo's.
        let mut d = detector(&["LeftShift+Space", "F13"]);
        press(&mut d, "LeftShift", true);
        assert!(!d.press_needs_resync(space));
        assert!(!d.press_needs_resync(code("F13")));
        assert!(!d.press_needs_resync(code("A")));
        // Nor while the combo waits for its keys to be released (Shift+A, then Space): the
        // OS can't make that press count.
        press(&mut d, "RightShift", true);
        assert!(d.press_needs_resync(space));
        press(&mut d, "A", true);
        assert!(!d.press_needs_resync(space));
    }

    #[test]
    fn lone_regular_key_is_kept() {
        let mut d = detector(&["A"]);
        assert_eq!(press(&mut d, "A", true), (Some(Down(*T, 0)), true));
        assert_eq!(press(&mut d, "A", true), (None, true)); // auto-repeat
        assert_eq!(press(&mut d, "A", false), (Some(Up(*T)), true));
        // Whatever modifiers are down.
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "A", true), (Some(Down(*T, 0)), true));
        assert_eq!(press(&mut d, "A", false), (Some(Up(*T)), true));
        // Only in a combo, A alone types.
        let mut d = detector(&["LeftShift+A"]);
        assert_eq!(press(&mut d, "A", true), (None, false));
        assert_eq!(press(&mut d, "A", false), (None, false));
    }

    #[test]
    fn modifier_only_hotkey_ignores_other_modifiers() {
        let mut d = detector(&["RightShift"]);
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(
            press(&mut d, "RightShift", true),
            (Some(Down(*T, 0)), false)
        );
        assert_eq!(press(&mut d, "LeftShift", false), (None, false)); // no Cancel either
        assert_eq!(press(&mut d, "RightShift", false), (Some(Up(*T)), false));
        // Every modifier has a bit in a `u64` set.
        assert!(
            KEYS.iter()
                .map(Key::from_entry)
                .filter(Key::is_modifier)
                .count()
                <= 64
        );
    }

    #[test]
    fn shared_modifier_combos() {
        let mut d = detector(&["LeftShift+F13", "LeftShift+F14"]);
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "F13", true), (Some(Down(*T, 0)), true));
        // Another key while holding cancels, and reaches the app: to the other combo, F13 was
        // another key with its Left Shift down (a shortcut), so it waits until its keys are up
        // and F14 doesn't count for it either.
        assert_eq!(press(&mut d, "F14", true), (Some(Cancel), false));
        assert_eq!(press(&mut d, "F13", false), (None, true));
        assert_eq!(press(&mut d, "F14", false), (None, false));
        // Left Shift still down: still waiting.
        assert_eq!(press(&mut d, "F14", true), (None, false));
        assert_eq!(press(&mut d, "F14", false), (None, false));
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "F14", true), (Some(Down(*T, 1)), true));
    }

    #[test]
    fn combo_key_during_another_hold_reaches_apps() {
        let space = code("Space");
        let mut d = detector(&["F13", "LeftShift+Space"]);
        assert_eq!(press(&mut d, "F13", true), (Some(Down(*T, 0)), true));
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        // The combo can't start while F13 holds: its Space cancels that hold, and types.
        press(&mut d, "RightShift", true);
        assert!(!d.press_needs_resync(space), "no point asking the OS");
        press(&mut d, "RightShift", false);
        assert_eq!(press(&mut d, "Space", true), (Some(Cancel), false));
        assert_eq!(press(&mut d, "Space", false), (None, false));
        assert_eq!(press(&mut d, "F13", false), (None, true));
        assert_eq!(press(&mut d, "LeftShift", false), (None, false));
        // A key that is a whole hotkey on its own is kept even while another hotkey holds.
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 1)), true));
        assert_eq!(press(&mut d, "F13", true), (Some(Cancel), true));
        assert_eq!(press(&mut d, "F13", false), (None, true));
    }

    #[test]
    fn escape_only_when_idle() {
        let mut d = detector(&["F13"]);
        // Disarmed (not hands-free, or a modifier held): Esc is the app's.
        assert_eq!(esc(&mut d, false, true), (None, false));
        assert_eq!(esc(&mut d, false, false), (None, false));
        // Armed with nothing down: ends a hands-free recording, and the app sees neither the
        // press, its auto-repeat nor its release (even if disarmed in between).
        assert_eq!(esc(&mut d, true, true), (Some(Escape), true));
        assert_eq!(esc(&mut d, false, true), (None, true));
        assert_eq!(esc(&mut d, false, false), (None, true));
        // Esc while a hotkey is held is the hold's cancel, not an Escape too.
        assert_eq!(press(&mut d, "F13", true), (Some(Down(*T, 0)), true));
        assert_eq!(esc(&mut d, true, true), (Some(Cancel), false));
        assert_eq!(esc(&mut d, true, false), (None, false));
        assert_eq!(
            esc(&mut d, true, true),
            (None, false),
            "still waiting for the hotkey's release"
        );
        assert_eq!(press(&mut d, "F13", false), (None, true));
        assert_eq!(esc(&mut d, true, true), (Some(Escape), true));
        assert_eq!(esc(&mut d, true, false), (None, true));
        // Other keys are never kept.
        assert_eq!(d.on_key(code("A"), true, false, true, 0, *T), (None, false));
        // Part of a combo down: a shortcut, not an Escape.
        let mut d = detector(&["LeftShift+RightShift"]);
        assert_eq!(press(&mut d, "LeftShift", true), (None, false));
        assert_eq!(esc(&mut d, true, true), (None, false));
    }

    #[test]
    fn reset_forgets_holds_and_modifiers() {
        let mut d = detector(&["LeftShift+Space"]);
        press(&mut d, "LeftShift", true);
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 0)), true));
        d.reset();
        assert_eq!(press(&mut d, "Space", false), (None, false));
        // Left Shift, still held, has to be pressed again.
        assert_eq!(press(&mut d, "Space", true), (None, false));
        press(&mut d, "Space", false);
        press(&mut d, "LeftShift", false);
        press(&mut d, "LeftShift", true);
        assert_eq!(press(&mut d, "Space", true), (Some(Down(*T, 0)), true));
    }

    #[test]
    fn repeat_in_chord_asks_every_hotkey_with_the_key() {
        let mut d = detector(&["F13", "LeftShift+RightShift"]);
        let right = code("RightShift");
        press(&mut d, "RightShift", true);
        assert!(!d.repeat_in_chord(right)); // held alone
        assert_eq!(press(&mut d, "LeftShift", true), (Some(Down(*T, 1)), false));
        assert!(d.repeat_in_chord(right));
        assert!(!d.repeat_in_chord(code("F13")));
        press(&mut d, "LeftShift", false);
        assert!(d.repeat_in_chord(right)); // being released
    }
}
