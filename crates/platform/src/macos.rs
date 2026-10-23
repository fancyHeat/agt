//! macOS: a `CGEventTap` on its own run-loop thread, `NSPasteboard`, `CGEventPost`, and the
//! privacy permissions (TCC) they need.
//!
//! | What | Needs |
//! |---|---|
//! | Seeing the hotkeys (event tap), swallowing their keys that aren't modifiers and the Globe key, sending Cmd+V, recording a hotkey (settings window) | Accessibility |
//! | Recording (otherwise CoreAudio delivers silence) | Microphone |
//!
//! Accessibility also lets an event tap see keystrokes, so AGT doesn't ask for Input
//! Monitoring (which macOS only applies after quitting and reopening the app). Neither needs a
//! restart once granted: the hotkey thread retries creating its tap every second until it
//! works, and the others are checked when used.

use crate::{
    Capture, Error, Hotkey, HotkeyEvent, Hotkeys, Key, LoginItem, Permission, capture::KeyCapture,
    hotkeys::Detector,
};
use block2::RcBlock;
use objc2::{
    msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyClass, AnyObject, Bool, ProtocolObject},
};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSRunningApplication, NSWorkspace,
};
use objc2_core_foundation::{
    CFGetTypeID, CFMachPort, CFRetained, CFRunLoop, CFString, CFType, CFTypeID, kCFBooleanTrue,
    kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
    CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGNullWindowID, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSError, NSNumber, NSString};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use std::{
    collections::HashSet,
    ffi::c_void,
    path::Path,
    ptr::{NonNull, null_mut},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// `kCGEventSourceUserData` on the keystrokes we synthesize, so our own tap ignores them.
const INJECTED_MARK: i64 = 0x4147_5421; // "AGT!"

/// How often the hotkey thread retries creating its tap while a permission is missing.
const TAP_RETRY: Duration = Duration::from_secs(1);

// Virtual key codes (Carbon `kVK_*`).
const KVK_COMMAND: u16 = 0x37;
const KVK_ESCAPE: u16 = 0x35;
const KVK_FUNCTION: u16 = 0x3F;
/// The key event macOS sends right after Fn is released on its own, which apps act on with
/// "Press 🌐 key to" (System Settings > Keyboard). Not in Carbon's list.
const KVK_GLOBE: u16 = 0xB3;
/// The key that types "v" on ANSI/ISO QWERTY, QWERTZ and AZERTY layouts.
const KVK_ANSI_V: u16 = 0x09;

/// Event times for the [`Detector`], in ms since this module was first used (wraps after 49
/// days, which the detector handles). Our own clock: `CGEventGetTimestamp`'s unit differs
/// between Intel and Apple Silicon.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ms() -> u32 {
    EPOCH.elapsed().as_millis() as u32
}

// ---------------------------------------------------------------------------------------
// Hotkey

/// The modifier flag that is set while the modifier with key code `code` (one in
/// [`crate::KEYS`]) is down: its device-dependent left/right bit from IOKit's
/// `NX_DEVICE*KEYMASK` (IOLLEvent.h), or Fn's flag. Empty for any other key, Caps Lock
/// included: macOS reports Caps Lock switching on and off, not being held.
fn down_flag(code: u16) -> CGEventFlags {
    let bits = match code {
        0x3B => 0x0001, // Left Control: NX_DEVICELCTLKEYMASK
        0x38 => 0x0002, // Left Shift: NX_DEVICELSHIFTKEYMASK
        0x3C => 0x0004, // Right Shift: NX_DEVICERSHIFTKEYMASK
        0x37 => 0x0008, // Left Command: NX_DEVICELCMDKEYMASK
        0x36 => 0x0010, // Right Command: NX_DEVICERCMDKEYMASK
        0x3A => 0x0020, // Left Option: NX_DEVICELALTKEYMASK
        0x3D => 0x0040, // Right Option: NX_DEVICERALTKEYMASK
        0x3E => 0x2000, // Right Control: NX_DEVICERCTLKEYMASK
        KVK_FUNCTION => return CGEventFlags::MaskSecondaryFn,
        _ => 0,
    };
    CGEventFlags::from_bits_retain(bits)
}

/// Whether macOS sets Fn's flag on every key event of `code`, Fn held or not: it does for the
/// F-keys and the navigation keys (arrows, Home, End, Page Up, Page Down, Forward Delete).
fn fn_flag_always(code: u16) -> bool {
    Key::from_code(code).is_some_and(|k| {
        k.name
            .strip_prefix('F')
            .is_some_and(|n| n.parse::<u8>().is_ok())
            || matches!(
                k.name,
                "LeftArrow"
                    | "RightArrow"
                    | "UpArrow"
                    | "DownArrow"
                    | "Home"
                    | "End"
                    | "PageUp"
                    | "PageDown"
                    | "ForwardDelete"
            )
    })
}

/// Whether the modifier `key` is down, by the `flags` of a key event of `code` (for
/// [`Detector::resync_modifiers`]): its own left/right bit, and Fn's flag unless `code` is a
/// key that always carries it (`None`: Fn stays as the tap saw it).
fn modifier_down_in(flags: CGEventFlags, code: u16, key: Key) -> Option<bool> {
    if key.code == KVK_FUNCTION && fn_flag_always(code) {
        return None;
    }
    Some(flags.intersects(down_flag(key.code)))
}

/// CoreFoundation objects owned by the hotkey thread but poked from others: enabling a tap
/// and stopping a run loop are thread-safe.
struct Cf<T>(T);
unsafe impl<T> Send for Cf<T> {}

struct TapState {
    detector: Detector,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
    tap: Option<Cf<CFRetained<CFMachPort>>>,
    run_loop: Option<Cf<CFRetained<CFRunLoop>>>,
}

struct Shared {
    /// Whether a key of any hotkey can be swallowed (one that isn't a modifier: F13-F20, A,
    /// Space...), or `hide_globe`: both take an active tap.
    swallow: bool,
    /// Whether one of the hotkeys is Fn alone, whose every tap is followed by [`KVK_GLOBE`]:
    /// apps never see that, or each dictation would also do what "Press 🌐 key to" says (the
    /// emoji picker, by default), which then takes the paste. Fn itself still reaches apps.
    hide_globe: bool,
    stop: AtomicBool,
    state: Mutex<TapState>,
}

