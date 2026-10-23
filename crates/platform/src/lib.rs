//! OS integration: the hold-to-talk global hotkey, clipboard paste, and (macOS) the privacy
//! permissions they need.
//!
//! - Windows: low-level keyboard hook, Win32 clipboard, `SendInput`.
//! - macOS: `CGEventTap`, `NSPasteboard`, `CGEventPost`; Microphone and Accessibility
//!   permissions (see [`missing_permissions`]).
//! - Recording a hotkey for the settings window ([`capture_keys`]): the keys pressed, kept from
//!   every other app and from AGT's own hotkey listener.
//! - Ducking other audio while dictating ([`Ducker`]): the default output device's volume
//!   (Windows: `IAudioEndpointVolume`; macOS: the CoreAudio HAL).
//! - Starting AGT at login ([`set_launch_at_login`]; Windows: the `Run` key; macOS:
//!   `SMAppService`).
//! - Anything else returns [`Error`]s saying it's unsupported.

use std::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
use unsupported as imp;

mod duck;
pub use duck::{Ducked, Ducker};

mod hotkeys;
pub use hotkeys::Hotkeys;

mod capture;
pub use capture::{Capture, CaptureHandle, capture_keys};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// What the hotkey thread reports. Delivered from the hook thread: handlers must not block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// Hotkey `.1` (its index in [`Hotkeys::list`]) went down, every key of it (auto-repeat
    /// is filtered out). Only one hotkey is held at a time. Can also arrive while a hold is
    /// still believed active, if its release was never seen (e.g. it happened on the secure
    /// desktop): the receiver should then drop the stale hold and start over.
    Down(Instant, usize),
    /// A key of the held hotkey was released after its `Down` (and no `Cancel`).
    Up(Instant),
    /// Another key was pressed while holding the hotkey (Esc, or a shortcut such as Ctrl+C),
    /// or the hold turned out to be stale (a chord key was pressed again long after its
    /// release was missed): the hold is not a dictation. No `Up` follows.
    Cancel,
    /// Esc (no modifier) was pressed while no hotkey was held (with one held, that's a
    /// `Cancel`) and [`arm_escape`] was on: ends a hands-free recording. The focused app
    /// doesn't see that Esc.
    Escape,
}

mod keys;
#[cfg(target_os = "macos")]
use keys::CAPS_LOCK_CODE;
pub use keys::KEYS;
#[cfg(not(target_os = "macos"))]
use keys::PAUSE_CODE;
use keys::{
    CHARACTER_MODIFIERS, CHORD_EXAMPLE, ESC_CODE, EVERYDAY_MODIFIERS, MENU_KEYS, START_KEYS,
    is_spare_f_key, key_names, types_character,
};

/// A single key, by name. See [`KEYS`]. Each configured hotkey is a [`Hotkey`]: one key or a
/// chord of several.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub name: &'static str,
    label: &'static str,
    /// Platform key code: Windows virtual-key code, or macOS virtual key code (`kVK_*`).
    code: u16,
    kind: KeyKind,
}

/// How a hotkey key behaves. Platform-independent: each platform's [`KEYS`] table says which
/// of its keys is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// A modifier passed through to apps that does nothing when held alone (Ctrl, Shift; on
    /// macOS also Fn, Option and Command).
    Modifier,
    /// A modifier passed through to apps that the OS reacts to when pressed and released
    /// alone (Windows: Win opens the Start menu, Alt the focused app's menu bar): while it's
    /// held as the hotkey, AGT injects a mask key so that release does nothing.
    Masked,
    /// Not a modifier, so passing it through would act in the focused app: the hook keeps it
    /// from apps when it's the hotkey's (F13-F24, letters, Space...). At most one per chord.
    Swallowed,
}

use KeyKind::Swallowed;

impl Key {
    /// The key named `name` in [`KEYS`] (case-insensitive), as the config file has it.
    pub fn parse(name: &str) -> Result<Self, String> {
        KEYS.iter()
            .find(|(n, ..)| n.eq_ignore_ascii_case(name.trim()))
            .map(Self::from_entry)
            .ok_or_else(|| {
                format!(
                    "unknown key {:?}; expected one of {}",
                    name.trim(),
                    key_names()
                )
            })
    }

    /// The key with platform key code `code` (as a keyboard hook sees it), if a hotkey can
    /// use it.
    pub fn from_code(code: u16) -> Option<Self> {
        KEYS.iter()
            .find(|(.., c, _)| *c == code)
            .map(Self::from_entry)
    }

    fn from_entry(&(name, label, code, kind): &(&'static str, &'static str, u16, KeyKind)) -> Self {
        Key {
            name,
            label,
            code,
            kind,
        }
    }

    /// The key in plain words, as the settings window shows it ("Left Option", "Space", "/").
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Platform key code: Windows virtual-key code, or macOS virtual key code (`kVK_*`).
    pub fn code(&self) -> u16 {
        self.code
    }

    pub fn kind(&self) -> KeyKind {
        self.kind
    }

    /// Whether it's a modifier ([`KeyKind::Modifier`] or [`KeyKind::Masked`]): passed through
    /// to apps, and a chord can have any number of them.
    pub fn is_modifier(&self) -> bool {
        matches!(self.kind, KeyKind::Modifier | KeyKind::Masked)
    }

    /// Whether the hook swallows this key: it's not a modifier, so passing it through would
    /// act in the focused app.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn swallowed(&self) -> bool {
        self.kind == Swallowed
    }
}

/// Why the key with platform code `code`, not in [`KEYS`], can't be part of a hotkey.
fn unusable_key(code: u16) -> &'static str {
    #[cfg(target_os = "macos")]
    if code == CAPS_LOCK_CODE {
        return "macOS doesn't say when Caps Lock is let go, so it can't be held as a hotkey.";
    }
    #[cfg(not(target_os = "macos"))]
    if code == PAUSE_CODE {
        return "Windows sends Pause's press and release together, so it can't be held as a hotkey.";
    }
    if code == ESC_CODE {
        "Esc cancels a recording, so it can't be a hotkey."
    } else {
        "AGT doesn't know that key."
    }
}

