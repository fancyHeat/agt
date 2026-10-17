//! `agt meeting start|stop|status`: the command line talking to the running app, for
//! Shortcuts, Raycast and scripts (and testing without clicking the menu bar).
//!
//! macOS only for now: a Unix socket, `agt.sock` in the data folder (so it follows `AGT_HOME`
//! like everything else), readable and writable by the user only. One line in (the command),
//! one line out (the answer), then the connection closes. The app removes a stale socket file
//! when it starts (it holds the instance lock by then, so no other AGT is listening there) and
//! its own on exit. On Windows the command says it isn't supported yet.
//!
//! Answers: `start` → `started <id>`, `already recording <id>` or `error: ...` (after the
//! meeting started or failed to); `stop` → `stopped <id>` once everything is transcribed and
//! written, or `not recording`; `status` → `idle`, `starting`, `recording <id> <elapsed_s>` or
//! `finishing <id>`. The command exits 1 on an `error: ...` answer, or when AGT isn't running.
//!
//! `start` only with `meeting.command_line` on ([`refusal`]): the socket is the user's, so any
//! program they run could otherwise record through AGT's permissions without a click.

use std::{sync::mpsc::Sender, time::Duration};

/// The socket's file name in [`agt_core::paths::data_dir`].
#[cfg(unix)]
const SOCKET: &str = "agt.sock";

/// A command from `agt meeting <command>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    Status,
}

impl Command {
    /// `start`, `stop` or `status` (surrounding whitespace and the newline ignored).
    pub fn parse(s: &str) -> Result<Command, String> {
        match s.trim() {
            "start" => Ok(Command::Start),
            "stop" => Ok(Command::Stop),
            "status" => Ok(Command::Status),
            "" => Err("no command (start|stop|status)".into()),
            other => Err(format!("unknown command {other:?} (start|stop|status)")),
        }
    }

    #[cfg_attr(
        all(not(unix), not(test)),
        expect(dead_code, reason = "the socket is unix only")
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Start => "start",
            Command::Stop => "stop",
            Command::Status => "status",
        }
    }

    /// How long the app may take to answer: a start opens both captures, a stop waits for
    /// the last segments to be transcribed (the session gives that a minute at most).
    #[cfg_attr(not(unix), expect(dead_code, reason = "the socket is unix only"))]
    fn answer_within(self) -> Duration {
        match self {
            Command::Start => Duration::from_secs(30),
            Command::Stop => Duration::from_secs(90),
            Command::Status => Duration::from_secs(5),
        }
    }
}

/// The answer for `command` when it isn't allowed, before it reaches the app: `start` while
/// `meeting.command_line` is off. Any program running as the user can connect to the socket
/// (it is the user's own), so letting it start a recording through AGT's microphone and system
/// audio permissions is opt-in. `status` and `stop` can't record anything.
#[cfg_attr(
    all(not(unix), not(test)),
    expect(dead_code, reason = "the socket is unix only")
)]
pub fn refusal(command: Command, command_line: bool) -> Option<&'static str> {
    (command == Command::Start && !command_line).then_some(
        "error: starting meetings from the command line is off (set command_line = true under \
         [meeting])",
    )
}

/// Where the app answers the command line.
#[cfg(unix)]
pub fn socket_path() -> std::path::PathBuf {
    agt_core::paths::data_dir().join(SOCKET)
}

/// Where a [`Command`]'s answer goes: one line, without the newline.
pub type Reply = Sender<String>;

const USAGE: &str = "usage: agt meeting start|stop|status";

