//! Windows: WH_KEYBOARD_LL hooks on their own threads (the hotkey's, the key recorder's),
//! Win32 clipboard, SendInput.

use crate::{
    Capture, Error, HotkeyEvent, Hotkeys, Key, KeyKind, LoginItem, Permission, capture::KeyCapture,
    hotkeys::Detector, needs_masker,
};
use std::{
    cell::RefCell,
    ffi::{OsStr, OsString, c_void},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::Path,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, ERROR_UNSUPPORTED_TYPE, GetLastError,
        GlobalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
    },
    System::{
        Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree},
        DataExchange::{
            CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
            GetClipboardSequenceNumber, GetOpenClipboardWindow, OpenClipboard,
            RegisterClipboardFormatW, SetClipboardData,
        },
        LibraryLoader::GetModuleHandleW,
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Ole::CF_UNICODETEXT,
        Registry::{
            HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
            RegSetKeyValueW,
        },
        Threading::{
            GetCurrentThread, GetCurrentThreadId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, VK_CONTROL, VK_ESCAPE, VK_LCONTROL,
            VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_V,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
            GetWindowThreadProcessId, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP,
            WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
        },
    },
};
use windows_sys::core::{GUID, HRESULT};

/// `dwExtraInfo` tag on the keystrokes we synthesize, so our own hook ignores them.
const INJECTED_MARK: usize = 0x4147_5421; // "AGT!"

/// Unassigned virtual key, injected while Alt/Win is held so its release doesn't
/// open the menu bar / Start menu (the same trick AutoHotkey uses).
const VK_MASK: u16 = 0xE8;

/// Posted to the hotkey thread: re-install the hook and forget any hold.
const WM_RESET_HOOK: u32 = WM_APP + 1;

// ---------------------------------------------------------------------------------------
// Hotkey

struct HookState {
    detector: Detector,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
    /// With Win or Alt in a hotkey ([`needs_masker`]): asks the masking thread to inject
    /// [`VK_MASK`].
    mask: Option<mpsc::Sender<()>>,
    /// [`crate::paste_count`] when the mask was last sent.
    masked_at_paste: u64,
}

/// For hotkeys with Win or Alt ([`KeyKind::Masked`]): once a hotkey is held, injects a
/// [`VK_MASK`] press so that Win or Alt is no longer "the last key pressed" when it's
/// released, and Windows doesn't open the Start menu or the focused app's menu bar (which it
/// does when Win or Alt is released last: held alone, or after Ctrl down, Win down, Ctrl up,
/// Win up). Done from its own thread, not inside the hook callback: the hook must return
/// fast, and the injected keys pass through the hook themselves.
fn spawn_masker() -> Result<mpsc::Sender<()>, Error> {
    let (tx, rx) = mpsc::channel::<()>();
    thread::Builder::new()
        .name("hotkey-mask".into())
        .spawn(move || {
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
            // Ends when the hook thread drops the sender.
            while rx.recv().is_ok() {
                let inputs = [key_input(VK_MASK, false), key_input(VK_MASK, true)];
                let sent = unsafe {
                    SendInput(
                        inputs.len() as u32,
                        inputs.as_ptr(),
                        size_of::<INPUT>() as i32,
                    )
                };
                if sent as usize != inputs.len() {
                    tracing::debug!("cannot inject the Start menu mask (error {})", unsafe {
                        GetLastError()
                    });
                }
            }
        })?;
    Ok(tx)
}

thread_local! {
    static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

pub struct HotkeyHandle {
    thread_id: u32,
}

#[derive(Clone)]
pub struct HotkeyReset {
    thread_id: u32,
}

impl HotkeyHandle {
    pub fn resetter(&self) -> HotkeyReset {
        HotkeyReset {
            thread_id: self.thread_id,
        }
    }
}

impl HotkeyReset {
    pub fn reset(&self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_RESET_HOOK, 0, 0) };
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
    }
}

fn install_hook() -> HHOOK {
    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), GetModuleHandleW(null()), 0) }
}

pub fn start_hotkey(
    hotkeys: Hotkeys,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
) -> Result<HotkeyHandle, Error> {
    let mask = if needs_masker(&hotkeys) {
        Some(spawn_masker()?)
    } else {
        None
    };
    let detector = Detector::new(&hotkeys);
    let name = detector.hotkeys().to_string();
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            // Windows silently removes a low-level hook that misses LowLevelHooksTimeout; keep
            // this thread ahead of the inference threads.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
            HOOK.with(|h| {
                *h.borrow_mut() = Some(HookState {
                    detector,
                    on_event,
                    mask,
                    masked_at_paste: 0,
                })
            });
            // The hook runs on this thread, inside GetMessageW; it needs this message loop.
            let mut hook = install_hook();
            if hook.is_null() {
                let _ = tx.send(Err(format!(
                    "SetWindowsHookExW failed (error {})",
                    unsafe { GetLastError() }
                )));
                return;
            }
            let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
                if msg.message == WM_RESET_HOOK {
                    unsafe { UnhookWindowsHookEx(hook) };
                    HOOK.with(|h| {
                        if let Some(s) = h.borrow_mut().as_mut() {
                            s.detector.reset();
                        }
                    });
                    hook = install_hook();
                    if hook.is_null() {
                        tracing::error!("cannot re-install keyboard hook (error {})", unsafe {
                            GetLastError()
                        });
                    } else {
                        tracing::info!("keyboard hook re-installed");
                    }
                    continue;
                }
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            unsafe { UnhookWindowsHookEx(hook) };
        })?;
    let thread_id = rx.recv().map_err(|_| "hotkey thread died")??;
    tracing::info!("hotkey: hold {name} to dictate");
    Ok(HotkeyHandle { thread_id })
}

