//! Keeps one server running: prepare (download), launch, wait until healthy, serve, and
//! restart after a crash. Independent of llama.cpp ([`Launcher`] / [`Server`]), so it's
//! tested with fake servers.

use super::{Status, gate::Gate};
use std::{
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

/// Two failures (crash, or no healthy start) within this window: give up. So a crash is
/// followed by one restart; a crash days later gets its restart again.
pub(crate) const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Starts and stops servers. Called from the supervisor thread only.
pub(crate) trait Launcher: Send + 'static {
    /// Makes sure everything the server needs is on disk (downloads; blocking). Reports
    /// [`Status::Downloading`] through `report` while it downloads.
    fn prepare(&mut self, report: &mut dyn FnMut(Status)) -> Result<(), String>;
    fn launch(&mut self) -> Result<Box<dyn Server>, String>;
}

/// A running server process.
pub(crate) trait Server: Send {
    /// The OpenAI-compatible base URL (`http://127.0.0.1:<port>/v1`), API key and gate
    /// (`generation` is set by the supervisor).
    fn endpoint(&self) -> Endpoint;
    /// Whether it answers its health check (model loaded).
    fn healthy(&mut self) -> bool;
    /// `Some(description)` once it has exited.
    fn try_wait(&mut self) -> Option<String>;
    /// Blocks until it exits.
    fn wait(&mut self) -> String;
    /// Stops it (and reaps it).
    fn kill(&mut self);
}

pub(crate) struct Timing {
    /// From launch to healthy (model load; the first start on a GPU compiles shaders).
    pub start_timeout: Duration,
    pub poll: Duration,
    pub restart_delay: Duration,
    pub failure_window: Duration,
    /// Download attempts before giving up, and the pause between them.
    pub prepare_attempts: u32,
    pub prepare_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(120),
            poll: Duration::from_millis(100),
            restart_delay: Duration::from_secs(1),
            failure_window: FAILURE_WINDOW,
            prepare_attempts: 3,
            prepare_retry: Duration::from_secs(5 * 60),
        }
    }
}

/// Where the polisher finds the server.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Endpoint {
    pub base_url: String,
    pub key: String,
    /// Increments with every server instance that became ready.
    pub generation: u64,
    /// Hold [`Gate::enter`]'s guard while talking to the server.
    pub gate: Arc<Gate>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum State {
    /// Downloading or starting for the first time.
    Preparing,
    Ready(Endpoint),
    /// Crashed; restarting.
    Down,
    Failed(String),
}

/// The supervisor's state, read by the polisher.
pub(crate) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::Preparing),
            changed: Condvar::new(),
        }
    }
}

impl Shared {
    fn set(&self, s: State) {
        *self.state.lock().unwrap() = s;
        self.changed.notify_all();
    }

    #[cfg(test)]
    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// The server, if it's ready now; never waits.
    pub fn endpoint(&self) -> Result<Endpoint, String> {
        match &*self.state.lock().unwrap() {
            State::Ready(ep) => Ok(ep.clone()),
            other => Err(not_ready(other)),
        }
    }

    /// Waits up to `timeout` for the server to be ready; fails at once if it gave up.
    pub fn wait_ready(&self, timeout: Duration) -> Result<Endpoint, String> {
        let guard = self.state.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |s| {
                matches!(s, State::Preparing | State::Down)
            })
            .unwrap();
        match &*guard {
            State::Ready(ep) => Ok(ep.clone()),
            other => Err(not_ready(other)),
        }
    }
}

fn not_ready(s: &State) -> String {
    match s {
        State::Preparing => "S1-mini is not ready yet (downloading or starting)".into(),
        State::Down => "S1-mini is restarting".into(),
        State::Failed(e) => format!("S1-mini is unavailable: {e}"),
        State::Ready(_) => unreachable!(),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Decision {
    Restart,
    GiveUp,
}

/// Restart after a failure, unless another failure happened within `window` before it.
pub(crate) struct RestartPolicy {
    window: Duration,
    last_failure: Option<Instant>,
}

impl RestartPolicy {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_failure: None,
        }
    }

    pub fn on_failure(&mut self, now: Instant) -> Decision {
        let previous = self.last_failure.replace(now);
        match previous {
            Some(t) if now.saturating_duration_since(t) < self.window => Decision::GiveUp,
            _ => Decision::Restart,
        }
    }
}

