//! Fail fast while the server can't be reached.
//!
//! A connection that's refused (nothing listening, e.g. Ollama not running) or can't be made
//! still costs time: on Windows a refused connection to localhost takes ~2 s of SYN retries
//! (per address: `localhost` is `::1` and `127.0.0.1`), so every dictation would wait for
//! the whole polish deadline before pasting raw. So after a failed request, a background
//! thread tries a plain TCP connect; if that fails too, requests fail at once (pasted raw)
//! until a later background check (one per request, at most one at a time) connects again.

use std::{
    net::{TcpStream, ToSocketAddrs},
    sync::atomic::{AtomicBool, Ordering::SeqCst},
    sync::{Arc, Mutex},
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Reach {
    /// `host:port` of the server.
    authority: String,
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    down: AtomicBool,
    checking: AtomicBool,
    /// For tests: the last check's thread.
    last: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Reach {
    /// `base`: the server's base URL (`http(s)://host[:port]/...`).
    pub fn new(base: &str) -> Self {
        // Host and port only: never the userinfo (this is logged when unreachable).
        let authority = url::Url::parse(base)
            .ok()
            .and_then(|u| Some(format!("{}:{}", u.host_str()?, u.port_or_known_default()?)))
            .unwrap_or_else(|| crate::INVALID_URL.into());
        Self {
            authority,
            state: Arc::default(),
        }
    }

    /// `Err` while the server is known to be unreachable (and checks again meanwhile).
    pub fn ready(&self) -> Result<(), String> {
        if self.state.down.load(SeqCst) {
            self.check();
            return Err(format!(
                "server {} is unreachable (not waiting for it; checking again in the background)",
                self.authority
            ));
        }
        Ok(())
    }

    /// A request failed before getting a response: check in the background whether the
    /// server can be reached at all.
    pub fn check(&self) {
        if self.state.checking.swap(true, SeqCst) {
            return;
        }
        let (state, authority) = (self.state.clone(), self.authority.clone());
        let spawned = std::thread::Builder::new()
            .name("polish-reach".into())
            .spawn(move || {
                let up = connects(&authority);
                let was_down = state.down.swap(!up, SeqCst);
                match (was_down, up) {
                    (false, false) => tracing::warn!(
                        "polish server {authority} is unreachable; dictations are pasted unpolished until it is back"
                    ),
                    (true, true) => tracing::info!("polish server {authority} is reachable again"),
                    _ => {}
                }
                state.checking.store(false, SeqCst);
            });
        match spawned {
            Ok(h) => *self.state.last.lock().unwrap() = Some(h),
            Err(_) => self.state.checking.store(false, SeqCst),
        }
    }

    #[cfg(test)]
    pub fn wait_for_check(&self) {
        if let Some(h) = self.state.last.lock().unwrap().take() {
            h.join().unwrap();
        }
    }

    #[cfg(test)]
    pub fn is_down(&self) -> bool {
        self.state.down.load(SeqCst)
    }
}

fn connects(authority: &str) -> bool {
    let Ok(addrs) = authority.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, CONNECT_TIMEOUT).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_from_base_url() {
        for (base, want) in [
            ("http://localhost:11434/v1", "localhost:11434"),
            ("https://api.anthropic.com", "api.anthropic.com:443"),
            ("http://127.0.0.1/v1", "127.0.0.1:80"),
            ("http://[::1]:8080/v1", "[::1]:8080"),
            ("http://[::1]/v1", "[::1]:80"),
            (
                "https://user:tok@proxy.example/v1?key=s",
                "proxy.example:443",
            ),
        ] {
            assert_eq!(Reach::new(base).authority, want, "{base}");
        }
    }
}
