//! macOS: a `CGEventTap` on its own run-loop thread, `NSPasteboard`, `CGEventPost`, and the
//! privacy permissions (TCC) they need.
//!
//! | What | Needs |
//! |---|---|
//! | Seeing the hotkey (listen-only event tap) | Input Monitoring |
//! | Swallowing F13-F20 (active event tap), sending Cmd+V | Accessibility |
//! | Recording (otherwise CoreAudio delivers silence) | Microphone |
//!
//! None of them needs a restart once granted: the hotkey thread retries creating its tap every
//! second until it works, and the others are checked when used.

use crate::{Error, HoldDetector, HotkeyEvent, Key, Permission};
use block2::RcBlock;
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, ProtocolObject},
};
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSWorkspace};
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFString, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
    CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGRequestListenEventAccess,
    CGRequestPostEventAccess, CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
    kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSString};
use std::{
    ffi::c_void,
    ptr::{NonNull, null_mut},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
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
/// The key that types "v" on ANSI/ISO QWERTY, QWERTZ and AZERTY layouts.
const KVK_ANSI_V: u16 = 0x09;

/// Event times for [`HoldDetector`], in ms since this module was first used (wraps after 49
/// days, which the detector handles). Our own clock: `CGEventGetTimestamp`'s unit differs
/// between Intel and Apple Silicon.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ms() -> u32 {
    EPOCH.elapsed().as_millis() as u32
}

// ---------------------------------------------------------------------------------------
// Hotkey

/// The modifier flag that is set while `code` (a modifier hotkey) is down: the
/// device-dependent left/right bits from IOKit's `NX_DEVICE*KEYMASK`, or Fn's flag.
fn down_flag(code: u16) -> CGEventFlags {
    match code {
        0x3D => CGEventFlags::from_bits_retain(0x40), // NX_DEVICERALTKEYMASK
        0x36 => CGEventFlags::from_bits_retain(0x10), // NX_DEVICERCMDKEYMASK
        0x3F => CGEventFlags::MaskSecondaryFn,
        _ => CGEventFlags::empty(),
    }
}

/// CoreFoundation objects owned by the hotkey thread but poked from others: enabling a tap
/// and stopping a run loop are thread-safe.
struct Cf<T>(T);
unsafe impl<T> Send for Cf<T> {}

struct TapState {
    detector: HoldDetector,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
    tap: Option<Cf<CFRetained<CFMachPort>>>,
    run_loop: Option<Cf<CFRetained<CFRunLoop>>>,
}

struct Shared {
    key: Key,
    swallow: bool,
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
    key: Key,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
) -> Result<HotkeyHandle, Error> {
    let shared = Arc::new(Shared {
        key,
        swallow: key.swallowed(),
        stop: AtomicBool::new(false),
        state: Mutex::new(TapState {
            detector: HoldDetector::default(),
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
                    if shared.swallow {
                        "Input Monitoring and Accessibility"
                    } else {
                        "Input Monitoring"
                    }
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
        }
        tracing::info!("hotkey: hold {} to dictate", shared.key.name);
        // Until the handle is dropped. (A stop that raced ahead of `run_loop` being stored is
        // caught here.)
        if !shared.stop.load(Ordering::SeqCst) {
            CFRunLoop::run();
        }
        CFMachPort::invalidate(&tap);
        let mut st = shared.state.lock().unwrap();
        st.tap = None;
        st.run_loop = None;
        return;
    }
}

fn create_tap(shared: &Arc<Shared>) -> Option<CFRetained<CFMachPort>> {
    // Without the permission a listen-only tap is still created, but never gets a keyboard
    // event: don't settle for one.
    if !CGPreflightListenEventAccess() || (shared.swallow && !CGPreflightPostEventAccess()) {
        return None;
    }
    let mask = (1u64 << CGEventType::KeyDown.0)
        | (1u64 << CGEventType::KeyUp.0)
        | (1u64 << CGEventType::FlagsChanged.0);
    // Listen-only (Input Monitoring) unless the key must be swallowed, which takes an active
    // tap (Accessibility too).
    let options = if shared.swallow {
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
    let is_hotkey = code == shared.key.code;
    let down = match ty {
        CGEventType::KeyDown => true,
        CGEventType::KeyUp => false,
        CGEventType::FlagsChanged if is_hotkey => {
            CGEvent::flags(Some(ev)).intersects(down_flag(code))
        }
        // Other modifiers don't cancel: they're part of a shortcut only together with a
        // non-modifier key, which does cancel.
        _ => return pass,
    };
    let Ok(mut st) = shared.state.lock() else {
        return pass;
    };
    if let Some(e) = st
        .detector
        .on_key(is_hotkey, down, now_ms(), Instant::now())
    {
        (st.on_event)(e);
    }
    if is_hotkey && shared.swallow {
        // F13-F20: apps never see them.
        return null_mut();
    }
    pass
}

pub fn key_down(key: Key) -> Option<bool> {
    // The hardware state: neither swallowing at the session tap nor our injected events
    // change it.
    Some(CGEventSource::key_state(
        CGEventSourceStateID::HIDSystemState,
        key.code,
    ))
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

pub fn send_paste() -> Result<(), Error> {
    if !CGPreflightPostEventAccess() {
        return Err(
            "AGT doesn't have the Accessibility permission (System Settings > Privacy & Security > Accessibility)"
                .into(),
        );
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
    if !CGPreflightListenEventAccess() {
        missing.push(Permission::InputMonitoring);
    }
    if !CGPreflightPostEventAccess() {
        missing.push(Permission::Accessibility);
    }
    missing
}

pub fn request_permissions(missing: &[Permission]) {
    for p in missing {
        match p {
            Permission::Microphone => match mic_access() {
                MicAccess::NotDetermined => {
                    let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
                        continue;
                    };
                    // The answer is picked up by polling `missing_permissions`.
                    let done = RcBlock::new(|granted: Bool| {
                        tracing::info!("microphone permission granted: {}", granted.as_bool());
                    });
                    let _: () = unsafe {
                        msg_send![cls, requestAccessForMediaType: AVMediaTypeAudio, completionHandler: &*done]
                    };
                }
                // macOS won't ask again; only System Settings can change it.
                MicAccess::Denied | MicAccess::Restricted => {}
                MicAccess::Authorized => {}
            },
            Permission::InputMonitoring => {
                CGRequestListenEventAccess();
            }
            Permission::Accessibility => {
                CGRequestPostEventAccess();
            }
        }
    }
}

pub fn open_permission_settings(permission: Permission) {
    let pane = match permission {
        Permission::Microphone => "Privacy_Microphone",
        Permission::InputMonitoring => "Privacy_ListenEvent",
        Permission::Accessibility => "Privacy_Accessibility",
    };
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
    if let Err(e) = std::process::Command::new("open").arg(&url).spawn() {
        tracing::error!("cannot open {url}: {e}");
    }
}
