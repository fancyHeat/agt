//! The dictation pipeline: hotkey → microphone → speech-to-text → paste.
//!
//! Threads (none of them is the UI thread, and nothing blocks the hook thread):
//! - `hotkey` (agt-platform): low-level keyboard hook; forwards hold events to `audio`.
//! - `audio`: owns the microphone. Starts recording on key down, and on release hands the
//!   samples to `stt`. Short taps and cancelled holds are dropped here. While recording it
//!   also polls the key, in case the hook missed the release (see [`RELEASE_POLL`]), and
//!   takes the new audio: a long recording is cut into segments as it comes in
//!   (`agt_stt::chunk::Live`, cheap), and each finished one goes to `stt` right away, so at
//!   release only the last few seconds are left to transcribe (see [`Feed`]). A double tap
//!   (`hotkey.double_tap_ms`) records hands-free instead: the release doesn't end that
//!   recording, the next press and release does (Esc cancels it, and so does
//!   [`HANDS_FREE_MAX`]). After a tap the status stays "recording" for `double_tap_ms`, so
//!   the bar doesn't flicker between the two taps.
//! - `duck` (agt-platform, with `audio.duck`): once the microphone has started, turns the
//!   system output volume down until the recording ends, however it ends.
//! - `stt`: downloads/loads the model at start-up, then transcribes jobs in order. A new
//!   recording may start while the previous one is still being transcribed: jobs queue up
//!   and are pasted in order (nothing is dropped). A recording's segments are transcribed
//!   while it's still being recorded, and their texts joined at release; a dropped
//!   (cancelled) recording's are skipped, or discarded if already being transcribed.
//! - `polish`: first applies the dictionary's replacements (`dictionary.toml`, see
//!   [`agt_core::dictionary`]) to each transcript, polish on or off. Then, with
//!   `polish.enabled`, cleans it up with an LLM (`agt-polish`), bounded by
//!   `polish.timeout_ms`; on any failure the transcript (replaced) is pasted. Then hands it to
//!   `paste`. A long dictation transcribed while recording is also polished while
//!   recording, in pieces that end at a sentence end (`agt_polish::ahead`, local servers
//!   only), so at release only the rest is left to polish (see [`Partial::feed_ahead`]); the
//!   replacements are then applied to each piece. A phrase is never split across segments
//!   (their texts are joined with a space before any replacement), only across pieces, which
//!   end at a sentence end (or, in a run-on of over `agt_polish::ahead::MAX_HELD_WORDS`
//!   words, at a segment boundary): a `from` spanning one of those isn't matched.
//! - `permissions` (macOS, only while something is missing): asks for the Microphone and
//!   Accessibility permissions at start-up, then re-checks every [`PERMISSION_POLL`] until
//!   both are granted, so no restart is needed.
//! - `paste`: saves the clipboard when a recording starts (on a helper thread, with a
//!   deadline, so a hung clipboard owner can't block pasting), pastes, and restores the
//!   clipboard `paste.restore_clipboard_ms` later. If that save is what keeps the clipboard
//!   busy, the paste waits for it (up to [`BUSY_SAVE_WAIT`]). It also waits while a hotkey key
//!   the paste would disturb is still held (see [`HOTKEY_RELEASE_POLL`]). It keeps the last
//!   transcript in memory for the tray's "Copy last transcript".
//!
//! Status changes are reported through a callback (the app forwards them to the tray and the
//! overlay).
//!
//! Each dictation's measurements travel with it as an [`agt_telemetry::Dictation`] and are
//! recorded (a channel send, never a disk write) wherever it ends: after the paste, or at the
//! stage that dropped it. Only counts and timings: never the audio or the text.
//!
//! With `[history]` on, a transcribed dictation's text and/or audio also travel with it, as an
//! [`agt_history::Entry`], and are handed to the history writer (again a channel send) after
//! the paste. With it off, no entry is built. Its `raw` text is the engine's, before the
//! dictionary's replacements.
//!
//! The dictionary in use is swapped as a whole when `dictionary.toml` changes
//! ([`Pipeline::set_dictionary`]); no restart. Each dictation keeps the one in use at its
//! key-down, for the engine's hints and all its replacements (see [`Words`]).

use agt_audio::{Level, Recorder};
use agt_core::{
    Config, Dictionary, Scope,
    config::{PolishConfig, PolishProvider},
    paths,
    polish::Polisher,
    stt::{SAMPLE_RATE, SttEngine, TranscribeOpts},
};
use agt_history::{self as history, History};
use agt_platform::{
    ClipboardSnapshot, Ducked, Ducker, Hotkey, HotkeyEvent, HotkeyHandle, HotkeyReset, Hotkeys,
    Key, MAX_CHORD, PasteToken, Permission,
};
use agt_polish::{Outcome, ahead::Ahead};
use agt_stt::chunk::{Cut, CutConfig, Live, LiveStats, Piece, Texts};
use agt_telemetry::{self as telemetry, Dictation, Outcome as Ended};
use std::{
    borrow::Cow,
    fmt,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

/// Debug/test hook: when set to a WAV path, every dictation transcribes that file instead
/// of the microphone recording (the mic is still opened and recorded as usual).
pub const TEST_WAV_ENV: &str = "AGT_TEST_WAV";

/// While recording, how often the audio thread checks that the hotkey is really still down.
/// The hook can miss a release: it happens on the secure desktop (UAC prompt, Ctrl+Alt+Del)
/// or Windows dropped a hook that was too slow once. A missed release is treated as a normal
/// release at the moment it's noticed: the user did let go, and what they said up to then is
/// what they meant to dictate (at most ~250 ms of extra audio). The hook is then re-installed.
/// It's also how often the new audio is taken and cut ([`Feed`]).
const RELEASE_POLL: Duration = Duration::from_millis(250);

/// A hands-free recording (double tap) that nobody ends is discarded after this, the longest
/// recording: it was most likely forgotten, and pasting it would put whatever was said since
/// into whatever window has focus by then.
const HANDS_FREE_MAX: Duration = Duration::from_secs(agt_audio::MAX_SECONDS as u64);

/// A press shorter than this (or than `min_hold_ms`, if that's longer) is a tap: a double tap
/// is two of them. Not tied to `min_hold_ms` alone, which may be 0.
const TAP: Duration = Duration::from_millis(150);

/// While a permission is missing (macOS), how often it's checked again.
const PERMISSION_POLL: Duration = Duration::from_secs(1);

/// How long a paste may wait for the clipboard snapshot that started at key down.
const SNAPSHOT_WAIT: Duration = Duration::from_millis(100);

/// If the clipboard is busy because our own key-down save is still reading it (an app that
/// renders a large copy lazily, e.g. a big Excel range), how long a paste waits for that save
/// before trying again (issue #3).
const BUSY_SAVE_WAIT: Duration = Duration::from_secs(2);

/// A paste waits while a hotkey key that pasting would have to release in software (Shift,
/// Left Ctrl, Win) is still held: a Win key held a moment longer than the chord's other key,
/// or the hotkey already held for the next dictation. Releasing them in software would leave
/// them logically up while physically down: a still-held Win then auto-repeats as a fresh
/// press and its release opens the Start menu, and the release watchdog can't trust its polls.
/// After [`HOTKEY_RELEASE_MAX`] it pastes anyway (releasing them, as before).
const HOTKEY_RELEASE_POLL: Duration = Duration::from_millis(15);
const HOTKEY_RELEASE_MAX: Duration = Duration::from_secs(10);

/// On exit, how long to wait for a ducked volume to be put back.
const DUCK_SHUTDOWN_WAIT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loading(String),
    /// Ready, possibly with a note (e.g. "on the CPU, DirectML unavailable").
    Ready(Option<String>),
    Recording,
    Transcribing,
    Error(String),
    /// Privacy permissions the user still has to grant (macOS).
    NeedsPermission(Vec<Permission>),
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Loading(what) => write!(f, "{what}"),
            Status::Ready(None) => write!(f, "Ready"),
            Status::Ready(Some(note)) => write!(f, "Ready ({note})"),
            Status::Recording => write!(f, "Recording..."),
            Status::Transcribing => write!(f, "Transcribing..."),
            Status::Error(e) => write!(f, "Error: {e}"),
            Status::NeedsPermission(missing) => write!(
                f,
                "Needs permission: {} (System Settings > Privacy & Security)",
                list(missing)
            ),
        }
    }
}

/// What the pipeline reports to the app.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The status line changed. With the number of failures (microphone, transcription,
    /// paste) so far: a `Status::Error` is a new failure only if that went up since.
    Status(Status, u64),
    /// The hotkey was pressed but no recording started (the model failed to load, or no
    /// microphone); the status line says why.
    Rejected,
    /// The hotkey was pressed while the model is still loading (or downloading): no
    /// recording.
    NotReady,
    /// Whether there is a last transcript to copy (see [`Pipeline::copy_last_transcript`]).
    CopyAvailable(bool),
    /// A dictation was pasted, but keyboard focus wasn't in anything that takes text (macOS):
    /// offer to copy it.
    NotInTextBox,
    /// The last transcript was put on the clipboard (see [`Pipeline::copy_last_transcript`]).
    Copied,
    /// New tray line about LLM polish (the managed S1-mini downloading, starting, failed).
    PolishInfo(String),
}

enum Engine {
    Loading(String),
    Ready(Option<String>),
    Failed(String),
}

struct HubState {
    engine: Engine,
    recording: bool,
    /// A tap just ended and a second one may still make it a double tap: still shown as
    /// recording, so the bar doesn't hide and come back between the two.
    lingering: bool,
    /// Jobs sent to `stt` and not yet pasted.
    pending: usize,
    /// Latest microphone/paste error, cleared by the next successful dictation.
    error: Option<String>,
    /// Permissions not granted yet (macOS); shown until they are.
    missing: Vec<Permission>,
    /// How many times `error` was set: tells a new failure from a still-shown old one.
    failures: u64,
    last: Option<Status>,
}

impl HubState {
    fn fail(&mut self, error: &str) {
        self.error = Some(error.into());
        self.failures += 1;
    }
}

/// Derives the one status line from the pipeline's state and reports changes.
#[derive(Clone)]
struct Hub {
    state: Arc<Mutex<HubState>>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
}

impl Hub {
    fn update(&self, f: impl FnOnce(&mut HubState)) {
        let mut s = self.state.lock().unwrap();
        f(&mut s);
        let status = if s.recording || s.lingering {
            Status::Recording
        } else if s.pending > 0 {
            Status::Transcribing
        } else if !s.missing.is_empty() && !matches!(s.engine, Engine::Failed(_)) {
            Status::NeedsPermission(s.missing.clone())
        } else {
            match (&s.engine, &s.error) {
                (Engine::Loading(what), _) => Status::Loading(what.clone()),
                (Engine::Failed(e), _) | (Engine::Ready(_), Some(e)) => Status::Error(e.clone()),
                (Engine::Ready(note), None) => Status::Ready(note.clone()),
            }
        };
        if s.last.as_ref() != Some(&status) {
            tracing::debug!("status: {status}");
            s.last = Some(status.clone());
            (self.notify)(Event::Status(status, s.failures));
        }
    }

    /// The hotkey was pressed but no recording started.
    fn reject(&self) {
        (self.notify)(Event::Rejected);
    }

    fn not_ready(&self) {
        (self.notify)(Event::NotReady);
    }

    fn copy_available(&self, available: bool) {
        (self.notify)(Event::CopyAvailable(available));
    }

    fn not_in_text_box(&self) {
        (self.notify)(Event::NotInTextBox);
    }

    fn copied(&self) {
        (self.notify)(Event::Copied);
    }

    fn engine_ready(&self) -> bool {
        matches!(self.state.lock().unwrap().engine, Engine::Ready(_))
    }

    fn engine_failed(&self) -> bool {
        matches!(self.state.lock().unwrap().engine, Engine::Failed(_))
    }

    fn missing(&self, p: Permission) -> bool {
        self.state.lock().unwrap().missing.contains(&p)
    }
}

/// Set once the speech model has loaded (or failed to): S1-mini waits for it before
/// downloading, so on a first run the two don't share a slow link and dictation, which needs
/// the speech model, can start sooner (issue #64).
#[derive(Clone, Default)]
struct SttLoaded(Arc<(Mutex<bool>, Condvar)>);

impl SttLoaded {
    fn set(&self) {
        let (done, cv) = &*self.0;
        *done.lock().unwrap() = true;
        cv.notify_all();
    }

    fn wait(&self) {
        let (done, cv) = &*self.0;
        let _done = cv.wait_while(done.lock().unwrap(), |d| !*d).unwrap();
    }
}

/// Sets [`SttLoaded`] when dropped: also if loading the model panics, so S1-mini isn't held
/// forever.
struct SetOnDrop(SttLoaded);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

/// The dictionary in use, replaced as a whole ([`Pipeline::set_dictionary`]). The `audio`
/// thread takes it at each key-down (an `Arc` clone under a lock held for nanoseconds) and
/// resolves it for the focused app ([`Scope`], see [`AudioThread::scope`]); that travels with
/// the dictation ([`Shared::scope`], [`Job::scope`], [`Transcribed::scope`]): the terms the
/// engine boosts and every replacement in one dictation come from the same dictionary and
/// app, whatever is reloaded or focused meanwhile.
#[derive(Clone, Default)]
struct Words(Arc<Mutex<Arc<Dictionary>>>);

impl Words {
    fn new(dictionary: Dictionary) -> Self {
        Self(Arc::new(Mutex::new(Arc::new(dictionary))))
    }

    fn get(&self) -> Arc<Dictionary> {
        self.0.lock().unwrap().clone()
    }

    fn set(&self, dictionary: Dictionary) {
        *self.0.lock().unwrap() = Arc::new(dictionary);
    }
}

/// The engine's options for a dictation: its scope's terms (an `Arc` clone), boosted by
/// `boost` (`stt.boost`). The engine keeps what it builds from the terms (Parakeet's trie)
/// across dictations.
fn opts(scope: &Scope, boost: f32) -> TranscribeOpts {
    TranscribeOpts {
        vocabulary: scope.terms().clone(),
        boost,
        ..Default::default()
    }
}

/// Applies the replacements that apply in `scope` to `text`; returns how many it made.
fn replace_in(scope: &Scope, text: &mut String) -> usize {
    let replaced = match scope.replace(text) {
        (Cow::Owned(out), n) => Some((out, n)),
        (Cow::Borrowed(_), _) => None,
    };
    match replaced {
        Some((out, n)) => {
            *text = out;
            n
        }
        None => 0,
    }
}

