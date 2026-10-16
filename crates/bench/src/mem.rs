//! Process resources for the report: peak resident set, (Windows) GPU dedicated memory and
//! CPU time.

/// Peak working set (Windows) / max RSS (macOS, Linux) of this process so far, in bytes.
#[cfg(windows)]
pub fn peak_rss() -> Option<u64> {
    use windows_sys::Win32::System::{
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: valid pseudo-handle and a correctly sized, writable struct.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (ok != 0).then_some(c.PeakWorkingSetSize as u64)
}

#[cfg(unix)]
pub fn peak_rss() -> Option<u64> {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the provided struct.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return None;
    }
    let max = u.ru_maxrss as u64;
    // Bytes on macOS, kilobytes on Linux.
    Some(if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    })
}

#[cfg(not(any(windows, unix)))]
pub fn peak_rss() -> Option<u64> {
    None
}

/// Dedicated GPU memory this process currently holds (all adapters), from the Windows
/// "GPU Process Memory" performance counters (covers D3D12/DirectML, CUDA and Vulkan alike).
/// Called at the end of the run while the engine is still loaded. `None` if unavailable.
#[cfg(windows)]
pub fn gpu_dedicated_bytes() -> Option<u64> {
    let pid = std::process::id();
    let script = format!(
        "(Get-Counter -ErrorAction SilentlyContinue '\\GPU Process Memory(pid_{pid}_*)\\Dedicated Usage').CounterSamples | Measure-Object -Sum CookedValue | ForEach-Object Sum"
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let v: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(v as u64)
}

#[cfg(not(windows))]
pub fn gpu_dedicated_bytes() -> Option<u64> {
    None
}

/// CPU time (user + kernel) this process has used so far, all threads.
#[cfg(windows)]
pub fn cpu_time() -> std::time::Duration {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let mut t: [FILETIME; 4] = unsafe { std::mem::zeroed() };
    let [c, e, k, u] = &mut t;
    // SAFETY: valid pseudo-handle and four writable FILETIMEs.
    unsafe { GetProcessTimes(GetCurrentProcess(), c, e, k, u) };
    let ticks = |f: &FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    std::time::Duration::from_nanos((ticks(&t[2]) + ticks(&t[3])) * 100)
}

#[cfg(unix)]
pub fn cpu_time() -> std::time::Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the provided struct.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| std::time::Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

#[cfg(not(any(windows, unix)))]
pub fn cpu_time() -> std::time::Duration {
    std::time::Duration::ZERO
}
