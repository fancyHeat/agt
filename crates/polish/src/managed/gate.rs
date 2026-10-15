//! Keeps an idle server from using any CPU.
//!
//! An idle llama-server is not idle: on Windows its HTTP listener polls every millisecond
//! (~1% of a core, ~1000 wake-ups a second); on macOS it wakes ~600 times a second. All day.
//! So between requests AGT suspends it and resumes it for each request; both take
//! microseconds, and a suspended process keeps its memory but uses no CPU at all.
//!
//! - Windows: `NtSuspendProcess` / `NtResumeProcess` on the server process.
//! - Unix: `SIGSTOP` / `SIGCONT` to the process group the watchdog leads (watchdog, its
//!   pipe reader and the server). If AGT dies while they're stopped, the group becomes an
//!   orphaned process group with stopped members, so the kernel sends it SIGHUP and SIGCONT
//!   (POSIX), which ends the watchdog and the server; the pipe reader would also wake up.

use std::sync::{Arc, Mutex};

pub(crate) struct Gate {
    #[cfg(windows)]
    process: Option<std::os::windows::io::OwnedHandle>,
    /// The watchdog's process group.
    #[cfg(unix)]
    group: Option<i32>,
    /// Requests in flight, and whether the process is suspended.
    state: Mutex<(u32, bool)>,
}

/// Holds the server awake; it may be suspended again once every guard is dropped.
pub(crate) struct Awake(Arc<Gate>);

impl Gate {
    /// A gate that never suspends anything.
    #[cfg(any(test, not(any(windows, unix))))]
    pub fn none() -> Arc<Self> {
        Arc::new(Self {
            #[cfg(windows)]
            process: None,
            #[cfg(unix)]
            group: None,
            state: Mutex::new((0, false)),
        })
    }

    #[cfg(windows)]
    pub fn for_child(child: &std::process::Child) -> Arc<Self> {
        use std::os::windows::io::AsHandle;
        let process = child
            .as_handle()
            .try_clone_to_owned()
            .map_err(|e| tracing::warn!("llama-server will not be suspended when idle: {e}"))
            .ok();
        Arc::new(Self {
            process,
            state: Mutex::new((0, false)),
        })
    }

    /// `child` must lead its own process group (`CommandExt::process_group(0)`).
    #[cfg(unix)]
    pub fn for_child(child: &std::process::Child) -> Arc<Self> {
        Arc::new(Self {
            group: i32::try_from(child.id()).ok(),
            state: Mutex::new((0, false)),
        })
    }

    #[cfg(not(any(windows, unix)))]
    pub fn for_child(_: &std::process::Child) -> Arc<Self> {
        Self::none()
    }

    /// Resumes the server if it's suspended; it stays awake while the guard lives.
    pub fn enter(self: &Arc<Self>) -> Awake {
        let mut s = self.state.lock().unwrap();
        if s.1 {
            s.1 = !self.set_suspended(false);
        }
        s.0 += 1;
        Awake(self.clone())
    }

    /// Resumes the server for good (it's being stopped: it must run to exit, and nothing may
    /// suspend it, or what's left of its process group, again).
    pub fn retire(self: &Arc<Self>) {
        std::mem::forget(self.enter());
    }

    /// Whether the process is currently suspended (for tests and logs).
    #[cfg(test)]
    pub fn is_suspended(&self) -> bool {
        self.state.lock().unwrap().1
    }

    /// Suspends (`true`) or resumes the process; returns whether that worked.
    #[cfg(windows)]
    fn set_suspended(&self, suspend: bool) -> bool {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtSuspendProcess(process: *mut std::ffi::c_void) -> i32;
            fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
        }
        let Some(h) = &self.process else {
            return false;
        };
        // SAFETY: `h` is a valid process handle we own (with PROCESS_ALL_ACCESS, as returned
        // by CreateProcess and duplicated with the same access).
        let status = unsafe {
            if suspend {
                NtSuspendProcess(h.as_raw_handle())
            } else {
                NtResumeProcess(h.as_raw_handle())
            }
        };
        if status < 0 {
            tracing::debug!("cannot suspend/resume llama-server: NTSTATUS {status:#x}");
        }
        status >= 0
    }

    #[cfg(unix)]
    fn set_suspended(&self, suspend: bool) -> bool {
        let Some(group) = self.group.filter(|g| *g > 1) else {
            return false;
        };
        let sig = if suspend {
            libc::SIGSTOP
        } else {
            libc::SIGCONT
        };
        // SAFETY: plain syscall; a negative pid addresses the process group.
        let ok = unsafe { libc::kill(-group, sig) } == 0;
        if !ok {
            tracing::debug!(
                "cannot suspend/resume llama-server: {}",
                std::io::Error::last_os_error()
            );
        }
        ok
    }

    #[cfg(not(any(windows, unix)))]
    fn set_suspended(&self, _: bool) -> bool {
        false
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap();
        s.0 -= 1;
        if s.0 == 0 && !s.1 {
            s.1 = self.0.set_suspended(true);
        }
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gate")
    }
}

/// Identity: the gate of one server instance.
impl PartialEq for Gate {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    /// A busy child stops using CPU while no guard is held, and runs while one is.
    #[test]
    fn suspends_when_idle_and_resumes_for_requests() {
        let mut child =
            Command::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
                .args(["-NoProfile", "-Command", "while ($true) { }"])
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
        let gate = Gate::for_child(&child);
        let cpu = || cpu_time(&child);
        std::thread::sleep(Duration::from_millis(500));
        drop(gate.enter()); // the supervisor does this once the server is up
        assert!(gate.is_suspended());
        let c0 = cpu();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(cpu(), c0, "a suspended process must not run");

        let awake = gate.enter();
        let nested = gate.enter();
        assert!(!gate.is_suspended());
        let t = Instant::now();
        while cpu() == c0 {
            assert!(t.elapsed() < Duration::from_secs(5), "never resumed");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(awake);
        assert!(!gate.is_suspended(), "still a request in flight");
        drop(nested);
        assert!(gate.is_suspended());
        let _ = child.kill(); // works on a suspended process
        let _ = child.wait();
    }

    fn cpu_time(child: &std::process::Child) -> u64 {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
        let z = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut a, mut b, mut k, mut u) = (z(), z(), z(), z());
        // SAFETY: valid process handle and out pointers.
        unsafe { GetProcessTimes(child.as_raw_handle(), &mut a, &mut b, &mut k, &mut u) };
        let t = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
        t(k) + t(u)
    }
}