pub struct HotkeyHandle {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct HotkeyReset {
    shared: Arc<Shared>,
}

impl HotkeyHandle {
    pub fn resetter(&self) -> HotkeyReset {
        HotkeyReset {
            shared: self.shared.clone(),
        }
    }
}

impl HotkeyReset {
    /// Forgets any hold and makes sure the tap is enabled (macOS disables a tap that was too
    /// slow, and it tells us; this covers the case where it didn't).
    pub fn reset(&self) {
        let mut st = self.shared.state.lock().unwrap();
        st.detector.reset();
        if let Some(tap) = &st.tap {
            if !CGEvent::tap_is_enabled(&tap.0) {
                tracing::info!("keyboard event tap was disabled; re-enabling it");
            }
            CGEvent::tap_enable(&tap.0, true);
        }
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(rl) = &self.shared.state.lock().unwrap().run_loop {
            rl.0.stop();
        }
    }
}

pub fn start_hotkey(
    hotkeys: Hotkeys,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
) -> Result<HotkeyHandle, Error> {
    let hide_globe = hotkeys
        .list()
        .iter()
        .any(|h| matches!(h.keys(), [k] if k.code == KVK_FUNCTION));
    let swallow = hotkeys
        .list()
        .iter()
        .flat_map(Hotkey::keys)
        .any(Key::swallowed);
    let shared = Arc::new(Shared {
        swallow: swallow || hide_globe,
        hide_globe,
        stop: AtomicBool::new(false),
        state: Mutex::new(TapState {
            detector: Detector::new(&hotkeys),
            on_event,
            tap: None,
            run_loop: None,
        }),
    });
    let s = shared.clone();
    thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || run_tap_thread(s))?;
    Ok(HotkeyHandle { shared })
}

fn run_tap_thread(shared: Arc<Shared>) {
    // macOS disables a tap whose callback is too slow; keep this thread ahead of inference.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    let mut waiting = false;
    while !shared.stop.load(Ordering::SeqCst) {
        let Some(tap) = create_tap(&shared) else {
            if !waiting {
                tracing::warn!(
                    "hotkey: cannot watch the keyboard yet (needs {}); retrying every second",
                    accessibility_name()
                );
                waiting = true;
            }
            thread::sleep(TAP_RETRY);
            continue;
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
            tracing::error!("hotkey: cannot create a run loop source for the event tap");
            return;
        };
        let Some(run_loop) = CFRunLoop::current() else {
            return;
        };
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&tap, true);
        {
            let mut st = shared.state.lock().unwrap();
            st.tap = Some(Cf(tap.clone()));
            st.run_loop = Some(Cf(run_loop));
            tracing::info!("hotkey: hold {} to dictate", st.detector.hotkeys());
        }
        // Until the handle is dropped. Its `stop()` ends a wait at once; the timeout catches a
        // stop that came before the run loop was running (stopping an idle loop does nothing).
        while !shared.stop.load(Ordering::SeqCst) {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 1.0, false);
        }
        CFMachPort::invalidate(&tap);
        let mut st = shared.state.lock().unwrap();
        st.tap = None;
        st.run_loop = None;
        return;
    }
}

fn create_tap(shared: &Arc<Shared>) -> Option<CFRetained<CFMachPort>> {
    // Without a permission a listen-only tap is still created, but never gets a keyboard
    // event: don't settle for one. Input Monitoring is enough for a listen-only tap (someone
    // may have granted only that); swallowing takes Accessibility.
    if !(post_access() || (!shared.swallow && listen_access())) {
        return None;
    }
    let mask = (1u64 << CGEventType::KeyDown.0)
        | (1u64 << CGEventType::KeyUp.0)
        | (1u64 << CGEventType::FlagsChanged.0);
    // Active whenever Accessibility allows it: the hotkey may have to be swallowed,
    // and so may Esc while it ends a hands-free recording ([`crate::arm_escape`]).
    let options = if post_access() {
        CGEventTapOptions::Default
    } else {
        CGEventTapOptions::ListenOnly
    };
    unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            options,
            mask,
            Some(tap_callback),
            Arc::as_ptr(shared) as *mut c_void,
        )
    }
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `info` is the `Shared` the thread running this tap holds an `Arc` of.
    let shared = unsafe { &*(info as *const Shared) };
    let pass = event.as_ptr();
    let ev = unsafe { event.as_ref() };
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        // A missed release while disabled is caught by the audio thread's key-state poll.
        if let Ok(st) = shared.state.lock()
            && let Some(tap) = &st.tap
        {
            CGEvent::tap_enable(&tap.0, true);
        }
        tracing::warn!(
            "keyboard event tap was disabled ({}); re-enabled it",
            if ty == CGEventType::TapDisabledByTimeout {
                "timeout"
            } else {
                "user input"
            }
        );
        return pass;
    }
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == INJECTED_MARK {
        return pass;
    }
    let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    if shared.hide_globe
        && code == KVK_GLOBE
        && matches!(ty, CGEventType::KeyDown | CGEventType::KeyUp)
    {
        return null_mut();
    }
    let flags = CGEvent::flags(Some(ev));
    let down = match ty {
        CGEventType::KeyDown => true,
        CGEventType::KeyUp => false,
        // A modifier is down while its flag is set. Caps Lock (and any other key that changes
        // the flags but isn't a modifier in KEYS) is left alone: as another key, it would
        // cancel a hold.
        CGEventType::FlagsChanged if !down_flag(code).is_empty() => {
            flags.intersects(down_flag(code))
        }
        _ => return pass,
    };
    let Ok(mut st) = shared.state.lock() else {
        return pass;
    };
    let modifiers = CGEventFlags::MaskShift
        | CGEventFlags::MaskControl
        | CGEventFlags::MaskAlternate
        | CGEventFlags::MaskCommand;
    let armed = crate::escape_armed() && !flags.intersects(modifiers);
    if down && st.detector.press_needs_resync(code) {
        // A modifier's release the tap missed may be what keeps this press from completing a
        // combo: the event's own flags say which modifiers are down.
        st.detector
            .resync_modifiers(|key| modifier_down_in(flags, code, key));
    }
    let (e, keep) = st.detector.on_key(
        code,
        down,
        code == KVK_ESCAPE,
        armed,
        now_ms(),
        Instant::now(),
    );
    if let Some(e) = e {
        (st.on_event)(e);
    }
    if keep {
        // A hotkey's key that isn't a modifier while it counts (F13-F20, a combo's Space...),
        // and the Esc that ends a hands-free recording: apps never see them.
        return null_mut();
    }
    pass
}

