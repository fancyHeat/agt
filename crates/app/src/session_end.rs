//! Windows ending the session (shutdown, restart, log off) and sleep/wake.
//!
//! Windows asks top-level windows before ending the session and may kill the process as soon
//! as `WM_ENDSESSION` returns, so the session's end is recorded right there. A hidden window
//! on the main thread gets these: winit's event loop already dispatches its messages.

use std::sync::OnceLock;
use std::time::Duration;

use agt_telemetry::ExitReason;

/// Set once Windows ends the session; it wins over the reason the event loop exits with.
static OS_REASON: OnceLock<ExitReason> = OnceLock::new();

/// The OS-initiated reason, if Windows ended the session.
pub fn os_reason() -> Option<ExitReason> {
    OS_REASON.get().copied()
}

/// `WM_ENDSESSION`'s `lParam`: log off, else shutdown or restart.
#[cfg_attr(not(windows), allow(dead_code))]
fn reason_for(lparam: isize, logoff_flag: u32) -> ExitReason {
    if lparam as u32 & logoff_flag != 0 {
        ExitReason::Logoff
    } else {
        ExitReason::OsShutdown
    }
}

/// Starts listening. Call on the event loop's thread, once it's running.
#[cfg(windows)]
pub fn listen(flush: Duration, history_flush: Duration) {
    use std::cell::Cell;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, ENDSESSION_LOGOFF, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
        RegisterClassW, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION, WNDCLASSW, WS_POPUP,
    };

    thread_local! {
        static FLUSH: Cell<(Duration, Duration)> = const { Cell::new((Duration::ZERO, Duration::ZERO)) };
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if wp != 0 {
                    let reason = reason_for(lp, ENDSESSION_LOGOFF);
                    if OS_REASON.set(reason).is_ok() {
                        tracing::info!("Windows is ending the session: {reason:?}");
                        let (flush, history) = FLUSH.get();
                        agt_telemetry::global().end_session(reason, flush);
                        agt_history::flush(history);
                    }
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

    FLUSH.set((flush, history_flush));
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
pub fn listen(_flush: Duration, _history_flush: Duration) {}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGOFF: u32 = 0x8000_0000;

    #[test]
    fn logoff_flag_means_logoff() {
        assert_eq!(
            reason_for(LOGOFF as i32 as isize, LOGOFF),
            ExitReason::Logoff
        );
        assert_eq!(
            reason_for((LOGOFF | 1) as i32 as isize, LOGOFF),
            ExitReason::Logoff
        );
    }

    #[test]
    fn otherwise_shutdown() {
        assert_eq!(reason_for(0, LOGOFF), ExitReason::OsShutdown);
        // ENDSESSION_CRITICAL / ENDSESSION_CLOSEAPP without log off.
        assert_eq!(reason_for(0x4000_0000, LOGOFF), ExitReason::OsShutdown);
        assert_eq!(reason_for(1, LOGOFF), ExitReason::OsShutdown);
    }
}
