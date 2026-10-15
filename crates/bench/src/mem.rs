//! Process memory for the report: peak resident set and (Windows) GPU dedicated memory.

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
    Some(if cfg!(target_os = "macos") { max } else { max * 1024 })
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
