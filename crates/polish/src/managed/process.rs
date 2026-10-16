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
//! - Its start-up output goes to `Env::log_file` (the previous run's kept as `.1`); later
//!   output is dropped (see `StartupLog`): at its default verbosity llama-server logs
//!   per-request timings, not prompts, but an error path could quote a request.

use super::{
    Status,
    assets::Assets,
    gate::{Gate, IDLE_AFTER},
    supervisor::{Endpoint, Launcher, Server},
};
use agt_core::config::{S1Device, S1MiniConfig};
use std::sync::Arc;
use std::{
    fs,
    hash::{BuildHasher, Hasher},
    net::TcpListener,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    time::Duration,
};

/// llama-server's context (tokens) for its one slot: the prompt (~60 tokens + the
/// transcript) plus the answer (about as long). 4096 covers ~5 minutes of dictation; longer
/// ones fail and are pasted raw. The KV cache for it is ~450 MB (f16).
const CTX: u32 = 4096;

pub(crate) struct LlamaLauncher {
    assets: Assets,
    device: S1Device,
    fast_drafting: bool,
    log_file: Option<PathBuf>,
    health: ureq::Agent,
}

impl LlamaLauncher {
    pub fn new(assets: Assets, s1: &S1MiniConfig, log_file: Option<PathBuf>) -> Self {
        let health = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .new_agent();
        Self {
            assets,
            device: s1.device,
            fast_drafting: s1.fast_drafting,
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
            // `GET /slots`, to see whether it's idle before suspending it (see `gate`).
            "--slots",
            // Never download anything (the model is a local file).
            "--offline",
            // No cache of past prompts' KV state in RAM (default: up to 8 GiB). Every
            // dictation is a new prompt and only the fixed ~50-token prefix could be reused,
            // which the slot's own cache already does, so AGT never hits it; it only grew the
            // server by ~25 MB per request.
            "--cache-ram",
            "0",
        ]
        .map(String::from)
        .into();
        if self.fast_drafting {
            // `polish.s1-mini.fast_drafting`. The answer is mostly the transcript again, so
            // draft from the prompt: find the last 3 tokens earlier in the context and propose
            // the (up to) 24 that followed them; one batched pass checks them all and keeps
            // what the model would have written anyway. Up to ~3x faster when the model
            // mostly keeps the (punctuated) transcript, no faster when it rewrites a lot.
            // Batched checking can flip near-tie choices, so now and then a word or a
            // punctuation mark differs (docs/benchmarks.md, "Polish speed").
            a.extend(
                [
                    "--spec-type",
                    "ngram-simple",
                    "--spec-ngram-simple-size-n",
                    "3",
                    "--spec-ngram-simple-size-m",
                    "24",
                    "--spec-draft-n-max",
                    "24",
                ]
                .map(String::from),
            );
        }
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
        // The files are checked against what was verified at download (size + mtime; a
        // changed file is hashed again), so nothing swapped in later is run.
        let exe = self.assets.verified_server_exe()?;
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
        let log = match &self.log_file {
            Some(path) => match rotate_and_create(path) {
                Ok(f) => Some(Arc::new(StartupLog::new(f))),
                Err(e) => {
                    tracing::warn!("cannot write {}: {e}", path.display());
                    None
                }
            },
            None => None,
        };
        let piped = || {
            if log.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            }
        };
        cmd.stdout(piped()).stderr(piped());
        let mut child = spawn(cmd).map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        if let Some(log) = &log {
            if let Some(out) = child.stdout.take() {
                log.clone().tap(out);
            }
            if let Some(err) = child.stderr.take() {
                log.clone().tap(err);
            }
        }
        let root = format!("http://127.0.0.1:{port}");
        let idle_check = {
            let (agent, url, key) = (self.health.clone(), format!("{root}/slots"), key.clone());
            Box::new(move || slots_idle(&agent, &url, &key))
        };
        Ok(Box::new(LlamaServer {
            gate: Gate::for_child(&child, idle_check, IDLE_AFTER),
            watchdog_pipe: child.stdin.take(),
            child,
            root,
            key,
            health: self.health.clone(),
            log,
        }))
    }
}

/// `GET /slots`: whether no slot is processing (an abandoned request may still be running).
/// If the endpoint can't be read, the idle period alone has to do.
fn slots_idle(agent: &ureq::Agent, url: &str, key: &str) -> bool {
    let slots = agent
        .get(url)
        .header("authorization", &format!("Bearer {key}"))
        .call()
        .ok()
        .filter(|r| r.status() == 200)
        .and_then(|mut r| r.body_mut().read_to_string().ok())
        .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok());
    match slots.as_ref().and_then(|v| v.as_array()) {
        Some(slots) => !slots.iter().any(|s| s["is_processing"] == true),
        None => {
            tracing::debug!("cannot read llama-server's /slots; assuming idle");
            true
        }
    }
}