pub fn foreground_window() -> Option<usize> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_null()).then_some(hwnd as usize)
}

/// The file name of the foreground window's program. A classic console's window reports
/// the shell it runs (`cmd.exe`, `powershell.exe`), not conhost.
pub fn foreground_app() -> Option<String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    process_name(hwnd)
}

/// Not checked on Windows yet (that takes UI Automation): always "can't tell".
pub fn focus_takes_text() -> Option<bool> {
    None
}

pub fn prepare_focus_check() {}

pub fn key_down(key: Key) -> Option<bool> {
    let down = vk_down(key.code);
    // A swallowed key (F13-F24) updates the system's key state only when the hook didn't
    // swallow its press (dropped, or too slow for that event). So "up" says nothing, but
    // "down" is a press the hook missed. (The hook lets that press's release through, so it
    // doesn't stay "down".)
    (down || !key.swallowed()).then_some(down)
}

pub fn key_down_past_hook(key: Key) -> Option<bool> {
    Some(vk_down(key.code))
}

/// Win or Alt: releasing it alone would open the Start menu or the menu bar.
fn is_masked(vk: u16) -> bool {
    Key::from_code(vk).is_some_and(|k| k.kind() == KeyKind::Masked)
}

/// `KBDLLHOOKSTRUCT::scanCode` of the fake Left Ctrl press/release that Windows sends along
/// with Right Alt on keyboard layouts where Right Alt is AltGr (German, French, ...).
const ALTGR_FAKE_LCTRL_SCAN: u32 = 0x21D;

/// The key code the detector should see of a (physical) key event, or `None` to leave the
/// event alone.
///
/// Left alone: the fake Left Ctrl of AltGr (it's really Right Alt: with `LeftCtrl+LeftWin`,
/// holding Win and pressing AltGr would otherwise start a dictation), and the generic Shift,
/// Ctrl and Alt codes (`VK_SHIFT`...; a low-level hook normally sees the left and right ones,
/// which the detector tracks): as another key, either would cancel a hold.
fn classify(vk: u32, scan_code: u32) -> Option<u16> {
    let vk = u16::try_from(vk).ok()?;
    if (vk == VK_LCONTROL && scan_code == ALTGR_FAKE_LCTRL_SCAN)
        || [VK_SHIFT, VK_CONTROL, VK_MENU].contains(&vk)
    {
        return None;
    }
    Some(vk)
}

/// Whether the modifier `key` is down, as the system has it (for
/// [`Detector::resync_modifiers`]).
/// While Right Alt is down, Left Ctrl may be AltGr's fake one, which the hook leaves out:
/// what the hook saw of Left Ctrl stands then (`None`).
fn modifier_down(key: Key) -> Option<bool> {
    (key.code != VK_LCONTROL || !vk_down(VK_RMENU)).then(|| vk_down(key.code))
}