/// The most keys a chord may have (e.g. Left Control + Left Option + Left Command + Space).
pub const MAX_CHORD: usize = 4;

/// A configured hotkey (see [`Hotkeys`]): one key, or a chord of up to [`MAX_CHORD`] keys
/// (e.g. `LeftCtrl+LeftWin`) that must all be held together. A dictation starts once every
/// key is down and ends as soon as any of them is released. A hotkey made only of modifiers
/// starts in whatever order they go down; a combo with a key that isn't a modifier
/// (`LeftShift+Space`) needs exactly its modifiers down first, then that key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    keys: Vec<Key>,
}

impl Hotkey {
    /// Parses `"RightCtrl"` or a `+`-separated chord such as `"LeftCtrl+LeftWin"`, as the
    /// config file has it (names are case-insensitive; left and right keys are distinct). The
    /// rules are [`Self::from_codes`]'s; errors start with `invalid hotkey "<spec>": `.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let invalid = |e: String| format!("invalid hotkey {spec:?}: {e}");
        let names: Vec<&str> = spec.split('+').map(str::trim).collect();
        if names.iter().any(|n| n.is_empty()) {
            return Err(invalid(format!(
                "expected a key name or a chord like \"{CHORD_EXAMPLE}\" (keys: {})",
                key_names()
            )));
        }
        let keys = names
            .into_iter()
            .map(Key::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(invalid)?;
        Self::from_keys(keys).map_err(invalid)
    }

    /// The hotkey made of the keys with platform key codes `codes` (as the key recorder saw
    /// them go down, in that order). A chord has 1 to [`MAX_CHORD`] distinct keys, at most one
    /// of which isn't a modifier; Esc and keys missing from [`KEYS`] can't be in one. Errors
    /// are plain sentences for the settings window. The modifiers come first, in the order they
    /// went down, then the key that isn't one: Space then Left Control is "Left Control +
    /// Space".
    pub fn from_codes(codes: &[u16]) -> Result<Self, String> {
        let (mut keys, others): (Vec<_>, Vec<_>) = codes
            .iter()
            .map(|&code| Key::from_code(code).ok_or_else(|| unusable_key(code).to_owned()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .partition(Key::is_modifier);
        keys.extend(others);
        Self::from_keys(keys)
    }

    /// The rules every hotkey follows, wherever it comes from.
    fn from_keys(keys: Vec<Key>) -> Result<Self, String> {
        if keys.is_empty() {
            return Err("No key was pressed.".into());
        }
        if keys.len() > MAX_CHORD {
            return Err(format!("A hotkey can have at most {MAX_CHORD} keys."));
        }
        for (i, key) in keys.iter().enumerate() {
            if keys[..i].contains(key) {
                return Err(format!("{} is listed twice.", key.label));
            }
        }
        let mut others = keys.iter().filter(|k| !k.is_modifier());
        if let (Some(a), Some(b)) = (others.next(), others.next()) {
            return Err(format!(
                "A hotkey can have only one key that isn't a modifier ({}, {}).",
                a.label, b.label
            ));
        }
        Ok(Hotkey { keys })
    }

    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// The hotkey in plain words, its keys' labels joined with " + " ("Left Control + Space").
    pub fn label(&self) -> String {
        self.keys
            .iter()
            .map(|k| k.label)
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// What the settings window says under this hotkey, if it costs something (one text, the
    /// most specific):
    ///
    /// - A key that isn't a modifier is kept from apps (F13-F24 cost nothing: nothing types
    ///   them): alone, always; in a combo, whenever the combo is held. In a combo that takes
    ///   over a character when the key types one and its modifiers only pick which (Shift,
    ///   Option, AltGr), and an app's shortcut otherwise (Shift + Tab, Option + Left Arrow).
    /// - Alone, a Windows Win or Alt key no longer opens its menu when tapped (AGT masks the
    ///   tap), and a modifier used in everyday shortcuts and typing starts AGT listening every
    ///   time it's held.
    /// - A combo of only modifiers costs nothing: they say they're not typing.
    pub fn warning(&self) -> Option<&'static str> {
        let Some(key) = self.keys.iter().find(|k| !k.is_modifier()) else {
            let [key] = self.keys.as_slice() else {
                return None;
            };
            return if START_KEYS.contains(&key.name) {
                Some("A tap of this key no longer opens Start while AGT runs.")
            } else if MENU_KEYS.contains(&key.name) {
                Some("A tap of this key no longer opens an app's menu while AGT runs.")
            } else {
                EVERYDAY_MODIFIERS.contains(&key.name).then_some(
                    "You use this key in shortcuts and typing. AGT starts listening each time you hold it.",
                )
            };
        };
        if is_spare_f_key(key.name) {
            None
        } else if self.keys.len() == 1 {
            Some("AGT takes this key over: it won't type in other apps while AGT runs.")
        } else if types_character(key.name)
            && self
                .keys
                .iter()
                .filter(|k| k.is_modifier())
                .all(|k| CHARACTER_MODIFIERS.contains(&k.name))
        {
            Some("This types a character, so AGT takes it over in every app while AGT runs.")
        } else {
            Some("Apps won't see this shortcut while AGT runs.")
        }
    }

    /// Whether `other` is made of the same keys, in whatever order.
    pub fn same_keys(&self, other: &Hotkey) -> bool {
        self.keys.len() == other.keys.len() && self.contains(other)
    }

    /// Whether every key of `other` is in this hotkey too, so holding this one holds `other`
    /// as well (true for the same keys).
    pub fn contains(&self, other: &Hotkey) -> bool {
        other.keys.iter().all(|k| self.keys.contains(k))
    }

    /// Which chord key the platform key `code` is, if any.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn index_of(&self, code: u32) -> Option<usize> {
        self.keys.iter().position(|k| k.code as u32 == code)
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

/// A privacy permission AGT needs (macOS only; on Windows nothing is ever missing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Recording (without it, macOS delivers silence).
    Microphone,
    /// Seeing the hotkeys (the keyboard event tap), sending Cmd+V and swallowing hotkey keys.
    /// (With it, Input Monitoring isn't needed.) Listed as "Device Control" in System Settings
    /// from macOS 27 on.
    Accessibility,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Permission::Microphone => "Microphone",
            Permission::Accessibility => imp::accessibility_name(),
        })
    }
}

