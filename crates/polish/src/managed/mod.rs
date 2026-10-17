//! `provider = "s1-mini"`: S1-mini by Superwhisper, run by AGT itself.
//!
//! On start ([`start`]) a background thread:
//! 1. downloads what's missing ([`assets`]), once [`Env::hold_download`] lets it: the
//!    S1-mini GGUF (pinned Hugging Face revision) into the models folder and an official
//!    llama.cpp release build (pinned tag) into AGT's own folder, both SHA-256 verified; the
//!    llama.cpp archive is unpacked with the OS's `tar`;
//! 2. starts `llama-server` on a free 127.0.0.1 port with a random API key ([`process`]),
//!    tied to AGT's lifetime: on Windows it's in a job object that kills it when AGT exits
//!    or crashes; on Unix a small `sh` watchdog kills it when AGT's end of a pipe closes;
//! 3. polls `/health` until the model is loaded, then serves polish requests through the
//!    OpenAI-compatible client, and restarts the server once if it dies; a second failure
//!    within [`supervisor::FAILURE_WINDOW`] gives up (see [`supervisor`]).
//!
//! Until the server is ready, and after giving up, [`Polisher::polish`] fails at once, so
//! dictations are pasted raw. Nothing here ever runs on the hotkey or STT path. Between
//! dictations an idle server is suspended, since it would otherwise poll ([`gate`]).

mod assets;
mod gate;
mod process;
mod supervisor;

use crate::{Api, Llm};
use agt_core::{
    config::PolishConfig,
    polish::{PolishCtx, Polisher, Result},
};
use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use supervisor::{Endpoint, Shared, Timing};

pub use assets::{LLAMA_BUILD, MODEL_DIR, legacy_dir, remove_legacy};

/// What the managed server needs from the app.
#[derive(Clone)]
pub struct Env {
    /// The models folder (`stt.models_dir` or the default); S1-mini goes in a subfolder of
    /// it. It may be shared with others, so nothing AGT runs is put there.
    pub models_dir: PathBuf,
    /// Where the llama.cpp builds are installed and run from. Must be AGT's own per-user
    /// folder: anyone who can write to it can make AGT run their code.
    pub llama_dir: PathBuf,
    /// llama-server's own output (start-up, device, timings; never the text), truncated on
    /// every start. `None`: discarded.
    pub log_file: Option<PathBuf>,
    /// Called from a background thread whenever the managed server's state changes.
    pub on_status: Arc<dyn Fn(&Status) + Send + Sync>,
    /// Called on the S1-mini thread before it downloads anything (not when everything is on
    /// disk); blocks until downloading may start, e.g. after a download the app needs first.
    /// `None`: downloads at once.
    pub hold_download: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// The managed server's state, for the tray (`Display` is the tray line).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Waiting for [`Env::hold_download`] before downloading.
    Queued,
    Downloading {
        what: &'static str,
        percent: u8,
    },
    Starting,
    Ready,
    Restarting,
    /// Gave up; the reason is in the log (and here).
    Failed(String),
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Queued => write!(f, "Polish: waiting to download S1-mini..."),
            Status::Downloading { what, percent } => {
                write!(f, "Polish: downloading {what} ({percent}%)...")
            }
            Status::Starting => write!(f, "Polish: starting S1-mini..."),
            Status::Ready => write!(f, "Polish: on (S1-mini by Superwhisper)"),
            Status::Restarting => write!(f, "Polish: restarting S1-mini..."),
            Status::Failed(_) => write!(f, "Polish: off (S1-mini failed, see log)"),
        }
    }
}

/// Longest [`Polisher::warmup`] waits for the server (a first start includes the download).
const READY_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Starts the background thread (never blocks). Fails only if there is no llama.cpp build
/// for this platform.
pub(crate) fn start(cfg: &PolishConfig, env: &Env) -> std::result::Result<Managed, String> {
    let assets = assets::Assets::for_this_platform(&env.models_dir, &env.llama_dir)?;
    let launcher = process::LlamaLauncher::new(
        assets,
        &cfg.s1_mini,
        env.log_file.clone(),
        env.hold_download.clone(),
    );
    let shared = Arc::new(Shared::default());
    let (s, report) = (shared.clone(), env.on_status.clone());
    std::thread::Builder::new()
        .name("s1-mini".into())
        .spawn(move || supervisor::run(launcher, &s, &*report, &Timing::default()))
        .map_err(|e| format!("cannot start the S1-mini thread: {e}"))?;
    Ok(Managed {
        cfg: cfg.clone(),
        shared,
        llm: Mutex::new(None),
    })
}

pub(crate) struct Managed {
    cfg: PolishConfig,
    shared: Arc<Shared>,
    /// The client for the current server instance (a restart changes port and key).
    llm: Mutex<Option<(u64, Arc<Llm>)>>,
}

impl Managed {
    fn client(&self, ep: &Endpoint) -> Arc<Llm> {
        let mut cached = self.llm.lock().unwrap();
        match &*cached {
            Some((generation, llm)) if *generation == ep.generation => llm.clone(),
            _ => {
                let llm = Arc::new(Llm::new(
                    &self.cfg,
                    Api::OpenAi,
                    &ep.base_url,
                    Some(ep.key.clone()),
                ));
                *cached = Some((ep.generation, llm.clone()));
                llm
            }
        }
    }
}

impl Polisher for Managed {
    fn name(&self) -> String {
        format!("S1-mini by Superwhisper (llama.cpp {LLAMA_BUILD}, run by AGT)")
    }

    /// Waits for the server (including a first download), then sends one request.
    fn warmup(&self) -> Result<()> {
        let ep = self.shared.wait_ready(READY_TIMEOUT)?;
        let _awake = ep.gate.enter()?;
        self.client(&ep).warmup()
    }

    fn polish(&self, text: &str, ctx: &PolishCtx) -> Result<String> {
        let ep = self.shared.endpoint()?;
        let _awake = ep.gate.enter()?;
        self.client(&ep).polish(text, ctx)
    }
}