/// Windows side of [`crate::paste_may_release`]: Shift, Left Ctrl, Alt and Win.
pub fn paste_may_release(key: Key) -> bool {
    matches!(key.code, 0xA0..=0xA2 | 0xA4 | 0xA5 | 0x5B | 0x5C)
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut swallow = false;
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if kb.dwExtraInfo != INJECTED_MARK {
            let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            HOOK.with(|h| {
                if let Ok(mut h) = h.try_borrow_mut()
                    && let Some(s) = h.as_mut()
                {
                    let Some(key) = classify(kb.vkCode, kb.scanCode) else {
                        return;
                    };
                    if let (true, Some(mask)) = (down, &s.mask)
                        && is_masked(key)
                        && s.detector.repeat_in_chord(key)
                    {
                        // Win or Alt auto-repeat while a hotkey with it is held or being
                        // released. If a paste released it in software since we last masked,
                        // the system takes this repeat for a fresh press, and the physical
                        // release would then open the Start menu or the menu bar: mask again.
                        // (A Win held without the rest of its hotkey, after some unrelated
                        // paste, is left alone: its tap still opens Start.)
                        let pastes = crate::paste_count();
                        if pastes != s.masked_at_paste {
                            s.masked_at_paste = pastes;
                            let _ = mask.send(());
                        }
                    }
                    if down && s.detector.press_needs_resync(key) {
                        // A modifier's release the hook missed (Win+L, Ctrl+Alt+Del) may be
                        // what keeps this press from completing a combo: ask the system.
                        s.detector.resync_modifiers(modifier_down);
                    }
                    let esc = key == VK_ESCAPE;
                    let armed = esc
                        && crate::escape_armed()
                        && ![VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                            .into_iter()
                            .any(vk_down);
                    let (ev, keep) =
                        s.detector
                            .on_key(key, down, esc, armed, kb.time, Instant::now());
                    // A release whose press got past the hook (dropped, or too slow) passes
                    // too, or the system would take the key as still down (see `key_down`).
                    // The hook runs before the key state updates: this is the press's.
                    swallow = keep && (down || !vk_down(key));
                    if let Some(ev) = ev {
                        if let (HotkeyEvent::Down(..), Some(mask)) = (ev, &s.mask) {
                            // Covers both a dictation and a hold later cancelled by another
                            // key (Ctrl+Win+D, ...). Non-blocking.
                            s.masked_at_paste = crate::paste_count();
                            let _ = mask.send(());
                        }
                        (s.on_event)(ev);
                    }
                }
            });
        }
    }
    if swallow {
        // A hotkey's key that isn't a modifier while it counts (F13-F24, a combo's Space...),
        // and the Esc that ends a hands-free recording: apps never see them.
        return 1;
    }
    // Modifiers (hotkey keys or not) and every other key pass through.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

// ---------------------------------------------------------------------------------------
// Key recorder

struct CaptureState {
    capture: KeyCapture,
    on: Box<dyn Fn(Capture) + Send>,
    /// [`KeyCapture::keys_up`], after each key event, for the handle (the state is this
    /// thread's).
    keys_up: Arc<AtomicBool>,
}

thread_local! {
    static CAPTURE: RefCell<Option<CaptureState>> = const { RefCell::new(None) };
}

pub struct CaptureHandle {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
    keys_up: Arc<AtomicBool>,
}

impl CaptureHandle {
    pub fn keys_up(&self) -> bool {
        self.keys_up.load(Ordering::SeqCst)
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // WM_QUIT ends its message loop. If that can't be posted, the thread has ended already,
        // or has no message queue (installing the hook makes one) and would never end: don't
        // wait for it.
        if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } == 0 {
            tracing::warn!("cannot stop the key recorder (error {})", unsafe {
                GetLastError()
            });
            return;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Windows side of [`crate::capture_keys`]: a low-level keyboard hook of its own. Windows
/// calls the newest one first, so it runs before AGT's (installed when AGT started).
pub fn capture_keys(on: Box<dyn Fn(Capture) + Send>) -> Result<CaptureHandle, Error> {
    let (tx, rx) = mpsc::sync_channel(1);
    let keys_up = Arc::new(AtomicBool::new(true));
    let thread_keys_up = keys_up.clone();
    let thread = thread::Builder::new()
        .name("key-capture".into())
        .spawn(move || {
            // Windows silently removes a low-level hook that misses LowLevelHooksTimeout.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
            CAPTURE.with(|c| {
                *c.borrow_mut() = Some(CaptureState {
                    capture: KeyCapture::default(),
                    on,
                    keys_up: thread_keys_up,
                })
            });
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(capture_proc),
                    GetModuleHandleW(null()),
                    0,
                )
            };
            if hook.is_null() {
                let _ = tx.send(Err(format!(
                    "SetWindowsHookExW failed (error {})",
                    unsafe { GetLastError() }
                )));
                return;
            }
            let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
            // The hook runs on this thread, inside GetMessageW. Until the handle is dropped
            // (WM_QUIT).
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            unsafe { UnhookWindowsHookEx(hook) };
            CAPTURE.with(|c| *c.borrow_mut() = None);
        })?;
    let thread_id = rx.recv().map_err(|_| "key recorder thread died")??;
    Ok(CaptureHandle {
        thread_id,
        thread: Some(thread),
        keys_up,
    })
}

unsafe extern "system" fn capture_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut keep = false;
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if kb.dwExtraInfo != INJECTED_MARK {
            let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            // The hook runs before the key state updates, and a press it keeps leaves the key
            // up: a key the system has down went down before the capture (or got past the
            // hook).
            keep = match classify(kb.vkCode, kb.scanCode) {
                Some(key) => {
                    let was_down = vk_down(key);
                    let keep = CAPTURE.with(|c| {
                        let Ok(mut c) = c.try_borrow_mut() else {
                            return true;
                        };
                        let Some(s) = c.as_mut() else {
                            return true;
                        };
                        let (capture, keep) = s.capture.on_key(key, down, down && was_down);
                        s.keys_up.store(s.capture.keys_up(), Ordering::SeqCst);
                        if let Some(capture) = capture {
                            (s.on)(capture);
                        }
                        keep
                    });
                    // A release whose press got past the hook passes too, or the system would
                    // take the key as still down.
                    keep && (down || !was_down)
                }
                // AltGr's fake Left Ctrl and the generic Shift, Ctrl and Alt codes aren't keys
                // of their own (see `classify`): not recorded, and kept unless the system has
                // the key down already.
                None => !vk_down(kb.vkCode as u16),
            };
        }
    }
    if keep {
        // Apps and AGT's own hook never see it.
        return 1;
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

// ---------------------------------------------------------------------------------------
// Clipboard

pub struct ClipboardSnapshot {
    formats: Vec<(u32, Vec<u8>)>,
    /// Clipboard sequence number the snapshot corresponds to.
    sequence: u32,
}

pub struct PasteToken {
    sequence: u32,
}

/// Holds the clipboard open; closes it on drop.
struct Open;

impl Open {
    fn new() -> Result<Self, Error> {
        // Another app may have it open for a moment; retry for ~250 ms.
        for _ in 0..50 {
            if unsafe { OpenClipboard(null_mut()) } != 0 {
                return Ok(Open);
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err(format!("cannot open the clipboard (error {})", unsafe {
            GetLastError()
        })
        .into())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

/// Formats whose handle is not an HGLOBAL (GDI objects, owner-display, private handles):
/// they can't be copied as bytes. Windows re-synthesizes CF_BITMAP etc. from CF_DIB.
fn copyable(format: u32) -> bool {
    !matches!(
        format,
        2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E | 0x200..=0x3FF
    )
}

/// Skip absurdly large clipboard contents rather than doubling a huge allocation.
const MAX_SNAPSHOT_BYTES: usize = 256 << 20;

pub fn snapshot_clipboard() -> Result<ClipboardSnapshot, Error> {
    let _open = Open::new()?;
    // Nobody can change the clipboard while we hold it open.
    let sequence = unsafe { GetClipboardSequenceNumber() };
    let mut formats = Vec::new();
    let mut total = 0;
    let mut format = 0;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if !copyable(format) {
            continue;
        }
        let h = unsafe { GetClipboardData(format) };
        if h.is_null() {
            continue;
        }
        let size = unsafe { GlobalSize(h) };
        let p = unsafe { GlobalLock(h) } as *const u8;
        if p.is_null() {
            continue;
        }
        total += size;
        if total <= MAX_SNAPSHOT_BYTES {
            formats.push((
                format,
                unsafe { std::slice::from_raw_parts(p, size) }.to_vec(),
            ));
        }
        unsafe { GlobalUnlock(h) };
    }
    if total > MAX_SNAPSHOT_BYTES {
        tracing::warn!(
            "clipboard holds {} MB; only part of it will be restored",
            total >> 20
        );
    }
    Ok(ClipboardSnapshot { formats, sequence })
}

pub fn is_stale(snapshot: &ClipboardSnapshot) -> bool {
    let now = unsafe { GetClipboardSequenceNumber() };
    now != snapshot.sequence
}

/// Copies `bytes` into a new HGLOBAL and hands it to the (open) clipboard.
fn set_data(format: u32, bytes: &[u8]) -> Result<(), Error> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if h.is_null() {
            return Err("GlobalAlloc failed".into());
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            GlobalFree(h);
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        // On success the clipboard owns the memory.
        if SetClipboardData(format, h as HANDLE).is_null() {
            GlobalFree(h);
            return Err(format!(
                "SetClipboardData({format}) failed (error {})",
                GetLastError()
            )
            .into());
        }
    }
    Ok(())
}

/// Data in this registered format keeps the clipboard content out of Win+V history, cloud
/// clipboard and clipboard managers: our transcript is only on the clipboard for a moment.
fn mark_transient() {
    let name: Vec<u16> = "ExcludeClipboardContentFromMonitorProcessing\0"
        .encode_utf16()
        .collect();
    let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    if format != 0 {
        let _ = set_data(format, &[0]);
    }
}

pub fn set_clipboard_text(text: &str) -> Result<PasteToken, Error> {
    let utf16: Vec<u8> = text
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    let _open = Open::new()?;
    if unsafe { EmptyClipboard() } == 0 {
        return Err(format!("EmptyClipboard failed (error {})", unsafe {
            GetLastError()
        })
        .into());
    }
    set_data(CF_UNICODETEXT as u32, &utf16)?;
    mark_transient();
    drop(_open);
    Ok(PasteToken {
        sequence: unsafe { GetClipboardSequenceNumber() },
    })
}

/// Clipboard listeners open the clipboard within ~2 ms of every change (measured: Logi
/// Options+ twice at 0.6-2.3 ms, WSLg's msrdc.exe up to 3 times at 1-1.8 ms, each for well
/// under a millisecond). A plain Win32 edit box whose WM_PASTE comes then can't open it, and
/// pastes nothing without an error. Waiting a little longer than that costs a few ms per paste.
const CLIPBOARD_SETTLE: Duration = Duration::from_millis(3);
/// After the settle time, a clipboard still open is waited for, up to this in all.
const CLIPBOARD_BUSY_MAX: Duration = Duration::from_millis(50);

pub fn wait_for_clipboard_listeners() -> Option<Duration> {
    let start = Instant::now();
    thread::sleep(CLIPBOARD_SETTLE);
    // Only sees whoever opened it with a window, but that's how listeners open it.
    loop {
        let holder = unsafe { GetOpenClipboardWindow() };
        if holder.is_null() {
            break;
        }
        if start.elapsed() >= CLIPBOARD_BUSY_MAX {
            tracing::warn!(
                "clipboard still in use by {} after {} ms; pasting anyway (the paste may be lost)",
                process_name(holder).unwrap_or_else(|| "another app".into()),
                CLIPBOARD_BUSY_MAX.as_millis()
            );
            break;
        }
        thread::sleep(Duration::from_micros(200));
    }
    Some(start.elapsed())
}

/// The file name of the program that owns `hwnd`, e.g. "logioptionsplus_agent.exe".
fn process_name(hwnd: HWND) -> Option<String> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    // MAX_PATH is enough for nearly every program; a longer path (long-path-aware systems)
    // fails with ERROR_INSUFFICIENT_BUFFER: once more with the longest path Windows has.
    let mut buf = vec![0u16; 260];
    let mut path = None;
    for size in [260, 32_768] {
        buf.resize(size, 0);
        let mut len = size as u32;
        if unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) } != 0 {
            path = Some(String::from_utf16_lossy(&buf[..len as usize]));
            break;
        }
    }
    unsafe { CloseHandle(process) };
    let path = path?;
    path.rsplit('\\').next().map(str::to_owned)
}