/// The permissions AGT doesn't have (yet). Cheap: fine to poll every second or so.
pub fn missing_permissions() -> Vec<Permission> {
    imp::missing_permissions()
}

/// Asks the user for `missing` (macOS shows its permission prompts; each one only the first
/// time it's asked). Returns at once; poll [`missing_permissions`] to see them granted.
pub fn request_permissions(missing: &[Permission]) {
    imp::request_permissions(missing)
}

/// Opens the system settings page where `permission` is granted.
pub fn open_permission_settings(permission: Permission) {
    imp::open_permission_settings(permission)
}

/// What [`set_launch_at_login`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItem {
    /// It already was as asked.
    Unchanged,
    Registered,
    Unregistered,
    /// macOS: registered, but turned off in System Settings > General > Login Items (or not
    /// approved there yet). Left as the user set it.
    NeedsApproval,
}

/// Makes the OS start AGT when the user logs in, or not, writing only if that changes.
/// Windows: the `AGT` value of HKCU's `Run` key, starting `program`; a program the user turned
/// off in Task Manager's Startup apps stays off (that's kept elsewhere, which this leaves
/// alone). macOS: the app bundle AGT runs from, as a login item (`SMAppService`); `program`
/// is unused.
pub fn set_launch_at_login(on: bool, program: &std::path::Path) -> Result<LoginItem, Error> {
    imp::set_launch_at_login(on, program)
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

/// Starts watching `hotkeys` on a dedicated, high-priority thread: any of them dictates, one
/// hold at a time. `on_event` runs on that thread and must return quickly (e.g. just send on
/// a channel): a slow handler delays every keystroke on the system. Modifier keys are passed
/// through; a hotkey's key that isn't a modifier is swallowed while it counts for that
/// hotkey (always for a key that is a whole hotkey on its own; for a combo's, only when its
/// press completes the combo, with the combo's modifiers and no others held first).
///
/// macOS: if Accessibility isn't granted yet, this still succeeds; the thread waits for the
/// permission and starts watching as soon as it's granted (no restart needed).
pub fn start_hotkey(
    hotkeys: &Hotkeys,
    on_event: impl Fn(HotkeyEvent) + Send + 'static,
) -> Result<HotkeyHandle, Error> {
    imp::start_hotkey(hotkeys.clone(), Box::new(on_event)).map(HotkeyHandle)
}

/// Whether a key of any of `hotkeys` is [`KeyKind::Masked`] (Windows: Win, Alt), so the hook
/// needs its masking thread.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn needs_masker(hotkeys: &Hotkeys) -> bool {
    hotkeys
        .list()
        .iter()
        .flat_map(Hotkey::keys)
        .any(|k| k.kind == KeyKind::Masked)
}

/// Whether `key` is physically down right now, if that can be known: `None` on unsupported
/// platforms, and for a swallowed key on Windows unless it got past the hook (its state
/// reaches the system only then).
pub fn key_down(key: Key) -> Option<bool> {
    imp::key_down(key)
}

/// [`key_down`] for a press that got past the hook (`key_down` said it was down): on
/// Windows, a swallowed key's "up" then counts too, since the hook lets that press's
/// release through to the system.
pub fn key_down_past_hook(key: Key) -> Option<bool> {
    imp::key_down_past_hook(key)
}

/// Identifies the window that has keyboard focus (where a paste would go), if known. Only
/// meant for comparing: "is it still the same window?".
pub fn foreground_window() -> Option<usize> {
    imp::foreground_window()
}

/// Identifies the app that has focus, for the dictionary's app profiles: on Windows the file
/// name of the foreground window's program (`Code.exe`), on macOS the frontmost app's bundle
/// identifier (`com.microsoft.VSCode`). `None` if there's none or it can't be told. Doesn't
/// talk to the app: microseconds (it runs at key-down).
pub fn foreground_app() -> Option<String> {
    imp::foreground_app()
}

/// Whether keyboard focus is in something that takes typed text (a text field or area, an
/// editable part of a web page), so a paste has somewhere to land. `Some(false)` only when
/// focus is clearly elsewhere (a web page, a link, a button, a list, the Finder's desktop).
/// `None` if that can't be told (an app that draws its own text, no permission, the app didn't
/// answer in time, a platform without the check): treat it as yes. Asks the focused app, a
/// quarter second at most per request.
pub fn focus_takes_text() -> Option<bool> {
    imp::focus_takes_text()
}