/// llama-server's output, written to its log file only until it's up (its first passed
/// health check, or a start-up line, whichever comes first): devices, memory, load
/// errors. After that it logs per-request lines, and even
/// at its default verbosity an error path could quote part of a request, i.e. dictated
/// text, so later output is read (the server must never block on a full pipe) and dropped.
struct StartupLog {
    file: std::sync::Mutex<Option<fs::File>>,
}

/// Lines that mean llama-server finished starting (wording differs between its versions).
const STARTED: &[&str] = &[
    "listening on http",
    "starting the main loop",
    "all slots are idle",
];

/// Parts of lines about requests: if one shows up before a start-up line (an unknown
/// version's wording), the log stops there, without it.
const REQUEST_LINE: &[&str] = &["| task ", "launch_slot", "request:"];

/// A start-up log longer than this is cut off (it's a few KB normally).
const STARTUP_LOG_MAX: u64 = 256 * 1024;

impl StartupLog {
    fn new(f: fs::File) -> Self {
        Self {
            file: std::sync::Mutex::new(Some(f)),
        }
    }

    /// Copies `stream` line by line on a helper thread (see the type docs).
    fn tap(self: Arc<Self>, stream: impl std::io::Read + Send + 'static) {
        let spawned = std::thread::Builder::new()
            .name("llama-server-log".into())
            .spawn(move || self.copy(stream));
        if let Err(e) = spawned {
            tracing::warn!("cannot read llama-server's output: {e}");
        }
    }

    /// Stops writing: the server passed its health check (or a line said it started).
    fn close(&self) {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = file.as_mut() {
            use std::io::Write;
            let _ = f.write_all(b"[AGT: later output is not kept, it may describe requests]\n");
        }
        *file = None;
    }

    /// Reads `stream` to its end, writing lines to the file until the server has started.
    /// It keeps reading after that, and after any error, so the server never blocks on a
    /// full pipe (nor gets SIGPIPE).
    fn copy(&self, mut stream: impl std::io::Read) {
        use std::io::{ErrorKind, Write};
        let (mut buf, mut line) = (vec![0u8; 16 * 1024], Vec::new());
        let mut written = 0u64;
        let mut errors = 0u64;
        loop {
            let n = match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    // Shouldn't happen on a pipe. Keep the read end open (closing it could
                    // kill the server with SIGPIPE), without spinning.
                    errors += 1;
                    std::thread::sleep(Duration::from_millis((50 * errors).min(1000)));
                    continue;
                }
            };
            let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
            if file.is_none() {
                continue; // drained to nowhere
            }
            for &b in &buf[..n] {
                if b != b'\n' {
                    line.push(b);
                    if line.len() <= 64 * 1024 {
                        continue;
                    }
                }
                let Some(f) = file.as_mut() else { break };
                let text = String::from_utf8_lossy(&line);
                let request = REQUEST_LINE.iter().any(|m| text.contains(m));
                if !request {
                    written += line.len() as u64 + 1;
                    let _ = f.write_all(&line);
                    let _ = f.write_all(b"\n");
                }
                let started = STARTED.iter().any(|m| text.contains(m));
                line.clear();
                if request || started || written > STARTUP_LOG_MAX {
                    let _ =
                        f.write_all(b"[AGT: later output is not kept, it may describe requests]\n");
                    *file = None;
                }
            }
        }
    }
}

/// Keeps the previous run's log as `<name>.1` (e.g. the one that says why it crashed).
fn rotate_and_create(path: &std::path::Path) -> std::io::Result<fs::File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::rename(path, old);
    }
    fs::File::create(path)
}