pub fn send_paste(before_releasing: impl FnOnce()) -> Result<(), Error> {
    let mut inputs = Vec::with_capacity(12);
    // Modifiers that are down would change the shortcut (Ctrl+Shift+V pastes without
    // formatting in many apps, Ctrl+Alt+V is something else, Win+Ctrl+V opens the sound
    // output panel); release them (our hook ignores these injected key-ups). A held Ctrl is
    // harmless: we press Ctrl anyway (but our Ctrl-up leaves Left Ctrl logically up).
    let held: Vec<u16> = [VK_LSHIFT, VK_RSHIFT, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
        .into_iter()
        .filter(|&vk| vk_down(vk))
        .collect();
    if !held.is_empty() || vk_down(VK_LCONTROL) {
        before_releasing();
    }
    if held
        .iter()
        .any(|vk| [VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN].contains(vk))
    {
        // A lone Alt/Win release would open the menu bar / Start menu: mask it.
        inputs.extend([key_input(VK_MASK, false), key_input(VK_MASK, true)]);
    }
    for vk in held {
        tracing::debug!("releasing held modifier {vk:#x} before pasting");
        inputs.push(key_input(vk, true));
    }
    inputs.extend([
        key_input(VK_CONTROL, false),
        key_input(VK_V, false),
        key_input(VK_V, true),
        key_input(VK_CONTROL, true),
    ]);
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        // Typically UIPI: the focused window belongs to an elevated (admin) process.
        return Err(format!(
            "SendInput sent {sent}/{} keystrokes (error {}); is the focused app running as administrator?",
            inputs.len(),
            unsafe { GetLastError() }
        )
        .into());
    }
    Ok(())
}