/// Keeps the pipeline running; dropping it stops the hotkey.
pub struct Pipeline {
    _hotkey: HotkeyHandle,
    /// Live microphone level (RMS, 0..1) while recording, for the overlay.
    pub level: Level,
    /// One line for the tray about LLM polish, e.g. "Polish: off".
    pub polish_info: String,
    to_paste: Sender<ToPaste>,
    key_down: Arc<Mutex<Option<Instant>>>,
    ducker: Ducker,
    words: Words,
}

impl Pipeline {
    /// Uses `dictionary` for dictations started (key-down) from now on. One recording or
    /// already transcribing keeps the dictionary it started with, for the engine's hints
    /// and all its replacements, so no dictation mixes two.
    pub fn set_dictionary(&self, dictionary: Dictionary) {
        self.words.set(dictionary);
    }

    /// Before exiting: puts the output volume back if a dictation in progress lowered it.
    pub fn shutdown(&self) {
        self.ducker.shutdown(DUCK_SHUTDOWN_WAIT);
    }

    /// Puts the last transcript (pasted or not) on the clipboard, for the user to paste by
    /// hand: the recovery path when a paste failed. It's kept in memory only, for
    /// [`LAST_TRANSCRIPT_TTL`] ([`Event::CopyAvailable`] says when there is one).
    pub fn copy_last_transcript(&self) {
        let _ = self.to_paste.send(ToPaste::CopyLast);
    }

    /// When the hotkey last went down (for latency logging).
    pub fn last_key_down(&self) -> Option<Instant> {
        *self.key_down.lock().unwrap()
    }
}

struct Job {
    /// The whole recording (for the history); transcribed unless `live` is set.
    audio: Vec<f32>,
    released: Instant,
    /// The foreground window at release ([`agt_platform::foreground_window`]).
    target: Option<usize>,
    dictation: Box<Dictation>,
    /// Segments were sent while recording: only the rest is left to transcribe.
    live: Option<(Vec<Piece>, Arc<Shared>)>,
    /// The dictionary in the app focused at key-down (see [`Words`]).
    scope: Arc<Scope>,
}

/// Shared by the audio, stt and polish threads for one recording whose segments are sent
/// while it's being recorded.
#[derive(Default)]
struct Shared {
    /// The recording was dropped (cancelled, too short, stale): its segments still queued
    /// are skipped, and the text of the one being transcribed is discarded.
    dropped: AtomicBool,
    /// Segments sent and not started yet.
    queued: AtomicUsize,
    /// The dictionary in the app focused at key-down, for its segments and pieces (see
    /// [`Words`]).
    scope: Arc<Scope>,
}

enum ToStt {
    Transcribe(Job),
    /// A finished segment of the recording in progress.
    Segment(Piece, Arc<Shared>),
    /// The recording those segments were from was dropped: recorded as `Ended` once they
    /// are out of the way.
    Dropped(Box<Dictation>, Ended),
    /// The model failed to load and the user pressed the hotkey: try again.
    RetryLoad,
}

enum ToPolish {
    /// A piece of the recording in progress (`agt_polish::ahead`), to polish right away.
    Piece(String, Arc<Shared>),
    /// A transcribed dictation: polish it (or what's left of it), then paste.
    Text(Box<Transcribed>),
}

struct Transcribed {
    /// The transcript, whitespace-normalized; never empty.
    text: String,
    /// Polished in pieces: those left at release, after the `Piece`s with this `Shared`.
    rest: Option<(Vec<String>, Arc<Shared>)>,
    /// When the transcript was ready.
    ready: Instant,
    released: Instant,
    target: Option<usize>,
    dictation: Box<Dictation>,
    /// Only with `[history]` on.
    saved: Option<Box<history::Entry>>,
    /// The dictionary in the app focused at key-down (see [`Words`]).
    scope: Arc<Scope>,
}

enum ToPaste {
    /// A recording started: save the clipboard now so pasting doesn't wait for it.
    Snapshot,
    Paste {
        text: String,
        released: Instant,
        target: Option<usize>,
        dictation: Box<Dictation>,
        /// Only with `[history]` on.
        saved: Option<Box<history::Entry>>,
    },
    /// The user asked for the last transcript on the clipboard (tray menu).
    CopyLast,
}

/// Starts all pipeline threads. Fails if the hotkey can't be installed (a bad key name or list
/// of hotkeys, or a platform without support yet); the model is then not loaded either.
pub fn start(
    config: &Config,
    dictionary: Dictionary,
    notify: impl Fn(Event) + Send + Sync + 'static,
) -> Result<Pipeline, String> {
    let words = Words::new(dictionary);
    let hub = Hub {
        state: Arc::new(Mutex::new(HubState {
            engine: Engine::Loading("Loading model...".into()),
            recording: false,
            lingering: false,
            pending: 0,
            error: None,
            missing: Vec::new(),
            failures: 0,
            last: None,
        })),
        notify: Arc::new(notify),
    };
    // First: it also puts back a volume a previous run left ducked, even if the rest fails.
    let ducker = Ducker::start(paths::ducked_volume_file());
    let hotkeys = Hotkeys::parse(&config.hotkey.keys)?;
    // The hook watches every hotkey; the audio and paste threads still go by the first one's
    // keys (for a missed release, and the paste wait).
    let key = hotkeys.list()[0].clone();
    // Before the audio thread starts: it doesn't open the microphone without the permission.
    watch_permissions(&hub);

    let (to_audio, audio_rx) = mpsc::channel::<HotkeyEvent>();
    let hotkey = agt_platform::start_hotkey(&hotkeys, move |ev| {
        let _ = to_audio.send(ev);
    })
    .map_err(|e| e.to_string())?;

    let (to_stt, stt_rx) = mpsc::channel::<ToStt>();
    let (to_polish, polish_rx) = mpsc::channel::<ToPolish>();
    let (to_paste, paste_rx) = mpsc::channel::<ToPaste>();
    let level = Level::default();
    let key_down = Arc::new(Mutex::new(None));

    let test_wav = load_test_wav();
    let chunk_secs = Arc::new(AtomicU32::new(agt_stt::CHUNK_SECS.to_bits()));
    let audio = AudioThread {
        config: config.clone(),
        key: key.clone(),
        reset: hotkey.resetter(),
        hub: hub.clone(),
        to_stt,
        to_paste: to_paste.clone(),
        test_wav,
        key_down: key_down.clone(),
        ducker: config.audio.duck.then(|| ducker.clone()),
        chunk_secs: chunk_secs.clone(),
        words: words.clone(),
    };
    let lvl = level.clone();
    spawn("audio", move || audio.run(audio_rx, lvl));

    remove_old_llama_builds(&config.stt.models_path());
    let stt_loaded = SttLoaded::default();
    let (polisher, polish_info) = load_polisher(config, &hub, &stt_loaded);
    let saver = history::start(&config.history, paths::dictations_dir());
    let polishing = polisher.is_some();
    let (h, cfg, hs) = (hub.clone(), config.clone(), saver.clone());
    spawn("stt", move || {
        stt_thread(
            cfg, h, stt_rx, to_polish, polishing, hs, chunk_secs, stt_loaded,
        )
    });
    let (cfg, tp) = (config.clone(), to_paste.clone());
    spawn("polish", move || {
        polish_thread(cfg, polish_rx, tp, polisher)
    });

    let (h, cfg) = (hub.clone(), config.clone());
    spawn("paste", move || paste_thread(cfg, h, paste_rx, key, saver));

    hub.update(|_| {});
    Ok(Pipeline {
        _hotkey: hotkey,
        level,
        polish_info,
        to_paste,
        key_down,
        ducker,
        words,
    })
}

/// AGT 0.0.16 and earlier installed llama.cpp in the models folder. Removed (in the
/// background, whatever the polish settings) from the default one, which is the user's own;
/// a folder set in the config may be shared with an older AGT still running those builds.
/// Never touches the disk on the calling thread: the models folder may be a slow share.
fn remove_old_llama_builds(models_dir: &std::path::Path) {
    let own = models_dir == paths::models_dir();
    let dir = models_dir.to_owned();
    spawn("cleanup", move || {
        let old = agt_polish::managed::legacy_dir(&dir);
        if !old.exists() {
            return;
        }
        if own {
            if agt_polish::managed::remove_legacy(&dir) {
                tracing::info!("removed AGT's old llama.cpp builds from {}", old.display());
            }
        } else {
            tracing::warn!(
                "{} is no longer used (llama.cpp now lives in {}); delete it once no older AGT uses this models folder",
                old.display(),
                paths::llama_dir().display()
            );
        }
    });
}

/// Builds the polisher if enabled (no network: cheap) and warms it up in the background,
/// so a local server loads its model before the first dictation. Returns the tray line.
fn load_polisher(
    config: &Config,
    hub: &Hub,
    stt_loaded: &SttLoaded,
) -> (Option<Arc<dyn Polisher>>, String) {
    if let Some(e) = &config.polish_error {
        tracing::error!("polish disabled, config error: {e}; the rest of the config is used");
        return (None, "Polish: off (config error, see log)".into());
    }
    for notice in &config.polish_notices {
        tracing::warn!("{notice}");
    }
    if !config.polish.enabled {
        return (None, "Polish: off".into());
    }
    let notify = hub.notify.clone();
    let env = agt_polish::Env {
        models_dir: config.stt.models_path(),
        llama_dir: paths::llama_dir(),
        // Start-up output only (see agt-polish), next to agt.db ("Open data folder").
        log_file: Some(paths::history_dir().join("llama-server.log")),
        on_status: Arc::new(move |s: &agt_polish::Status| {
            tracing::info!("polish: {s}");
            notify(Event::PolishInfo(s.to_string()));
        }),
        hold_download: Some(Arc::new({
            let stt_loaded = stt_loaded.clone();
            move || stt_loaded.wait()
        })),
    };
    match agt_polish::load(&config.polish, &env) {
        Ok(p) => {
            let p: Arc<dyn Polisher> = Arc::from(p);
            tracing::info!("polish: {}", p.name());
            let warm = p.clone();
            // S1-mini's own wait for the server is bounded; don't count the speech model's
            // download (it holds S1-mini's) against it, or in polish_warmup_ms.
            let held =
                (config.polish.provider == PolishProvider::S1Mini).then(|| stt_loaded.clone());
            spawn("polish-warmup", move || {
                if let Some(held) = held {
                    held.wait();
                }
                let t = Instant::now();
                let result = warm.warmup();
                let took = ms(t.elapsed());
                match &result {
                    Ok(()) => tracing::info!("polish warmed up ({took:.0} ms)"),
                    Err(e) => tracing::warn!(
                        "polish warmup failed ({e}); dictations are pasted unpolished until the server responds"
                    ),
                }
                telemetry::global().session(telemetry::SessionUpdate {
                    polish_warmup_ms: Some(took),
                    polish_warmup_ok: Some(result.is_ok()),
                    ..Default::default()
                });
            });
            // The managed S1-mini reports its own state (PolishInfo) as it downloads/starts.
            let info = match config.polish.provider {
                PolishProvider::S1Mini => agt_polish::Status::Starting.to_string(),
                _ => format!("Polish: on ({})", config.polish.model()),
            };
            (Some(p), info)
        }
        Err(e) => {
            tracing::error!("polish disabled: {e}");
            (None, "Polish: off (config problem, see log)".into())
        }
    }
}

/// macOS: asks for whatever permission is missing (the system prompts appear once each), shows
/// it in the status line and re-checks every [`PERMISSION_POLL`] until everything is granted.
/// Nothing to do on Windows.
fn watch_permissions(hub: &Hub) {
    let missing = agt_platform::missing_permissions();
    if missing.is_empty() {
        return;
    }
    tracing::warn!("missing permissions: {}; asking for them", list(&missing));
    agt_platform::request_permissions(&missing);
    hub.update(|s| s.missing = missing.clone());
    let hub = hub.clone();
    spawn("permissions", move || {
        let mut last = missing;
        while !last.is_empty() {
            thread::sleep(PERMISSION_POLL);
            let now = agt_platform::missing_permissions();
            if now != last {
                if now.is_empty() {
                    tracing::info!("all permissions granted");
                } else {
                    tracing::info!("still missing permissions: {}", list(&now));
                }
                hub.update(|s| s.missing = now.clone());
                last = now;
            }
        }
    });
}

fn list(permissions: &[Permission]) -> String {
    let names: Vec<String> = permissions.iter().map(ToString::to_string).collect();
    names.join(", ")
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .unwrap_or_else(|e| panic!("cannot spawn {name} thread: {e}"));
}

fn load_test_wav() -> Option<Vec<f32>> {
    let path = std::env::var_os(TEST_WAV_ENV)?;
    match agt_audio::wav::read_16k_mono(path.as_ref()) {
        Ok(samples) => {
            tracing::warn!(
                "{TEST_WAV_ENV} is set: every dictation will transcribe {} ({:.1} s) instead of the microphone",
                std::path::Path::new(&path).display(),
                samples.len() as f64 / SAMPLE_RATE as f64
            );
            Some(samples)
        }
        Err(e) => {
            tracing::error!("{TEST_WAV_ENV}: {e}");
            None
        }
    }
}

/// Decides whether a key-state poll that says "up" really means the hook missed a release.
///
/// Every key of the hotkey (all keys of a chord) is polled; any one of them up means the hold
/// ended.
///
/// The poll sees the *logical* key state, which our own paste can change: pasting releases
/// held Shift and Win keys, and its Ctrl-up maps to Left Ctrl. A paste normally waits until
/// those hotkey keys are up ([`HOTKEY_RELEASE_MAX`]), but if it gave up waiting and released
/// them in software ([`agt_platform::paste_count`] went up since this recording started), a
/// poll for such a key is not trusted (the chord's other keys still are): a genuinely missed
/// release is then caught on the next press instead (the stale recording is discarded).
/// Right Ctrl (the default) is never touched by a paste.
#[derive(Debug, Clone, Copy)]
struct ReleaseWatch {
    /// Bit `i`: a paste can make chord key `i` look released.
    paste_may_release: u8,
    pastes_at_start: u64,
}

impl ReleaseWatch {
    fn new(hotkey: &Hotkey, pastes_now: u64) -> Self {
        let may_release = hotkey
            .keys()
            .iter()
            .map(|&k| agt_platform::paste_may_release(k));
        Self::with(may_release, pastes_now)
    }

    /// `may_release`: per chord key, whether a paste can make it look released.
    fn with(may_release: impl IntoIterator<Item = bool>, pastes_now: u64) -> Self {
        let paste_may_release = may_release
            .into_iter()
            .enumerate()
            .fold(0, |bits, (i, may)| bits | (may as u8) << i);
        Self {
            paste_may_release,
            pastes_at_start: pastes_now,
        }
    }

