//! Keeps an idle server from using any CPU.
//!
//! An idle llama-server is not idle: on Windows its HTTP listener (cpp-httplib) polls every
//! millisecond (~1.2% of a core, ~1000 wake-ups a second); on macOS it wakes ~600 times a
//! second. All day, and no llama-server option changes that. So once it has had nothing to
//! do for a while, AGT suspends it, and resumes it for the next request (microseconds); a
//! suspended process keeps its memory but uses no CPU at all.
//!
//! When to suspend matters: after a request that timed out on AGT's side, the server is
//! still working on the abandoned task (it notices the closed connection only about once a
//! second); frozen mid-task, that work would be queued ahead of every later dictation. So a
//! timer thread suspends only after [`IDLE_AFTER`] without requests *and* once the server
//! itself reports that no slot is processing (`GET /slots`); otherwise it checks again
//! later. Back-to-back dictations never pay a suspend/resume either.
//!
//! - Windows: `NtSuspendProcess` / `NtResumeProcess` on the server process.
//! - Unix: `SIGSTOP` / `SIGCONT` to the process group the watchdog leads (watchdog, its
//!   pipe reader and the server). If AGT dies while they're stopped, the group becomes an
//!   orphaned process group with stopped members, so the kernel sends it SIGHUP and SIGCONT
//!   (POSIX), which ends the watchdog and the server; the pipe reader would also wake up.

use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// How long the server must have been without requests before it's suspended.
pub(crate) const IDLE_AFTER: Duration = Duration::from_secs(8);

/// Suspends (`true`) or resumes (`false`) the server; returns whether that worked.
pub(crate) type Suspend = Box<dyn Fn(bool) -> bool + Send + Sync>;
/// Whether the server has no task in progress.
pub(crate) type IdleCheck = Box<dyn Fn() -> bool + Send + Sync>;

pub(crate) struct Gate {
    /// `None`: never suspends.
    os: Option<Suspend>,
    idle_check: IdleCheck,
    idle_after: Duration,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    in_flight: u32,
    suspended: bool,
    last_used: Instant,
    /// The server is going away: resumed, and never suspended again.
    retired: bool,
    /// The timer thread runs.
    started: bool,
}

/// Holds the server awake; it may be suspended again once idle.
pub(crate) struct Awake(Arc<Gate>);

impl Gate {
    pub fn new(os: Option<Suspend>, idle_check: IdleCheck, idle_after: Duration) -> Arc<Self> {
        Arc::new(Self {
            os,
            idle_check,
            idle_after,
            state: Mutex::new(State {
                in_flight: 0,
                suspended: false,
                last_used: Instant::now(),
                retired: false,
                started: false,
            }),
            changed: Condvar::new(),
        })
    }

    /// A gate that never suspends anything.
    #[cfg(any(test, not(any(windows, unix))))]
    pub fn none() -> Arc<Self> {
        Self::new(None, Box::new(|| true), IDLE_AFTER)
    }

    /// Suspends `child` (Windows: the process; Unix: its process group, which it must lead)
    /// when idle.
    pub fn for_child(
        child: &std::process::Child,
        idle_check: IdleCheck,
        idle_after: Duration,
    ) -> Arc<Self> {
        Self::new(os_suspend(child), idle_check, idle_after)
    }

    /// Starts suspending when idle (call once the server is up).
    pub fn start(self: &Arc<Self>) {
        if self.os.is_none() {
            return;
        }
        {
            let mut s = self.state.lock().unwrap();
            if s.started || s.retired {
                return;
            }
            s.started = true;
            s.last_used = Instant::now();
        }
        let gate = self.clone();
        let spawned = std::thread::Builder::new()
            .name("s1-mini-idle".into())
            .spawn(move || gate.run());
        if let Err(e) = spawned {
            tracing::warn!("llama-server will not be suspended when idle: {e}");
        }
    }