fn vk_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: INJECTED_MARK,
            },
        },
    }
}

pub fn restore_clipboard(snapshot: &ClipboardSnapshot, token: &PasteToken) -> Result<bool, Error> {
    if unsafe { GetClipboardSequenceNumber() } != token.sequence {
        return Ok(false);
    }
    let _open = Open::new()?;
    // Re-check now that we hold the clipboard (nobody can change it while it's open).
    if unsafe { GetClipboardSequenceNumber() } != token.sequence {
        return Ok(false);
    }
    if unsafe { EmptyClipboard() } == 0 {
        return Err(format!("EmptyClipboard failed (error {})", unsafe {
            GetLastError()
        })
        .into());
    }
    for (format, bytes) in &snapshot.formats {
        if let Err(e) = set_data(*format, bytes) {
            tracing::debug!("restoring clipboard format {format}: {e}");
        }
    }
    // The original copy is already in clipboard history; don't add a duplicate.
    if !snapshot.formats.is_empty() {
        mark_transient();
    }
    Ok(true)
}

// ---------------------------------------------------------------------------------------
// Permissions: Windows has no per-app prompts for any of this.

pub fn accessibility_name() -> &'static str {
    "Accessibility"
}

pub fn missing_permissions() -> Vec<Permission> {
    Vec::new()
}

pub fn request_permissions(_missing: &[Permission]) {}

pub fn open_permission_settings(_permission: Permission) {}

// ---------------------------------------------------------------------------------------
// Launch at login: a value in HKCU's Run key. Task Manager's Startup apps on/off switch is
// kept in `Explorer\StartupApproved\Run` under the same name, which is never touched here.

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "AGT";

pub fn set_launch_at_login(on: bool, program: &Path) -> Result<LoginItem, Error> {
    let command = on.then(|| {
        let mut c = OsString::from("\"");
        c.push(program);
        c.push("\"");
        c
    });
    set_run_value(RUN_KEY, RUN_VALUE, command.as_deref())
}