struct LlamaServer {
    child: Child,
    /// Unix: AGT's end of the watchdog's stdin; dropping it stops the server. Kept out of
    /// `child`, because `Child::wait` closes the stdin it holds.
    watchdog_pipe: Option<ChildStdin>,
    /// `http://127.0.0.1:<port>`
    root: String,
    key: String,
    health: ureq::Agent,
    gate: Arc<Gate>,
    /// Closed when the first health check passes: no request can come before that.
    log: Option<Arc<StartupLog>>,
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
        let healthy = self
            .health
            .get(format!("{}/health", self.root))
            .call()
            .is_ok_and(|r| r.status() == 200);
        if healthy && let Some(log) = self.log.take() {
            log.close();
        }
        healthy
    }

    fn try_wait(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(s)) => Some(s.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("cannot wait for it: {e}")),
        }
    }

    fn wait(&mut self) -> String {
        let status = match self.child.wait() {
            Ok(s) => s.to_string(),
            Err(e) => format!("cannot wait for it: {e}"),
        };
        // Its process (group) is gone: nothing may be suspended under its name any more.
        self.gate.retire();
        status
    }

    fn kill(&mut self) {
        // A suspended server must run again to exit.
        self.gate.retire();
        // Unix: closing the watchdog's stdin makes it stop the server...
        drop(self.watchdog_pipe.take());
        // ...but one hung in start-up may not act on SIGTERM soon: kill the whole group.
        #[cfg(unix)]
        if let Ok(group) = i32::try_from(self.child.id())
            && group > 1
        {
            // SAFETY: plain syscall; a negative pid addresses the watchdog's process group.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
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
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(WATCHDOG)
        .arg("agt-llama-server")
        .arg(exe)
        .args(args)
        .stdin(Stdio::piped())
        // Its own process group, so `gate` can stop and continue all of it at once.
        .process_group(0);
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
    fn server_log_keeps_only_start_up() {
        let path = std::env::temp_dir().join(format!("agt-startup-log-{}", std::process::id()));
        let log = StartupLog::new(fs::File::create(&path).unwrap());
        let out = "load_tensors: offloaded 29/29 layers to GPU\n\
                   main: server is listening on http://127.0.0.1:4242 - starting the main loop\n\
                   srv  log_server_r: request: {\"messages\":[\"dictated words\"]}\n";
        log.copy(out.as_bytes());
        drop(log);
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert!(text.contains("offloaded 29/29"), "{text}");
        assert!(text.contains("starting the main loop"), "{text}");
        assert!(!text.contains("dictated words"), "{text}");

        // Closed by the health check: nothing more is written, but the pipe is still read
        // to its end, also past a read error.
        struct Flaky(Vec<std::io::Result<&'static [u8]>>);
        impl std::io::Read for Flaky {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop() {
                    None => Ok(0),
                    Some(Err(e)) => Err(e),
                    Some(Ok(b)) => {
                        buf[..b.len()].copy_from_slice(b);
                        Ok(b.len())
                    }
                }
            }
        }
        let log = StartupLog::new(fs::File::create(&path).unwrap());
        let mut chunks = vec![
            Ok(&b"load_model: loading\n"[..]),
            Err(std::io::Error::other("glitch")),
            Ok(&b"init: threads = 8\n"[..]),
        ];
        chunks.reverse();
        log.copy(Flaky(chunks));
        log.close();
        let mut stream = Flaky(vec![Ok(&b"srv something: dictated words\n"[..])]);
        log.copy(&mut stream);
        assert!(stream.0.is_empty(), "drained");
        drop(log);
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert!(
            text.contains("loading") && text.contains("threads = 8"),
            "{text}"
        );
        assert!(!text.contains("dictated words"), "{text}");

        // b11669's wording, and a request line where a start-up line was expected.
        for (out, kept) in [
            (
                "srv  llama_server: model loaded\n\
                 srv  llama_server: listening on http://127.0.0.1:64599\n\
                 slot launch_slot_: id  0 | task 0 | processing task\n",
                "listening on",
            ),
            (
                "srv    load_model: loading model\n\
                 slot launch_slot_: id  0 | task 0 | processing task\n\
                 srv  later: dictated words\n",
                "loading model",
            ),
        ] {
            let log = StartupLog::new(fs::File::create(&path).unwrap());
            log.copy(out.as_bytes());
            drop(log);
            let text = fs::read_to_string(&path).unwrap();
            let _ = fs::remove_file(&path);
            assert!(text.contains(kept), "{text}");
            assert!(!text.contains("task 0"), "{text}");
            assert!(!text.contains("dictated words"), "{text}");
        }
    }

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
            &S1MiniConfig::default(),
            None,
        );
        let a = l.args(4242).join(" ");
        for want in [
            "--host 127.0.0.1 --port 4242",
            r#"--jinja --chat-template-kwargs {"enable_thinking":false} --temp 0"#,
            "--n-gpu-layers all",
            "--cache-ram 0",
        ] {
            assert!(a.contains(want), "{a}");
        }
        // Drafting only when the setting is on.
        assert!(!a.contains("--spec"), "{a}");
        let l = LlamaLauncher {
            fast_drafting: true,
            ..l
        };
        let a = l.args(1).join(" ");
        assert!(
            a.contains("--spec-type ngram-simple --spec-ngram-simple-size-n 3"),
            "{a}"
        );
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
    fn unix_gate_stops_and_continues_the_server() {
        use std::time::Instant;
        let pid_file = std::env::temp_dir().join(format!("agt-gate-{}", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let mut child = spawn(command(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), script],
        ))
        .unwrap();
        let pipe = child.stdin.take();
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
        // Whether the server is stopped, once the signal has arrived (it's asynchronous).
        let stopped = |pid: &str| {
            let is = || {
                let out = Command::new("ps")
                    .args(["-o", "stat=", "-p", pid])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).trim().starts_with('T')
            };
            std::thread::sleep(Duration::from_millis(200));
            is()
        };
        let gate = Gate::for_child(&child, Box::new(|| true), Duration::from_millis(100));
        gate.start();
        assert!(stopped(&pid), "the idle server must be stopped");
        assert!(gate.is_suspended());
        let awake = gate.enter().unwrap();
        assert!(!stopped(&pid));
        drop(awake);
        std::thread::sleep(Duration::from_millis(100));
        assert!(stopped(&pid));
        // Stopping it for good: resumed, then ended by the watchdog.
        gate.retire();
        assert!(!stopped(&pid));
        drop(pipe);
        child.wait().unwrap();
        let _ = fs::remove_file(&pid_file);
    }

    /// Run by [`unix_suspended_server_dies_with_a_crashed_agt`] as a stand-in for AGT:
    /// starts a server, lets it be suspended, writes the server's pid, waits to be killed.
    #[cfg(unix)]
    #[test]
    #[ignore = "helper process for unix_suspended_server_dies_with_a_crashed_agt"]
    #[allow(
        clippy::zombie_processes,
        reason = "this process is killed, never exits"
    )]
    fn fake_agt() {
        let Some(pid_file) = std::env::var_os("AGT_TEST_FAKE_AGT_PIDFILE") else {
            return;
        };
        let pid_file = PathBuf::from(pid_file);
        let script = format!("echo $$ > '{}.tmp'; exec sleep 60", pid_file.display());
        let mut child = spawn(command(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), script],
        ))
        .unwrap();
        let _pipe = child.stdin.take();
        let tmp = PathBuf::from(format!("{}.tmp", pid_file.display()));
        while !fs::read_to_string(&tmp).is_ok_and(|s| s.ends_with('\n')) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let gate = Gate::for_child(&child, Box::new(|| true), Duration::from_millis(50));
        gate.start();
        while !gate.is_suspended() {
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::rename(&tmp, &pid_file).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    /// AGT crashes (SIGKILL) while the server is suspended: the orphaned, stopped process
    /// group gets SIGHUP + SIGCONT from the kernel, so the server doesn't linger.
    #[cfg(unix)]
    #[test]
    fn unix_suspended_server_dies_with_a_crashed_agt() {
        use std::time::Instant;
        let pid_file = std::env::temp_dir().join(format!("agt-orphan-{}", std::process::id()));
        let _ = fs::remove_file(&pid_file);
        let mut agt = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "managed::process::tests::fake_agt",
                "--ignored",
                "--nocapture",
            ])
            .env("AGT_TEST_FAKE_AGT_PIDFILE", &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let t = Instant::now();
        let pid = loop {
            if let Ok(s) = fs::read_to_string(&pid_file) {
                break s.trim().to_owned();
            }
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "fake AGT never got ready"
            );
            std::thread::sleep(Duration::from_millis(20));
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
        agt.kill().unwrap(); // SIGKILL: no clean-up code runs
        agt.wait().unwrap();
        let t = Instant::now();
        while alive(&pid) {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "the server outlived AGT"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_file(&pid_file);
    }

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
        // As `LlamaServer` does: keep the pipe out of `child` (`Child::wait` would close it)
        // and wait while it's open; the server keeps running until the pipe closes.
        let pipe = child.stdin.take().unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            let closing = Instant::now();
            drop(pipe);
            closing
        });
        let status = child.wait().unwrap();
        let exited = Instant::now();
        let closed = closer.join().unwrap();
        assert!(exited >= closed, "the server stopped before AGT went away");
        assert!(
            exited - closed < Duration::from_secs(5),
            "{:?}",
            exited - closed
        );
        assert!(!status.success());
        assert!(!alive(&pid), "the server outlived the watchdog");
        let _ = fs::remove_file(&pid_file);

        // The server's own exit status is passed on.
        let mut child = spawn(command(
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "exit 7".into()],
        ))
        .unwrap();
        let _pipe = child.stdin.take();
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
