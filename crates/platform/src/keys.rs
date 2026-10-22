//! The keys a hotkey can use: each platform's table of file names, plain-word labels, key
//! codes and kinds, and the few keys the hotkey rules single out.

#[cfg(not(target_os = "macos"))]
use crate::KeyKind::Masked;
use crate::KeyKind::{self, Modifier, Swallowed};

/// Every key a hotkey can use on Windows: its name in the config file (case-insensitive, one
/// word), its label in plain words (what the settings window shows), its virtual-key code and
/// its kind.
///
/// - Ctrl and Shift are passed through to apps (holding them alone does nothing), and their
///   state can be polled to catch a release the hook missed.
/// - Win and Alt are passed through too, but releasing one alone would open the Start menu or
///   the focused app's menu bar: [`KeyKind::Masked`].
/// - Every other key is kept from apps while it acts as the hotkey ([`KeyKind::Swallowed`]).
///   F13-F24 are typically mapped to spare mouse or keyboard buttons, so that costs nothing;
///   for any other key it costs typing it (see [`crate::Hotkey::warning`]).
///
/// Punctuation is named and labelled after the US layout's keys (its virtual-key codes).
/// Deliberately absent: Esc ([`ESC_CODE`]).
#[cfg(not(target_os = "macos"))]
pub const KEYS: &[(&str, &str, u16, KeyKind)] = &[
    ("RightCtrl", "Right Ctrl", 0xA3, Modifier),
    ("LeftCtrl", "Left Ctrl", 0xA2, Modifier),
    ("RightShift", "Right Shift", 0xA1, Modifier),
    ("LeftShift", "Left Shift", 0xA0, Modifier),
    ("LeftWin", "Left Win", 0x5B, Masked),
    ("RightWin", "Right Win", 0x5C, Masked),
    ("LeftAlt", "Left Alt", 0xA4, Masked),
    ("RightAlt", "Right Alt", 0xA5, Masked),
    ("CapsLock", "Caps Lock", 0x14, Swallowed),
    ("F13", "F13", 0x7C, Swallowed),
    ("F14", "F14", 0x7D, Swallowed),
    ("F15", "F15", 0x7E, Swallowed),
    ("F16", "F16", 0x7F, Swallowed),
    ("F17", "F17", 0x80, Swallowed),
    ("F18", "F18", 0x81, Swallowed),
    ("F19", "F19", 0x82, Swallowed),
    ("F20", "F20", 0x83, Swallowed),
    ("F21", "F21", 0x84, Swallowed),
    ("F22", "F22", 0x85, Swallowed),
    ("F23", "F23", 0x86, Swallowed),
    ("F24", "F24", 0x87, Swallowed),
    ("F1", "F1", 0x70, Swallowed),
    ("F2", "F2", 0x71, Swallowed),
    ("F3", "F3", 0x72, Swallowed),
    ("F4", "F4", 0x73, Swallowed),
    ("F5", "F5", 0x74, Swallowed),
    ("F6", "F6", 0x75, Swallowed),
    ("F7", "F7", 0x76, Swallowed),
    ("F8", "F8", 0x77, Swallowed),
    ("F9", "F9", 0x78, Swallowed),
    ("F10", "F10", 0x79, Swallowed),
    ("F11", "F11", 0x7A, Swallowed),
    ("F12", "F12", 0x7B, Swallowed),
    ("A", "A", 0x41, Swallowed),
    ("B", "B", 0x42, Swallowed),
    ("C", "C", 0x43, Swallowed),
    ("D", "D", 0x44, Swallowed),
    ("E", "E", 0x45, Swallowed),
    ("F", "F", 0x46, Swallowed),
    ("G", "G", 0x47, Swallowed),
    ("H", "H", 0x48, Swallowed),
    ("I", "I", 0x49, Swallowed),
    ("J", "J", 0x4A, Swallowed),
    ("K", "K", 0x4B, Swallowed),
    ("L", "L", 0x4C, Swallowed),
    ("M", "M", 0x4D, Swallowed),
    ("N", "N", 0x4E, Swallowed),
    ("O", "O", 0x4F, Swallowed),
    ("P", "P", 0x50, Swallowed),
    ("Q", "Q", 0x51, Swallowed),
    ("R", "R", 0x52, Swallowed),
    ("S", "S", 0x53, Swallowed),
    ("T", "T", 0x54, Swallowed),
    ("U", "U", 0x55, Swallowed),
    ("V", "V", 0x56, Swallowed),
    ("W", "W", 0x57, Swallowed),
    ("X", "X", 0x58, Swallowed),
    ("Y", "Y", 0x59, Swallowed),
    ("Z", "Z", 0x5A, Swallowed),
    ("0", "0", 0x30, Swallowed),
    ("1", "1", 0x31, Swallowed),
    ("2", "2", 0x32, Swallowed),
    ("3", "3", 0x33, Swallowed),
    ("4", "4", 0x34, Swallowed),
    ("5", "5", 0x35, Swallowed),
    ("6", "6", 0x36, Swallowed),
    ("7", "7", 0x37, Swallowed),
    ("8", "8", 0x38, Swallowed),
    ("9", "9", 0x39, Swallowed),
    ("Space", "Space", 0x20, Swallowed),
    ("Tab", "Tab", 0x09, Swallowed),
    ("Enter", "Enter", 0x0D, Swallowed),
    ("Backspace", "Backspace", 0x08, Swallowed),
    ("Delete", "Delete", 0x2E, Swallowed),
    ("Home", "Home", 0x24, Swallowed),
    ("End", "End", 0x23, Swallowed),
    ("PageUp", "Page Up", 0x21, Swallowed),
    ("PageDown", "Page Down", 0x22, Swallowed),
    ("LeftArrow", "Left Arrow", 0x25, Swallowed),
    ("RightArrow", "Right Arrow", 0x27, Swallowed),
    ("UpArrow", "Up Arrow", 0x26, Swallowed),
    ("DownArrow", "Down Arrow", 0x28, Swallowed),
    ("Minus", "-", 0xBD, Swallowed),
    ("Equal", "=", 0xBB, Swallowed),
    ("LeftBracket", "[", 0xDB, Swallowed),
    ("RightBracket", "]", 0xDD, Swallowed),
    ("Backslash", "\\", 0xDC, Swallowed),
    ("Semicolon", ";", 0xBA, Swallowed),
    ("Quote", "'", 0xDE, Swallowed),
    ("Comma", ",", 0xBC, Swallowed),
    ("Period", ".", 0xBE, Swallowed),
    ("Slash", "/", 0xBF, Swallowed),
    ("Grave", "`", 0xC0, Swallowed),
    ("Keypad0", "Keypad 0", 0x60, Swallowed),
    ("Keypad1", "Keypad 1", 0x61, Swallowed),
    ("Keypad2", "Keypad 2", 0x62, Swallowed),
    ("Keypad3", "Keypad 3", 0x63, Swallowed),
    ("Keypad4", "Keypad 4", 0x64, Swallowed),
    ("Keypad5", "Keypad 5", 0x65, Swallowed),
    ("Keypad6", "Keypad 6", 0x66, Swallowed),
    ("Keypad7", "Keypad 7", 0x67, Swallowed),
    ("Keypad8", "Keypad 8", 0x68, Swallowed),
    ("Keypad9", "Keypad 9", 0x69, Swallowed),
];