    /// Resumes the server if it's suspended; it stays awake while the guard lives. Fails at
    /// once if it can't be resumed (a request would only wait for its deadline).
    pub fn enter(self: &Arc<Self>) -> Result<Awake, String> {
        let mut s = self.state.lock().unwrap();
        if s.suspended {
            if !self.set_suspended(false) {
                tracing::warn!("cannot resume the suspended llama-server");
                return Err("cannot resume the suspended llama-server".into());
            }
            s.suspended = false;
        }
        s.in_flight += 1;
        self.changed.notify_all();
        Ok(Awake(self.clone()))
    }

    /// Resumes the server for good (it's being stopped or is gone: it must run to exit, and
    /// nothing may suspend it, or what's left of its process group, again). Ends the timer.
    pub fn retire(&self) {
        let mut s = self.state.lock().unwrap();
        if s.suspended {
            self.set_suspended(false);
            s.suspended = false;
        }
        s.retired = true;
        self.changed.notify_all();
    }

    /// Whether the process is currently suspended.
    #[cfg(test)]
    pub fn is_suspended(&self) -> bool {
        self.state.lock().unwrap().suspended
    }

    fn set_suspended(&self, suspend: bool) -> bool {
        self.os.as_ref().is_some_and(|os| os(suspend))
    }

    /// The timer thread: sleeps (no wake-ups) while the server is busy or suspended.
    fn run(&self) {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.retired {
                return;
            }
            if s.suspended || s.in_flight > 0 {
                s = self.changed.wait(s).unwrap();
                continue;
            }
            let due = s.last_used + self.idle_after;
            let now = Instant::now();
            if now < due {
                s = self.changed.wait_timeout(s, due - now).unwrap().0;
                continue;
            }
            // Idle long enough on our side; ask the server (unlocked: requests may start).
            let stamp = s.last_used;
            drop(s);
            let idle = (self.idle_check)();
            s = self.state.lock().unwrap();
            if s.retired || s.suspended || s.in_flight > 0 || s.last_used != stamp {
                continue;
            }
            if !idle {
                // Still finishing something (e.g. a request AGT gave up on): check later.
                tracing::debug!("llama-server still busy; not suspending it yet");
                s.last_used = Instant::now();
                continue;
            }
            if self.set_suspended(true) {
                s.suspended = true;
            } else {
                tracing::warn!("cannot suspend the idle llama-server; leaving it running");
                while !s.retired {
                    s = self.changed.wait(s).unwrap();
                }
            }
        }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap();
        s.in_flight -= 1;
        s.last_used = Instant::now();
        self.0.changed.notify_all();
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

// Not covered: the microseconds between `spawn` and the job assignment on Windows, or
// between a suspend decision and the syscall; neither can leave anything stuck.

#[cfg(windows)]
fn os_suspend(child: &std::process::Child) -> Option<Suspend> {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSuspendProcess(process: *mut std::ffi::c_void) -> i32;
        fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
    }
    let h = child
        .as_handle()
        .try_clone_to_owned()
        .map_err(|e| tracing::warn!("llama-server will not be suspended when idle: {e}"))
        .ok()?;
    Some(Box::new(move |suspend| {
        // SAFETY: `h` is a valid process handle we own (duplicated from CreateProcess's,
        // with the same full access).
        let status = unsafe {
            if suspend {
                NtSuspendProcess(h.as_raw_handle())
            } else {
                NtResumeProcess(h.as_raw_handle())
            }
        };
        if status < 0 {
            tracing::debug!("NtSuspend/ResumeProcess: NTSTATUS {status:#x}");
        }
        status >= 0
    }))
}

#[cfg(unix)]
fn os_suspend(child: &std::process::Child) -> Option<Suspend> {
    let group = i32::try_from(child.id()).ok().filter(|g| *g > 1)?;
    Some(Box::new(move |suspend| {
        let sig = if suspend {
            libc::SIGSTOP
        } else {
            libc::SIGCONT
        };
        // SAFETY: plain syscall; a negative pid addresses the process group.
        let ok = unsafe { libc::kill(-group, sig) } == 0;
        if !ok {
            tracing::debug!("kill(-{group}): {}", std::io::Error::last_os_error());
        }
        ok
    }))
}