/// macOS side of [`crate::paste_may_release`]: Left Command and V. [`send_paste`] posts their
/// key-ups at the HID level, whose key state [`key_down`] reads, so either one held as (part
/// of) a hotkey looks released. It posts nothing for the other keys the user holds.
pub fn paste_may_release(key: Key) -> bool {
    matches!(key.code, KVK_COMMAND | KVK_ANSI_V)
}

/// Swallowing doesn't hide a key's state here: the same as [`key_down`].
pub fn key_down_past_hook(key: Key) -> Option<bool> {
    key_down(key)
}

pub fn key_down(key: Key) -> Option<bool> {
    // The HID system state: swallowing at the session tap doesn't change it. (Events posted at
    // the HID tap do: `send_paste`'s Left Command and V, see `paste_may_release`.)
    Some(CGEventSource::key_state(
        CGEventSourceStateID::HIDSystemState,
        key.code,
    ))
}

// ---------------------------------------------------------------------------------------
// Key recorder

/// How long the key recorder's run loop runs at a time, in seconds: the longest its thread
/// can miss that the handle was dropped (stopping a run loop that isn't running does nothing).
const CAPTURE_WAKE: f64 = 0.2;

struct CaptureState {
    capture: KeyCapture,
    on: Box<dyn Fn(Capture) + Send>,
    tap: Option<Cf<CFRetained<CFMachPort>>>,
    run_loop: Option<Cf<CFRetained<CFRunLoop>>>,
}

struct CaptureShared {
    stop: AtomicBool,
    state: Mutex<CaptureState>,
}

pub struct CaptureHandle {
    shared: Arc<CaptureShared>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Ok(st) = self.shared.state.lock()
            && let Some(rl) = &st.run_loop
        {
            rl.0.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// macOS side of [`crate::capture_keys`]: an active tap at the HID level, ahead of AGT's own
/// tap (at the session level) and of every app.
pub fn capture_keys(on: Box<dyn Fn(Capture) + Send>) -> Result<CaptureHandle, Error> {
    if !post_access() {
        let name = accessibility_name();
        return Err(format!(
            "Recording a key needs the {name} permission (System Settings > Privacy & Security > {name})"
        )
        .into());
    }
    let shared = Arc::new(CaptureShared {
        stop: AtomicBool::new(false),
        state: Mutex::new(CaptureState {
            capture: KeyCapture::default(),
            on,
            tap: None,
            run_loop: None,
        }),
    });
    let (tx, rx) = mpsc::sync_channel(1);
    let s = shared.clone();
    let thread = thread::Builder::new()
        .name("key-capture".into())
        .spawn(move || run_capture_thread(s, tx))?;
    rx.recv().map_err(|_| "key recorder thread died")??;
    Ok(CaptureHandle {
        shared,
        thread: Some(thread),
    })
}

fn run_capture_thread(shared: Arc<CaptureShared>, ready: mpsc::SyncSender<Result<(), String>>) {
    // Every key event on the system waits for this tap: keep the thread ahead of the others.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    let mask = (1u64 << CGEventType::KeyDown.0)
        | (1u64 << CGEventType::KeyUp.0)
        | (1u64 << CGEventType::FlagsChanged.0);
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::HIDEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(capture_callback),
            Arc::as_ptr(&shared) as *mut c_void,
        )
    };
    let Some(tap) = tap else {
        let _ = ready.send(Err("cannot create the key recorder's event tap".into()));
        return;
    };
    let (Some(source), Some(run_loop)) = (
        CFMachPort::new_run_loop_source(None, Some(&tap), 0),
        CFRunLoop::current(),
    ) else {
        CFMachPort::invalidate(&tap);
        let _ = ready.send(Err("cannot run the key recorder's event tap".into()));
        return;
    };
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&tap, true);
    {
        let mut st = shared.state.lock().unwrap();
        st.tap = Some(Cf(tap.clone()));
        st.run_loop = Some(Cf(run_loop));
    }
    let _ = ready.send(Ok(()));
    // Until the handle is dropped. Its `stop()` ends a wait at once; the timeout catches a stop
    // that came before the run loop was running (stopping an idle loop does nothing).
    while !shared.stop.load(Ordering::SeqCst) {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, CAPTURE_WAKE, false);
    }
    CFMachPort::invalidate(&tap);
    let mut st = shared.state.lock().unwrap();
    st.tap = None;
    st.run_loop = None;
}

unsafe extern "C-unwind" fn capture_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `info` is the `CaptureShared` the thread running this tap holds an `Arc` of.
    let shared = unsafe { &*(info as *const CaptureShared) };
    let pass = event.as_ptr();
    let ev = unsafe { event.as_ref() };
    let Ok(mut st) = shared.state.lock() else {
        return pass;
    };
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        if let Some(tap) = &st.tap {
            CGEvent::tap_enable(&tap.0, true);
        }
        tracing::warn!("key recorder's event tap was disabled; re-enabled it");
        return pass;
    }
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == INJECTED_MARK {
        return pass;
    }
    let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    let st = &mut *st;
    let mut report = |down| {
        if let Some(c) = st.capture.on_key(code, down) {
            (st.on)(c);
        }
    };
    match ty {
        // Not a key: what macOS sends after Fn is released on its own. Apps don't see it
        // either.
        CGEventType::KeyDown | CGEventType::KeyUp if code == KVK_GLOBE => {}
        CGEventType::KeyDown => report(true),
        CGEventType::KeyUp => report(false),
        CGEventType::FlagsChanged if !down_flag(code).is_empty() => {
            report(CGEvent::flags(Some(ev)).intersects(down_flag(code)))
        }
        // Caps Lock: macOS reports it switching on or off, not being let go. Taken as a press
        // and its release, so the window can say why it can't be a hotkey.
        CGEventType::FlagsChanged => {
            report(true);
            report(false);
        }
        _ => return pass,
    }
    // Apps and AGT's own tap never see it.
    null_mut()
}

