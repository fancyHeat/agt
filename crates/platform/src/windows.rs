//! Windows: WH_KEYBOARD_LL hook on its own thread, Win32 clipboard, SendInput.

use crate::{Error, HoldDetector, HotkeyEvent, Key, Permission, is_modifier};
use std::{
    cell::RefCell,
    ptr::{null, null_mut},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, GlobalFree, HANDLE, LPARAM, LRESULT, WPARAM},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
            GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        LibraryLoader::GetModuleHandleW,
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Ole::CF_UNICODETEXT,
        Threading::{
            GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput, VK_CONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN,
            VK_RMENU, VK_RSHIFT, VK_RWIN, VK_V,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, HC_ACTION, HHOOK,
            KBDLLHOOKSTRUCT, MSG, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
            UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
        },
    },
};

/// `dwExtraInfo` tag on the keystrokes we synthesize, so our own hook ignores them.
const INJECTED_MARK: usize = 0x4147_5421; // "AGT!"

/// Unassigned virtual key, injected before releasing a held Alt/Win so the release doesn't
/// open the menu bar / Start menu (the same trick AutoHotkey uses).
const VK_MASK: u16 = 0xE8;

/// Posted to the hotkey thread: re-install the hook and forget any hold.
const WM_RESET_HOOK: u32 = WM_APP + 1;

// ---------------------------------------------------------------------------------------
// Hotkey

struct HookState {
    vk: u32,
    swallow: bool,
    detector: HoldDetector,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
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
    key: Key,
    on_event: Box<dyn Fn(HotkeyEvent) + Send>,
) -> Result<HotkeyHandle, Error> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            // Windows silently removes a low-level hook that misses LowLevelHooksTimeout; keep
            // this thread ahead of the inference threads.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
            HOOK.with(|h| {
                *h.borrow_mut() = Some(HookState {
                    vk: key.code as u32,
                    swallow: key.swallowed(),
                    detector: HoldDetector::default(),
                    on_event,
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
    tracing::info!("hotkey: hold {} to dictate", key.name);
    Ok(HotkeyHandle { thread_id })
}

pub fn foreground_window() -> Option<usize> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_null()).then_some(hwnd as usize)
}

pub fn key_down(key: Key) -> Option<bool> {
    // A swallowed key never updates the system's key state.
    (!key.swallowed()).then(|| vk_down(key.code))
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
                    let is_hotkey = kb.vkCode == s.vk;
                    swallow = is_hotkey && s.swallow;
                    // Other modifiers don't cancel: they're part of a shortcut only together
                    // with a non-modifier key, which does cancel. (This also ignores the
                    // fake LeftCtrl that AltGr layouts send with RightAlt.)
                    if (is_hotkey || !is_modifier(kb.vkCode))
                        && let Some(ev) =
                            s.detector.on_key(is_hotkey, down, kb.time, Instant::now())
                    {
                        (s.on_event)(ev);
                    }
                }
            });
        }
    }
    if swallow {
        // F13-F24 hotkeys: apps never see them.
        return 1;
    }
    // Ctrl/Shift hotkeys and every other key pass through.
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

pub fn send_paste() -> Result<(), Error> {
    let mut inputs = Vec::with_capacity(12);
    // Modifiers that are down would change the shortcut (Ctrl+Shift+V pastes without
    // formatting in many apps, Ctrl+Alt+V is something else); release them (our hook ignores
    // these injected key-ups). A held Ctrl is harmless: we press Ctrl anyway.
    let held: Vec<u16> = [VK_LSHIFT, VK_RSHIFT, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
        .into_iter()
        .filter(|&vk| vk_down(vk))
        .collect();
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

pub fn missing_permissions() -> Vec<Permission> {
    Vec::new()
}

pub fn request_permissions(_missing: &[Permission]) {}

pub fn open_permission_settings(_permission: Permission) {}