/// The supervisor thread's body. Returns only when it gives up.
pub(crate) fn run(
    mut launcher: impl Launcher,
    shared: &Shared,
    report: &(dyn Fn(&Status) + Send + Sync),
    t: &Timing,
) {
    let give_up = |why: String| {
        tracing::error!("S1-mini: giving up: {why}");
        shared.set(State::Failed(why.clone()));
        report(&Status::Failed(why));
    };

    let started = Instant::now();
    let mut attempt = 0;
    loop {
        match launcher.prepare(&mut |s| report(&s)) {
            Ok(()) => break,
            Err(e) => {
                attempt += 1;
                if attempt >= t.prepare_attempts {
                    return give_up(e);
                }
                tracing::warn!("S1-mini: {e}; retrying in {} s", t.prepare_retry.as_secs());
                thread::sleep(t.prepare_retry);
            }
        }
    }

    let mut policy = RestartPolicy::new(t.failure_window);
    let mut generation = 0;
    loop {
        report(&if generation == 0 {
            Status::Starting
        } else {
            Status::Restarting
        });
        let launched = Instant::now();
        let failure = match launcher.launch() {
            Err(e) => e,
            Ok(mut server) => match wait_healthy(server.as_mut(), t) {
                Err(e) => {
                    server.kill();
                    e
                }
                Ok(()) => {
                    generation += 1;
                    let ep = Endpoint {
                        generation,
                        ..server.endpoint()
                    };
                    tracing::info!(
                        "S1-mini ready at {} (server start {:.1} s; {:.1} s since AGT asked for it)",
                        ep.base_url,
                        launched.elapsed().as_secs_f64(),
                        started.elapsed().as_secs_f64(),
                    );
                    // Suspended whenever it has been idle a while (see `gate`).
                    ep.gate.start();
                    shared.set(State::Ready(ep));
                    report(&Status::Ready);
                    let exit = server.wait();
                    shared.set(State::Down);
                    format!("llama-server exited unexpectedly ({exit})")
                }
            },
        };
        match policy.on_failure(Instant::now()) {
            Decision::GiveUp => {
                return give_up(format!(
                    "{failure}; it failed twice within {} min",
                    t.failure_window.as_secs() / 60
                ));
            }
            Decision::Restart => {
                tracing::error!("S1-mini: {failure}; restarting it");
                shared.set(State::Down);
                thread::sleep(t.restart_delay);
            }
        }
    }
}