/// An example chord for error messages (goes with [`KEYS`]).
#[cfg(not(target_os = "macos"))]
pub(crate) const CHORD_EXAMPLE: &str = "LeftCtrl+LeftWin";

/// Esc's virtual-key code (`VK_ESCAPE`). Never part of a hotkey: it cancels recording one, and
/// AGT already uses it to cancel a dictation and end a hands-free recording.
#[cfg(not(target_os = "macos"))]
pub(crate) const ESC_CODE: u16 = 0x1B;

/// The modifiers used all the time in shortcuts and typing (goes with [`KEYS`]): as a whole
/// hotkey, AGT starts listening each time one is held.
#[cfg(not(target_os = "macos"))]
pub(crate) const EVERYDAY_MODIFIERS: &[&str] = &[
    "LeftCtrl",
    "LeftShift",
    "RightShift",
    "LeftAlt",
    "LeftWin",
    "RightWin",
];

/// Every key a hotkey can use on macOS: its name in the config file (case-insensitive, one
/// word), its label in plain words (what the settings window shows), its macOS virtual key
/// code (Carbon `kVK_*`) and its kind.
///
/// - The modifiers (Fn, the Globe key on newer keyboards, and both sides of Shift, Control,
///   Option and Command) are passed through to apps (holding one alone does nothing;
///   Option+letter or Command+letter while holding cancels the dictation). As the whole
///   hotkey, Fn's Globe-key event is kept from apps, or each tap would also do what System
///   Settings > Keyboard > "Press 🌐 key to" says (the emoji picker, by default), which then
///   takes the paste.
/// - Every other key is kept from apps while it acts as the hotkey ([`KeyKind::Swallowed`]).
///   F13-F20 are typically mapped to spare mouse or keyboard buttons, so that costs nothing;
///   for any other key it costs typing it (see [`crate::Hotkey::warning`]).
///
/// Letters, digits and punctuation are the keys at those places on a US (ANSI) keyboard, named
/// and labelled after what they type there. Deliberately absent: Esc ([`ESC_CODE`]) and Caps
/// Lock ([`CAPS_LOCK_CODE`]).
#[cfg(target_os = "macos")]
pub const KEYS: &[(&str, &str, u16, KeyKind)] = &[
    ("Fn", "Fn", 0x3F, Modifier),
    ("RightOption", "Right Option", 0x3D, Modifier),
    ("RightCommand", "Right Command", 0x36, Modifier),
    ("LeftOption", "Left Option", 0x3A, Modifier),
    ("LeftCommand", "Left Command", 0x37, Modifier),
    ("LeftControl", "Left Control", 0x3B, Modifier),
    ("RightControl", "Right Control", 0x3E, Modifier),
    ("LeftShift", "Left Shift", 0x38, Modifier),
    ("RightShift", "Right Shift", 0x3C, Modifier),
    ("F13", "F13", 0x69, Swallowed),
    ("F14", "F14", 0x6B, Swallowed),
    ("F15", "F15", 0x71, Swallowed),
    ("F16", "F16", 0x6A, Swallowed),
    ("F17", "F17", 0x40, Swallowed),
    ("F18", "F18", 0x4F, Swallowed),
    ("F19", "F19", 0x50, Swallowed),
    ("F20", "F20", 0x5A, Swallowed),
    ("F1", "F1", 0x7A, Swallowed),
    ("F2", "F2", 0x78, Swallowed),
    ("F3", "F3", 0x63, Swallowed),
    ("F4", "F4", 0x76, Swallowed),
    ("F5", "F5", 0x60, Swallowed),
    ("F6", "F6", 0x61, Swallowed),
    ("F7", "F7", 0x62, Swallowed),
    ("F8", "F8", 0x64, Swallowed),
    ("F9", "F9", 0x65, Swallowed),
    ("F10", "F10", 0x6D, Swallowed),
    ("F11", "F11", 0x67, Swallowed),
    ("F12", "F12", 0x6F, Swallowed),
    ("A", "A", 0x00, Swallowed),
    ("B", "B", 0x0B, Swallowed),
    ("C", "C", 0x08, Swallowed),
    ("D", "D", 0x02, Swallowed),
    ("E", "E", 0x0E, Swallowed),
    ("F", "F", 0x03, Swallowed),
    ("G", "G", 0x05, Swallowed),
    ("H", "H", 0x04, Swallowed),
    ("I", "I", 0x22, Swallowed),
    ("J", "J", 0x26, Swallowed),
    ("K", "K", 0x28, Swallowed),
    ("L", "L", 0x25, Swallowed),
    ("M", "M", 0x2E, Swallowed),
    ("N", "N", 0x2D, Swallowed),
    ("O", "O", 0x1F, Swallowed),
    ("P", "P", 0x23, Swallowed),
    ("Q", "Q", 0x0C, Swallowed),
    ("R", "R", 0x0F, Swallowed),
    ("S", "S", 0x01, Swallowed),
    ("T", "T", 0x11, Swallowed),
    ("U", "U", 0x20, Swallowed),
    ("V", "V", 0x09, Swallowed),
    ("W", "W", 0x0D, Swallowed),
    ("X", "X", 0x07, Swallowed),
    ("Y", "Y", 0x10, Swallowed),
    ("Z", "Z", 0x06, Swallowed),
    ("0", "0", 0x1D, Swallowed),
    ("1", "1", 0x12, Swallowed),
    ("2", "2", 0x13, Swallowed),
    ("3", "3", 0x14, Swallowed),
    ("4", "4", 0x15, Swallowed),
    ("5", "5", 0x17, Swallowed),
    ("6", "6", 0x16, Swallowed),
    ("7", "7", 0x1A, Swallowed),
    ("8", "8", 0x1C, Swallowed),
    ("9", "9", 0x19, Swallowed),
    ("Space", "Space", 0x31, Swallowed),
    ("Tab", "Tab", 0x30, Swallowed),
    ("Return", "Return", 0x24, Swallowed),
    // The key above Return types a backspace; Mac keyboards call it Delete.
    ("Backspace", "Delete", 0x33, Swallowed),
    ("ForwardDelete", "Forward Delete", 0x75, Swallowed),
    ("Home", "Home", 0x73, Swallowed),
    ("End", "End", 0x77, Swallowed),
    ("PageUp", "Page Up", 0x74, Swallowed),
    ("PageDown", "Page Down", 0x79, Swallowed),
    ("LeftArrow", "Left Arrow", 0x7B, Swallowed),
    ("RightArrow", "Right Arrow", 0x7C, Swallowed),
    ("UpArrow", "Up Arrow", 0x7E, Swallowed),
    ("DownArrow", "Down Arrow", 0x7D, Swallowed),
    ("Minus", "-", 0x1B, Swallowed),
    ("Equal", "=", 0x18, Swallowed),
    ("LeftBracket", "[", 0x21, Swallowed),
    ("RightBracket", "]", 0x1E, Swallowed),
    ("Backslash", "\\", 0x2A, Swallowed),
    ("Semicolon", ";", 0x29, Swallowed),
    ("Quote", "'", 0x27, Swallowed),
    ("Comma", ",", 0x2B, Swallowed),
    ("Period", ".", 0x2F, Swallowed),
    ("Slash", "/", 0x2C, Swallowed),
    ("Grave", "`", 0x32, Swallowed),
    ("Keypad0", "Keypad 0", 0x52, Swallowed),
    ("Keypad1", "Keypad 1", 0x53, Swallowed),
    ("Keypad2", "Keypad 2", 0x54, Swallowed),
    ("Keypad3", "Keypad 3", 0x55, Swallowed),
    ("Keypad4", "Keypad 4", 0x56, Swallowed),
    ("Keypad5", "Keypad 5", 0x57, Swallowed),
    ("Keypad6", "Keypad 6", 0x58, Swallowed),
    ("Keypad7", "Keypad 7", 0x59, Swallowed),
    ("Keypad8", "Keypad 8", 0x5B, Swallowed),
    ("Keypad9", "Keypad 9", 0x5C, Swallowed),
];