/// Sets HKCU `key`'s `name` value to `command`, or deletes it (`None`), unless it already is.
fn set_run_value(key: &str, name: &str, command: Option<&OsStr>) -> Result<LoginItem, Error> {
    let (key, name) = (wide(key), wide(name));
    let current = read_string(&key, &name)?;
    let code = match (command, current) {
        // Paths are case-insensitive (the stub on disk is `AGT.exe`; this writes `agt.exe`).
        (Some(c), Some(current))
            if current
                .to_string_lossy()
                .eq_ignore_ascii_case(&c.to_string_lossy()) =>
        {
            return Ok(LoginItem::Unchanged);
        }
        (None, None) => return Ok(LoginItem::Unchanged),
        (Some(c), _) => {
            let data = wide(c);
            let code = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            if code == ERROR_SUCCESS {
                return Ok(LoginItem::Registered);
            }
            code
        }
        (None, Some(_)) => {
            let code =
                unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
            match code {
                ERROR_SUCCESS => return Ok(LoginItem::Unregistered),
                // Deleted since it was read.
                ERROR_FILE_NOT_FOUND => return Ok(LoginItem::Unchanged),
                code => code,
            }
        }
    };
    Err(std::io::Error::from_raw_os_error(code as i32).into())
}

/// A REG_SZ value under HKCU (`None` if it or its key doesn't exist; "" if it isn't a REG_SZ).
fn read_string(key: &[u16], name: &[u16]) -> Result<Option<OsString>, Error> {
    let get = |data: *mut c_void, size: &mut u32| unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            data,
            size,
        )
    };
    let mut size = 0;
    let mut code = get(null_mut(), &mut size);
    let mut buf = Vec::new();
    if code == ERROR_SUCCESS {
        buf = vec![0u16; (size as usize).div_ceil(2)];
        code = get(buf.as_mut_ptr().cast(), &mut size);
    }
    match code {
        ERROR_SUCCESS => {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok(Some(OsString::from_wide(&buf[..end])))
        }
        ERROR_FILE_NOT_FOUND => Ok(None),
        // Not a REG_SZ (not as AGT writes it): replaced, or deleted.
        ERROR_UNSUPPORTED_TYPE => Ok(Some(OsString::new())),
        code => Err(std::io::Error::from_raw_os_error(code as i32).into()),
    }
}

/// NUL-terminated UTF-16.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

// ---------------------------------------------------------------------------------------
// Output volume (ducking): the default render endpoint's IAudioEndpointVolume, through
// hand-written vtables (windows-sys has no COM interfaces, and the `windows` crate is a lot of
// code for three calls).

const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xA95664D2_9614_4F35_A746_DE8DB63617E6);
const IID_IAUDIO_ENDPOINT_VOLUME: GUID = GUID::from_u128(0x5CDF2C82_841E_4546_9722_0CF74078229A);
const E_RENDER: i32 = 0;
const E_CONSOLE: i32 = 0;
/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: no output device at all.
const E_NOTFOUND: HRESULT = 0x8007_0490_u32 as HRESULT;

type Fn0 = unsafe extern "system" fn(*mut c_void) -> u32;
type Slot = usize; // a vtable entry we never call

#[repr(C)]
struct UnknownVtbl {
    _query_interface: Slot,
    _add_ref: Fn0,
    release: Fn0,
}

#[repr(C)]
struct EnumeratorVtbl {
    unknown: UnknownVtbl,
    _enum_audio_endpoints: Slot,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> HRESULT,
    get_device: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct DeviceVtbl {
    unknown: UnknownVtbl,
    activate: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        u32,
        *const c_void,
        *mut *mut c_void,
    ) -> HRESULT,
    _open_property_store: Slot,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
    get_state: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
}

/// `DEVICE_STATE_ACTIVE`: present and enabled (not unplugged or disabled).
const DEVICE_STATE_ACTIVE: u32 = 1;

#[repr(C)]
struct EndpointVolumeVtbl {
    unknown: UnknownVtbl,
    _register_control_change_notify: Slot,
    _unregister_control_change_notify: Slot,
    _get_channel_count: Slot,
    _set_master_volume_level: Slot,
    set_master_volume_level_scalar:
        unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> HRESULT,
    _get_master_volume_level: Slot,
    get_master_volume_level_scalar: unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
    _set_channel_volume_level: Slot,
    _set_channel_volume_level_scalar: Slot,
    _get_channel_volume_level: Slot,
    _get_channel_volume_level_scalar: Slot,
    _set_mute: Slot,
    get_mute: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
}

/// An owned COM interface pointer whose vtable is `V`; released on drop.
struct Com<V>(*mut c_void, std::marker::PhantomData<V>);

impl<V> Com<V> {
    fn vtbl(&self) -> &V {
        unsafe { &**(self.0 as *const *const V) }
    }
}

impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        let unknown = unsafe { &**(self.0 as *const *const UnknownVtbl) };
        unsafe { (unknown.release)(self.0) };
    }
}

fn check(hr: HRESULT, what: &str) -> Result<(), Error> {
    if hr < 0 {
        return Err(format!("{what} failed (HRESULT {:#010x})", hr as u32).into());
    }
    Ok(())
}

/// An output device's master volume. Lives on the thread that opened it.
pub struct OutputVolume {
    volume: Com<EndpointVolumeVtbl>,
    /// The endpoint ID, to find the same device again ([`OutputVolume::open`]).
    id: String,
}