    /// `keys_down`: the poll result for each chord key; `pastes_now`:
    /// [`agt_platform::paste_count`].
    fn missed(&self, keys_down: &[Option<bool>], pastes_now: u64) -> bool {
        let pasted = pastes_now != self.pastes_at_start;
        keys_down.iter().enumerate().any(|(i, &down)| {
            down == Some(false) && !(pasted && self.paste_may_release & (1 << i) != 0)
        })
    }
}

/// Hands-free recording (double tap): how the recording in progress ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Latch {
    /// Holding to talk: the release ends it.
    Off,
    /// The double tap's second press is still down: if it's a tap, its release doesn't end
    /// it (if it's held, it was a hold after all).
    Pressed,
    /// Recording with the hotkey up: the next press's release ends it.
    On,
    /// That press is down: its release ends it. Another press meanwhile (an auto-repeat after
    /// a missed press re-installed the hook) changes nothing; another key (a shortcut with the
    /// hotkey, or Esc) goes back to `On`.
    Ending,
}

/// Whether a press at `now` may be the second tap of a double tap: a tap (see [`TAP`]) was
/// let go at most `gap` before (`last_tap`). A zero `gap` turns it off.
fn is_double_tap(last_tap: Option<Instant>, now: Instant, gap: Duration) -> bool {
    !gap.is_zero() && last_tap.is_some_and(|up| now.saturating_duration_since(up) <= gap)
}

struct AudioThread {
    config: Config,
    key: Hotkey,
    reset: HotkeyReset,
    hub: Hub,
    to_stt: Sender<ToStt>,
    to_paste: Sender<ToPaste>,
    test_wav: Option<Vec<f32>>,
    key_down: Arc<Mutex<Option<Instant>>>,
    /// `None` with `audio.duck` off.
    ducker: Option<Ducker>,
    /// The longest segment to cut a recording into (seconds, `f32` bits; set by `stt`).
    chunk_secs: Arc<AtomicU32>,
    /// Each recording takes the dictionary in use at key-down (see [`Words`]).
    words: Words,
}

/// A recording in progress: taken from the recorder every [`RELEASE_POLL`] and cut as it
/// comes in, finished segments sent to `stt` (with what `Live` says to transcribe). The cutter
/// only looks at 10 ms levels (microseconds per poll), so capture is never held up; nothing
/// is transcribed on this thread. If nothing was cut by release, the recording is transcribed
/// whole after release as it always was: a short dictation's release costs nothing extra.
struct Feed {
    live: Live,
    /// The recording so far: the history may keep it, and a short one is transcribed whole.
    audio: Vec<f32>,
    /// Swapped with the recorder's buffer (see [`Recorder::take_new`]).
    buf: Vec<f32>,
    /// Samples pushed into `live` (with [`TEST_WAV_ENV`], of the test file, at the pace of
    /// the recording).
    fed: usize,
    shared: Arc<Shared>,
    /// A segment was sent to `stt`.
    sent: bool,
    max_queue: usize,
}

impl Feed {
    /// `scope`: the dictionary in use at key-down in the app focused then, for the whole
    /// dictation.
    fn new(chunk_secs: f32, scope: Arc<Scope>) -> Self {
        Self {
            live: Live::new(CutConfig::live(chunk_secs)),
            audio: Vec::new(),
            // Swapped into the callback: room for 250 ms polls, so it never allocates there.
            buf: Vec::with_capacity(SAMPLE_RATE as usize / 2),
            fed: 0,
            shared: Arc::new(Shared {
                scope,
                ..Shared::default()
            }),
            sent: false,
            max_queue: 0,
        }
    }

    /// Takes the new audio from `recorder` and sends the segments that are finished.
    fn pump(&mut self, recorder: &Recorder, test_wav: Option<&[f32]>, to_stt: &Sender<ToStt>) {
        recorder.take_new(&mut self.buf);
        self.audio.append(&mut self.buf);
        self.push(test_wav, to_stt);
    }

    /// Pushes what's new in `audio` (or the test file) into the cutter.
    fn push(&mut self, test_wav: Option<&[f32]>, to_stt: &Sender<ToStt>) {
        let source = test_wav.unwrap_or(&self.audio);
        let to = self.audio.len().min(source.len());
        if to <= self.fed {
            return;
        }
        let pieces = self.live.push(&source[self.fed..to]);
        self.fed = to;
        for piece in pieces {
            tracing::debug!(
                "segment cut while recording ({:?}, {:.1} s); transcribing it",
                piece.cut,
                secs(&piece.audio)
            );
            let queued = self.shared.queued.fetch_add(1, Relaxed) + 1;
            self.max_queue = self.max_queue.max(queued);
            self.sent = true;
            let _ = to_stt.send(ToStt::Segment(piece, self.shared.clone()));
        }
    }
}

/// A recording's cuts so far into its telemetry (released or dropped).
fn live_stats(d: &mut Dictation, stats: &LiveStats, max_queue: usize) {
    let n = |cut: Cut| Some(stats.cuts[cut as usize] as i64);
    d.live_pause_cuts = n(Cut::Pause);
    d.live_limit_cuts = n(Cut::Limit);
    d.live_end_cuts = n(Cut::End);
    d.live_skipped = Some(stats.skipped as i64);
    d.live_trimmed_secs = Some((stats.received - stats.kept) as f64 / SAMPLE_RATE as f64);
    d.live_max_queue = Some(max_queue as i64);
}

