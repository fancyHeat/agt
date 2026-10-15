//! Running `llama-server` as a child process that can't outlive AGT.
//!
//! - It listens on 127.0.0.1 only, on a port picked free just before the start, and requires
//!   a random API key (passed in the environment, not on the command line), so other local
//!   programs and web pages can't use it.
//! - Windows: no console window (`CREATE_NO_WINDOW`), and the process is put in a job object
//!   with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. AGT never closes the job handle, so Windows
//!   closes it, and kills the server, when AGT exits for any reason (quit, crash, killed).
//! - Unix: started through a tiny `sh` watchdog whose stdin is a pipe from AGT. When AGT
//!   exits for any reason, the pipe closes, `read` returns, and the watchdog sends the server
//!   SIGTERM. The watchdog exits with the server's status, so AGT still sees crashes.
//! - Its output goes to `Env::log_file` (truncated on every start). At its default
//!   verbosity llama-server logs start-up, devices and per-request timings, never prompts.

use super::{
    Status,
    assets::Assets,
    gate::Gate,
    supervisor::{Endpoint, Launcher, Server},
};
use agt_core::config::S1Device;
use std::sync::Arc;
use std::{
    fs,
    hash::{BuildHasher, Hasher},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

/// llama-server's context (tokens) for its one slot: the prompt (~60 tokens + the
/// transcript) plus the answer (about as long). 4096 covers ~5 minutes of dictation; longer
/// ones fail and are pasted raw. The KV cache for it is ~450 MB (f16).
const CTX: u32 = 4096;

pub(crate) struct LlamaLauncher {
    assets: Assets,
    device: S1Device,
    log_file: Option<PathBuf>,
    health: ureq::Agent,
}

impl LlamaLauncher {
    pub fn new(assets: Assets, device: S1Device, log_file: Option<PathBuf>) -> Self {
        let health = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .new_agent();
        Self {
            assets,
            device,
            log_file,
            health,
        }
    }

    /// llama-server's arguments (without the program).
    fn args(&self, port: u16) -> Vec<String> {
        let mut a: Vec<String> = [
            "-m",
            &self.assets.model_path().to_string_lossy(),
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            // S1-mini was trained with thinking off, which only the jinja template can do.
            "--jinja",
            "--chat-template-kwargs",
            r#"{"enable_thinking":false}"#,
            // Greedy decoding (requests say so too); the GGUF's metadata suggests 0.6.
            "--temp",
            "0",
            "--ctx-size",
            &CTX.to_string(),
            // One request at a time; a single slot gets the whole context.
            "--parallel",
            "1",
            "--no-webui",
            // Never download anything (the model is a local file).
            "--offline",
        ]
        .map(String::from)
        .into();
        a.extend(
            match self.device {
                // Windows (Vulkan): read the weights instead of memory-mapping them; they live
                // in VRAM anyway, and a mapping keeps the whole file resident (640 vs 230 MB
                // working set). On a Mac, Metal uses the mapping directly (unified memory).
                S1Device::Auto if cfg!(windows) => {
                    &["--n-gpu-layers", "all", "--load-mode", "none"][..]
                }
                S1Device::Auto => &["--n-gpu-layers", "all"][..],
                S1Device::Cpu => &["--n-gpu-layers", "0", "--device", "none"][..],
            }
            .iter()
            .map(|s| s.to_string()),
        );
        a
    }
}

impl Launcher for LlamaLauncher {
    fn prepare(&mut self, report: &mut dyn FnMut(Status)) -> Result<(), String> {
        self.assets.ensure(report)
    }

    fn launch(&mut self) -> Result<Box<dyn Server>, String> {
        let exe = self.assets.server_exe();
        let port = free_port()?;
        let key = random_key();
        let args = self.args(port);
        tracing::info!(
            "starting {} on 127.0.0.1:{port} ({:?})",
            exe.display(),
            self.device
        );
        let mut cmd = command(&exe, &args);
        // Settings for llama-server in the user's environment (LLAMA_ARG_*) must not change
        // ours; the key goes in the environment so it isn't visible in the process list.
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("LLAMA_") {
                cmd.env_remove(k);
            }
        }
        cmd.env("LLAMA_API_KEY", &key);
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        let (stdout, stderr) = match &self.log_file {
            Some(path) => match fs::File::create(path).and_then(|f| Ok((f.try_clone()?, f))) {
                Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
                Err(e) => {
                    tracing::warn!("cannot write {}: {e}", path.display());
                    (Stdio::null(), Stdio::null())
                }
            },
            None => (Stdio::null(), Stdio::null()),
        };
        cmd.stdout(stdout).stderr(stderr);
        let child = spawn(cmd).map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        Ok(Box::new(LlamaServer {
            gate: Gate::for_child(&child),
            child,
            root: format!("http://127.0.0.1:{port}"),
            key,
            health: self.health.clone(),
        }))
    }
}

struct LlamaServer {
    child: Child,
    /// `http://127.0.0.1:<port>`
    root: String,
    key: String,
    health: ureq::Agent,
    gate: Arc<Gate>,
}

impl Server for LlamaServer {
    fn endpoint(&self) -> Endpoint {
        Endpoint {
            base_url: format!("{}/v1", self.root),
            key: self.key.clone(),
            generation: 0,
            gate: self.gate.clone(),
        }
    }

    /// `GET /health` is 503 while the model loads, 200 once it can serve.
    fn healthy(&mut self) -> bool {
        self.health
            .get(format!("{}/health", self.root))
            .call()
            .is_ok_and(|r| r.status() == 200)
    }

