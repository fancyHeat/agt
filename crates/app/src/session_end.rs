//! Windows ending the session (shutdown, restart, log off) and sleep/wake.
//!
//! Windows asks top-level windows before ending the session and may kill the process as soon
//! as `WM_ENDSESSION` returns, so the session's end is recorded right there. A hidden window
//! on the main thread gets these: winit's event loop already dispatches its messages.

use std::sync::OnceLock;

use agt_telemetry::ExitReason;

/// Set once Windows ends the session; it wins over the reason the event loop exits with.
static OS_REASON: OnceLock<ExitReason> = OnceLock::new();

/// The OS-initiated reason, if Windows ended the session.
pub fn os_reason() -> Option<ExitReason> {
    OS_REASON.get().copied()
}

/// `ENDSESSION_LOGOFF` and `ENDSESSION_CLOSEAPP` (WinUser.h), kept here so tests run anywhere.
#[cfg_attr(not(windows), allow(dead_code))]
const LOGOFF: u32 = 0x8000_0000;
#[cfg_attr(not(windows), allow(dead_code))]
const CLOSEAPP: u32 = 0x1;

/// `WM_ENDSESSION`'s `lParam`: log off, shutdown or restart; `None` for Restart Manager (an
/// installer closing apps), which isn't the session ending.
#[cfg_attr(not(windows), allow(dead_code))]
fn reason_for(lparam: isize) -> Option<ExitReason> {
    let flags = lparam as u32;
    if flags & LOGOFF != 0 {
        Some(ExitReason::Logoff)
    } else if flags & CLOSEAPP != 0 {
        None
    } else {
        Some(ExitReason::OsShutdown)
    }
}

/// Starts listening. Call on the event loop's thread, once it's running.
#[cfg(windows)]
pub fn listen() {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, RegisterClassW,
        WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION, WNDCLASSW, WS_POPUP,
    };

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if let Some(reason) = reason_for(lp).filter(|_| wp != 0)
                    && OS_REASON.set(reason).is_ok()
                {
                    tracing::info!("Windows is ending the session: {reason:?}");
                    agt_telemetry::global().end_session(reason, crate::TELEMETRY_FLUSH);
                    agt_history::flush(crate::HISTORY_FLUSH);
                }
                0
            }
            WM_POWERBROADCAST => {
                match wp as u32 {
                    PBT_APMSUSPEND => tracing::info!("system suspending"),
                    PBT_APMRESUMEAUTOMATIC => tracing::info!("system resumed"),
                    _ => {}
                }
                1
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }

    let class: Vec<u16> = "AGT.SessionEnd".encode_utf16().chain([0]).collect();
    unsafe {
        let instance = GetModuleHandleW(null_mut());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        // Top-level (not message-only, which misses these broadcasts) and never shown.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null_mut(),
        );
        if hwnd.is_null() {
            tracing::warn!("no session-end window: OS shutdowns will show as unknown");
        }
    }
}

#[cfg(not(windows))]
pub fn listen() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logoff_flag_means_logoff() {
        assert_eq!(reason_for(LOGOFF as i32 as isize), Some(ExitReason::Logoff));
        // Log off wins even when apps are also asked to close.
        let both = (LOGOFF | CLOSEAPP) as i32 as isize;
        assert_eq!(reason_for(both), Some(ExitReason::Logoff));
    }

    #[test]
    fn restart_manager_is_ignored() {
        assert_eq!(reason_for(CLOSEAPP as isize), None);
        // ENDSESSION_CRITICAL with CLOSEAPP is still Restart Manager.
        assert_eq!(reason_for((0x4000_0000 | CLOSEAPP) as isize), None);
    }

    #[test]
    fn otherwise_shutdown() {
        assert_eq!(reason_for(0), Some(ExitReason::OsShutdown));
        // ENDSESSION_CRITICAL: a forced shutdown.
        assert_eq!(reason_for(0x4000_0000), Some(ExitReason::OsShutdown));
    }
}