// ---------------------------------------------------------------------------------------
// Paste target

/// The frontmost app's pid and its frontmost normal (layer 0) on-screen window, packed in one
/// number. `None` if no app is frontmost.
pub fn foreground_window() -> Option<usize> {
    let pid = NSWorkspace::sharedWorkspace()
        .frontmostApplication()?
        .processIdentifier();
    let window = front_window_of(pid).unwrap_or(0);
    Some(((pid as u32 as usize) << 32) | window as u32 as usize)
}

/// The frontmost app's bundle identifier (`com.microsoft.VSCode`); `None` for an app without
/// one (a bare executable).
pub fn foreground_app() -> Option<String> {
    autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()?
            .bundleIdentifier()
            .map(|b| b.to_string())
    })
}

/// The window number of `pid`'s frontmost on-screen normal window. Window-server data only:
/// needs no permission and doesn't talk to the app (which may be busy).
fn front_window_of(pid: i32) -> Option<i64> {
    let list = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    // Toll-free bridged: CFArray of CFDictionary is an NSArray of NSDictionary.
    let windows = unsafe { &*(CFRetained::as_ptr(&list).as_ptr() as *const NSArray<AnyObject>) };
    let key = |k: &CFString| unsafe { &*(k as *const CFString as *const NSString) };
    let (owner, layer, number) = unsafe {
        (
            key(kCGWindowOwnerPID),
            key(kCGWindowLayer),
            key(kCGWindowNumber),
        )
    };
    // Front to back.
    for w in windows.iter() {
        let Ok(d) = w.downcast::<NSDictionary>() else {
            continue;
        };
        let num = |k: &NSString| {
            d.objectForKey(k)
                .and_then(|o| o.downcast::<NSNumber>().ok())
                .map(|n| n.as_i64())
        };
        if num(owner) == Some(pid as i64) && num(layer) == Some(0) {
            return num(number);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------
// Focus

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> Option<NonNull<CFType>>;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementCopyAttributeValue(
        element: &CFType,
        attribute: &CFString,
        value: *mut *mut CFType,
    ) -> i32;
    fn AXUIElementSetAttributeValue(element: &CFType, attribute: &CFString, value: &CFType) -> i32;
    fn AXUIElementSetMessagingTimeout(element: &CFType, seconds: f32) -> i32;
    fn AXUIElementGetPid(element: &CFType, pid: *mut i32) -> i32;
}

/// `kAXErrorNoValue`: the attribute is supported but empty (e.g. nothing has focus).
const AX_NO_VALUE: i32 = -25212;
/// `kAXErrorAttributeUnsupported`: the element (or app) doesn't have the attribute.
const AX_UNSUPPORTED: i32 = -25205;
/// `kAXErrorIllegalArgument`: what we report when an app hands back something that isn't an
/// accessibility element.
const AX_ILLEGAL_ARGUMENT: i32 = -25201;

/// How long one accessibility request may take before giving up (the default is 6 s, and a
/// busy app answers late or not at all). Set on the system-wide element, so it is the default
/// for every accessibility request AGT makes.
const AX_TIMEOUT: f32 = 0.25;

/// Focused-element roles that take typed text. Secure (password) fields are `AXTextField`s.
const TEXT_ROLES: &[&str] = &["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"];

/// Focused-element roles that clearly aren't a place to type: a web page or link with no field
/// focused, a button or other control, a list, a table, a menu. Any other role is "can't tell":
/// apps that draw their own text (terminals like kitty or WezTerm, editors like Zed or Emacs)
/// report a plain group or window, and a paste there works.
const NOT_TEXT_ROLES: &[&str] = &[
    "AXWebArea",
    "AXLink",
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXDisclosureTriangle",
    "AXSlider",
    "AXTabGroup",
    "AXList",
    "AXOutline",
    "AXTable",
    "AXRow",
    "AXCell",
    "AXBrowser",
    "AXImage",
    "AXMenu",
    "AXMenuBar",
    "AXMenuBarItem",
    "AXMenuItem",
];

/// The Finder, where a plain group (the desktop, an icon view) or nothing focused at all means
/// files, not text.
const FINDER: &str = "com.apple.finder";

fn ax_element(raw: Option<NonNull<CFType>>) -> Option<CFRetained<CFType>> {
    // SAFETY: `AXUIElementCreate*` return a +1 reference.
    raw.map(|p| unsafe { CFRetained::from_raw(p) })
}

fn ax_attribute(element: &CFType, name: &'static str) -> Result<CFRetained<CFType>, i32> {
    let mut value = null_mut();
    let rc = unsafe {
        AXUIElementCopyAttributeValue(element, &CFString::from_static_str(name), &mut value)
    };
    match NonNull::new(value) {
        // SAFETY: a `Copy` function: the value is +1.
        Some(v) if rc == 0 => Ok(unsafe { CFRetained::from_raw(v) }),
        _ if rc == 0 => Err(AX_NO_VALUE),
        _ => Err(rc),
    }
}

/// An attribute whose value is another accessibility element (it comes from another process,
/// so it's checked before being asked anything).
fn ax_element_attribute(element: &CFType, name: &'static str) -> Result<CFRetained<CFType>, i32> {
    let value = ax_attribute(element, name)?;
    if CFGetTypeID(Some(&value)) == unsafe { AXUIElementGetTypeID() } {
        Ok(value)
    } else {
        Err(AX_ILLEGAL_ARGUMENT)
    }
}

/// The accessibility element of the app with the keyboard focus, and its pid (if known);
/// requests to it are bounded by [`AX_TIMEOUT`]. Asked of the accessibility API, not
/// `NSWorkspace`, whose frontmost app only follows along on a thread running the main run loop.
fn focused_app() -> Option<(CFRetained<CFType>, Option<i32>)> {
    if !unsafe { AXIsProcessTrusted() } {
        return None;
    }
    let all = ax_element(unsafe { AXUIElementCreateSystemWide() })?;
    unsafe { AXUIElementSetMessagingTimeout(&all, AX_TIMEOUT) };
    let app = ax_element_attribute(&all, "AXFocusedApplication").ok()?;
    let mut pid = 0;
    let pid = (unsafe { AXUIElementGetPid(&app, &mut pid) } == 0).then_some(pid);
    Some((app, pid))
}

/// Apps (by pid) already asked to build their accessibility tree: it stays built until they
/// quit, so each is asked once.
static PREPARED: LazyLock<Mutex<HashSet<i32>>> = LazyLock::new(Default::default);

pub fn prepare_focus_check() {
    let Some((app, Some(pid))) = focused_app() else {
        return;
    };
    let prepared = || PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    if prepared().contains(&pid) {
        return;
    }
    // Electron's switch for building its accessibility tree (Chromium does it only for
    // assistive apps, which it can't detect on macOS). Other apps answer "unsupported".
    if let Some(yes) = unsafe { kCFBooleanTrue } {
        let rc = unsafe {
            AXUIElementSetAttributeValue(
                &app,
                &CFString::from_static_str("AXManualAccessibility"),
                yes,
            )
        };
        tracing::debug!("focus: pid {pid} AXManualAccessibility → {rc}");
        // Asked again next time if it didn't get the message (busy, just launched).
        if rc == 0 || rc == AX_UNSUPPORTED {
            prepared().insert(pid);
        }
    }
}

pub fn focus_takes_text() -> Option<bool> {
    let (app, pid) = focused_app()?;
    let pid = pid.unwrap_or(0);
    let is_finder = || {
        autoreleasepool(|_| {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .and_then(|a| a.bundleIdentifier())
                .is_some_and(|b| b.to_string() == FINDER)
        })
    };
    let focused = match ax_element_attribute(&app, "AXFocusedUIElement") {
        Ok(f) => f,
        // The app has nothing focused (no window, or a window with no focusable control).
        Err(AX_NO_VALUE) => {
            let takes = takes_text(None, false, is_finder());
            tracing::debug!("focus: pid {pid}: nothing focused → takes text: {takes:?}");
            return takes;
        }
        Err(e) => {
            tracing::debug!("focus: pid {pid}: can't tell (accessibility error {e})");
            return None;
        }
    };
    let role = ax_attribute(&focused, "AXRole")
        .ok()
        .and_then(|r| r.downcast_ref::<CFString>().map(ToString::to_string));
    let Some(role) = role else {
        tracing::debug!("focus: pid {pid}: can't tell (no role)");
        return None;
    };
    // In an editable region of a web page (Chromium and WebKit): a rich text editor, say.
    // Not "has a text selection": Chromium gives every element of a page one, links too.
    // Only asked when the role doesn't already answer it: it's another round trip to the app.
    let editable = if TEXT_ROLES.contains(&role.as_str()) {
        false
    } else {
        match ax_attribute(&focused, "AXEditableAncestor") {
            Ok(_) => true,
            Err(AX_NO_VALUE | AX_UNSUPPORTED) => false,
            // Didn't answer in time, say: a link in an editor would look like a plain link.
            Err(e) => {
                tracing::debug!("focus: pid {pid}: {role}, can't tell (accessibility error {e})");
                return None;
            }
        }
    };
    let finder = !editable && matches!(role.as_str(), "AXGroup" | "AXScrollArea") && is_finder();
    let takes = takes_text(Some(&role), editable, finder);
    tracing::debug!("focus: pid {pid}: {role} (editable {editable}) → takes text: {takes:?}");
    takes
}

/// Whether a focused element with `role` (`None`: nothing is focused) takes typed text, or
/// `None` if that can't be told. `editable`: it is in an editable region of a web page.
/// `finder`: the focused app is the Finder.
fn takes_text(role: Option<&str>, editable: bool, finder: bool) -> Option<bool> {
    match role {
        _ if editable => Some(true),
        Some(r) if TEXT_ROLES.contains(&r) => Some(true),
        Some(r) if NOT_TEXT_ROLES.contains(&r) => Some(false),
        // The desktop, an icon view, or nothing selected.
        None | Some("AXGroup" | "AXScrollArea") if finder => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Clipboard

/// [nspasteboard.org](http://nspasteboard.org) markers: clipboard managers (and Universal
/// Clipboard) skip content carrying them. Our transcript is only on the clipboard for a moment.
const TRANSIENT: &str = "org.nspasteboard.TransientType";
const CONCEALED: &str = "org.nspasteboard.ConcealedType";

/// Skip absurdly large clipboard contents rather than doubling a huge allocation.
const MAX_SNAPSHOT_BYTES: usize = 256 << 20;

pub struct ClipboardSnapshot {
    /// Per pasteboard item: (type, data) for every type it has.
    items: Vec<Vec<(String, Vec<u8>)>>,
    change_count: isize,
}

pub struct PasteToken {
    change_count: isize,
}

pub fn snapshot_clipboard() -> Result<ClipboardSnapshot, Error> {
    let pb = NSPasteboard::generalPasteboard();
    let change_count = pb.changeCount();
    let mut items = Vec::new();
    let mut total = 0;
    for item in pb.pasteboardItems().iter().flat_map(|a| a.iter()) {
        let mut formats = Vec::new();
        for ty in item.types().iter() {
            // May make the owning app render a promised type: this can be slow (the caller
            // runs it on a thread it can abandon).
            let Some(data) = item.dataForType(&ty) else {
                continue;
            };
            total += data.len();
            if total <= MAX_SNAPSHOT_BYTES {
                formats.push((ty.to_string(), data.to_vec()));
            }
        }
        items.push(formats);
    }
    if total > MAX_SNAPSHOT_BYTES {
        tracing::warn!(
            "clipboard holds {} MB; only part of it will be restored",
            total >> 20
        );
    }
    Ok(ClipboardSnapshot {
        items,
        change_count,
    })
}

pub fn is_stale(snapshot: &ClipboardSnapshot) -> bool {
    NSPasteboard::generalPasteboard().changeCount() != snapshot.change_count
}

fn write_items(pb: &NSPasteboard, items: Vec<Retained<NSPasteboardItem>>) -> Result<(), Error> {
    let items: Vec<_> = items
        .into_iter()
        .map(ProtocolObject::from_retained)
        .collect();
    if pb.writeObjects(&NSArray::from_retained_slice(&items)) {
        Ok(())
    } else {
        Err("NSPasteboard writeObjects failed".into())
    }
}

fn add_markers(item: &NSPasteboardItem, markers: &[&str]) {
    let empty = NSData::new();
    for m in markers {
        item.setData_forType(&empty, &NSString::from_str(m));
    }
}

pub fn set_clipboard_text(text: &str) -> Result<PasteToken, Error> {
    let pb = NSPasteboard::generalPasteboard();
    let item = NSPasteboardItem::new();
    if !item.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString }) {
        return Err("cannot put text in a pasteboard item".into());
    }
    add_markers(&item, &[TRANSIENT, CONCEALED]);
    pb.clearContents();
    write_items(&pb, vec![item])?;
    Ok(PasteToken {
        change_count: pb.changeCount(),
    })
}

pub fn restore_clipboard(snapshot: &ClipboardSnapshot, token: &PasteToken) -> Result<bool, Error> {
    let pb = NSPasteboard::generalPasteboard();
    // The user copied something since our paste: keep it. (Unlike Windows there is no
    // clipboard lock, so this check and the write below aren't atomic; the gap is a few µs.)
    if pb.changeCount() != token.change_count {
        return Ok(false);
    }
    pb.clearContents();
    if snapshot.items.is_empty() {
        return Ok(true);
    }
    let items = snapshot
        .items
        .iter()
        .map(|formats| {
            let item = NSPasteboardItem::new();
            for (ty, data) in formats {
                item.setData_forType(&NSData::with_bytes(data), &NSString::from_str(ty));
            }
            // The original copy is already in clipboard managers' history; don't add a duplicate.
            add_markers(&item, &[TRANSIENT]);
            item
        })
        .collect();
    write_items(&pb, items)?;
    Ok(true)
}

/// Reading the pasteboard never fails because someone else is reading it: nothing to wait for.
pub fn wait_for_clipboard_listeners() -> Option<Duration> {
    None
}

/// Calls `before_releasing` first if Left Command or V is held: its posted key-up makes that
/// key look released (see [`paste_may_release`]).
pub fn send_paste(before_releasing: impl FnOnce()) -> Result<(), Error> {
    if !post_access() {
        let name = accessibility_name();
        return Err(format!(
            "AGT doesn't have the {name} permission (System Settings > Privacy & Security > {name})"
        )
        .into());
    }
    let held = |code| CGEventSource::key_state(CGEventSourceStateID::HIDSystemState, code);
    if held(KVK_COMMAND) || held(KVK_ANSI_V) {
        before_releasing();
    }
    // A private source: the events carry exactly the flags we set, not the modifiers the user
    // is holding (e.g. Option for the next dictation, which would make it Cmd+Option+V).
    let source = CGEventSource::new(CGEventSourceStateID::Private);
    let command = CGEventFlags::MaskCommand;
    for (code, down, flags) in [
        (KVK_COMMAND, true, command),
        (KVK_ANSI_V, true, command),
        (KVK_ANSI_V, false, command),
        (KVK_COMMAND, false, CGEventFlags::empty()),
    ] {
        let event = CGEvent::new_keyboard_event(source.as_deref(), code, down)
            .ok_or("cannot create a keyboard event")?;
        CGEvent::set_flags(Some(&event), flags);
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUserData,
            INJECTED_MARK,
        );
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Permissions

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVMediaTypeAudio: &'static NSString;
}

/// `AVAuthorizationStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicAccess {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

fn mic_access() -> MicAccess {
    let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
        return MicAccess::NotDetermined;
    };
    let status: isize =
        unsafe { msg_send![cls, authorizationStatusForMediaType: AVMediaTypeAudio] };
    match status {
        1 => MicAccess::Restricted,
        2 => MicAccess::Denied,
        3 => MicAccess::Authorized,
        _ => MicAccess::NotDetermined,
    }
}

pub fn missing_permissions() -> Vec<Permission> {
    let mut missing = Vec::new();
    if mic_access() != MicAccess::Authorized {
        missing.push(Permission::Microphone);
    }
    if !post_access() {
        missing.push(Permission::Accessibility);
    }
    missing
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// `IOHIDRequestType` 1 = `kIOHIDRequestTypeListenEvent`; returns `IOHIDAccessType`
    /// (0 = granted).
    fn IOHIDCheckAccess(request: u32) -> u32;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: &NSDictionary<NSString, AnyObject>) -> bool;
    static kAXTrustedCheckOptionPrompt: &'static NSString;
}