/// Prepares the calling thread for [`OutputVolume`] (COM, multithreaded apartment).
pub fn init_volume_thread() {
    // S_FALSE (already initialized) is fine; on failure, opening the device reports it.
    unsafe { CoInitializeEx(null(), COINIT_MULTITHREADED as u32) };
}

fn enumerator() -> Result<Com<EnumeratorVtbl>, Error> {
    let mut p = null_mut();
    check(
        unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut p,
            )
        },
        "CoCreateInstance(MMDeviceEnumerator)",
    )?;
    Ok(Com(p, Default::default()))
}

impl OutputVolume {
    /// `Ok(None)`: there is no default output device.
    pub fn open_default() -> Result<Option<Self>, Error> {
        let enumerator = enumerator()?;
        let mut p = null_mut();
        let hr = unsafe {
            (enumerator.vtbl().get_default_audio_endpoint)(
                enumerator.0,
                E_RENDER,
                E_CONSOLE,
                &mut p,
            )
        };
        if hr == E_NOTFOUND {
            return Ok(None);
        }
        check(hr, "GetDefaultAudioEndpoint")?;
        if p.is_null() {
            return Ok(None);
        }
        Self::activate(Com(p, Default::default()))
    }

    /// The device with this [`OutputVolume::id`]. `Ok(None)`: it's not there (unplugged,
    /// disabled, or gone).
    pub fn open(id: &str) -> Result<Option<Self>, Error> {
        let enumerator = enumerator()?;
        let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
        let mut p = null_mut();
        let hr = unsafe { (enumerator.vtbl().get_device)(enumerator.0, wide.as_ptr(), &mut p) };
        if hr == E_NOTFOUND {
            return Ok(None);
        }
        check(hr, "IMMDeviceEnumerator::GetDevice")?;
        if p.is_null() {
            return Ok(None);
        }
        let device = Com::<DeviceVtbl>(p, Default::default());
        let mut state = 0;
        check(
            unsafe { (device.vtbl().get_state)(device.0, &mut state) },
            "IMMDevice::GetState",
        )?;
        if state != DEVICE_STATE_ACTIVE {
            return Ok(None);
        }
        Self::activate(device)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn activate(device: Com<DeviceVtbl>) -> Result<Option<Self>, Error> {
        let mut wide = null_mut();
        check(
            unsafe { (device.vtbl().get_id)(device.0, &mut wide) },
            "IMMDevice::GetId",
        )?;
        let id = unsafe {
            let len = (0..).take_while(|&i| *wide.add(i) != 0).count();
            let id = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
            CoTaskMemFree(wide as *const c_void);
            id
        };
        let mut p = null_mut();
        check(
            unsafe {
                (device.vtbl().activate)(
                    device.0,
                    &IID_IAUDIO_ENDPOINT_VOLUME,
                    CLSCTX_ALL,
                    null(),
                    &mut p,
                )
            },
            "IMMDevice::Activate(IAudioEndpointVolume)",
        )?;
        Ok(Some(Self {
            volume: Com(p, Default::default()),
            id,
        }))
    }

    /// The volume slider's position, 0..=1.
    pub fn volume(&self) -> Result<f32, Error> {
        let mut v = 0.0;
        check(
            unsafe { (self.volume.vtbl().get_master_volume_level_scalar)(self.volume.0, &mut v) },
            "GetMasterVolumeLevelScalar",
        )?;
        Ok(v)
    }

    pub fn set_volume(&self, v: f32) -> Result<(), Error> {
        check(
            unsafe {
                (self.volume.vtbl().set_master_volume_level_scalar)(self.volume.0, v, null())
            },
            "SetMasterVolumeLevelScalar",
        )
    }

    pub fn muted(&self) -> Result<bool, Error> {
        let mut m = 0;
        check(
            unsafe { (self.volume.vtbl().get_mute)(self.volume.0, &mut m) },
            "GetMute",
        )?;
        Ok(m != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded low-level hook event: (vkCode, scanCode, down, time).
    type Ev = (u32, u32, bool, u32);

    /// Feeds hook events through [`classify`] and the detector, like `hook_proc` does.
    fn run(hotkey: &str, events: &[Ev]) -> Vec<HotkeyEvent> {
        let mut d = Detector::new(&Hotkeys::parse(&[hotkey.to_owned()]).unwrap());
        let t = Instant::now();
        events
            .iter()
            .filter_map(|&(vk, scan, down, time)| {
                let key = classify(vk, scan)?;
                d.on_key(key, down, false, false, time, t).0
            })
            .collect()
    }

    const LWIN_DOWN: Ev = (0x5B, 0x5B, true, 1000);
    // AltGr on a German layout (AltGr+Q = @), as the hook sees it: a fake Left Ctrl with
    // scan code 0x21D and the same timestamp as the Right Alt that follows it.
    const ALTGR_Q: [Ev; 6] = [
        (0xA2, 0x21D, true, 1100),
        (0xA5, 0x38, true, 1100),
        (0x51, 0x10, true, 1150),
        (0x51, 0x10, false, 1200),
        (0xA2, 0x21D, false, 1250),
        (0xA5, 0x38, false, 1250),
    ];

    #[test]
    fn altgr_fake_left_ctrl_is_not_the_chord_key() {
        let mut events = vec![LWIN_DOWN];
        events.extend(ALTGR_Q);
        // Holding Win and typing with AltGr: the chord must not even start.
        assert_eq!(run("LeftCtrl+LeftWin", &events), []);
        assert_eq!(run("LeftCtrl", &ALTGR_Q), []);
        // A real Left Ctrl (scan code 0x1D) still completes the chord.
        let real = [
            LWIN_DOWN,
            (0xA2, 0x1D, true, 1100),
            (0xA2, 0x1D, false, 2000),
        ];
        let got = run("LeftCtrl+LeftWin", &real);
        assert!(
            matches!(got[..], [HotkeyEvent::Down(..), HotkeyEvent::Up(_)]),
            "{got:?}"
        );
    }

    #[test]
    fn other_modifiers_neither_cancel_nor_count() {
        // Ctrl+Win held, then Shift and Alt pressed and released: still one dictation.
        let events = [
            (0xA2, 0x1D, true, 0),
            LWIN_DOWN,
            (0xA0, 0x2A, true, 1100),
            (0xA0, 0x2A, false, 1200),
            (0xA4, 0x38, true, 1300),
            (0xA4, 0x38, false, 1400),
            (0x5B, 0x5B, false, 2000),
        ];
        let got = run("LeftCtrl+LeftWin", &events);
        assert!(
            matches!(got[..], [HotkeyEvent::Down(..), HotkeyEvent::Up(_)]),
            "{got:?}"
        );
        // Ctrl+Win+Right (switch virtual desktop) cancels.
        let events = [(0xA2, 0x1D, true, 0), LWIN_DOWN, (0x27, 0x4D, true, 1100)];
        let got = run("LeftCtrl+LeftWin", &events);
        assert!(
            matches!(got[..], [HotkeyEvent::Down(..), HotkeyEvent::Cancel]),
            "{got:?}"
        );
        // The generic Shift, Ctrl and Alt codes (some injected input has them) aren't another
        // key either.
        let events = [
            (0xA3, 0x1D, true, 0),
            (0x10, 0x2A, true, 100),
            (0x11, 0x1D, true, 200),
            (0x12, 0x38, true, 300),
            (0x10, 0x2A, false, 400),
            (0xA3, 0x1D, false, 1000),
        ];
        let got = run("RightCtrl", &events);
        assert!(
            matches!(got[..], [HotkeyEvent::Down(..), HotkeyEvent::Up(_)]),
            "{got:?}"
        );
    }

    /// The Run key code, on a key of its own (the real Run key is left alone).
    #[test]
    fn run_value_is_written_only_when_it_changes() {
        use windows_sys::Win32::System::Registry::RegDeleteKeyW;
        /// Deletes the test key, also when an assert fails.
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(&self.0).as_ptr()) };
            }
        }
        let key = format!(r"Software\AGT-test-{}", std::process::id());
        let _cleanup = Cleanup(key.clone());
        let set = |c: Option<&str>| set_run_value(&key, "AGT", c.map(OsStr::new)).unwrap();
        let a = r#""C:\Users\A B\AppData\Local\AGT\agt.exe""#;
        assert_eq!(set(None), LoginItem::Unchanged); // no key yet
        assert_eq!(set(Some(a)), LoginItem::Registered);
        assert_eq!(set(Some(a)), LoginItem::Unchanged);
        assert_eq!(set(Some(&a.to_uppercase())), LoginItem::Unchanged);
        assert_eq!(
            read_string(&wide(&key), &wide("AGT")).unwrap().unwrap(),
            OsStr::new(a)
        );
        assert_eq!(set(Some(r#""D:\AGT\agt.exe""#)), LoginItem::Registered);
        assert_eq!(set(None), LoginItem::Unregistered);
        assert_eq!(set(None), LoginItem::Unchanged);
        assert_eq!(read_string(&wide(&key), &wide("AGT")).unwrap(), None);
        // A value of another type is replaced.
        let n = 1u32;
        let code = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                wide(&key).as_ptr(),
                wide("AGT").as_ptr(),
                windows_sys::Win32::System::Registry::REG_DWORD,
                (&raw const n).cast(),
                4,
            )
        };
        assert_eq!(code, ERROR_SUCCESS);
        assert_eq!(set(Some(a)), LoginItem::Registered);
        assert_eq!(set(None), LoginItem::Unregistered);
    }

    #[test]
    fn paste_may_release_shift_left_ctrl_alt_and_win() {
        let k = |n| Key::parse(n).unwrap();
        assert!(!paste_may_release(k("RightCtrl")));
        assert!(!paste_may_release(k("F13")));
        assert!(!paste_may_release(k("V")));
        for n in [
            "LeftCtrl",
            "LeftShift",
            "RightShift",
            "LeftAlt",
            "RightAlt",
            "LeftWin",
            "RightWin",
        ] {
            assert!(paste_may_release(k(n)), "{n}");
        }
    }
}