impl AudioThread {
    fn run(self, events: Receiver<HotkeyEvent>, level: Level) {
        let mut recorder = Recorder::new(
            self.config.audio.keep_mic_open,
            self.config.audio.device.clone(),
            level,
        );
        // Without the Microphone permission (macOS) the stream would be built on a device
        // that delivers silence; it's built on the first recording after the grant instead.
        if self.hub.missing(Permission::Microphone) {
            tracing::info!("microphone permission not granted yet; not opening the microphone");
        } else if let Err(e) = recorder.prepare() {
            tracing::warn!("microphone not available yet: {e}");
        }
        // The hold in progress: when the key went down, its measurements so far, the ducked
        // output volume, and its audio. Dropping that guard puts the volume back, however the
        // hold ends (including this thread exiting or panicking).
        let mut holding: Option<(Instant, Box<Dictation>, Option<Ducked>, Feed)> = None;
        let mut watch = ReleaseWatch::new(&self.key, 0);
        let mut latch = Latch::Off;
        // When the last tap was let go: maybe the first half of a double tap.
        let mut last_tap: Option<Instant> = None;
        // Until when the bar stays up after that tap ([`HubState::lingering`]).
        let mut linger: Option<Instant> = None;
        let double_tap = Duration::from_millis(self.config.hotkey.double_tap_ms);
        let min_hold = Duration::from_millis(self.config.hotkey.min_hold_ms);
        let tap = min_hold.max(TAP);
        // The press ending a hands-free recording was synthesized by `missed_press`: its
        // release is polled with `key_down_past_hook`.
        let mut press_missed = false;

        loop {
            press_missed &= latch == Latch::Ending;
            let hands_free = latch == Latch::On && holding.is_some();
            // Esc is ours only while it can end something.
            agt_platform::arm_escape(hands_free);
            // `missed`: a release synthesized after the hook missed the real one.
            let (ev, missed) = if let Some((down, .., feed)) = &mut holding {
                // Hands-free, the hotkey is up: no release to watch for, but a press the hook
                // missed, and the time limit.
                let until = *down + HANDS_FREE_MAX;
                let wait = match hands_free {
                    true => until.saturating_duration_since(Instant::now()),
                    false => RELEASE_POLL,
                };
                match events.recv_timeout(wait.min(RELEASE_POLL)) {
                    Ok(ev) => (ev, false),
                    Err(RecvTimeoutError::Timeout) if hands_free && Instant::now() >= until => {
                        tracing::warn!(
                            "hands-free recording reached {} minutes; discarded it",
                            HANDS_FREE_MAX.as_secs() / 60
                        );
                        latch = Latch::Off;
                        if let Some((down, d, _ducked, feed)) = holding.take() {
                            let now = Instant::now();
                            self.abort_recording(
                                &mut recorder,
                                down,
                                now,
                                d,
                                feed,
                                Ended::TimedOut,
                            );
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        feed.pump(&recorder, self.test_wav.as_deref(), &self.to_stt);
                        let missed = match hands_free {
                            true => self.missed_press(&events).map(|(ev, synthesized)| {
                                press_missed = synthesized;
                                (ev, false)
                            }),
                            false => self.missed_release(&events, &watch, press_missed),
                        };
                        match missed {
                            Some(ev) => ev,
                            None => continue,
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else if let Some(until) = linger {
                match events.recv_timeout(until.saturating_duration_since(Instant::now())) {
                    Ok(ev) => (ev, false),
                    Err(RecvTimeoutError::Timeout) => {
                        linger = None;
                        self.hub.update(|s| s.lingering = false);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match events.recv() {
                    Ok(ev) => (ev, false),
                    Err(_) => return,
                }
            };
            match ev {
                HotkeyEvent::Down(t, _) => {
                    *self.key_down.lock().unwrap() = Some(t);
                    if matches!(latch, Latch::On | Latch::Ending) && holding.is_some() {
                        // Ends the hands-free recording on its release, like a hold: every
                        // paste comes after the hotkey is let go.
                        if latch == Latch::On {
                            tracing::debug!("hands-free recording ending: {} pressed", self.key);
                        }
                        latch = Latch::Ending;
                        continue;
                    }
                    latch = Latch::Off;
                    if let Some((down, d, _ducked, feed)) = holding.take() {
                        // The previous hold's release was never seen (and the poll couldn't
                        // tell, e.g. a swallowed F13-F24 key). Its audio may span minutes of
                        // unrelated speech: discard it rather than paste it.
                        tracing::warn!("previous hold was never released; discarded it");
                        let outcome = Ended::StaleDiscarded;
                        self.abort_recording(&mut recorder, down, t, d, feed, outcome);
                    }
                    // Before starting: any paste from here on counts as "during this recording".
                    watch = ReleaseWatch::new(&self.key, agt_platform::paste_count());
                    let double = is_double_tap(last_tap.take(), t, double_tap);
                    holding = self.start_recording(&mut recorder, t).map(|mut d| {
                        // Only once the microphone runs: never delays it (a channel send).
                        let ducked = self
                            .ducker
                            .as_ref()
                            .map(|k| k.duck(self.config.audio.duck_level()));
                        let secs = f32::from_bits(self.chunk_secs.load(Relaxed));
                        let scope = self.scope(&mut d);
                        (t, d, ducked, Feed::new(secs, scope))
                    });
                    // After `start_recording`: the status goes straight from lingering to
                    // recording.
                    if linger.take().is_some() {
                        self.hub.update(|s| s.lingering = false);
                    }
                    if double && holding.is_some() {
                        latch = Latch::Pressed;
                        tracing::info!(
                            "double tap: recording hands-free until {} is pressed again",
                            self.key
                        );
                    }
                }
                HotkeyEvent::Up(released) => {
                    let held = holding
                        .as_ref()
                        .map(|(down, ..)| released.saturating_duration_since(*down));
                    if latch == Latch::Pressed && held.is_some_and(|h| h < tap) {
                        latch = Latch::On;
                        continue;
                    }
                    // A second press held down was a hold: a tap just before doesn't matter.
                    latch = Latch::Off;
                    let Some((down, mut d, _ducked, feed)) = holding.take() else {
                        continue;
                    };
                    if held.is_some_and(|h| h < tap) {
                        last_tap = Some(released);
                        // Only for a tap that was dropped: with `min_hold_ms` below TAP, a tap
                        // that long is transcribed, and the bar should say so.
                        let dropped = held.is_some_and(|h| h < min_hold);
                        if dropped && !double_tap.is_zero() {
                            linger = Some(released + double_tap);
                            self.hub.update(|s| s.lingering = true);
                        }
                    }
                    d.missed_release = missed;
                    self.finish_recording(&mut recorder, down, released, d, feed);
                }
                HotkeyEvent::Cancel if latch == Latch::Ending && holding.is_some() => {
                    // A shortcut with the hotkey while hands-free: not the end of it.
                    tracing::debug!("{} was part of a shortcut; still hands-free", self.key);
                    latch = Latch::On;
                }
                HotkeyEvent::Cancel => {
                    latch = Latch::Off;
                    if let Some((down, d, _ducked, feed)) = holding.take() {
                        tracing::info!(
                            "dictation cancelled (another key was pressed, or the hold was stale)"
                        );
                        let now = Instant::now();
                        self.abort_recording(&mut recorder, down, now, d, feed, Ended::Cancelled);
                    }
                }
                HotkeyEvent::Escape => {
                    if latch != Latch::On {
                        continue; // only sent while armed; a late one
                    }
                    latch = Latch::Off;
                    if let Some((down, d, _ducked, feed)) = holding.take() {
                        tracing::info!("hands-free dictation cancelled (Esc)");
                        let now = Instant::now();
                        let outcome = Ended::EscCancelled;
                        self.abort_recording(&mut recorder, down, now, d, feed, outcome);
                    }
                }
            }
        }
    }

    /// Called every [`RELEASE_POLL`] while recording with no event: returns a synthesized
    /// release (with `true`) if the key turns out not to be down any more. `press_missed`:
    /// the press being held was synthesized by [`Self::missed_press`].
    fn missed_release(
        &self,
        events: &Receiver<HotkeyEvent>,
        watch: &ReleaseWatch,
        press_missed: bool,
    ) -> Option<(HotkeyEvent, bool)> {
        let keys = self.key.keys();
        let mut keys_down = [None; MAX_CHORD];
        for (down, &k) in keys_down.iter_mut().zip(keys) {
            *down = match press_missed {
                true => agt_platform::key_down_past_hook(k),
                false => agt_platform::key_down(k),
            };
        }
        if !watch.missed(&keys_down[..keys.len()], agt_platform::paste_count()) {
            return None; // still held, can't tell, or our own paste made it look released
        }
        // The hook runs before the key state updates, so a real release would normally be
        // queued already; give a racing one a moment to arrive.
        if let Ok(ev) = events.recv_timeout(Duration::from_millis(20)) {
            return Some((ev, false));
        }
        if press_missed {
            // Expected: the hook was re-installed after the press, so it never saw it.
            return Some((HotkeyEvent::Up(Instant::now()), true));
        }
        tracing::warn!(
            "{} is no longer down but no release was seen; finishing the dictation and re-installing the keyboard hook",
            self.key
        );
        self.reset.reset();
        Some((HotkeyEvent::Up(Instant::now()), true))
    }

    /// Called every [`RELEASE_POLL`] during a hands-free recording with no event: returns a
    /// synthesized press if the hotkey turns out to be down without the hook having said so
    /// (macOS disabled the event tap after a timeout, Windows dropped the hook), so the
    /// recording can still be ended; with `true` if synthesized. A tap shorter than the poll
    /// is caught only if a poll falls within it (a 100 ms tap: 40% of the time); the next
    /// tap then gets another chance. (`GetAsyncKeyState`'s "pressed since the last call" bit
    /// would catch it, but Windows documents it as unreliable.)
    fn missed_press(&self, events: &Receiver<HotkeyEvent>) -> Option<(HotkeyEvent, bool)> {
        let keys = self.key.keys();
        if !keys
            .iter()
            .all(|&k| agt_platform::key_down(k) == Some(true))
        {
            return None; // up, or can't tell
        }
        // As in `missed_release`: a real press would normally be queued already.
        if let Ok(ev) = events.recv_timeout(Duration::from_millis(20)) {
            return Some((ev, false));
        }
        tracing::warn!(
            "{} is down but no press was seen; ending the hands-free recording and re-installing the keyboard hook",
            self.key
        );
        self.reset.reset();
        Some((HotkeyEvent::Down(Instant::now(), 0), true))
    }

    /// Stops a recording that isn't transcribed (cancelled, or stale; `end`: when): recorded
    /// as `outcome`.
    fn abort_recording(
        &self,
        recorder: &mut Recorder,
        down: Instant,
        end: Instant,
        mut d: Box<Dictation>,
        feed: Feed,
        outcome: Ended,
    ) {
        recorder.stop();
        self.hub.update(|s| s.recording = false);
        d.hold_ms = Some(ms(end.saturating_duration_since(down)));
        self.drop_dictation(feed, d, outcome);
    }

    /// The dictionary in use, as it applies in the app focused now, for the dictation that
    /// is starting (see [`Words`]); notes the app, its profiles and how many terms are boosted
    /// in `d`. After the microphone started: never delays it (and takes microseconds).
    fn scope(&self, d: &mut Dictation) -> Arc<Scope> {
        let t = Instant::now();
        let scope = Scope::new(self.words.get(), agt_platform::foreground_app());
        let profiles: Vec<&str> = scope.profile_names().collect();
        let boosting = self.config.stt.engine == agt_core::config::Engine::Parakeet
            && self.config.stt.boost() > 0.0;
        tracing::debug!(
            "focused app: {} (dictionary profiles: {}; {} terms boosted; {:.2} ms)",
            scope.app().unwrap_or("unknown"),
            match profiles.is_empty() {
                true => "none".into(),
                false => profiles.join(", "),
            },
            if boosting { scope.terms().len() } else { 0 },
            ms(t.elapsed())
        );
        d.app_id = scope.app().map(Into::into);
        d.profiles = (!profiles.is_empty()).then(|| profiles.join(","));
        d.boosted_terms = Some(if boosting {
            scope.terms().len() as i64
        } else {
            0
        });
        Arc::new(scope)
    }

    /// Returns the new dictation's record if recording started.
    fn start_recording(&self, recorder: &mut Recorder, t: Instant) -> Option<Box<Dictation>> {
        let mut d = Box::new(Dictation {
            started_at: wall_clock(t),
            ..Default::default()
        });
        if !self.hub.engine_ready() {
            tracing::info!("{} pressed but the model isn't ready; ignoring", self.key);
            if self.hub.engine_failed() {
                let _ = self.to_stt.send(ToStt::RetryLoad);
                self.hub.reject();
            } else {
                self.hub.not_ready();
            }
            telemetry::global().dictation(d, Ended::NotReady);
            return None;
        }
        if self.hub.missing(Permission::Microphone) {
            // macOS would record silence. The status line already says what to do.
            tracing::info!(
                "{} pressed but the Microphone permission isn't granted; ignoring",
                self.key
            );
            self.hub.reject();
            return None;
        }
        match recorder.start() {
            Ok(()) => {
                let mic = ms(Instant::now() - t);
                let _ = self.to_paste.send(ToPaste::Snapshot);
                self.hub.update(|s| s.recording = true);
                tracing::debug!("recording (key down → mic started {mic:.1} ms)");
                d.key_down_to_mic_ms = Some(mic);
                Some(d)
            }
            Err(e) => {
                tracing::error!("cannot start microphone: {e}");
                self.hub
                    .update(|s| s.fail("microphone unavailable (see log)"));
                self.hub.reject();
                telemetry::global().dictation(d, Ended::MicFailed);
                None
            }
        }
    }

    /// A recording ends without being transcribed: recorded as `outcome`. If segments of it
    /// went to `stt`, by `stt` once it has skipped them (or discarded the one in flight).
    fn drop_dictation(&self, feed: Feed, mut d: Box<Dictation>, outcome: Ended) {
        if feed.sent {
            feed.shared.dropped.store(true, Relaxed);
            // As far as it got (up to the last poll).
            d.audio_secs = Some(secs(&feed.audio));
            let queued = feed.shared.queued.load(Relaxed);
            live_stats(&mut d, feed.live.stats(), feed.max_queue.max(queued));
            let _ = self.to_stt.send(ToStt::Dropped(d, outcome));
        } else {
            telemetry::global().dictation(d, outcome);
        }
    }

    fn finish_recording(
        &self,
        recorder: &mut Recorder,
        down: Instant,
        released: Instant,
        mut d: Box<Dictation>,
        mut feed: Feed,
    ) {
        // Where the text should go: pasting is skipped if focus has moved by then.
        let target = agt_platform::foreground_window();
        let mut rec = recorder.stop();
        feed.audio.append(&mut rec.samples);
        let samples = &feed.audio;
        let held = released.saturating_duration_since(down);
        // A few µs per second of audio.
        let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt();
        tracing::debug!(
            "recorded {:.2} s from the microphone (rms {rms:.4})",
            secs(samples),
        );
        if let Some(first) = rec.first_audio {
            tracing::debug!("first audio {:.1} ms after recording started", ms(first));
        }
        d.hold_ms = Some(ms(held));
        d.rms = Some(rms.into());
        d.first_audio_ms = rec.first_audio.map(ms);
        if held < Duration::from_millis(self.config.hotkey.min_hold_ms) {
            tracing::info!(
                "hold of {} ms is shorter than min_hold_ms; ignored",
                held.as_millis()
            );
            self.hub.update(|s| s.recording = false);
            self.drop_dictation(feed, d, Ended::TooShort);
            return;
        }
        // The last bit of audio may still end a segment.
        feed.push(self.test_wav.as_deref(), &self.to_stt);
        let scope = feed.shared.scope.clone();
        let live = feed.live.cut().then(|| {
            let (tail, stats) = feed.live.finish();
            let queued = feed.shared.queued.load(Relaxed) + tail.len();
            live_stats(&mut d, &stats, feed.max_queue.max(queued));
            d.live_tail_joined = Some(stats.tail_joined);
            d.live_tail_secs = Some(stats.tail as f64 / SAMPLE_RATE as f64);
            (tail, feed.shared)
        });
        let audio = self.test_wav.clone().unwrap_or(feed.audio);
        d.audio_secs = Some(secs(&audio));
        self.hub.update(|s| {
            s.recording = false;
            s.pending += 1;
        });
        let _ = self.to_stt.send(ToStt::Transcribe(Job {
            audio,
            released,
            target,
            dictation: d,
            live,
            scope,
        }));
    }
}

fn load_engine(config: &Config, hub: &Hub) -> Option<Box<dyn SttEngine>> {
    let t = Instant::now();
    let mut last_pct = u64::MAX;
    // When the download finished, and its size (the model files are fetched first).
    let mut downloaded: Option<(Instant, u64)> = None;
    let loaded = agt_stt::load(&config.stt, &mut |p| {
        if p.done >= p.total {
            downloaded = Some((Instant::now(), p.total));
        }
        let pct = p.done * 100 / p.total.max(1);
        if pct != last_pct {
            last_pct = pct;
            if pct % 10 == 0 {
                tracing::info!(
                    "model download {pct}% ({} / {} MB)",
                    p.done >> 20,
                    p.total >> 20
                );
            }
            hub.update(|s| s.engine = Engine::Loading(format!("Downloading model... {pct}%")));
        }
    })
    .and_then(|mut engine| {
        let loaded = Instant::now();
        hub.update(|s| s.engine = Engine::Loading("Loading model...".into()));
        engine.warmup()?;
        Ok((engine, loaded))
    });
    match loaded {
        Ok((engine, loaded)) => {
            let name = engine.name();
            tracing::info!("{name} ready ({:.1} s)", t.elapsed().as_secs_f64());
            let note = engine.notice();
            let load_start = downloaded.map_or(t, |(at, _)| at);
            telemetry::global().session(telemetry::SessionUpdate {
                stt_backend: Some(name),
                stt_notice: note.clone(),
                model_download_ms: downloaded.map(|(at, _)| ms(at - t)),
                model_download_bytes: downloaded.map(|(_, bytes)| bytes as i64),
                model_load_ms: Some(ms(loaded.saturating_duration_since(load_start))),
                warmup_ms: Some(ms(loaded.elapsed())),
                ..Default::default()
            });
            hub.update(|s| s.engine = Engine::Ready(note));
            Some(engine)
        }
        Err(e) => {
            tracing::error!("cannot load speech model: {e}");
            hub.update(|s| {
                s.engine =
                    Engine::Failed("model unavailable, see log (press the hotkey to retry)".into())
            });
            None
        }
    }
}

/// The texts so far of the recording whose segments are transcribed while it's recorded.
struct Partial {
    shared: Arc<Shared>,
    texts: Texts,
    /// Time spent on its segments, and on calls whose text was discarded (it was dropped
    /// meanwhile).
    stt_ms: f64,
    discarded_ms: f64,
    error: Option<String>,
    /// Polishing it in pieces while recording; `None` when polish is off or needs the whole
    /// text (see [`Ahead::new`]).
    ahead: Option<Ahead>,
    /// How many of `texts` went to `ahead`.
    fed: usize,
}

impl Partial {
    /// `polish`: the settings, with polish on.
    fn new(shared: Arc<Shared>, polish: Option<&PolishConfig>) -> Self {
        Self {
            shared,
            texts: Texts::default(),
            stt_ms: 0.0,
            discarded_ms: 0.0,
            error: None,
            ahead: polish.and_then(Ahead::new),
            fed: 0,
        }
    }

    /// Hands every text but the last to `ahead`, and its pieces to `polish`. The last one
    /// waits for the next, or for release: a short tail may be transcribed again together
    /// with it, and that text replaces it ([`Texts::add`] with `redo`).
    fn feed_ahead(&mut self, to_polish: &Sender<ToPolish>) {
        // After a failed segment the dictation fails too: nothing more to polish.
        let Some(ahead) = self.ahead.as_mut().filter(|_| self.error.is_none()) else {
            return;
        };
        while self.fed + 1 < self.texts.0.len() {
            for piece in ahead.push(&self.texts.0[self.fed]) {
                tracing::debug!(
                    "polishing {} words while recording",
                    piece.split_whitespace().count()
                );
                let _ = to_polish.send(ToPolish::Piece(piece, self.shared.clone()));
            }
            self.fed += 1;
        }
    }

    /// At release, once the tail is transcribed: the pieces left to polish, if it's
    /// polished in pieces.
    fn finish_ahead(&mut self) -> Option<Vec<String>> {
        let rest = self.texts.0[self.fed.min(self.texts.0.len())..].join(" ");
        let pieces = self.ahead.take()?.finish(&rest);
        Some(pieces.into_iter().filter(|p| !p.is_empty()).collect())
    }
}

/// One engine call. Notices a change of backend (e.g. the GPU failed and the engine
/// switched to the CPU), which may also change how long segments should be.
fn transcribe(
    engine: &mut dyn SttEngine,
    audio: &[f32],
    opts: &TranscribeOpts,
    hub: &Hub,
    chunk_secs: &AtomicU32,
) -> Result<String, String> {
    let notice = engine.notice();
    let result = engine.transcribe(audio, opts);
    if engine.notice() != notice {
        let note = engine.notice();
        telemetry::global().session(telemetry::SessionUpdate {
            stt_backend: Some(engine.name()),
            stt_notice: note.clone(),
            ..Default::default()
        });
        hub.update(|s| s.engine = Engine::Ready(note));
        set_chunk_secs(engine, chunk_secs);
    }
    result.map(|t| t.text).map_err(|e| e.to_string())
}

/// Tells the audio thread how long the engine's chunks are (shorter for a GPU window).
fn set_chunk_secs(engine: &dyn SttEngine, chunk_secs: &AtomicU32) {
    let secs = engine.chunk_secs().unwrap_or(agt_stt::CHUNK_SECS);
    chunk_secs.store(secs.to_bits(), Relaxed);
}

#[allow(clippy::too_many_arguments)]
fn stt_thread(
    config: Config,
    hub: Hub,
    jobs: Receiver<ToStt>,
    to_polish: Sender<ToPolish>,
    polishing: bool,
    history: Option<History>,
    chunk_secs: Arc<AtomicU32>,
    stt_loaded: SttLoaded,
) {
    let polish = polishing.then_some(&config.polish);
    let boost = config.stt.boost();
    let loading = SetOnDrop(stt_loaded);
    let mut engine = load_engine(&config, &hub);
    // Loaded or failed: either way S1-mini no longer competes with it for the network.
    drop(loading);
    if let Some(e) = &engine {
        set_chunk_secs(e.as_ref(), &chunk_secs);
    }
    let mut partial: Option<Partial> = None;
    // When the engine's last call ended (it's been idle since: a GPU wakes up slowly).
    let mut engine_done: Option<Instant> = None;
    loop {
        // Free from here on, until the next message.
        let free_at = Instant::now();
        let Ok(msg) = jobs.recv() else { return };
        let mut job = match msg {
            ToStt::RetryLoad => {
                if engine.is_none() {
                    hub.update(|s| s.engine = Engine::Loading("Loading model...".into()));
                    engine = load_engine(&config, &hub);
                    if let Some(e) = &engine {
                        set_chunk_secs(e.as_ref(), &chunk_secs);
                    }
                }
                continue;
            }
            ToStt::Segment(piece, shared) => {
                shared.queued.fetch_sub(1, Relaxed);
                if !partial
                    .as_ref()
                    .is_some_and(|p| Arc::ptr_eq(&p.shared, &shared))
                {
                    partial = Some(Partial::new(shared.clone(), polish));
                }
                let p = partial.get_or_insert_with(|| Partial::new(shared.clone(), polish));
                let Some(engine) = engine.as_mut().filter(|_| !shared.dropped.load(Relaxed)) else {
                    continue;
                };
                let t = Instant::now();
                let opts = &opts(&shared.scope, boost);
                let result = transcribe(engine.as_mut(), &piece.audio, opts, &hub, &chunk_secs);
                engine_done = Some(Instant::now());
                let took = ms(t.elapsed());
                if shared.dropped.load(Relaxed) {
                    tracing::debug!("recording dropped while a segment was transcribed");
                    p.discarded_ms += took;
                    continue;
                }
                tracing::debug!(
                    "segment of {:.1} s transcribed while recording ({took:.0} ms)",
                    secs(&piece.audio)
                );
                p.stt_ms += took;
                match result {
                    Ok(text) => {
                        p.texts.add(text, piece.redo);
                        p.feed_ahead(&to_polish);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "segment {} ({:.1} s) failed to transcribe; the dictation will fail: {e}",
                            p.texts.0.len() + 1,
                            secs(&piece.audio)
                        );
                        p.error = Some(e);
                    }
                }
                continue;
            }
            ToStt::Dropped(mut d, outcome) => {
                // Its segments are behind us: skipped, or the last one discarded.
                if let Some(p) = partial.take() {
                    d.live_stt_ms = Some(p.stt_ms);
                    d.live_discarded_ms = (p.discarded_ms > 0.0).then_some(p.discarded_ms);
                }
                telemetry::global().dictation(d, outcome);
                continue;
            }
            ToStt::Transcribe(job) => job,
        };
        let mut d = job.dictation;
        let Some(engine) = engine.as_mut() else {
            hub.update(|s| s.pending -= 1);
            telemetry::global().dictation(d, Ended::NotReady);
            continue;
        };
        let stt_start = Instant::now();
        // Polished in pieces: those left to polish.
        let mut rest = None;
        let opts = &opts(&job.scope, boost);
        let result = match job.live.take() {
            None => transcribe(engine.as_mut(), &job.audio, opts, &hub, &chunk_secs),
            // Most of it was transcribed while recording: only the tail is left.
            Some((tail, shared)) => {
                let mut p = partial
                    .take()
                    .filter(|p| Arc::ptr_eq(&p.shared, &shared))
                    .unwrap_or_else(|| Partial::new(shared, None));
                // Still busy at release: a segment, or an earlier dictation.
                d.live_busy_at_release = Some(free_at > job.released);
                d.live_idle_ms = engine_done.map(|t| ms(stt_start.saturating_duration_since(t)));
                d.live_stt_ms = Some(p.stt_ms);
                for piece in tail {
                    match transcribe(engine.as_mut(), &piece.audio, opts, &hub, &chunk_secs) {
                        Ok(text) => p.texts.add(text, piece.redo),
                        Err(e) => p.error = Some(e),
                    }
                }
                rest = p.finish_ahead().map(|r| (r, p.shared.clone()));
                p.error.map_or_else(|| Ok(p.texts.join()), Err)
            }
        };
        let stt_end = Instant::now();
        engine_done = Some(stt_end);
        d.queue_wait_ms = Some(ms(stt_start.saturating_duration_since(job.released)));
        d.stt_ms = Some(ms(stt_end - stt_start));
        d.release_to_text_ms = Some(ms(stt_end.saturating_duration_since(job.released)));
        d.stt_backend = Some(engine.name());
        if d.live_stt_ms.is_some() {
            tracing::debug!(
                "{:.1} s transcribed while recording ({} segments, {:.0} ms); release → text {:.0} ms ({:.1} s left)",
                d.audio_secs.unwrap_or_default(),
                d.live_pause_cuts.unwrap_or(0) + d.live_limit_cuts.unwrap_or(0),
                d.live_stt_ms.unwrap_or_default(),
                d.release_to_text_ms.unwrap_or_default(),
                d.live_tail_secs.unwrap_or_default(),
            );
        }
        match result {
            Ok(t) => {
                let text = finish_text(&t, false);
                d.words_raw = Some(text.split_whitespace().count() as i64);
                // The audio is moved, not copied: transcription is done with it. The raw
                // text's copy is a few hundred bytes: nanoseconds before the paste.
                let saved = history.as_ref().map(|h| {
                    Box::new(history::Entry {
                        started_at: d.started_at,
                        audio: if h.save_audio {
                            std::mem::take(&mut job.audio)
                        } else {
                            Vec::new()
                        },
                        audio_secs: d.audio_secs.unwrap_or_default(),
                        text: h.save_text.then(|| history::Text {
                            raw: text.clone(),
                            ..Default::default()
                        }),
                    })
                });
                if text.is_empty() {
                    tracing::info!(
                        "no speech recognized in {:.1} s of audio (stt {:.0} ms)",
                        d.audio_secs.unwrap_or_default(),
                        ms(stt_end - stt_start)
                    );
                    hub.update(|s| s.pending -= 1);
                    // No speech is kept too: what the model missed is worth having.
                    if let (Some(h), Some(e)) = (&history, saved) {
                        h.save(e);
                        d.history_saved = true;
                    }
                    telemetry::global().dictation(d, Ended::NoSpeech);
                } else {
                    let _ = to_polish.send(ToPolish::Text(Box::new(Transcribed {
                        text,
                        rest,
                        ready: stt_end,
                        released: job.released,
                        target: job.target,
                        dictation: d,
                        saved,
                        scope: job.scope,
                    })));
                }
            }
            Err(e) => {
                tracing::error!("transcription failed: {e}");
                hub.update(|s| {
                    s.pending -= 1;
                    s.fail("transcription failed (see log)");
                });
                telemetry::global().dictation(d, Ended::TranscribeFailed);
            }
        }
    }
}

/// A dictation polished in pieces (`agt_polish::ahead`): its pieces polished so far.
struct Pieces {
    shared: Arc<Shared>,
    texts: Vec<String>,
    outcomes: Vec<Outcome>,
    /// Words of the pieces as transcribed (before the dictionary's replacements).
    raw_words: usize,
    /// The dictionary's replacements made in the pieces.
    replacements: usize,
    /// Time spent on the pieces polished in the background, before the transcript was ready.
    ahead_ms: f64,
    /// A request ran out of time: the rest of the pieces stay raw.
    timed_out: bool,
}

impl Pieces {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            texts: Vec::new(),
            outcomes: Vec::new(),
            raw_words: 0,
            replacements: 0,
            ahead_ms: 0.0,
            timed_out: false,
        }
    }

    /// Applies the dictionary's replacements to the next piece (`raw`, as transcribed) and
    /// polishes it, or keeps it unpolished once a request for this dictation failed by
    /// running out of time: a hung server would make every piece wait out its own deadline,
    /// one after the other. A quick failure (a server restarting) doesn't stop the rest.
    fn polish(&mut self, p: &dyn Polisher, raw: &str, cfg: &PolishConfig, scope: &Scope) {
        self.raw_words += raw.split_whitespace().count();
        let (piece, replaced) = scope.replace(raw);
        self.replacements += replaced;
        let piece = piece.as_ref();
        let words = piece.split_whitespace().count();
        let (text, outcome) = if self.timed_out {
            let why = "an earlier piece timed out".to_owned();
            (piece.to_owned(), Outcome::Failed(why))
        } else {
            let t = Instant::now();
            let (text, outcome) = agt_polish::polish_or_raw(p, piece, cfg);
            log_outcome(&outcome, piece, "piece");
            self.timed_out =
                matches!(outcome, Outcome::Failed(_)) && t.elapsed() >= cfg.timeout(words);
            (text, outcome)
        };
        self.texts.push(text);
        self.outcomes.push(outcome);
    }

    /// The pieces joined, how they went as one outcome ("partial" if only some were
    /// polished), and how many weren't.
    fn finish(self) -> (String, &'static str, usize) {
        let raw = self
            .outcomes
            .iter()
            .filter(|o| **o != Outcome::Polished)
            .count();
        let outcome = match (raw, self.outcomes.first()) {
            (0, _) => "polished",
            (n, Some(first)) if n == self.outcomes.len() => outcome_name(first),
            _ => "partial",
        };
        (self.texts.join(" "), outcome, raw)
    }
}

fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Polished => "polished",
        Outcome::Skipped(agt_polish::guard::TOO_LONG) => "too_long",
        Outcome::Skipped(_) => "skipped",
        Outcome::Failed(_) | Outcome::Fallback(_) => "fallback",
    }
}

/// `what`: "transcript", or "piece" of one.
fn log_outcome(outcome: &Outcome, text: &str, what: &str) {
    match outcome {
        Outcome::Failed(why) | Outcome::Fallback(why) => {
            tracing::warn!("polish failed, pasting the raw {what}: {why}")
        }
        Outcome::Skipped(agt_polish::guard::TOO_LONG) => tracing::info!(
            "{what} too long to polish ({} words); pasting it raw",
            text.split_whitespace().count()
        ),
        _ => {}
    }
}

fn polish_thread(
    config: Config,
    jobs: Receiver<ToPolish>,
    to_paste: Sender<ToPaste>,
    polisher: Option<Arc<dyn Polisher>>,
) {
    // A piece may be shorter than `polish.min_words`; the dictation it's from isn't.
    let piece_cfg = PolishConfig {
        min_words: 1,
        ..config.polish.clone()
    };
    // The recording in progress, or the last one, if it's polished in pieces.
    let mut pieces: Option<Pieces> = None;
    for msg in jobs {
        let job = match msg {
            ToPolish::Piece(text, shared) => {
                if !pieces
                    .as_ref()
                    .is_some_and(|p| Arc::ptr_eq(&p.shared, &shared))
                {
                    pieces = Some(Pieces::new(shared.clone()));
                }
                let (Some(polisher), Some(p)) = (polisher.as_deref(), pieces.as_mut()) else {
                    continue;
                };
                // Dropped (cancelled): its pieces still queued are skipped.
                if !shared.dropped.load(Relaxed) {
                    let t = Instant::now();
                    p.polish(polisher, &text, &piece_cfg, &shared.scope);
                    p.ahead_ms += ms(t.elapsed());
                }
                continue;
            }
            ToPolish::Text(job) => job,
        };
        let Transcribed {
            mut text,
            rest,
            ready,
            released,
            target,
            dictation: mut d,
            mut saved,
            scope,
        } = *job;
        // Pieces sent before it are its own (or a failed dictation's).
        let earlier = pieces.take();
        // Polished in pieces, the replacements are made in each (see `Pieces::polish`).
        let mut replacements = None;
        if let Some(polisher) = polisher.as_deref() {
            let start = Instant::now();
            d.polish_wait_ms = Some(ms(start.saturating_duration_since(ready)));
            let split = rest.and_then(|(rest, shared)| {
                let p = earlier
                    .filter(|p| Arc::ptr_eq(&p.shared, &shared))
                    .unwrap_or_else(|| Pieces::new(shared));
                let rest_words: usize = rest.iter().map(|r| r.split_whitespace().count()).sum();
                if p.raw_words + rest_words != text.split_whitespace().count() {
                    tracing::warn!(
                        "polished pieces don't add up to the transcript; polishing it whole"
                    );
                    return None;
                }
                // Nothing went ahead and it's one piece: the whole text, as always.
                (!p.texts.is_empty() || rest.len() > 1).then_some((p, rest, rest_words))
            });
            let outcome = match split {
                Some((mut p, rest, words)) => {
                    let ahead = p.texts.len();
                    for piece in &rest {
                        p.polish(polisher, piece, &piece_cfg, &scope);
                    }
                    replacements = Some(p.replacements);
                    tracing::debug!(
                        "polished in {} pieces, {ahead} in the background ({:.0} ms); {words} words left at release",
                        ahead + rest.len(),
                        p.ahead_ms
                    );
                    d.polish_ahead_pieces = Some(ahead as i64);
                    d.polish_ahead_ms = Some(p.ahead_ms);
                    d.polish_release_words = Some(words as i64);
                    let (joined, outcome, raw) = p.finish();
                    d.polish_fallbacks = Some(raw as i64);
                    text = joined;
                    outcome
                }
                None => {
                    replacements = Some(replace_in(&scope, &mut text));
                    let (polished, outcome) =
                        agt_polish::polish_or_raw(polisher, &text, &config.polish);
                    log_outcome(&outcome, &text, "transcript");
                    text = polished;
                    outcome_name(&outcome)
                }
            };
            d.polish_ms = Some(ms(start.elapsed()));
            d.polish_outcome = Some(outcome);
        }
        let replacements = replacements.unwrap_or_else(|| replace_in(&scope, &mut text));
        d.replacements = Some(replacements as i64);
        if let Some(t) = saved.as_mut().and_then(|e| e.text.as_mut()) {
            t.polish_outcome = d.polish_outcome;
            t.polished =
                matches!(d.polish_outcome, Some("polished" | "partial")).then(|| text.clone());
        }
        if config.paste.trailing_space && !text.is_empty() {
            text.push(' ');
        }
        let _ = to_paste.send(ToPaste::Paste {
            text,
            released,
            target,
            dictation: d,
            saved,
        });
    }
}

/// Trims and collapses whitespace; optionally adds one trailing space.
fn finish_text(raw: &str, trailing_space: bool) -> String {
    let mut text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if trailing_space && !text.is_empty() {
        text.push(' ');
    }
    text
}

/// The user's clipboard, saved off-thread: a clipboard owner that renders lazily can take
/// seconds, or hang forever, so the paste thread never waits on it for long.
#[derive(Default)]
struct Saver {
    saved: Option<ClipboardSnapshot>,
    /// Which save finished last (see [`Saver::start`]) and how long it took (ms), for
    /// telemetry.
    save_ms: Option<(u64, f64)>,
    /// How many saves were started; the running one is number `starts`.
    starts: u64,
    in_flight: Option<Receiver<(Option<ClipboardSnapshot>, f64)>>,
}

impl Saver {
    /// Starts saving the clipboard unless a save is already running. Returns the new save's
    /// number, if it started one.
    fn start(&mut self) -> Option<u64> {
        if self.in_flight.is_some() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        spawn("clipboard-save", move || {
            let _ = tx.send(snapshot());
        });
        self.in_flight = Some(rx);
        self.starts += 1;
        Some(self.starts)
    }

    /// Picks up a finished save, waiting at most `wait`.
    fn collect(&mut self, wait: Duration) {
        if let Some(rx) = &self.in_flight {
            match rx.recv_timeout(wait) {
                Ok((s, took)) => {
                    self.saved = s;
                    self.save_ms = Some((self.starts, took));
                    self.in_flight = None;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.in_flight = None,
            }
        }
    }
}

/// Puts our text on the clipboard with `set`. If that fails while our own key-down save is
/// still running, the clipboard is most likely busy *because of that save* (it holds the
/// clipboard open while a slow app renders a large copy): wait up to `wait` for the save to
/// finish, then try once more (issue #3). A save still running after that is a hung app;
/// retrying would fail the same way.
fn set_clipboard_retrying<T>(
    saver: &mut Saver,
    wait: Duration,
    mut set: impl FnMut() -> Result<T, agt_platform::Error>,
) -> Result<T, agt_platform::Error> {
    let err = match set() {
        Err(e) if saver.in_flight.is_some() => e,
        result => return result,
    };
    tracing::info!(
        "clipboard busy while our clipboard save is still running ({err}); waiting for it"
    );
    let t = Instant::now();
    saver.collect(wait);
    if saver.in_flight.is_some() {
        tracing::warn!(
            "clipboard save still running after {} ms; giving up",
            wait.as_millis()
        );
        return Err(err);
    }
    tracing::info!(
        "clipboard save finished after {:.0} ms; retrying",
        ms(t.elapsed())
    );
    set()
}

/// Whether the paste keystroke may be sent. Between key release and paste there can be seconds
/// (transcription, LLM polish, a busy clipboard), and the user may have switched to another
/// app meanwhile: the dictation must not land there. So only paste if the window that had
/// focus at release (`at_release`) still has it (`now`; a `None` now means focus is in
/// transit, e.g. mid Alt+Tab, so no).
///
/// If the window at release is unknown (`None`: no foreground window at that moment, e.g.
/// the desktop or a focus change in progress, or a platform without the check), there's
/// nothing to compare with: paste as before, rather than never pasting.
fn may_paste(at_release: Option<usize>, now: Option<usize>) -> bool {
    at_release.is_none() || at_release == now
}

/// The hotkey keys a paste would have to release in software if they were held.
fn paste_sensitive_keys(hotkey: &Hotkey) -> Vec<Key> {
    hotkey
        .keys()
        .iter()
        .copied()
        .filter(|&k| agt_platform::paste_may_release(k))
        .collect()
}

/// Waits while `held()` says a paste-sensitive hotkey key is down, polling every `poll`, for
/// at most `max` (see [`HOTKEY_RELEASE_MAX`]). Returns how long it waited, and whether the
/// keys were released (false: gave up).
fn wait_for_hotkey_release(
    mut held: impl FnMut() -> bool,
    poll: Duration,
    max: Duration,
) -> (Duration, bool) {
    let start = Instant::now();
    loop {
        if !held() {
            return (start.elapsed(), true);
        }
        if start.elapsed() >= max {
            return (start.elapsed(), false);
        }
        thread::sleep(poll);
    }
}

/// How long the last transcript is kept for "Copy last transcript".
const LAST_TRANSCRIPT_TTL: Duration = Duration::from_secs(120);

/// The last transcript (pasted or not), for "Copy last transcript": in memory only, and
/// forgotten after [`LAST_TRANSCRIPT_TTL`].
#[derive(Default)]
struct LastTranscript(Option<(String, Instant)>);

impl LastTranscript {
    fn set(&mut self, text: &str, now: Instant) {
        self.0 = Some((text.to_owned(), now));
    }

    fn get(&self, now: Instant) -> Option<&str> {
        self.0
            .as_ref()
            .filter(|(_, at)| now < *at + LAST_TRANSCRIPT_TTL)
            .map(|(t, _)| t.as_str())
    }

    /// When it has to be forgotten.
    fn deadline(&self) -> Option<Instant> {
        self.0.as_ref().map(|(_, at)| *at + LAST_TRANSCRIPT_TTL)
    }

    /// Forgets it if it's due; returns whether it did.
    fn expire(&mut self, now: Instant) -> bool {
        if self.0.is_some() && self.get(now).is_none() {
            self.0 = None;
            return true;
        }
        false
    }
}

/// Ends the tray text of every paste failure: the transcript can still be had.
const USE_COPY: &str = "use Copy last transcript";
const COPY_FAILED: &str = "clipboard busy; last transcript not copied";

/// Errors that a successful "Copy last transcript" makes moot.
fn is_paste_error(e: &str) -> bool {
    e.ends_with(USE_COPY) || e == COPY_FAILED
}

/// Puts the user's clipboard back right away (our text went nowhere).
fn undo_clipboard(
    saver: &mut Saver,
    token: &PasteToken,
    pending_restore: &mut Option<(PasteToken, Instant)>,
) {
    if let Some(snap) = saver.saved.take() {
        if let Err(e) = agt_platform::restore_clipboard(&snap, token) {
            tracing::warn!("cannot restore clipboard: {e}");
        }
        *pending_restore = None;
    }
}

fn paste_thread(
    config: Config,
    hub: Hub,
    rx: Receiver<ToPaste>,
    hotkey: Hotkey,
    history: Option<History>,
) {
    let restore_after = Duration::from_millis(config.paste.restore_clipboard_ms);
    let restore = !restore_after.is_zero();
    // Only the overlay shows the "not in a text box" notice: without it, don't ask apps.
    let check_focus = config.overlay.enabled;
    let mut saver = Saver::default();
    // Our paste that still has to be undone, and when.
    let mut pending_restore: Option<(PasteToken, Instant)> = None;
    let mut last = LastTranscript::default();
    let sensitive = paste_sensitive_keys(&hotkey);
    // The clipboard save started for the next paste (its time is credited to that one).
    let mut save_for_paste: Option<u64> = None;

    loop {
        let deadline = [pending_restore.as_ref().map(|(_, at)| *at), last.deadline()]
            .into_iter()
            .flatten()
            .min();
        let msg = match deadline {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(ToPaste::Snapshot) => {
                // While our transcript is still on the clipboard, the saved snapshot already
                // holds the user's data; saving now would save our own text instead.
                if restore && pending_restore.is_none() {
                    saver.saved = None;
                    save_for_paste = saver.start();
                }
                // After starting the save (it runs on its own thread), and here, not on the
                // audio thread: it asks the frontmost app, which may be slow.
                if check_focus {
                    agt_platform::prepare_focus_check();
                }
            }
            Ok(ToPaste::Paste {
                text,
                released,
                target,
                dictation: mut d,
                saved,
            }) => {
                // Later messages (the next dictation's paste included) queue up behind this
                // wait, so pastes stay in order.
                let held = || {
                    sensitive
                        .iter()
                        .any(|&k| agt_platform::key_down(k) == Some(true))
                };
                if !sensitive.is_empty() && held() {
                    let (waited, released) =
                        wait_for_hotkey_release(held, HOTKEY_RELEASE_POLL, HOTKEY_RELEASE_MAX);
                    d.hotkey_wait_ms = Some(ms(waited));
                    if released {
                        tracing::debug!(
                            "paste waited {:.0} ms for {hotkey} to be released",
                            ms(waited)
                        );
                    } else {
                        tracing::warn!(
                            "{hotkey} still held after {} s; pasting anyway",
                            waited.as_secs()
                        );
                    }
                }
                let paste_start = Instant::now();
                if restore && pending_restore.is_none() {
                    saver.collect(SNAPSHOT_WAIT);
                    // Save again if the user copied something during the hold, or if no save is
                    // running. A save still in flight (slow or hung clipboard owner) is not waited
                    // for again: we paste now and the restore picks up its result if it arrives.
                    let stale = saver
                        .saved
                        .as_ref()
                        .is_some_and(ClipboardSnapshot::is_stale);
                    if stale || (saver.saved.is_none() && saver.in_flight.is_none()) {
                        saver.saved = None;
                        save_for_paste = saver.start().or(save_for_paste);
                        saver.collect(SNAPSHOT_WAIT);
                    }
                    if saver.saved.is_none() && saver.in_flight.is_none() {
                        tracing::warn!("clipboard could not be saved; it won't be restored");
                    }
                }
                if last.get(paste_start).is_none() {
                    hub.copy_available(true);
                }
                last.set(&text, paste_start);
                // Focus already moved on: don't even touch the clipboard.
                let set = if may_paste(target, agt_platform::foreground_window()) {
                    let set = set_clipboard_retrying(&mut saver, BUSY_SAVE_WAIT, || {
                        agt_platform::set_clipboard_text(&text)
                    });
                    if saver.saved.is_none() && saver.in_flight.is_some() {
                        tracing::warn!(
                            "clipboard not saved in time; it won't be restored unless the save finishes first"
                        );
                    }
                    Some(set)
                } else {
                    None
                };
                if matches!(set, Some(Ok(_))) {
                    // Else clipboard listeners may still hold the clipboard when the target
                    // reads it, and some targets then paste nothing (#75).
                    d.clipboard_settle_ms = agt_platform::wait_for_clipboard_listeners().map(ms);
                }
                let result = match set {
                    None => Err(PasteError::WindowChanged),
                    Some(Err(e)) => Err(PasteError::ClipboardBusy(e)),
                    // Checked again right before pasting: setting the clipboard can take seconds.
                    Some(Ok(token)) if !may_paste(target, agt_platform::foreground_window()) => {
                        // Don't leave the user's clipboard replaced by text that went nowhere.
                        undo_clipboard(&mut saver, &token, &mut pending_restore);
                        Err(PasteError::WindowChanged)
                    }
                    Some(Ok(token)) => match agt_platform::send_paste() {
                        Ok(()) => Ok(token),
                        Err(e) => {
                            undo_clipboard(&mut saver, &token, &mut pending_restore);
                            Err(PasteError::Blocked(e))
                        }
                    },
                };
                let pasted = Instant::now();
                hub.update(|s| {
                    s.pending -= 1;
                    match &result {
                        Ok(_) => s.error = None,
                        Err(e) => s.fail(&e.tray_text()),
                    }
                });
                // Everything below is bookkeeping after the paste: off the latency path.
                let chars = text.chars().count();
                d.chars_out = Some(chars as i64);
                d.words_out = Some(text.split_whitespace().count() as i64);
                // Only the save made for this paste, and only if it finished by now.
                let ours = save_for_paste.take();
                d.clipboard_save_ms = saver
                    .save_ms
                    .filter(|&(n, _)| Some(n) == ours)
                    .map(|(_, took)| took);
                d.paste_ms = Some(ms(pasted - paste_start));
                if let (Some(h), Some(mut e)) = (&history, saved) {
                    if let Some(t) = e.text.as_mut() {
                        t.pasted = result.is_ok().then_some(text);
                    }
                    h.save(e);
                    d.history_saved = true;
                }
                match result {
                    Ok(token) => {
                        d.release_to_paste_ms = Some(ms(pasted - released));
                        // At debug: the same numbers are in the `dictations` table.
                        tracing::debug!("dictation: {}", timings(&d));
                        if restore {
                            pending_restore = Some((token, pasted + restore_after));
                        }
                        telemetry::global().dictation(d, Ended::Pasted);
                        // Last, once the restore is scheduled: it asks the focused app, which
                        // may take up to a second while it handles the paste. Pasted anyway
                        // (the check can be wrong, and an app may take a paste outside a text
                        // field, like a design canvas), so this only offers a way out.
                        if check_focus && agt_platform::focus_takes_text() == Some(false) {
                            tracing::info!("pasted, but focus isn't in a text box; offering Copy");
                            hub.not_in_text_box();
                        }
                    }
                    Err(e) => {
                        tracing::error!("paste failed ({chars} chars not pasted): {e}");
                        telemetry::global().dictation(d, e.outcome());
                    }
                }
            }
            Ok(ToPaste::CopyLast) => {
                let Some(text) = last.get(Instant::now()) else {
                    tracing::info!("no transcript to copy (none yet, or forgotten)");
                    continue;
                };
                // Like our pasted text, it's kept out of Win+V history and clipboard managers.
                let set = set_clipboard_retrying(&mut saver, BUSY_SAVE_WAIT, || {
                    agt_platform::set_clipboard_text(text)
                });
                match set {
                    Ok(_) => {
                        // The user wants it there now: don't put the old clipboard back.
                        pending_restore = None;
                        hub.update(|s| {
                            if s.error.as_deref().is_some_and(is_paste_error) {
                                s.error = None;
                            }
                        });
                        hub.copied();
                        tracing::info!(
                            "last transcript ({} chars) copied to the clipboard",
                            text.chars().count()
                        );
                    }
                    Err(e) => {
                        tracing::error!("cannot copy the last transcript: {e}");
                        // Shown in the tray, but not a dictation failure (no overlay flash).
                        hub.update(|s| s.error = Some(COPY_FAILED.into()));
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if last.expire(now) {
                    tracing::debug!("last transcript forgotten");
                    hub.copy_available(false);
                }
                if pending_restore.as_ref().is_none_or(|(_, at)| now < *at) {
                    continue;
                }
                saver.collect(Duration::ZERO);
                if let (Some((token, _)), Some(snap)) = (pending_restore.take(), saver.saved.take())
                {
                    match agt_platform::restore_clipboard(&snap, &token) {
                        Ok(true) => tracing::debug!("clipboard restored"),
                        Ok(false) => {
                            tracing::debug!("clipboard changed since paste; not restoring")
                        }
                        Err(e) => tracing::warn!("cannot restore clipboard: {e}"),
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

enum PasteError {
    /// Another app is holding the clipboard open (or a hung app never closed it).
    ClipboardBusy(agt_platform::Error),
    /// The paste keystroke could not be sent: on Windows typically because the focused app runs
    /// as administrator, on macOS because AGT lacks the Accessibility permission.
    Blocked(agt_platform::Error),
    /// Pasting was delayed and the user switched windows meanwhile.
    WindowChanged,
}

impl PasteError {
    fn outcome(&self) -> Ended {
        match self {
            PasteError::ClipboardBusy(_) => Ended::ClipboardBusy,
            PasteError::Blocked(_) => Ended::PasteBlocked,
            PasteError::WindowChanged => Ended::FocusChanged,
        }
    }

    fn tray_text(&self) -> String {
        let what = match self {
            PasteError::ClipboardBusy(_) => {
                "clipboard busy (another app is holding it); not pasted"
            }
            PasteError::Blocked(_) if cfg!(target_os = "macos") => &format!(
                "paste blocked ({} permission?); clipboard restored",
                Permission::Accessibility
            ),
            PasteError::Blocked(_) => "paste blocked (admin window?); clipboard restored",
            PasteError::WindowChanged => "not pasted: focus moved to another window",
        };
        format!("{what} - {USE_COPY}")
    }
}

impl fmt::Display for PasteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PasteError::ClipboardBusy(e) => write!(f, "cannot put the text on the clipboard: {e}"),
            PasteError::Blocked(e) => write!(f, "cannot send the paste keystroke: {e}"),
            PasteError::WindowChanged => write!(
                f,
                "another window got focus since the key was released; not pasting there"
            ),
        }
    }
}

/// Saves the clipboard; returns the snapshot and how long it took (ms).
fn snapshot() -> (Option<ClipboardSnapshot>, f64) {
    let t = Instant::now();
    let snap = match agt_platform::snapshot_clipboard() {
        Ok(s) => {
            tracing::debug!("clipboard saved in {:.1} ms", ms(t.elapsed()));
            Some(s)
        }
        Err(e) => {
            tracing::warn!("cannot save clipboard (it won't be restored): {e}");
            None
        }
    };
    (snap, ms(t.elapsed()))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Length of 16 kHz audio in seconds.
fn secs(audio: &[f32]) -> f64 {
    audio.len() as f64 / SAMPLE_RATE as f64
}

/// One line of a pasted dictation's timings, e.g. `audio 2.97 s | release→stt 0 ms |
/// stt 157 ms | paste 4 ms | release→paste 161 ms | 42 chars`.
fn timings(d: &Dictation) -> String {
    let v = |x: Option<f64>| x.unwrap_or(0.0);
    let polish = match (d.polish_ms, d.polish_outcome) {
        (Some(t), Some(outcome)) => format!("polish {t:.0} ms ({outcome}) | "),
        _ => String::new(),
    };
    format!(
        "audio {:.2} s | release→stt {:.0} ms | stt {:.0} ms | {polish}paste {:.0} ms | release→paste {:.0} ms | {} chars",
        v(d.audio_secs),
        v(d.queue_wait_ms),
        v(d.stt_ms),
        v(d.paste_ms),
        v(d.release_to_paste_ms),
        d.chars_out.unwrap_or(0)
    )
}

/// The wall-clock time (unix ms) of an `Instant` in the recent past.
fn wall_clock(t: Instant) -> i64 {
    telemetry::now_ms() - ms(t.elapsed()) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_tap_is_a_press_soon_after_a_tap() {
        let t = Instant::now();
        let gap = Duration::from_millis(400);
        assert!(is_double_tap(Some(t), t + gap, gap));
        assert!(!is_double_tap(
            Some(t),
            t + gap + Duration::from_millis(1),
            gap
        ));
        assert!(!is_double_tap(None, t, gap), "no tap before");
        assert!(!is_double_tap(Some(t), t, Duration::ZERO), "turned off");
    }

    /// `n` words of sentences: "w w w w w. W w w w w. ..." (a sentence end every 5 words).
    fn sentences(n: usize, w: &str) -> String {
        let cap = w[..1].to_uppercase() + &w[1..];
        (0..n)
            .map(|i| match (i % 5, i % 5 == 4) {
                (0, _) => cap.clone(),
                (_, true) => format!("{w}."),
                _ => w.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn pieces_sent(rx: &Receiver<ToPolish>) -> Vec<String> {
        rx.try_iter()
            .map(|m| match m {
                ToPolish::Piece(text, _) => text,
                ToPolish::Text(_) => panic!("not a piece"),
            })
            .collect()
    }

    #[test]
    fn the_last_segment_waits_so_a_redone_tail_can_replace_it() {
        let (tx, rx) = mpsc::channel();
        let polish = PolishConfig::default();
        let mut p = Partial::new(Arc::default(), Some(&polish));
        p.texts.add(sentences(30, "one"), false);
        p.feed_ahead(&tx);
        assert!(pieces_sent(&rx).is_empty(), "the only segment so far waits");
        p.texts.add(sentences(30, "two"), false);
        p.feed_ahead(&tx);
        // The first segment, up to its last sentence end that isn't at its end.
        assert_eq!(pieces_sent(&rx), [sentences(25, "one")]);
        // At release a short tail is transcribed again with the second segment.
        p.texts
            .add(format!("{} and done.", sentences(30, "Two")), true);
        let rest = p.finish_ahead().unwrap();
        assert_eq!(
            rest.join(" "),
            format!(
                "{} {} and done.",
                &sentences(30, "one")[sentences(25, "one").len() + 1..],
                sentences(30, "Two")
            )
        );
        assert!(!rest.join(" ").contains("two"), "the replaced text is gone");
        assert_eq!(
            format!("{} {}", sentences(25, "one"), rest.join(" ")),
            p.texts.join()
        );
    }

    #[test]
    fn nothing_goes_ahead_without_polish_or_for_whole_text_settings() {
        let (tx, rx) = mpsc::channel();
        let mut lists = PolishConfig::default();
        lists.s1_mini.structure = agt_core::config::S1Structure::Lists;
        for polish in [None, Some(&lists)] {
            let mut p = Partial::new(Arc::default(), polish);
            for _ in 0..5 {
                p.texts.add(sentences(30, "word"), false);
                p.feed_ahead(&tx);
            }
            assert!(pieces_sent(&rx).is_empty());
            assert!(p.finish_ahead().is_none());
        }
    }

    /// Ends each text with " ok." (one more word, so the guard accepts it). Fails texts with
    /// "FAIL" in them at once and those with "HANG" after the deadline ([`TEST_TIMEOUT`]),
    /// and answers those with "REPLY" like an assistant (the guard rejects that).
    struct Marks;
    impl Polisher for Marks {
        fn name(&self) -> String {
            "marks".into()
        }
        fn polish(
            &self,
            text: &str,
            _: &agt_core::polish::PolishCtx,
        ) -> agt_core::polish::Result<String> {
            if text.contains("HANG") {
                thread::sleep(TEST_TIMEOUT + Duration::from_millis(5));
                Err("timed out".into())
            } else if text.contains("FAIL") {
                Err("failed".into())
            } else if text.contains("REPLY") {
                Ok("Sure! Here you go.".into())
            } else {
                Ok(format!("{text} ok."))
            }
        }
    }

    const TEST_TIMEOUT: Duration = Duration::from_millis(20);

    /// Runs the polish thread over `msgs`; returns what it hands to `paste`.
    fn polish_all(msgs: Vec<ToPolish>) -> Vec<(String, Box<Dictation>)> {
        polish_with(msgs, Some(Arc::new(Marks)))
            .into_iter()
            .map(|(text, d, _)| (text, d))
            .collect()
    }

    /// [`polish_all`] with a polisher (or none: polish off); also returns the history
    /// entries. The dictionary is each message's own.
    fn polish_with(
        msgs: Vec<ToPolish>,
        polisher: Option<Arc<dyn Polisher>>,
    ) -> Vec<(String, Box<Dictation>, Option<Box<history::Entry>>)> {
        let (tx, rx) = mpsc::channel();
        let (to_paste, pasted) = mpsc::channel();
        for m in msgs {
            tx.send(m).unwrap();
        }
        drop(tx);
        let mut config = Config::default();
        config.paste.trailing_space = false;
        config.polish.timeout_ms = TEST_TIMEOUT.as_millis() as u64;
        config.polish.timeout_per_word_ms = 0;
        polish_thread(config, rx, to_paste, polisher);
        pasted
            .try_iter()
            .map(|m| match m {
                ToPaste::Paste {
                    text,
                    dictation,
                    saved,
                    ..
                } => (text, dictation, saved),
                _ => panic!("not a paste"),
            })
            .collect()
    }

    /// `maine` -> `main`, `dot env` -> `.env` (two words to one).
    fn test_dictionary() -> Arc<Scope> {
        in_app(
            "[[replace]]\nfrom = \"maine\"\nto = \"main\"\n\n[[replace]]\nfrom = \"dot env\"\nto = \".env\"\n",
            None,
        )
    }

    /// The dictionary in `text` as it applies in `app`.
    fn in_app(text: &str, app: Option<&str>) -> Arc<Scope> {
        let d = Arc::new(Dictionary::parse(text).unwrap());
        Arc::new(Scope::new(d, app.map(Into::into)))
    }

    /// A recording's `Shared` with `scope` (as [`Feed::new`] takes it at key-down).
    fn shared_with(scope: Arc<Scope>) -> Arc<Shared> {
        Arc::new(Shared {
            scope,
            ..Shared::default()
        })
    }

    /// A short dictation's transcript, with the dictionary taken at its key-down.
    fn whole_with(text: &str, scope: Arc<Scope>) -> ToPolish {
        let ToPolish::Text(mut t) = whole(text, None) else {
            unreachable!()
        };
        t.scope = scope;
        ToPolish::Text(t)
    }

    /// A transcript saved to the history with its raw text.
    fn with_history(msg: ToPolish) -> ToPolish {
        let ToPolish::Text(mut t) = msg else {
            panic!("not a transcript")
        };
        t.saved = Some(Box::new(history::Entry {
            text: Some(history::Text {
                raw: t.text.clone(),
                ..Default::default()
            }),
            ..Default::default()
        }));
        ToPolish::Text(t)
    }

    #[test]
    fn replacements_apply_before_polish_and_the_history_keeps_the_raw_text() {
        let text = "merge it into Maine and copy the dot env file";
        let out = polish_with(
            vec![with_history(whole_with(text, test_dictionary()))],
            Some(Arc::new(Marks)),
        );
        let [(pasted, d, saved)] = &out[..] else {
            panic!()
        };
        assert_eq!(pasted, "merge it into main and copy the .env file ok.");
        assert_eq!(d.replacements, Some(2));
        let t = saved.as_ref().unwrap().text.as_ref().unwrap();
        assert_eq!(t.raw, text, "the engine's output, for the eval");
        assert_eq!(t.polished.as_deref(), Some(pasted.as_str()));
    }

    #[test]
    fn replacements_apply_with_polish_off_skipped_or_failed() {
        let cases = [
            // Polish off.
            (None, "into Maine please", "into main please"),
            // Too short to polish.
            (Some(Arc::new(Marks) as Arc<dyn Polisher>), "Maine", "main"),
            // Polish failed: the transcript pasted as is, replaced.
            (
                Some(Arc::new(Marks) as Arc<dyn Polisher>),
                "FAIL into Maine please",
                "FAIL into main please",
            ),
        ];
        for (polisher, text, want) in cases {
            let out = polish_with(vec![whole_with(text, test_dictionary())], polisher);
            let [(pasted, d, _)] = &out[..] else { panic!() };
            assert_eq!(pasted, want);
            assert_eq!(d.replacements, Some(1), "{text}");
        }
        // Nothing to replace (or no dictionary): the text as it was, and a count of 0.
        let out = polish_with(vec![whole("hi there", None)], None);
        assert_eq!(out[0].0, "hi there");
        assert_eq!(out[0].1.replacements, Some(0));
    }

    #[test]
    fn replacements_apply_to_every_piece_of_a_long_dictation() {
        let a = shared_with(test_dictionary());
        // Each piece and the rest have a phrase to replace; "dot env" -> ".env" drops a word,
        // which must not make the pieces look like they don't add up to the transcript.
        let one = format!("{} Maine dot env.", sentences(25, "one"));
        let two = format!("{} then dot env.", sentences(25, "two"));
        let out = polish_with(
            vec![
                piece(&one, &a),
                piece(&two, &a),
                transcribed(&[&one, &two], &["into Maine"], &a),
            ],
            Some(Arc::new(Marks)),
        );
        let [(text, d, _)] = &out[..] else { panic!() };
        assert_eq!(
            text,
            &format!(
                "{} main .env. ok. {} then .env. ok. into main ok.",
                sentences(25, "one"),
                sentences(25, "two")
            )
        );
        assert_eq!(
            d.polish_ahead_pieces,
            Some(2),
            "polished in pieces, not whole"
        );
        assert_eq!(d.replacements, Some(4));
    }

    /// The transcript of `ahead` (sent as pieces before) and `rest`, with `rest` left to
    /// polish at release.
    fn transcribed(ahead: &[&str], rest: &[&str], shared: &Arc<Shared>) -> ToPolish {
        let text = ahead
            .iter()
            .chain(rest)
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let rest = rest.iter().map(|r| r.to_string()).collect();
        whole(&text, Some((rest, shared.clone())))
    }

    /// With `rest`, the recording's dictionary is its `Shared`'s (as in the app: both are
    /// the key-down one); else none.
    fn whole(text: &str, rest: Option<(Vec<String>, Arc<Shared>)>) -> ToPolish {
        let now = Instant::now();
        let scope = rest
            .as_ref()
            .map_or_else(Arc::default, |(_, s)| s.scope.clone());
        ToPolish::Text(Box::new(Transcribed {
            text: text.into(),
            rest,
            ready: now,
            released: now,
            target: None,
            dictation: Box::default(),
            saved: None,
            scope,
        }))
    }

    #[test]
    fn a_dictionary_reloaded_mid_dictation_doesnt_mix_into_it() {
        // Key-down: the recording takes the dictionary in use.
        let words = Words::new(
            Dictionary::parse("[[replace]]\nfrom = \"maine\"\nto = \"main\"\n").unwrap(),
        );
        let feed = Feed::new(25.0, Arc::new(Scope::new(words.get(), None)));
        // Reloaded while the user is still talking: Maine now becomes "MAIN".
        words.set(Dictionary::parse("[[replace]]\nfrom = \"maine\"\nto = \"MAIN\"\n").unwrap());
        assert!(!std::ptr::eq(feed.shared.scope.dictionary(), &*words.get()));
        assert!(opts(&feed.shared.scope, 2.0).vocabulary.is_empty());
        // Its pieces polished ahead and the rest at release all use the key-down one.
        let one = format!("{} Maine.", sentences(25, "one"));
        let out = polish_all(vec![
            piece(&one, &feed.shared),
            transcribed(&[&one], &["into Maine"], &feed.shared),
        ]);
        let [(text, d)] = &out[..] else { panic!() };
        assert_eq!(
            text,
            &format!("{} main. ok. into main ok.", sentences(25, "one"))
        );
        assert_eq!(d.replacements, Some(2));
        // The next dictation gets the new one.
        let next = Feed::new(25.0, Arc::new(Scope::new(words.get(), None)));
        let out = polish_all(vec![whole_with(
            "merge into Maine please",
            next.shared.scope.clone(),
        )]);
        assert_eq!(out[0].0, "merge into MAIN please ok.");
    }

    #[test]
    fn the_focused_app_picks_the_profiles() {
        let text = "terms = [\"AGT\"]\n[profiles.code]\napps = [\"Code.exe\"]\nterms = [\"main\"]\n[[replace]]\nfrom = \"dot env\"\nto = \".env\"\nprofiles = [\"code\"]\n";
        let (code, other) = (
            in_app(text, Some("code.EXE")),
            in_app(text, Some("notepad.exe")),
        );
        // The engine gets the app's terms and the boost weight.
        let o = opts(&code, 2.5);
        assert_eq!(
            (&o.vocabulary[..], o.boost),
            (&["AGT".into(), "main".into()][..], 2.5)
        );
        assert_eq!(&opts(&other, 2.5).vocabulary[..], ["AGT"]);
        // A profile's replacement applies only in its apps.
        let out = polish_all(vec![
            whole_with("copy the dot env file", code),
            whole_with("copy the dot env file", other),
        ]);
        assert_eq!(out[0].0, "copy the .env file ok.");
        assert_eq!(out[1].0, "copy the dot env file ok.");
        assert_eq!(
            (out[0].1.replacements, out[1].1.replacements),
            (Some(1), Some(0))
        );
    }

    fn piece(text: &str, shared: &Arc<Shared>) -> ToPolish {
        ToPolish::Piece(text.into(), shared.clone())
    }

    #[test]
    fn pieces_polished_while_recording_are_joined_with_the_rest() {
        let a = Arc::<Shared>::default();
        let (one, two) = (sentences(25, "one"), sentences(25, "two"));
        let out = polish_all(vec![
            piece(&one, &a),
            piece(&two, &a),
            transcribed(&[&one, &two], &["and the end"], &a),
        ]);
        let [(text, d)] = &out[..] else { panic!() };
        // "and the end" is under min_words, but a piece of a long dictation all the same.
        assert_eq!(text, &format!("{one} ok. {two} ok. and the end ok."));
        assert_eq!(d.polish_outcome, Some("polished"));
        assert_eq!(d.polish_ahead_pieces, Some(2));
        assert_eq!(d.polish_release_words, Some(3));
        assert_eq!(d.polish_fallbacks, Some(0));
        assert!(d.polish_ms.is_some() && d.polish_wait_ms.is_some());
        assert!(d.polish_ahead_ms.is_some());
    }

    #[test]
    fn a_rejected_piece_is_pasted_raw_and_the_rest_polished() {
        let a = Arc::<Shared>::default();
        let one = sentences(25, "one");
        let two = format!("{} REPLY.", sentences(25, "two"));
        let out = polish_all(vec![
            piece(&one, &a),
            piece(&two, &a),
            transcribed(&[&one, &two], &["the end"], &a),
        ]);
        let [(text, d)] = &out[..] else { panic!() };
        assert_eq!(text, &format!("{one} ok. {two} the end ok."));
        assert_eq!(d.polish_outcome, Some("partial"));
        assert_eq!(d.polish_fallbacks, Some(1));
    }

    #[test]
    fn after_a_timed_out_request_the_rest_of_the_pieces_stay_raw() {
        let a = Arc::<Shared>::default();
        let one = format!("{} HANG.", sentences(25, "one"));
        let two = sentences(25, "two");
        let out = polish_all(vec![
            piece(&one, &a),
            piece(&two, &a),
            transcribed(&[&one, &two], &["the end"], &a),
        ]);
        let [(text, d)] = &out[..] else { panic!() };
        // Not sent: a hung server would make each wait out its own deadline.
        assert_eq!(text, &format!("{one} {two} the end"));
        // All raw: the failure's own outcome.
        assert_eq!(d.polish_outcome, Some("fallback"));
        assert_eq!(d.polish_fallbacks, Some(3));
    }

    #[test]
    fn a_quick_failure_doesnt_stop_the_rest() {
        let a = Arc::<Shared>::default();
        let one = format!("{} FAIL.", sentences(25, "one"));
        let two = sentences(25, "two");
        let out = polish_all(vec![
            piece(&one, &a),
            piece(&two, &a),
            transcribed(&[&one, &two], &["the end"], &a),
        ]);
        let [(text, d)] = &out[..] else { panic!() };
        assert_eq!(text, &format!("{one} {two} ok. the end ok."));
        assert_eq!(d.polish_outcome, Some("partial"));
        assert_eq!(d.polish_fallbacks, Some(1));
    }

    #[test]
    fn pieces_that_dont_add_up_fall_back_to_the_whole_text() {
        let a = Arc::<Shared>::default();
        let text = format!("{} the end", sentences(50, "word"));
        // The pieces polished ahead went missing: only the rest arrives.
        let out = polish_all(vec![whole(&text, Some((vec!["the end".into()], a)))]);
        let [(got, d)] = &out[..] else { panic!() };
        assert_eq!(got, &format!("{text} ok."));
        assert_eq!(d.polish_ahead_pieces, None);
    }

    #[test]
    fn a_cancelled_recordings_pieces_are_skipped_and_not_pasted() {
        let (cancelled, next) = (Arc::<Shared>::default(), Arc::<Shared>::default());
        cancelled.dropped.store(true, Relaxed);
        let one = sentences(25, "next");
        let out = polish_all(vec![
            piece(&sentences(25, "gone"), &cancelled),
            piece(&one, &next),
            transcribed(&[&one], &["the end"], &next),
        ]);
        let [(text, d)] = &out[..] else { panic!() };
        assert_eq!(text, &format!("{one} ok. the end ok."));
        assert_eq!(d.polish_ahead_pieces, Some(1));
    }

    #[test]
    fn a_short_dictation_is_polished_whole_as_always() {
        let a = Arc::<Shared>::default();
        let out = polish_all(vec![
            whole("um so this is short", None),
            whole("hi", None),
            // Cut while recording but nothing went ahead: one piece, the whole text.
            transcribed(&[], &["hi there"], &a),
        ]);
        let got: Vec<_> = out
            .iter()
            .map(|(t, d)| (t.as_str(), d.polish_outcome, d.polish_ahead_pieces))
            .collect();
        assert_eq!(
            got,
            [
                ("um so this is short ok.", Some("polished"), None),
                ("hi", Some("skipped"), None),
                ("hi there", Some("skipped"), None),
            ]
        );
    }

    #[test]
    fn text_is_normalized() {
        assert_eq!(finish_text("  Hello,\n world.  ", true), "Hello, world. ");
        assert_eq!(finish_text("Hi", false), "Hi");
        assert_eq!(finish_text("   ", true), "");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn release_watch_trusts_the_hardware_state_on_macos() {
        // The poll reads the HID state, which our injected Cmd+V never changes.
        for name in ["Fn", "RightOption", "RightCommand+F13", "Fn+RightOption"] {
            let w = ReleaseWatch::new(&Hotkey::parse(name).unwrap(), 5);
            assert_eq!(w.paste_may_release, 0, "{name}");
            assert!(paste_sensitive_keys(&Hotkey::parse(name).unwrap()).is_empty());
        }
    }

    #[test]
    fn release_watch_ignores_our_own_paste() {
        let (up, down) = (Some(false), Some(true));
        // A key a paste never touches (Right Ctrl): "up" is always trusted.
        let w = ReleaseWatch::with([false], 5);
        assert!(w.missed(&[up], 5));
        assert!(w.missed(&[up], 6));
        assert!(!w.missed(&[down], 6));
        assert!(!w.missed(&[None], 5));
        // One a paste may release (Shift / Left Ctrl / Win): trusted until a paste released
        // held keys during the recording.
        let w = ReleaseWatch::with([true], 5);
        assert!(w.missed(&[up], 5));
        assert!(!w.missed(&[up], 6), "our paste released it");
        assert!(!w.missed(&[down], 5));
        // Swallowed keys can't be polled at all.
        assert!(!ReleaseWatch::with([false], 0).missed(&[None], 0));
    }

    #[test]
    fn release_watch_checks_every_chord_key() {
        let (up, down) = (Some(false), Some(true));
        // LeftCtrl+LeftWin: a paste may release both.
        let w = ReleaseWatch::with([true, true], 5);
        assert!(!w.missed(&[down, down], 5));
        assert!(w.missed(&[up, down], 5));
        assert!(w.missed(&[down, up], 5));
        assert!(!w.missed(&[up, up], 6));
        // RightCtrl+RightWin: Right Ctrl is still trusted after a paste, Win isn't.
        let w = ReleaseWatch::with([false, true], 5);
        assert!(w.missed(&[up, down], 6));
        assert!(!w.missed(&[down, up], 6));
        // RightCtrl+F13: F13 can't be polled; its Ctrl partner still can.
        let w = ReleaseWatch::with([false, false], 0);
        assert!(!w.missed(&[down, None], 0));
        assert!(w.missed(&[up, None], 0));
    }

    #[cfg(windows)]
    #[test]
    fn release_watch_uses_the_platform_rules() {
        let k = |n| Hotkey::parse(n).unwrap();
        let w = ReleaseWatch::new(&k("RightCtrl+RightWin"), 5);
        assert_eq!(w.paste_may_release, 0b10);
        assert_eq!(
            paste_sensitive_keys(&k("LeftCtrl+RightCtrl+LeftWin"))
                .iter()
                .map(|k| k.name)
                .collect::<Vec<_>>(),
            ["LeftCtrl", "LeftWin"]
        );
        assert!(paste_sensitive_keys(&k("RightCtrl")).is_empty());
    }

    /// A fake key-state source: held for the first `n` polls.
    fn held_for(n: usize) -> impl FnMut() -> bool {
        let mut polls = 0;
        move || {
            polls += 1;
            polls <= n
        }
    }

    #[test]
    fn paste_waits_for_the_hotkey_release() {
        let poll = Duration::from_millis(2);
        // Not held: no wait at all.
        let (waited, released) = wait_for_hotkey_release(held_for(0), poll, Duration::ZERO);
        assert!(released);
        assert!(waited < Duration::from_millis(50));
        // Held for a few polls (a lingering Win), then released.
        let mut calls = 0;
        let mut src = held_for(3);
        let (waited, released) = wait_for_hotkey_release(
            || {
                calls += 1;
                src()
            },
            poll,
            Duration::from_secs(5),
        );
        assert!(released);
        assert_eq!(calls, 4);
        assert!(waited >= 3 * poll);
        // Held throughout (the user keeps holding the chord): gives up after `max`.
        let max = Duration::from_millis(30);
        let (waited, released) = wait_for_hotkey_release(|| true, poll, max);
        assert!(!released);
        assert!(waited >= max && waited < Duration::from_secs(2));
    }

    /// A clipboard that is busy while our save holds it, then free.
    fn saver_finishing_after(delay: Option<Duration>) -> Saver {
        let (tx, rx) = mpsc::channel();
        if let Some(d) = delay {
            thread::spawn(move || {
                thread::sleep(d);
                let _ = tx.send((None, 0.0)); // the save finished (its result doesn't matter here)
            });
        } else {
            // Hung save: keep the sender alive, never send.
            std::mem::forget(tx);
        }
        Saver {
            in_flight: Some(rx),
            ..Default::default()
        }
    }

    #[test]
    fn busy_clipboard_waits_for_our_save_and_retries_once() {
        let mut saver = saver_finishing_after(Some(Duration::from_millis(50)));
        let mut calls = 0;
        let t = Instant::now();
        let r = set_clipboard_retrying(&mut saver, Duration::from_secs(2), || {
            calls += 1;
            if calls == 1 {
                Err("busy".into())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(r.unwrap(), 2);
        assert!(saver.in_flight.is_none(), "save collected");
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "didn't wait the full timeout"
        );
    }

    #[test]
    fn busy_clipboard_without_our_save_fails_at_once() {
        let mut saver = Saver::default();
        let mut calls = 0;
        let r: Result<(), _> = set_clipboard_retrying(&mut saver, Duration::from_secs(2), || {
            calls += 1;
            Err("busy".into())
        });
        assert!(r.is_err());
        assert_eq!(calls, 1, "someone else holds it: no point waiting");
    }

    #[test]
    fn hung_save_gives_up_after_the_wait_without_retrying() {
        let mut saver = saver_finishing_after(None);
        let mut calls = 0;
        let t = Instant::now();
        let r: Result<(), _> =
            set_clipboard_retrying(&mut saver, Duration::from_millis(100), || {
                calls += 1;
                Err("busy".into())
            });
        assert!(r.is_err());
        assert_eq!(calls, 1);
        assert!(t.elapsed() >= Duration::from_millis(100));
        assert!(
            saver.in_flight.is_some(),
            "still running; a later restore may pick it up"
        );
    }

    #[test]
    fn retry_failing_again_reports_the_error() {
        let mut saver = saver_finishing_after(Some(Duration::ZERO));
        let mut calls = 0;
        let r: Result<(), _> = set_clipboard_retrying(&mut saver, Duration::from_secs(2), || {
            calls += 1;
            Err("busy".into())
        });
        assert!(r.is_err());
        assert_eq!(calls, 2, "exactly one retry");
    }

    #[test]
    fn success_does_not_wait_for_a_running_save() {
        let mut saver = saver_finishing_after(None);
        let t = Instant::now();
        let r = set_clipboard_retrying(&mut saver, Duration::from_secs(2), || Ok(()));
        assert!(r.is_ok());
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn paste_only_into_the_window_that_had_focus_at_release() {
        assert!(may_paste(Some(1), Some(1)), "same window: paste");
        assert!(!may_paste(Some(1), Some(2)), "user switched apps");
        assert!(!may_paste(Some(1), None), "focus in transit: don't");
        // Unknown at release (no foreground window, or no platform support): nothing to
        // compare with, so paste as before instead of never pasting.
        assert!(may_paste(None, None));
        assert!(may_paste(None, Some(2)));
    }

    #[test]
    fn last_transcript_is_forgotten_after_the_ttl() {
        let t = Instant::now();
        let mut last = LastTranscript::default();
        assert_eq!(last.get(t), None);
        assert_eq!(last.deadline(), None);
        last.set("hello", t);
        assert_eq!(last.get(t + LAST_TRANSCRIPT_TTL / 2), Some("hello"));
        assert_eq!(last.deadline(), Some(t + LAST_TRANSCRIPT_TTL));
        assert!(!last.expire(t + LAST_TRANSCRIPT_TTL / 2));
        assert_eq!(
            last.get(t + LAST_TRANSCRIPT_TTL),
            None,
            "expired on read too"
        );
        assert!(last.expire(t + LAST_TRANSCRIPT_TTL));
        assert_eq!(last.deadline(), None, "gone from memory; no more wake-ups");
        assert!(!last.expire(t + LAST_TRANSCRIPT_TTL * 2));
    }

    #[test]
    fn copy_clears_only_paste_errors() {
        for e in [
            PasteError::ClipboardBusy("x".into()),
            PasteError::Blocked("x".into()),
            PasteError::WindowChanged,
        ] {
            assert!(is_paste_error(&e.tray_text()));
        }
        assert!(is_paste_error(COPY_FAILED));
        assert!(!is_paste_error("microphone unavailable (see log)"));
        assert!(!is_paste_error("transcription failed (see log)"));
    }

    #[test]
    fn status_text() {
        assert_eq!(Status::Ready(None).to_string(), "Ready");
        assert_eq!(
            Status::Ready(Some("on the CPU, DirectML unavailable".into())).to_string(),
            "Ready (on the CPU, DirectML unavailable)"
        );
        assert_eq!(
            Status::NeedsPermission(vec![Permission::Microphone, Permission::Accessibility])
                .to_string(),
            format!(
                "Needs permission: Microphone, {} (System Settings > Privacy & Security)",
                Permission::Accessibility
            )
        );
    }
}