#[cfg(not(any(windows, unix)))]
fn os_suspend(_: &std::process::Child) -> Option<Suspend> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

    const SHORT: Duration = Duration::from_millis(150);

    struct Fake {
        gate: Arc<Gate>,
        suspended: Arc<AtomicBool>,
        calls: Arc<AtomicU32>,
        idle: Arc<AtomicBool>,
    }

    fn fake(resume_ok: bool) -> Fake {
        let suspended = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicU32::new(0));
        let idle = Arc::new(AtomicBool::new(true));
        let (s, c, i) = (suspended.clone(), calls.clone(), idle.clone());
        let gate = Gate::new(
            Some(Box::new(move |suspend| {
                c.fetch_add(1, SeqCst);
                if !suspend && !resume_ok {
                    return false;
                }
                s.store(suspend, SeqCst);
                true
            })),
            Box::new(move || i.load(SeqCst)),
            SHORT,
        );
        Fake {
            gate,
            suspended,
            calls,
            idle,
        }
    }

    fn settle() {
        std::thread::sleep(SHORT * 3);
    }

    #[test]
    fn suspends_only_after_an_idle_period_and_when_the_server_is_idle() {
        let f = fake(true);
        f.gate.start();
        assert!(!f.suspended.load(SeqCst), "not right away");
        settle();
        assert!(f.suspended.load(SeqCst) && f.gate.is_suspended());

        // A request resumes it; it isn't suspended again right after (the server may still
        // be finishing up), nor while the server says it's busy.
        let awake = f.gate.enter().unwrap();
        assert!(!f.suspended.load(SeqCst));
        f.idle.store(false, SeqCst);
        drop(awake);
        std::thread::sleep(SHORT / 3);
        assert!(!f.suspended.load(SeqCst), "suspended right after a request");
        settle();
        assert!(
            !f.suspended.load(SeqCst),
            "suspended while the server is busy"
        );
        f.idle.store(true, SeqCst);
        settle();
        assert!(f.suspended.load(SeqCst));

        // Back-to-back requests: no suspend/resume between them.
        drop(f.gate.enter().unwrap());
        let calls = f.calls.load(SeqCst);
        for _ in 0..5 {
            drop(f.gate.enter().unwrap());
        }
        assert_eq!(f.calls.load(SeqCst), calls);

        // Retired: resumed for good, the timer ends.
        settle();
        assert!(f.suspended.load(SeqCst));
        f.gate.retire();
        assert!(!f.suspended.load(SeqCst));
        settle();
        assert!(!f.suspended.load(SeqCst));
    }

    #[test]
    fn a_server_that_cannot_be_resumed_fails_requests_at_once() {
        let f = fake(false);
        f.gate.start();
        settle();
        assert!(f.gate.is_suspended());
        assert!(f.gate.enter().is_err());
    }

    #[test]
    fn none_never_suspends() {
        let g = Gate::none();
        g.start();
        settle();
        assert!(!g.is_suspended());
        drop(g.enter().unwrap());
    }

    /// The real thing: a busy child stops using CPU once suspended, and runs again on enter.
    #[cfg(windows)]
    #[test]
    fn windows_process_is_suspended_and_resumed() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        // A single-threaded busy loop (threads created after a suspend wouldn't be
        // suspended, so nothing that's still starting up).
        let mut child = Command::new("C:\\Windows\\System32\\cmd.exe")
            .args(["/d", "/c", "for /l %i in (0,0,1) do @rem"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let gate = Gate::new(os_suspend(&child), Box::new(|| true), SHORT);
        let cpu = || cpu_time(&child);
        std::thread::sleep(Duration::from_millis(300));
        gate.start();
        std::thread::sleep(SHORT * 2);
        assert!(gate.is_suspended());
        // Suspension is asynchronous for a thread that's running on a CPU right now.
        std::thread::sleep(Duration::from_millis(100));
        let c0 = cpu();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(cpu(), c0, "a suspended process must not run");

        let awake = gate.enter().unwrap();
        let t = Instant::now();
        while cpu() == c0 {
            assert!(t.elapsed() < Duration::from_secs(5), "never resumed");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(awake);
        gate.retire();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(windows)]
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