/// Shows the Accessibility ("Device Control") prompt. Not `CGRequestPostEventAccess`: once
/// `AXIsProcessTrusted` (polled by `missing_permissions`) has registered the app as "off",
/// that no longer prompts (seen on macOS 27); this does.
fn request_accessibility() {
    let yes = NSNumber::new_bool(true);
    let options = NSDictionary::from_slices(
        &[unsafe { kAXTrustedCheckOptionPrompt }],
        &[&*yes as &AnyObject],
    );
    unsafe { AXIsProcessTrustedWithOptions(&options) };
}

// `CGPreflightListenEventAccess` / `CGPreflightPostEventAccess` keep answering "no" for the
// rest of the process after a grant (seen on macOS 27), so a grant would need a restart.
// `AXIsProcessTrusted` asks TCC on every call.

/// Input Monitoring.
fn listen_access() -> bool {
    (unsafe { IOHIDCheckAccess(1) } == 0) || CGPreflightListenEventAccess()
}

/// Accessibility ("Device Control"): posting events.
fn post_access() -> bool {
    (unsafe { AXIsProcessTrusted() }) || CGPreflightPostEventAccess()
}

/// What System Settings calls the Accessibility permission: "Device Control" from macOS 27 on.
pub fn accessibility_name() -> &'static str {
    static NAME: LazyLock<&str> = LazyLock::new(|| {
        let mut buf = [0u8; 32];
        let mut len = buf.len();
        // SAFETY: `buf`/`len` describe a writable buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                null_mut(),
                0,
            )
        };
        let major = (rc == 0)
            .then(|| std::str::from_utf8(&buf[..len]).ok())
            .flatten()
            .and_then(|v| {
                v.trim_end_matches('\0')
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .unwrap_or(0);
        if major >= 27 {
            "Device Control"
        } else {
            "Accessibility"
        }
    });
    *NAME
}