/// Gets the frontmost app ready for [`focus_takes_text`]: Chromium-based apps (Slack, VS Code,
/// Notion...) only describe their contents to assistive apps that ask for it, and building
/// that takes them a moment. Call it well before the check (at key down). Asks each app once
/// (the switch stays on until it quits). Ignored by others.
pub fn prepare_focus_check() {
    imp::prepare_focus_check()
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

/// Call between [`set_clipboard_text`] and [`send_paste`]. Windows: clipboard listeners
/// (Logi Options+, WSLg, clipboard managers) open the clipboard right after every change, and
/// an app whose paste finds it open pastes nothing, so this waits until they're likely done
/// (a few ms). Returns how long it waited; `None` where nothing needs waiting for (macOS).
pub fn wait_for_clipboard_listeners() -> Option<Duration> {
    imp::wait_for_clipboard_listeners()
}

/// Sends Ctrl+V to the focused window (Windows: releasing held Shift/Alt/Win first, so e.g. a
/// Win key still held from a `LeftCtrl+LeftWin` chord doesn't turn it into Win+Ctrl+V; macOS:
/// Cmd+V with no other modifier, whatever is held). Returns once the keystrokes are queued;
/// the target app reads the clipboard asynchronously, so wait a bit before restoring it.
/// Fails e.g. when the focused app runs elevated (Windows) or without the Accessibility
/// permission (macOS).
///
/// To avoid that release altogether, wait until the hotkey's keys are up first (see
/// [`paste_may_release`]).
pub fn send_paste() -> Result<(), Error> {
    // Counted before the keystrokes go out, so a poll can never see a fake release that
    // isn't counted yet.
    imp::send_paste(|| {
        PASTES.fetch_add(1, Ordering::SeqCst);
    })
}

static PASTES: AtomicU64 = AtomicU64::new(0);

static ESCAPE_ARMED: AtomicBool = AtomicBool::new(false);

/// While armed, an Esc press with no modifier and no hotkey key down is reported as
/// [`HotkeyEvent::Escape`] and kept from the focused app (macOS: if the event tap could be an
/// active one, i.e. with Accessibility). Disarmed, Esc is left alone.
pub fn arm_escape(armed: bool) {
    ESCAPE_ARMED.store(armed, Ordering::Relaxed);
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn escape_armed() -> bool {
    ESCAPE_ARMED.load(Ordering::Relaxed)
}

/// How many times [`send_paste`] released a key that was held down (and so could have made a
/// hotkey key look released). With [`paste_may_release`], lets a caller tell a real release
/// of the hotkey from our own injected key-ups.
pub fn paste_count() -> u64 {
    PASTES.load(Ordering::SeqCst)
}

/// Whether [`send_paste`] can make `key` *look* released to [`key_down`] while it is still
/// physically held (Windows: it injects key-ups for held Shift, Alt and Win keys, and a
/// Ctrl-up that maps to Left Ctrl; Right Ctrl is never touched. macOS: Left Command and V,
/// whose key-ups Cmd+V posts at the level `key_down` reads).
pub fn paste_may_release(key: Key) -> bool {
    imp::paste_may_release(key)
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
/// disarm: they only matter together with a non-modifier key, which does). Its `Down`s say
/// hotkey 0: [`Detector`](hotkeys::Detector), which runs one per hotkey, puts in the right
/// one. Platform-independent so it can be unit-tested.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
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

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
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

    /// Whether a press of chord key `i` would start the hold now: the chord is armed (not
    /// waiting for its keys to be released) and every other chord key is down.
    pub(crate) fn would_complete(&self, i: usize) -> bool {
        self.state == Hold::Idle && (self.down | (1 << i)) == (1u8 << self.len) - 1
    }

    /// `chord_key`: which chord key this transition is for (`None`: another key); `down`:
    /// press (incl. auto-repeat) or release; `time_ms`: the event's timestamp in ms, wrapping
    /// (`KBDLLHOOKSTRUCT::time`, or our own clock on macOS).
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
                return Some(HotkeyEvent::Down(now, 0));
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
            Some(HotkeyEvent::Down(now, 0))
        } else {
            was_held.then_some(HotkeyEvent::Cancel)
        }
    }

    /// Whether a press of chord key `i` (call before [`Self::on_key`]) is an auto-repeat while
    /// the chord is held or being released: the key is already down and the chord completed
    /// (or was disarmed) without all its keys having been released since. False for a key
    /// held on its own (e.g. a lone Win), the chord never having been complete.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn repeat_in_chord(&self, i: usize) -> bool {
        self.down & (1 << i) != 0 && self.state != Hold::Idle
    }

    /// Whether no chord key is down (so there's no hold, nor a wait for one to end).
    pub(crate) fn idle(&self) -> bool {
        self.state == Hold::Idle && self.down == 0
    }

    /// Drops the hold that just started: another hotkey holds. Like a hold that ended, it
    /// waits until every chord key is released.
    pub(crate) fn rearm(&mut self) {
        self.state = Hold::Rearm;
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
    #[cfg(not(target_os = "macos"))]
    use KeyKind::Masked;
    #[cfg(target_os = "macos")]
    use KeyKind::Modifier;

    const HOT: Option<usize> = Some(0);
    const OTHER: Option<usize> = None;

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_keys_macos() {
        for (name, kind) in [
            ("Fn", Modifier),
            ("RightOption", Modifier),
            ("LeftOption", Modifier),
            ("LeftCommand", Modifier),
            ("LeftControl", Modifier),
            ("RightShift", Modifier),
            ("F13", Swallowed),
            ("F5", Swallowed),
            ("A", Swallowed),
            ("Space", Swallowed),
        ] {
            assert_eq!(Key::parse(name).unwrap().kind(), kind, "{name}");
        }
        assert_eq!(Key::parse("leftoption").unwrap().label(), "Left Option");
        assert_eq!(Key::from_code(0x3F).unwrap().name, "Fn");
        assert!(Key::parse("CapsLock").is_err());
        assert_eq!(
            Hotkey::from_codes(&[0x39]).unwrap_err(),
            "macOS doesn't say when Caps Lock is let go, so it can't be held as a hotkey."
        );
        assert_eq!(
            Hotkey::from_codes(&[0x35]).unwrap_err(),
            "Esc cancels a recording, so it can't be a hotkey."
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_old_configs_parse() {
        for old in [
            "RightCtrl",
            "LeftCtrl+LeftWin",
            "LeftWin+LeftShift",
            "RightCtrl+F13",
            "LeftCtrl+LeftShift+LeftWin",
            "F24",
        ] {
            assert!(Hotkey::parse(old).is_ok(), "{old}");
        }
        assert!(Hotkey::parse("LeftWin").is_ok());
        assert_eq!(Key::parse("LeftAlt").unwrap().kind(), Masked);
        assert_eq!(Key::parse("LeftWin").unwrap().kind(), Masked);
        assert_eq!(Key::parse("CapsLock").unwrap().kind(), Swallowed);
        assert!(Key::parse("Pause").is_err());
        assert_eq!(
            Hotkey::from_codes(&[0x13]).unwrap_err(),
            "Windows sends Pause's press and release together, so it can't be held as a hotkey."
        );
        assert_eq!(
            Hotkey::from_codes(&[0x1B]).unwrap_err(),
            "Esc cancels a recording, so it can't be a hotkey."
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn masked_keys_need_masker() {
        let hk = |v: &[&str]| {
            Hotkeys::parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
        };
        assert!(needs_masker(&hk(&["LeftCtrl+LeftWin"])));
        assert!(needs_masker(&hk(&["RightCtrl", "LeftAlt"])));
        assert!(!needs_masker(&hk(&["RightCtrl", "F13"])));
    }

    #[test]
    fn hotkey_rules() {
        let err = |s: &str| Hotkey::parse(s).unwrap_err();
        // Platform-neutral names: A, Space, F13, F14 exist on both.
        assert!(Hotkey::parse("A").is_ok());
        assert!(err("A+Space").contains("only one key that isn't a modifier (A, Space)"));
        assert!(err("F13+F14+A+Space+F15").contains("at most 4 keys"));
        assert!(err("A+a").contains("twice"));
        assert!(err("a+F13").contains("(A, F13)"));
        // The config file's own mistakes.
        for empty in ["", "LeftShift+", "LeftShift++A"] {
            assert!(err(empty).contains("chord like"), "{empty:?}");
        }
        assert!(err("LeftShift+Hyper").contains("unknown key \"Hyper\"; expected one of"));
        assert!(err("Hyper").contains("LeftShift"), "lists valid names");
    }

    const TAKEN: Option<&str> =
        Some("AGT takes this key over: it won't type in other apps while AGT runs.");
    const EVERYDAY: Option<&str> = Some(
        "You use this key in shortcuts and typing. AGT starts listening each time you hold it.",
    );
    const TYPES: Option<&str> =
        Some("This types a character, so AGT takes it over in every app while AGT runs.");
    const SHORTCUT: Option<&str> = Some("Apps won't see this shortcut while AGT runs.");

    #[test]
    fn labels_and_warnings() {
        let lone = Hotkey::parse("Space").unwrap();
        assert_eq!(lone.label(), "Space");
        assert_eq!(lone.warning(), TAKEN);
        assert_eq!(Hotkey::parse("F13").unwrap().warning(), None);
        // Names both platforms have.
        let k = |s| Hotkey::parse(s).unwrap();
        for (name, warning) in [
            ("LeftShift+A", TYPES),
            ("A+LeftShift", TYPES),
            ("LeftShift+RightShift+Space", TYPES),
            ("LeftShift+Slash", TYPES),
            ("RightShift+KeypadMinus", TYPES),
            // Keys that type nothing: a shortcut whatever the modifiers.
            ("LeftShift+F5", SHORTCUT),
            ("LeftShift+Tab", SHORTCUT),
            ("RightShift+UpArrow", SHORTCUT),
            ("LeftShift+Backspace", SHORTCUT),
            ("LeftShift+F13", None),
            ("LeftShift+RightShift", None),
            ("LeftShift", EVERYDAY),
            ("RightShift", EVERYDAY),
        ] {
            assert_eq!(k(name).warning(), warning, "{name}");
        }
        #[cfg(target_os = "macos")]
        {
            let combo = Hotkey::parse("LeftControl+Space").unwrap();
            assert_eq!(combo.label(), "Left Control + Space");
            for (name, warning) in [
                ("LeftControl+Space", SHORTCUT),
                ("RightCommand+F5", SHORTCUT),
                ("Fn+A", SHORTCUT),
                ("LeftShift+LeftControl+A", SHORTCUT),
                ("RightOption+E", TYPES),
                ("LeftOption+LeftShift+2", TYPES),
                ("LeftOption+KeypadEquals", TYPES),
                ("LeftShift+Section", TYPES),
                ("RightOption+LeftArrow", SHORTCUT),
                ("LeftOption+Return", SHORTCUT),
                ("LeftShift+KeypadEnter", SHORTCUT),
                ("RightOption+Help", SHORTCUT),
                ("LeftShift+ForwardDelete", SHORTCUT),
                ("RightCommand+F20", None),
                ("LeftControl+LeftOption", None),
                ("Fn", None),
                ("RightOption", None),
                ("RightCommand", None),
                ("LeftCommand", EVERYDAY),
                ("LeftOption", EVERYDAY),
                ("LeftControl", EVERYDAY),
                ("RightControl", EVERYDAY),
                ("Help", TAKEN),
                ("KeypadEnter", TAKEN),
                ("Section", TAKEN),
            ] {
                assert_eq!(k(name).warning(), warning, "{name}");
            }
        }
    }

    /// Every key the warnings name is in [`KEYS`], as a modifier.
    #[test]
    fn warning_lists_name_modifiers() {
        for name in EVERYDAY_MODIFIERS
            .iter()
            .chain(CHARACTER_MODIFIERS)
            .chain(START_KEYS)
            .chain(MENU_KEYS)
        {
            assert!(Key::parse(name).unwrap().is_modifier(), "{name}");
        }
    }

    /// Every key [`NON_CHARACTER_KEYS`] names is in [`KEYS`], not as a modifier, and a key
    /// labelled with the one character it types (A, 5, /) is never among them.
    #[test]
    fn keys_that_type_a_character() {
        for name in keys::NON_CHARACTER_KEYS {
            assert!(!Key::parse(name).unwrap().is_modifier(), "{name}");
            assert!(!types_character(name), "{name}");
        }
        for &(name, label, _, kind) in KEYS {
            if kind == Swallowed && label.chars().count() == 1 {
                assert!(types_character(name), "{name}");
            }
        }
        for name in ["Space", "Keypad5", "KeypadPlus", "KeypadDecimal", "F"] {
            assert!(types_character(name), "{name}");
        }
        for name in ["F1", "F5", "F12", "F13", "LeftArrow", "Tab"] {
            assert!(!types_character(name), "{name}");
        }
    }

    #[test]
    fn same_keys_and_contains() {
        let k = |s| Hotkey::parse(s).unwrap();
        // The brief's "F13+A" is refused (two keys that aren't modifiers): LeftShift instead.
        assert!(k("LeftShift+A").same_keys(&k("A+LeftShift")));
        assert!(k("F13").contains(&k("F13")) && !k("F13").contains(&k("F14")));
        assert!(!k("LeftShift+A").same_keys(&k("LeftShift")));
        assert!(k("LeftShift+A").contains(&k("LeftShift")));
        assert!(!k("LeftShift").contains(&k("LeftShift+A")));
        assert!(!k("LeftShift+A").contains(&k("RightShift")));
    }

    /// Every name and code appears once, Esc is absent, names stay one word, and the keys
    /// today's configs use keep their codes.
    #[test]
    fn key_tables_are_consistent() {
        for (i, &(name, label, code, kind)) in KEYS.iter().enumerate() {
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric()), "{name:?}");
            assert!(!label.is_empty(), "{name}");
            assert_ne!(code, ESC_CODE, "{name}");
            #[cfg(not(target_os = "macos"))]
            assert_ne!(code, PAUSE_CODE, "{name}");
            for &(other, _, other_code, _) in &KEYS[..i] {
                assert!(!other.eq_ignore_ascii_case(name), "{name} is listed twice");
                assert_ne!(other_code, code, "{name} and {other} share a code");
            }
            let key = Key::from_code(code).unwrap();
            assert_eq!(
                (key.name, key.label(), key.code(), key.kind()),
                (name, label, code, kind)
            );
            assert_eq!(key.is_modifier(), kind != Swallowed, "{name}");
        }
        #[cfg(target_os = "macos")]
        let old = [
            ("Fn", 0x3F),
            ("RightOption", 0x3D),
            ("RightCommand", 0x36),
            ("F13", 0x69),
            ("F14", 0x6B),
            ("F15", 0x71),
            ("F16", 0x6A),
            ("F17", 0x40),
            ("F18", 0x4F),
            ("F19", 0x50),
            ("F20", 0x5A),
        ];
        #[cfg(not(target_os = "macos"))]
        let old = [
            ("RightCtrl", 0xA3),
            ("LeftCtrl", 0xA2),
            ("RightShift", 0xA1),
            ("LeftShift", 0xA0),
            ("LeftWin", 0x5B),
            ("RightWin", 0x5C),
            ("F13", 0x7C),
            ("F18", 0x81),
            ("F24", 0x87),
        ];
        for (name, code) in old {
            assert_eq!(Key::parse(name).unwrap().code(), code, "{name}");
        }
        assert_eq!(Key::parse(" f13 ").unwrap().name, "F13");
        assert_eq!(
            Hotkey::parse("leftshift + a").unwrap().to_string(),
            "LeftShift+A"
        );
    }

    #[test]
    fn hotkeys_from_pressed_codes() {
        let code = |name| Key::parse(name).unwrap().code();
        // The modifiers first, in press order (not the table's), then the other key.
        let hk = Hotkey::from_codes(&[code("A"), code("LeftShift")]).unwrap();
        assert_eq!(hk.to_string(), "LeftShift+A");
        assert_eq!(hk.label(), "Left Shift + A");
        let hk = Hotkey::from_codes(&[code("RightShift"), code("Space"), code("LeftShift")]);
        assert_eq!(hk.unwrap().to_string(), "RightShift+LeftShift+Space");
        let hk = Hotkey::from_codes(&[code("LeftShift"), code("RightShift")]).unwrap();
        assert_eq!(hk.to_string(), "LeftShift+RightShift");
        // The window shows these as they are (no "invalid hotkey" prefix).
        assert_eq!(
            Hotkey::from_codes(&[code("A"), code("Space")]).unwrap_err(),
            "A hotkey can have only one key that isn't a modifier (A, Space)."
        );
        let five = ["LeftShift", "RightShift", "F13", "F14", "A"].map(code);
        assert_eq!(
            Hotkey::from_codes(&five).unwrap_err(),
            "A hotkey can have at most 4 keys."
        );
        assert_eq!(
            Hotkey::from_codes(&[ESC_CODE]).unwrap_err(),
            "Esc cancels a recording, so it can't be a hotkey."
        );
        assert_eq!(
            Hotkey::from_codes(&[code("LeftShift"), 0xFF]).unwrap_err(),
            "AGT doesn't know that key."
        );
        assert!(Hotkey::from_codes(&[]).is_err());
        // The config file gets the same text, after the spec as written.
        assert_eq!(
            Hotkey::parse("LeftShift+RightShift+F13+F14+A").unwrap_err(),
            "invalid hotkey \"LeftShift+RightShift+F13+F14+A\": A hotkey can have at most 4 keys."
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_labels_and_warnings() {
        let k = |s| Hotkey::parse(s).unwrap();
        assert_eq!(k("LeftCtrl+LeftWin").label(), "Left Ctrl + Left Win");
        let start = Some("A tap of this key no longer opens Start while AGT runs.");
        let menu = Some("A tap of this key no longer opens an app's menu while AGT runs.");
        for (name, warning) in [
            ("LeftCtrl", EVERYDAY),
            ("LeftShift", EVERYDAY),
            ("RightShift", EVERYDAY),
            // Everyday modifiers too, but a tap of one opening a menu is more to the point.
            ("LeftWin", start),
            ("RightWin", start),
            ("LeftAlt", menu),
            ("RightAlt", menu),
            ("RightCtrl", None),
            ("F24", None),
            ("LeftCtrl+LeftWin", None),
            ("RightAlt+LeftShift", None),
            ("RightCtrl+F13", None),
            ("RightCtrl+A", SHORTCUT),
            ("LeftWin+Space", SHORTCUT),
            ("LeftAlt+F4", SHORTCUT),
            ("LeftCtrl+LeftShift+A", SHORTCUT),
            ("RightAlt+Q", TYPES),
            ("RightAlt+LeftShift+2", TYPES),
            ("RightShift+Keypad5", TYPES),
            ("RightAlt+IsoKey", TYPES),
            ("LeftShift+KeypadDecimal", TYPES),
            ("RightAlt+LeftArrow", SHORTCUT),
            ("LeftShift+Enter", SHORTCUT),
            ("RightAlt+Insert", SHORTCUT),
            ("LeftShift+Delete", SHORTCUT),
            ("CapsLock", TAKEN),
            ("F12", TAKEN),
            ("Keypad5", TAKEN),
            ("Enter", TAKEN),
            ("Insert", TAKEN),
            ("ScrollLock", TAKEN),
            ("Menu", TAKEN),
            ("KeypadPlus", TAKEN),
            ("IsoKey", TAKEN),
        ] {
            assert_eq!(k(name).warning(), warning, "{name}");
        }
        assert!(!k("LeftAlt").keys()[0].swallowed());
    }

    #[test]
    fn hold_ignores_repeat_and_reports_release() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        assert_eq!(d.on_key(HOT, true, 1000, t), Some(HotkeyEvent::Down(t, 0)));
        assert_eq!(d.on_key(HOT, true, 1500, t), None); // first auto-repeat
        assert_eq!(d.on_key(HOT, true, 1533, t), None);
        assert_eq!(d.on_key(OTHER, false, 1600, t), None); // release of some other key
        assert_eq!(d.on_key(HOT, false, 9000, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(HOT, false, 9001, t), None);
        assert_eq!(d.on_key(HOT, true, 9100, t), Some(HotkeyEvent::Down(t, 0)));
    }

    #[test]
    fn other_key_cancels_and_swallows_release() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        assert_eq!(d.on_key(OTHER, true, 0, t), None); // typing while idle
        assert_eq!(d.on_key(HOT, true, 10, t), Some(HotkeyEvent::Down(t, 0)));
        assert_eq!(d.on_key(OTHER, true, 20, t), Some(HotkeyEvent::Cancel)); // e.g. Esc
        assert_eq!(d.on_key(OTHER, true, 30, t), None);
        assert_eq!(d.on_key(HOT, false, 40, t), None);
        // Next hold works normally.
        assert_eq!(d.on_key(HOT, true, 50, t), Some(HotkeyEvent::Down(t, 0)));
    }

    #[test]
    fn missed_release_does_not_turn_next_press_into_repeat() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        assert_eq!(d.on_key(HOT, true, 1_000, t), Some(HotkeyEvent::Down(t, 0)));
        // The release happened on the secure desktop; the hook never saw it. Much later:
        assert_eq!(
            d.on_key(HOT, true, 60_000, t),
            Some(HotkeyEvent::Down(t, 0))
        );
        assert_eq!(d.on_key(HOT, false, 61_000, t), Some(HotkeyEvent::Up(t)));
        // Same after a cancelled hold whose release was missed, across tick wrap-around.
        let mut d = HoldDetector::new(1);
        d.on_key(HOT, true, u32::MAX - 10, t);
        d.on_key(OTHER, true, u32::MAX - 5, t);
        assert_eq!(d.on_key(HOT, true, 5_000, t), Some(HotkeyEvent::Down(t, 0)));
    }

    #[test]
    fn reset_forgets_the_hold() {
        let t = Instant::now();
        let mut d = HoldDetector::new(1);
        d.on_key(HOT, true, 0, t);
        d.reset();
        assert_eq!(d.on_key(HOT, false, 100, t), None);
        assert_eq!(d.on_key(HOT, true, 200, t), Some(HotkeyEvent::Down(t, 0)));
    }

    // Chords: key 0 = Ctrl, key 1 = Win.
    const CTRL: Option<usize> = Some(0);
    const WIN: Option<usize> = Some(1);

    #[test]
    fn chord_starts_when_all_keys_are_down_in_any_order() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(300);
        for (first, second) in [(CTRL, WIN), (WIN, CTRL)] {
            let mut d = HoldDetector::new(2);
            assert_eq!(d.on_key(first, true, 0, t0), None);
            assert_eq!(d.on_key(first, true, 500, t0), None); // auto-repeat
            // The hold (and the short-hold threshold) starts when the chord completes.
            assert_eq!(
                d.on_key(second, true, 600, t1),
                Some(HotkeyEvent::Down(t1, 0))
            );
            assert_eq!(d.on_key(second, true, 1100, t1), None); // auto-repeat
            assert_eq!(d.on_key(first, true, 1150, t1), None); // late repeat of the first
            // Releasing either key ends it.
            assert_eq!(d.on_key(first, false, 2000, t1), Some(HotkeyEvent::Up(t1)));
            assert_eq!(d.on_key(second, false, 2010, t1), None);
            // Ready again.
            assert_eq!(d.on_key(second, true, 3000, t0), None);
            assert_eq!(
                d.on_key(first, true, 3010, t1),
                Some(HotkeyEvent::Down(t1, 0))
            );
            assert_eq!(d.on_key(second, false, 3500, t1), Some(HotkeyEvent::Up(t1)));
        }
    }

    #[test]
    fn chord_short_tap_is_a_normal_down_up() {
        // The receiver measures Up - Down against min_hold_ms; both are reported.
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t, 0)));
        assert_eq!(d.on_key(WIN, false, 40, t), Some(HotkeyEvent::Up(t)));
    }

    #[test]
    fn partial_release_and_repress_does_not_start_a_second_hold() {
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t, 0)));
        assert_eq!(d.on_key(WIN, false, 1000, t), Some(HotkeyEvent::Up(t)));
        // Ctrl still held; Win again: no new dictation...
        assert_eq!(d.on_key(WIN, true, 1100, t), None);
        assert_eq!(d.on_key(WIN, false, 1200, t), None);
        // ...until everything was released.
        assert_eq!(d.on_key(CTRL, false, 1300, t), None);
        d.on_key(WIN, true, 1400, t);
        assert_eq!(d.on_key(CTRL, true, 1410, t), Some(HotkeyEvent::Down(t, 0)));
    }

    #[test]
    fn other_key_during_chord_cancels() {
        // Ctrl+Win+Right (switch desktop), Ctrl+Win+D, ...: cancel, then wait for release.
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t, 0)));
        assert_eq!(d.on_key(OTHER, true, 50, t), Some(HotkeyEvent::Cancel));
        assert_eq!(d.on_key(OTHER, false, 60, t), None);
        assert_eq!(d.on_key(OTHER, true, 70, t), None); // again: Ctrl+Win+Right twice
        assert_eq!(d.on_key(WIN, false, 80, t), None);
        assert_eq!(d.on_key(WIN, true, 90, t), None); // Ctrl never released
        assert_eq!(d.on_key(WIN, false, 95, t), None);
        assert_eq!(d.on_key(CTRL, false, 100, t), None);
        d.on_key(CTRL, true, 200, t);
        assert_eq!(d.on_key(WIN, true, 210, t), Some(HotkeyEvent::Down(t, 0)));
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
        assert_eq!(d.on_key(CTRL, true, 100, t), Some(HotkeyEvent::Down(t, 0)));
    }

    #[test]
    fn would_complete_only_when_armed_with_the_rest_down() {
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        assert!(!d.would_complete(WIN.unwrap()));
        d.on_key(CTRL, true, 0, t);
        assert!(d.would_complete(WIN.unwrap()));
        assert!(!d.would_complete(CTRL.unwrap()), "Win isn't down");
        d.on_key(OTHER, true, 10, t); // Ctrl+C disarms the chord
        assert!(!d.would_complete(WIN.unwrap()));
        d.on_key(CTRL, false, 20, t);
        d.on_key(CTRL, true, 30, t);
        assert!(d.would_complete(WIN.unwrap()));
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
        assert_eq!(
            d.on_key(WIN, true, 60_100, t),
            Some(HotkeyEvent::Down(t, 0))
        );
        assert_eq!(d.on_key(CTRL, false, 61_000, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(WIN, false, 61_010, t), None);

        // During a hold, both releases are missed; a much later press of one key drops the
        // stale hold (Cancel), the other key's press then starts a new one.
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        assert_eq!(d.on_key(WIN, true, 10, t), Some(HotkeyEvent::Down(t, 0)));
        assert_eq!(d.on_key(CTRL, true, 30_000, t), Some(HotkeyEvent::Cancel));
        assert_eq!(
            d.on_key(WIN, true, 30_050, t),
            Some(HotkeyEvent::Down(t, 0))
        );

        // Only one release missed (Win), Ctrl's was seen: re-pressing completes the chord.
        let mut d = HoldDetector::new(2);
        d.on_key(CTRL, true, 0, t);
        d.on_key(WIN, true, 10, t);
        assert_eq!(d.on_key(OTHER, true, 20, t), Some(HotkeyEvent::Cancel));
        assert_eq!(d.on_key(CTRL, false, 5_000, t), None);
        assert_eq!(d.on_key(CTRL, true, 9_000, t), None); // Win still believed down
        assert_eq!(d.on_key(WIN, true, 9_100, t), Some(HotkeyEvent::Down(t, 0)));
    }

    #[test]
    fn three_key_chord() {
        let t = Instant::now();
        let mut d = HoldDetector::new(3);
        assert_eq!(d.on_key(Some(2), true, 0, t), None);
        assert_eq!(d.on_key(Some(0), true, 10, t), None);
        assert_eq!(
            d.on_key(Some(1), true, 20, t),
            Some(HotkeyEvent::Down(t, 0))
        );
        assert_eq!(d.on_key(Some(0), false, 500, t), Some(HotkeyEvent::Up(t)));
        assert_eq!(d.on_key(Some(0), true, 600, t), None);
        d.reset();
        // After a reset (hook re-installed), keys must be pressed again.
        assert_eq!(d.on_key(Some(0), true, 700, t), None);
        assert_eq!(d.on_key(Some(1), true, 710, t), None);
        assert_eq!(
            d.on_key(Some(2), true, 720, t),
            Some(HotkeyEvent::Down(t, 0))
        );
    }

    #[test]
    fn win_repeat_counts_as_in_chord_only_while_the_chord_is_engaged() {
        // The Windows hook re-masks Win repeats after a paste only when this says so.
        let t = Instant::now();
        let mut d = HoldDetector::new(2);
        // A lone Win held (e.g. after an unrelated paste): its repeats are not in a chord, so
        // releasing it still opens the Start menu.
        assert!(!d.repeat_in_chord(1)); // first press: not even down yet
        d.on_key(WIN, true, 0, t);
        assert!(!d.repeat_in_chord(1));
        d.on_key(WIN, true, 500, t);
        assert!(!d.repeat_in_chord(1));
        d.on_key(WIN, false, 600, t);
        // The chord held: Win repeats are in the chord.
        d.on_key(CTRL, true, 1000, t);
        assert!(!d.repeat_in_chord(1)); // the press completing the chord isn't a repeat
        assert_eq!(d.on_key(WIN, true, 1010, t), Some(HotkeyEvent::Down(t, 0)));
        assert!(d.repeat_in_chord(1));
        // Being released: Ctrl let go first, Win lingers and repeats.
        assert_eq!(d.on_key(CTRL, false, 2000, t), Some(HotkeyEvent::Up(t)));
        assert!(d.repeat_in_chord(1));
        // Everything released: a new lone Win hold is not in a chord again.
        d.on_key(WIN, false, 2100, t);
        d.on_key(WIN, true, 3000, t);
        assert!(!d.repeat_in_chord(1));
    }

    /// It runs at every key-down: never slow, whatever has focus (or nothing, on a CI
    /// runner without a desktop).
    #[test]
    fn foreground_app_is_quick() {
        let t = Instant::now();
        let mut app = None;
        for _ in 0..100 {
            app = foreground_app();
        }
        let each = t.elapsed() / 100;
        eprintln!("foreground app: {app:?}, {each:?} per call");
        assert!(app.as_deref() != Some(""));
        assert!(each < std::time::Duration::from_millis(5), "{each:?}");
    }
}