    fn try_wait(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(s)) => Some(s.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("cannot wait for it: {e}")),
        }
    }

    fn wait(&mut self) -> String {
        match self.child.wait() {
            Ok(s) => s.to_string(),
            Err(e) => format!("cannot wait for it: {e}"),
        }
    }

    fn kill(&mut self) {
        // Unix: closing the watchdog's stdin makes it stop the server.
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
    }
}

/// A port that was free a moment ago. If something takes it before llama-server binds it,
/// the server exits and is restarted (on another port).
fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free local port: {e}"))
}

/// 128 random bits as hex, from std's per-process random hash keys (OS randomness).
fn random_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (0..2u8)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u8(i);
            h.write_u128(nanos);
            format!("{:016x}", h.finish())
        })
        .collect()
}

#[cfg(windows)]
fn command(exe: &std::path::Path, args: &[String]) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(windows)]
fn spawn(mut cmd: Command) -> std::io::Result<Child> {
    let mut child = cmd.spawn()?;
    if let Err(e) = job::assign(&child) {
        // Without the job, a crash of AGT would leave the server running: don't run it.
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other(format!(
            "cannot tie llama-server to AGT's lifetime (job object): {e}"
        )));
    }
    Ok(child)
}

/// `sh -c WATCHDOG agt-llama-server <exe> <args...>`: runs the server in the background
/// with stdin from /dev/null, waits for EOF on the original stdin (fd 3) in a second
/// background job and then stops the server, and exits with the server's status.
#[cfg(unix)]
const WATCHDOG: &str = r#"exec 3<&0 </dev/null
"$@" 3<&- &
pid=$!
{ read -r _ <&3; kill -TERM "$pid" 2>/dev/null; } &
watch=$!
exec 3<&-
wait "$pid"
status=$?
kill "$watch" 2>/dev/null
exit "$status""#;

#[cfg(unix)]
fn command(exe: &std::path::Path, args: &[String]) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(WATCHDOG)
        .arg("agt-llama-server")
        .arg(exe)
        .args(args)
        .stdin(Stdio::piped());
    cmd
}

#[cfg(unix)]
fn spawn(mut cmd: Command) -> std::io::Result<Child> {
    cmd.spawn()
}

#[cfg(windows)]
mod job {
    use std::{ffi::c_void, os::windows::io::AsRawHandle, process::Child, sync::OnceLock};
    use windows_sys::Win32::{
        Foundation::HANDLE,
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
    };

    /// The job, created once and never closed (its handle closes when AGT exits).
    pub(super) static JOB: OnceLock<Result<usize, String>> = OnceLock::new();

    fn create() -> Result<usize, String> {
        // SAFETY: plain Win32 calls with valid arguments; the info struct is zeroed POD.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(job as usize)
        }
    }

    pub fn assign(child: &Child) -> Result<(), String> {
        let job = JOB.get_or_init(create).clone()?;
        // SAFETY: both handles are valid for the duration of the call.
        let ok =
            unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_random_hex() {
        let (a, b) = (random_key(), random_key());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn args_pin_s1_minis_required_settings() {
        let dir = std::env::temp_dir().join("agt-args-test");
        let l = LlamaLauncher::new(
            Assets::for_this_platform(&dir).unwrap_or_else(|_| Assets::test(&dir)),
            S1Device::Auto,
            None,
        );
        let a = l.args(4242).join(" ");
        for want in [
            "--host 127.0.0.1 --port 4242",
            r#"--jinja --chat-template-kwargs {"enable_thinking":false} --temp 0"#,
            "--n-gpu-layers all",
        ] {
            assert!(a.contains(want), "{a}");
        }
        let l = LlamaLauncher {
            device: S1Device::Cpu,
            ..l
        };
        assert!(
            l.args(1)
                .join(" ")
                .ends_with("--n-gpu-layers 0 --device none")
        );
    }

    /// A child that would run "forever" dies with the watchdog's stdin (AGT's end of the
    /// pipe), and its exit status comes through.
    #[cfg(unix)]
    #[test]
    fn unix_watchdog_stops_the_server_when_agt_goes_away() {
        use std::time::Instant;
        let pid_file = std::env::temp_dir().join(format!("agt-watchdog-{}", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let mut child = spawn(command(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), script],
        ))
        .unwrap();
        let t = Instant::now();
        let pid = loop {
            if let Ok(s) = fs::read_to_string(&pid_file)
                && s.ends_with('\n')
            {
                break s.trim().to_owned();
            }
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        };
        let alive = |pid: &str| {
            Command::new("kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive(&pid));
        assert!(child.try_wait().unwrap().is_none());
        let t = Instant::now();
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(!status.success());
        assert!(!alive(&pid), "the server outlived the watchdog");
        let _ = fs::remove_file(&pid_file);

        // The server's own exit status is passed on.
        let mut child = spawn(command(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "exit 7".into()],
        ))
        .unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }

    /// The child is in a kill-on-close job: a process in it can't outlive the job handle.
    #[cfg(windows)]
    #[test]
    fn windows_children_are_in_the_kill_on_close_job() {
        use windows_sys::Win32::{Foundation::HANDLE, System::JobObjects::IsProcessInJob};
        let mut cmd = command(
            std::path::Path::new("C:\\Windows\\System32\\cmd.exe"),
            &["/c".into(), "ping -n 30 127.0.0.1 >nul".into()],
        );
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn(cmd).unwrap();
        let job = super::job::JOB.get().unwrap().clone().unwrap();
        let mut inside = 0;
        // SAFETY: valid handles; `inside` is a valid out pointer.
        let ok = unsafe {
            use std::os::windows::io::AsRawHandle;
            IsProcessInJob(child.as_raw_handle() as HANDLE, job as HANDLE, &mut inside)
        };
        let _ = child.kill();
        let _ = child.wait();
        assert!(ok != 0 && inside != 0);
    }
}