/// `agt meeting <arg>`: asks the running app, prints its answer. Returns the exit code.
pub fn cli(arg: Option<&str>) -> i32 {
    let command = match Command::parse(arg.unwrap_or_default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agt meeting: {e}\n{USAGE}");
            return 2;
        }
    };
    #[cfg(unix)]
    {
        match unix::request(&socket_path(), command) {
            Ok(answer) => {
                println!("{answer}");
                i32::from(answer.starts_with("error"))
            }
            Err(e) if unix::not_running(&e) => {
                eprintln!("AGT isn't running");
                1
            }
            Err(e) => {
                eprintln!("agt meeting {}: {e}", command.as_str());
                1
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        eprintln!("agt meeting: not supported on Windows yet");
        1
    }
}

#[cfg(unix)]
pub use unix::serve;

#[cfg(unix)]
mod unix {
    use super::{Command, Reply};
    use std::{
        fs,
        io::{self, BufRead as _, BufReader, Read as _, Write as _},
        os::unix::{
            fs::PermissionsExt as _,
            net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

    /// How long the command line or the app waits for the other side to write its line.
    const LINE_WAIT: Duration = Duration::from_secs(5);
    /// A command line is never longer than this.
    const MAX_LINE: u64 = 64;
    /// The longest socket path (`sun_path` holds 104 bytes on macOS, with its NUL).
    const MAX_PATH: usize = 103;

    /// A path the socket can't have (a long `AGT_HOME`), said plainly.
    fn check(path: &Path) -> io::Result<()> {
        if path.as_os_str().len() > MAX_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is too long for a socket", path.display()),
            ));
        }
        Ok(())
    }

    /// The socket, listened on by the `control` thread; dropping it removes the file (the
    /// thread is left blocked in `accept` until the process exits).
    pub struct Listening(PathBuf);

    impl Drop for Listening {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// Listens on `path` (replacing a stale socket file), handing each command to `post` with
    /// where its answer goes. `post` returns false once the app can no longer answer.
    pub fn serve(
        path: PathBuf,
        post: impl Fn(Command, Reply) -> bool + Send + Sync + 'static,
    ) -> io::Result<Listening> {
        check(&path)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let post = Arc::new(post);
        thread::Builder::new()
            .name("control".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let stream = match stream {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!("control: accept failed: {e}");
                            continue;
                        }
                    };
                    let post = post.clone();
                    // A stop waits for the meeting to finish: answer each on its own thread so
                    // a `status` meanwhile isn't stuck behind it.
                    let spawned = thread::Builder::new()
                        .name("control-client".into())
                        .spawn(move || answer(stream, &*post));
                    if let Err(e) = spawned {
                        tracing::warn!("control: cannot answer a command: {e}");
                    }
                }
            })?;
        Ok(Listening(path))
    }

    /// Reads one command from `stream`, has the app answer it, writes the answer.
    fn answer(stream: UnixStream, post: &dyn Fn(Command, Reply) -> bool) {
        let _ = stream.set_read_timeout(Some(LINE_WAIT));
        let _ = stream.set_write_timeout(Some(LINE_WAIT));
        let mut line = String::new();
        let read = BufReader::new((&stream).take(MAX_LINE)).read_line(&mut line);
        let reply = match read
            .map_err(|e| e.to_string())
            .and_then(|_| Command::parse(&line))
        {
            Err(e) => format!("error: {e}"),
            Ok(command) => {
                let (tx, rx) = mpsc::channel();
                if !post(command, tx) {
                    "error: AGT is quitting".to_owned()
                } else {
                    match rx.recv_timeout(command.answer_within()) {
                        Ok(reply) => reply,
                        Err(mpsc::RecvTimeoutError::Timeout) => "error: AGT didn't answer".into(),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            "error: AGT is quitting".into()
                        }
                    }
                }
            }
        };
        let mut stream = stream;
        if let Err(e) = writeln!(stream, "{reply}") {
            tracing::debug!("control: cannot answer: {e}");
        }
    }

    /// Sends `command` to the app listening on `path`; returns its answer.
    pub fn request(path: &Path, command: Command) -> io::Result<String> {
        check(path)?;
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(command.answer_within() + LINE_WAIT))?;
        stream.set_write_timeout(Some(LINE_WAIT))?;
        writeln!(stream, "{}", command.as_str())?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "AGT closed the connection without answering",
            ));
        }
        Ok(line.trim_end().to_owned())
    }

    /// No socket there, or nothing listening on it (a socket file left by a crash).
    pub fn not_running(e: &io::Error) -> bool {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_command_parse() {
        assert_eq!(Command::parse("start"), Ok(Command::Start));
        assert_eq!(Command::parse("stop\n"), Ok(Command::Stop));
        assert_eq!(Command::parse("  status \r\n"), Ok(Command::Status));
        for c in [Command::Start, Command::Stop, Command::Status] {
            assert_eq!(Command::parse(c.as_str()), Ok(c));
        }
        for bad in ["", "pause", "Start", "start now", "stats"] {
            let e = Command::parse(bad).unwrap_err();
            assert!(
                e.contains("start|stop|status") || e.contains("no command"),
                "{bad:?}: {e}"
            );
        }
    }

    #[test]
    fn control_start_is_opt_in() {
        // Off (the default): only `start` is refused, and says how to turn it on.
        assert_eq!(
            refusal(Command::Start, false),
            Some(
                "error: starting meetings from the command line is off (set command_line = \
                 true under [meeting])"
            ),
            "the command exits 1 on an error"
        );
        assert_eq!(refusal(Command::Stop, false), None);
        assert_eq!(refusal(Command::Status, false), None);
        for c in [Command::Start, Command::Stop, Command::Status] {
            assert_eq!(refusal(c, true), None, "{c:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn control_round_trip() {
        let dir = std::env::temp_dir().join(format!("agt-control-{}", std::process::id()));
        let path = dir.join(SOCKET);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A stale file where the socket goes is replaced.
        std::fs::write(&path, b"stale").unwrap();
        let listening = serve(path.clone(), |command, reply| {
            let answer = match command {
                Command::Start => "started 2026-10-16-143005".to_owned(),
                Command::Stop => "not recording".to_owned(),
                // No answer at all: the client gets an error, not a hang.
                Command::Status => return false,
            };
            reply.send(answer).is_ok()
        })
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the user may connect");
        assert_eq!(
            unix::request(&path, Command::Start).unwrap(),
            "started 2026-10-16-143005"
        );
        assert_eq!(
            unix::request(&path, Command::Stop).unwrap(),
            "not recording"
        );
        assert_eq!(
            unix::request(&path, Command::Status).unwrap(),
            "error: AGT is quitting"
        );
        drop(listening);
        assert!(!path.exists(), "the socket is removed on exit");
        let e = unix::request(&path, Command::Status).unwrap_err();
        assert!(unix::not_running(&e), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