/// Asks for the Microphone first and for Accessibility only once it's answered: two prompts
/// at once don't work (while the Microphone dialog was up, macOS 27 recorded the other
/// permission as denied without showing its prompt).
pub fn request_permissions(missing: &[Permission]) {
    let rest: Vec<Permission> = missing
        .iter()
        .copied()
        .filter(|p| *p != Permission::Microphone)
        .collect();
    if missing.contains(&Permission::Microphone)
        && mic_access() == MicAccess::NotDetermined
        && let Some(cls) = AnyClass::get(c"AVCaptureDevice")
    {
        // The answer is also picked up by polling `missing_permissions`.
        let done = RcBlock::new(move |granted: Bool| {
            tracing::info!("microphone permission granted: {}", granted.as_bool());
            request_event_access(&rest);
        });
        let _: () = unsafe {
            msg_send![cls, requestAccessForMediaType: AVMediaTypeAudio, completionHandler: &*done]
        };
        return;
    }
    // Microphone denied or restricted: macOS won't ask again; only System Settings can change
    // it.
    request_event_access(&rest);
}

fn request_event_access(missing: &[Permission]) {
    for p in missing {
        match p {
            Permission::Microphone => {}
            Permission::Accessibility => request_accessibility(),
        }
    }
}

pub fn open_permission_settings(permission: Permission) {
    let pane = match permission {
        Permission::Microphone => "Privacy_Microphone",
        Permission::Accessibility => "Privacy_Accessibility",
    };
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
    if let Err(e) = std::process::Command::new("open").arg(&url).spawn() {
        tracing::error!("cannot open {url}: {e}");
    }
}