fn wait_healthy(server: &mut dyn Server, t: &Timing) -> Result<(), String> {
    let deadline = Instant::now() + t.start_timeout;
    loop {
        if let Some(exit) = server.try_wait() {
            return Err(format!("llama-server exited during start-up ({exit})"));
        }
        if server.healthy() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "llama-server did not become ready within {} s",
                t.start_timeout.as_secs()
            ));
        }
        thread::sleep(t.poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    #[test]
    fn policy_restarts_once_per_window() {
        let w = Duration::from_secs(600);
        let t0 = Instant::now();
        let mut p = RestartPolicy::new(w);
        assert_eq!(p.on_failure(t0), Decision::Restart);
        // A crash long after the last one gets a restart again...
        assert_eq!(p.on_failure(t0 + w * 3), Decision::Restart);
        // ...a second one soon after doesn't.
        assert_eq!(
            p.on_failure(t0 + w * 3 + Duration::from_secs(5)),
            Decision::GiveUp
        );
    }

    /// What a fake server does after launch.
    #[derive(Clone, Copy)]
    enum Script {
        /// Becomes healthy after this many polls, then runs until told to exit.
        Healthy(u32),
        /// Exits during start-up.
        ExitsAtStart,
        /// Never becomes healthy.
        Hangs,
    }

    struct FakeLauncher {
        prepare: Vec<Result<(), String>>,
        scripts: Vec<Script>,
        launches: Arc<Mutex<u32>>,
        killed: Arc<Mutex<u32>>,
        /// Each healthy server's `wait` returns when its sender is dropped / sends.
        exits: mpsc::Sender<mpsc::Sender<()>>,
    }

    struct FakeServer {
        script: Script,
        polls: u32,
        port: u32,
        killed: Arc<Mutex<u32>>,
        exit: mpsc::Receiver<()>,
    }

    impl Launcher for FakeLauncher {
        fn prepare(&mut self, report: &mut dyn FnMut(Status)) -> Result<(), String> {
            report(Status::Downloading {
                what: "S1-mini",
                percent: 50,
            });
            if self.prepare.is_empty() {
                Ok(())
            } else {
                self.prepare.remove(0)
            }
        }
        fn launch(&mut self) -> Result<Box<dyn Server>, String> {
            let mut n = self.launches.lock().unwrap();
            *n += 1;
            let script = self
                .scripts
                .get(*n as usize - 1)
                .copied()
                .unwrap_or(Script::ExitsAtStart);
            let (tx, rx) = mpsc::channel();
            self.exits.send(tx).unwrap();
            Ok(Box::new(FakeServer {
                script,
                polls: 0,
                port: 1000 + *n,
                killed: self.killed.clone(),
                exit: rx,
            }))
        }
    }

    impl Server for FakeServer {
        fn endpoint(&self) -> Endpoint {
            Endpoint {
                base_url: format!("http://127.0.0.1:{}/v1", self.port),
                key: "key".into(),
                generation: 0,
                gate: Gate::none(),
            }
        }
        fn healthy(&mut self) -> bool {
            self.polls += 1;
            matches!(self.script, Script::Healthy(n) if self.polls > n)
        }
        fn try_wait(&mut self) -> Option<String> {
            matches!(self.script, Script::ExitsAtStart).then(|| "exit code 1".into())
        }
        fn wait(&mut self) -> String {
            let _ = self.exit.recv();
            "exit code 3".into()
        }
        fn kill(&mut self) {
            *self.killed.lock().unwrap() += 1;
        }
    }

    fn fast() -> Timing {
        Timing {
            start_timeout: Duration::from_millis(200),
            poll: Duration::from_millis(1),
            restart_delay: Duration::from_millis(1),
            failure_window: Duration::from_secs(600),
            prepare_attempts: 2,
            prepare_retry: Duration::from_millis(1),
        }
    }

    struct Run {
        shared: Arc<Shared>,
        statuses: Arc<Mutex<Vec<Status>>>,
        launches: Arc<Mutex<u32>>,
        killed: Arc<Mutex<u32>>,
        exits: mpsc::Receiver<mpsc::Sender<()>>,
        done: mpsc::Receiver<()>,
    }

    fn start(scripts: Vec<Script>, prepare: Vec<Result<(), String>>) -> Run {
        let (exits_tx, exits) = mpsc::channel();
        let launches = Arc::new(Mutex::new(0));
        let killed = Arc::new(Mutex::new(0));
        let launcher = FakeLauncher {
            prepare,
            scripts,
            launches: launches.clone(),
            killed: killed.clone(),
            exits: exits_tx,
        };
        let shared = Arc::new(Shared::default());
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let (s, st) = (shared.clone(), statuses.clone());
        let (done_tx, done) = mpsc::channel();
        thread::spawn(move || {
            let report = move |x: &Status| st.lock().unwrap().push(x.clone());
            run(launcher, &s, &report, &fast());
            let _ = done_tx.send(());
        });
        Run {
            shared,
            statuses,
            launches,
            killed,
            exits,
            done,
        }
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn becomes_ready_restarts_once_then_gives_up() {
        let r = start(vec![Script::Healthy(3), Script::Healthy(0)], vec![]);
        let ep = r.shared.wait_ready(WAIT).unwrap();
        assert_eq!(ep.base_url, "http://127.0.0.1:1001/v1");
        assert_eq!(ep.generation, 1);
        assert_eq!(r.shared.endpoint().unwrap(), ep);

        // The server crashes: restarted on another port, with a new generation.
        drop(r.exits.recv().unwrap());
        let exit2 = r.exits.recv_timeout(WAIT).unwrap();
        let ep2 = loop {
            match r.shared.wait_ready(WAIT) {
                Ok(ep) if ep.generation == 2 => break ep,
                _ => thread::sleep(Duration::from_millis(1)),
            }
        };
        assert_eq!(ep2.base_url, "http://127.0.0.1:1002/v1");

        // It crashes again right away: give up, and say so.
        drop(exit2);
        r.done.recv_timeout(WAIT).unwrap();
        assert!(matches!(r.shared.state(), State::Failed(_)));
        assert!(r.shared.endpoint().unwrap_err().contains("unavailable"));
        assert!(
            r.shared.wait_ready(WAIT).is_err(),
            "must not wait after giving up"
        );
        assert_eq!(*r.launches.lock().unwrap(), 2);
        let statuses = r.statuses.lock().unwrap().clone();
        assert_eq!(
            statuses[0],
            Status::Downloading {
                what: "S1-mini",
                percent: 50
            }
        );
        assert_eq!(
            statuses[1..4],
            [Status::Starting, Status::Ready, Status::Restarting]
        );
        assert_eq!(statuses[4], Status::Ready);
        assert!(matches!(statuses.last(), Some(Status::Failed(_))));
    }

    #[test]
    fn start_failures_count_and_hung_servers_are_killed() {
        let r = start(vec![Script::Hangs, Script::ExitsAtStart], vec![]);
        r.done.recv_timeout(WAIT).unwrap();
        assert_eq!(*r.launches.lock().unwrap(), 2);
        // Every failed start is cleaned up, in particular the hung one.
        assert_eq!(*r.killed.lock().unwrap(), 2);
        assert!(
            r.statuses
                .lock()
                .unwrap()
                .iter()
                .all(|s| *s != Status::Ready)
        );
        let State::Failed(why) = r.shared.state() else {
            panic!("not failed")
        };
        assert!(why.contains("exited during start-up"), "{why}");
    }

    #[test]
    fn download_failures_are_retried_then_give_up() {
        // One failed download, then success.
        let r = start(
            vec![Script::Healthy(0)],
            vec![Err("offline".into()), Ok(())],
        );
        r.shared.wait_ready(WAIT).unwrap();
        assert_eq!(*r.launches.lock().unwrap(), 1);
        // Two failures: never launched.
        let r = start(vec![], vec![Err("offline".into()), Err("offline".into())]);
        r.done.recv_timeout(WAIT).unwrap();
        assert_eq!(*r.launches.lock().unwrap(), 0);
        assert_eq!(r.shared.state(), State::Failed("offline".into()));
    }

    #[test]
    fn not_ready_fails_fast() {
        let shared = Shared::default();
        let t = Instant::now();
        assert!(shared.endpoint().unwrap_err().contains("not ready"));
        assert!(shared.wait_ready(Duration::from_millis(20)).is_err());
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