#[cfg(target_os = "macos")]
pub(crate) const CHORD_EXAMPLE: &str = "Fn+RightOption";

/// Esc's virtual key code (`kVK_Escape`). Never part of a hotkey: it cancels recording one,
/// and AGT already uses it to cancel a dictation and end a hands-free recording.
#[cfg(target_os = "macos")]
pub(crate) const ESC_CODE: u16 = 0x35;

/// Caps Lock's virtual key code (`kVK_CapsLock`). Not in [`KEYS`]: macOS reports it switching
/// on and off, not being let go, so a hold of it can't be timed.
#[cfg(target_os = "macos")]
pub(crate) const CAPS_LOCK_CODE: u16 = 0x39;

/// The modifiers used all the time in shortcuts and typing (goes with [`KEYS`]): as a whole
/// hotkey, AGT starts listening each time one is held.
#[cfg(target_os = "macos")]
pub(crate) const EVERYDAY_MODIFIERS: &[&str] = &[
    "LeftOption",
    "LeftCommand",
    "LeftControl",
    "RightControl",
    "LeftShift",
    "RightShift",
];

/// Whether the key named `name` is one of F13-F24: on no keyboard's typing keys (typically
/// mapped to spare mouse or keyboard buttons), so a hotkey can take it over for free.
pub(crate) fn is_spare_f_key(name: &str) -> bool {
    name.strip_prefix('F')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (13..=24).contains(&n))
}

/// The names in [`KEYS`], for error messages.
pub(crate) fn key_names() -> String {
    KEYS.iter()
        .map(|&(name, ..)| name)
        .collect::<Vec<_>>()
        .join(", ")
}