// ---------------------------------------------------------------------------------------
// Launch at login: the app as a login item (`SMAppService.mainAppService`, macOS 13+). Listed
// in System Settings > General > Login Items, where the user can turn it off.

pub fn set_launch_at_login(on: bool, _program: &Path) -> Result<LoginItem, Error> {
    let service = unsafe { SMAppService::mainAppService() };
    let status = unsafe { service.status() };
    let error = |e: Retained<NSError>| -> Error {
        format!("{} (code {})", e.localizedDescription(), e.code()).into()
    };
    match (on, status) {
        (true, SMAppServiceStatus::Enabled) => Ok(LoginItem::Unchanged),
        // Turned off in System Settings: registering again would only fail with "denied".
        (true, SMAppServiceStatus::RequiresApproval) => Ok(LoginItem::NeedsApproval),
        (true, _) => {
            unsafe { service.registerAndReturnError() }.map_err(error)?;
            Ok(match unsafe { service.status() } {
                SMAppServiceStatus::RequiresApproval => LoginItem::NeedsApproval,
                _ => LoginItem::Registered,
            })
        }
        // `NotFound` too: what it says before the first registration on some versions.
        (false, SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound) => {
            Ok(LoginItem::Unchanged)
        }
        (false, _) => {
            unsafe { service.unregisterAndReturnError() }.map_err(error)?;
            Ok(LoginItem::Unregistered)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Output volume (ducking): the default output device's volume through the CoreAudio HAL.

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectHasProperty(id: u32, address: *const PropertyAddress) -> u8;
    fn AudioObjectIsPropertySettable(
        id: u32,
        address: *const PropertyAddress,
        settable: *mut u8,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        id: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectSetPropertyData(
        id: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: u32 = 1; // kAudioObjectSystemObject
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const ELEMENT_MAIN: u32 = 0;
const DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
/// `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`: the menu bar's volume slider,
/// which also covers devices that only have per-channel volumes.
const VIRTUAL_MAIN_VOLUME: u32 = fourcc(b"vmvc");
/// `kAudioDevicePropertyVolumeScalar`.
const VOLUME_SCALAR: u32 = fourcc(b"volm");
const MUTE: u32 = fourcc(b"mute");
/// `kAudioDevicePropertyDeviceUID`.
const DEVICE_UID: u32 = fourcc(b"uid ");
/// `kAudioHardwarePropertyTranslateUIDToDevice`.
const TRANSLATE_UID_TO_DEVICE: u32 = fourcc(b"uidd");

fn output_address(selector: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope: SCOPE_OUTPUT,
        element: ELEMENT_MAIN,
    }
}

/// Reads a fixed-size property.
fn get_property<T: Copy>(id: u32, address: &PropertyAddress, mut value: T) -> Result<T, Error> {
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            address,
            0,
            std::ptr::null(),
            &mut size,
            (&raw mut value).cast(),
        )
    };
    if status != 0 {
        return Err(format!("AudioObjectGetPropertyData failed (OSStatus {status})").into());
    }
    Ok(value)
}

/// An output device's volume control.
pub struct OutputVolume {
    device: u32,
    /// [`VIRTUAL_MAIN_VOLUME`] or [`VOLUME_SCALAR`].
    selector: u32,
    /// The device's UID (stable across reconnects, unlike its `AudioDeviceID`), to find it
    /// again ([`OutputVolume::open`]).
    uid: String,
}

pub fn init_volume_thread() {}

fn global_address(selector: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    }
}

impl OutputVolume {
    /// `Ok(None)`: no output device, or its volume can't be set (HDMI/DisplayPort outputs).
    pub fn open_default() -> Result<Option<Self>, Error> {
        let device = get_property(SYSTEM_OBJECT, &global_address(DEFAULT_OUTPUT_DEVICE), 0u32)?;
        Self::for_device(device)
    }

    /// The device with this [`OutputVolume::id`]. `Ok(None)`: it's not connected now.
    pub fn open(uid: &str) -> Result<Option<Self>, Error> {
        let uid = CFString::from_str(uid);
        let qualifier: *const CFString = &*uid;
        let mut device = 0u32;
        let mut size = size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &global_address(TRANSLATE_UID_TO_DEVICE),
                size_of::<*const CFString>() as u32,
                (&raw const qualifier).cast(),
                &mut size,
                (&raw mut device).cast(),
            )
        };
        if status != 0 {
            return Err(format!("translating a device UID failed (OSStatus {status})").into());
        }
        Self::for_device(device)
    }

    pub fn id(&self) -> &str {
        &self.uid
    }

    fn for_device(device: u32) -> Result<Option<Self>, Error> {
        if device == 0 {
            return Ok(None); // kAudioObjectUnknown
        }
        for selector in [VIRTUAL_MAIN_VOLUME, VOLUME_SCALAR] {
            let address = output_address(selector);
            let mut settable = 0u8;
            if unsafe { AudioObjectHasProperty(device, &address) } != 0
                && unsafe { AudioObjectIsPropertySettable(device, &address, &mut settable) } == 0
                && settable != 0
            {
                let uid: *const CFString =
                    get_property(device, &global_address(DEVICE_UID), std::ptr::null())?;
                let uid = NonNull::new(uid.cast_mut()).ok_or("the device has no UID")?;
                // A copy we own (the property's rule for CFString values).
                let uid = unsafe { CFRetained::from_raw(uid) }.to_string();
                return Ok(Some(Self {
                    device,
                    selector,
                    uid,
                }));
            }
        }
        Ok(None)
    }

    /// The volume slider's position, 0..=1.
    pub fn volume(&self) -> Result<f32, Error> {
        get_property(self.device, &output_address(self.selector), 0f32)
    }

    pub fn set_volume(&self, v: f32) -> Result<(), Error> {
        let status = unsafe {
            AudioObjectSetPropertyData(
                self.device,
                &output_address(self.selector),
                0,
                std::ptr::null(),
                size_of::<f32>() as u32,
                (&raw const v).cast(),
            )
        };
        if status != 0 {
            return Err(format!("AudioObjectSetPropertyData failed (OSStatus {status})").into());
        }
        Ok(())
    }

    pub fn muted(&self) -> Result<bool, Error> {
        let address = output_address(MUTE);
        if unsafe { AudioObjectHasProperty(self.device, &address) } == 0 {
            return Ok(false);
        }
        Ok(get_property(self.device, &address, 0u32)? != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KEYS, keys::CAPS_LOCK_CODE};

    #[test]
    fn every_modifier_has_its_own_flag() {
        // IOKit's NX_DEVICE*KEYMASK bits (IOLLEvent.h), and Fn's flag.
        for (name, bits) in [
            ("LeftControl", 0x1),
            ("LeftShift", 0x2),
            ("RightShift", 0x4),
            ("LeftCommand", 0x8),
            ("RightCommand", 0x10),
            ("LeftOption", 0x20),
            ("RightOption", 0x40),
            ("RightControl", 0x2000),
            ("Fn", CGEventFlags::MaskSecondaryFn.bits()),
        ] {
            assert_eq!(
                down_flag(Key::parse(name).unwrap().code).bits(),
                bits,
                "{name}"
            );
        }
        // Those are all of the modifiers; Caps Lock and the keys that aren't modifiers have none.
        for &(name, _, code, _) in KEYS {
            let modifier = Key::parse(name).unwrap().is_modifier();
            assert_eq!(!down_flag(code).is_empty(), modifier, "{name}");
        }
        assert!(down_flag(CAPS_LOCK_CODE).is_empty());
    }

    #[test]
    fn modifiers_read_from_a_key_event() {
        let key = |n| Key::parse(n).unwrap();
        // Left Shift, Right Option and Fn held.
        let flags = CGEventFlags::from_bits_retain(0x2 | 0x40) | CGEventFlags::MaskSecondaryFn;
        let a = key("A").code;
        assert_eq!(modifier_down_in(flags, a, key("LeftShift")), Some(true));
        assert_eq!(modifier_down_in(flags, a, key("RightShift")), Some(false));
        assert_eq!(modifier_down_in(flags, a, key("RightOption")), Some(true));
        assert_eq!(modifier_down_in(flags, a, key("Fn")), Some(true));
        let none = CGEventFlags::empty();
        assert_eq!(modifier_down_in(none, a, key("Fn")), Some(false));
        // These carry Fn's flag whether or not it's held: Fn stays as the tap saw it.
        for n in [
            "LeftArrow",
            "DownArrow",
            "F1",
            "F5",
            "F13",
            "F20",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "ForwardDelete",
        ] {
            assert_eq!(modifier_down_in(none, key(n).code, key("Fn")), None, "{n}");
            assert_eq!(
                modifier_down_in(flags, key(n).code, key("LeftShift")),
                Some(true),
                "{n}"
            );
        }
        for n in ["A", "F", "Space", "Return", "Backspace", "Keypad5", "Slash"] {
            assert!(!fn_flag_always(key(n).code), "{n}");
        }
    }

    #[test]
    fn paste_may_release_left_command_and_v() {
        // Cmd+V goes out at the HID level, whose key state `key_down` reads.
        let sensitive = |spec: &str| {
            Hotkey::parse(spec)
                .unwrap()
                .keys()
                .iter()
                .filter(|&&k| paste_may_release(k))
                .map(|k| k.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(sensitive("LeftCommand"), ["LeftCommand"]);
        assert_eq!(sensitive("LeftControl+V"), ["V"]);
        for spec in ["Fn", "RightOption", "RightCommand+F13", "Fn+RightOption"] {
            assert!(sensitive(spec).is_empty(), "{spec}");
        }
    }

    #[test]
    fn text_roles_and_editable_regions_take_text() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert_eq!(takes_text(Some(role), false, false), Some(true), "{role}");
        }
        // A paragraph or a link in a contenteditable editor (Notion, Google Docs, Slack).
        for role in ["AXStaticText", "AXGroup", "AXLink"] {
            assert_eq!(takes_text(Some(role), true, false), Some(true), "{role}");
        }
    }

    #[test]
    fn only_clear_non_text_roles_say_no() {
        // A link or a page with no field focused, a list, a button.
        for role in ["AXLink", "AXWebArea", "AXList", "AXOutline", "AXButton"] {
            assert_eq!(takes_text(Some(role), false, false), Some(false), "{role}");
        }
        // Apps that draw their own text (terminals, some editors) or have nothing focused.
        for role in [
            Some("AXGroup"),
            Some("AXWindow"),
            Some("AXScrollArea"),
            None,
        ] {
            assert_eq!(takes_text(role, false, false), None, "{role:?}");
        }
    }

    #[test]
    fn the_finder_desktop_is_not_a_text_box() {
        for role in [Some("AXGroup"), Some("AXScrollArea"), None] {
            assert_eq!(takes_text(role, false, true), Some(false), "{role:?}");
        }
        // Renaming a file.
        assert_eq!(takes_text(Some("AXTextField"), false, true), Some(true));
        assert_eq!(takes_text(Some("AXWindow"), false, true), None);
    }
}
