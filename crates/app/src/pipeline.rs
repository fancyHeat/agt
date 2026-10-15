//! The dictation pipeline: hotkey → microphone → speech-to-text → paste.
//!
//! Threads (none of them is the UI thread, and nothing blocks the hook thread):
//! - `hotkey` (agt-platform): low-level keyboard hook; forwards hold events to `audio`.
//! - `audio`: owns the microphone. Starts recording on key down, and on release hands the
//!   samples to `stt`. Short taps and cancelled holds are dropped here. While recording it
//!   also polls the key, in case the hook missed the release (see [`RELEASE_POLL`]).
//! - `stt`: downloads/loads the model at start-up, then transcribes jobs in order. A new
//!   recording may start while the previous one is still being transcribed: jobs queue up
//!   and are pasted in order (nothing is dropped). Long recordings are chunked by `agt-stt`.
//! - `paste`: saves the clipboard when a recording starts (on a helper thread, with a
//!   deadline, so a hung clipboard owner can't block pasting), pastes, and restores the
//!   clipboard `paste.restore_clipboard_ms` later.
//!
//! Status changes are reported through a callback (the app forwards them to the tray).

use agt_audio::{Level, Recorder};
use agt_core::{
    Config,
    stt::{SAMPLE_RATE, SttEngine, TranscribeOpts},
};
use agt_platform::{ClipboardSnapshot, HotkeyEvent, HotkeyHandle, HotkeyReset, Key, PasteToken};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
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
const RELEASE_POLL: Duration = Duration::from_millis(250);

/// How long a paste may wait for the clipboard snapshot that started at key down.
const SNAPSHOT_WAIT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loading(String),
    /// Ready, possibly with a note (e.g. "GPU unavailable, using the CPU").
    Ready(Option<String>),
    Recording,
    Transcribing,
    Error(String),
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
        }
    }
}

enum Engine {
    Loading(String),
    Ready(Option<String>),
    Failed(String),
}

struct HubState {
    engine: Engine,
    recording: bool,
    /// Jobs sent to `stt` and not yet pasted.
    pending: usize,
    /// Latest microphone/paste error, cleared by the next successful dictation.
    error: Option<String>,
    last: Option<Status>,
}

/// Derives the one status line from the pipeline's state and reports changes.
#[derive(Clone)]
struct Hub {
    state: Arc<Mutex<HubState>>,
    notify: Arc<dyn Fn(Status) + Send + Sync>,
}

impl Hub {
    fn update(&self, f: impl FnOnce(&mut HubState)) {
        let mut s = self.state.lock().unwrap();
        f(&mut s);
        let status = if s.recording {
            Status::Recording
        } else if s.pending > 0 {
            Status::Transcribing
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
            (self.notify)(status);
        }
    }

    fn engine_ready(&self) -> bool {
        matches!(self.state.lock().unwrap().engine, Engine::Ready(_))
    }

    fn engine_failed(&self) -> bool {
        matches!(self.state.lock().unwrap().engine, Engine::Failed(_))
    }
}

/// Keeps the pipeline running; dropping it stops the hotkey.
pub struct Pipeline {
    _hotkey: HotkeyHandle,
    /// Live microphone level (RMS, 0..1) while recording, for a level meter / overlay.
    #[allow(dead_code)] // consumed by the Phase 3 overlay
    pub level: Level,
}

struct Job {
    audio: Vec<f32>,
    released: Instant,
}

enum ToStt {
    Transcribe(Job),
    /// The model failed to load and the user pressed the hotkey: try again.
    RetryLoad,
}

enum ToPaste {
    /// A recording started: save the clipboard now so pasting doesn't wait for it.
    Snapshot,
    Paste {
        text: String,
        audio_secs: f64,
        released: Instant,
        stt_start: Instant,
        stt_end: Instant,
    },
}

/// Starts all pipeline threads. Fails if the hotkey can't be installed (bad key name, or a
/// platform without support yet); the model is then not loaded either.
pub fn start(
    config: &Config,
    notify: impl Fn(Status) + Send + Sync + 'static,
) -> Result<Pipeline, String> {
    let hub = Hub {
        state: Arc::new(Mutex::new(HubState {
            engine: Engine::Loading("Loading model...".into()),
            recording: false,
            pending: 0,
            error: None,
            last: None,
        })),
        notify: Arc::new(notify),
    };
    let key = Key::parse(&config.hotkey.key)?;

    let (to_audio, audio_rx) = mpsc::channel::<HotkeyEvent>();
    let hotkey = agt_platform::start_hotkey(key, move |ev| {
        let _ = to_audio.send(ev);
    })
    .map_err(|e| e.to_string())?;

    let (to_stt, stt_rx) = mpsc::channel::<ToStt>();
    let (to_paste, paste_rx) = mpsc::channel::<ToPaste>();
    let level = Level::default();

    let test_wav = load_test_wav();
    let audio = AudioThread {
        config: config.clone(),
        key,
        reset: hotkey.resetter(),
        hub: hub.clone(),
        to_stt,
        to_paste: to_paste.clone(),
        test_wav,
    };
    let lvl = level.clone();
    spawn("audio", move || audio.run(audio_rx, lvl));

    let (h, cfg) = (hub.clone(), config.clone());
    spawn("stt", move || stt_thread(cfg, h, stt_rx, to_paste));

    let (h, cfg) = (hub.clone(), config.clone());
    spawn("paste", move || paste_thread(cfg, h, paste_rx));

    hub.update(|_| {});
    Ok(Pipeline {
        _hotkey: hotkey,
        level,
    })
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

struct AudioThread {
    config: Config,
    key: Key,
    reset: HotkeyReset,
    hub: Hub,
    to_stt: Sender<ToStt>,
    to_paste: Sender<ToPaste>,
    test_wav: Option<Vec<f32>>,
}

impl AudioThread {
    fn run(self, events: Receiver<HotkeyEvent>, level: Level) {
        let mut recorder = Recorder::new(self.config.audio.keep_mic_open, level);
        if let Err(e) = recorder.prepare() {
            tracing::warn!("microphone not available yet: {e}");
        }
        let mut down_at: Option<Instant> = None;

        loop {
            let ev = if down_at.is_some() {
                match events.recv_timeout(RELEASE_POLL) {
                    Ok(ev) => ev,
                    Err(RecvTimeoutError::Timeout) => match self.missed_release(&events) {
                        Some(ev) => ev,
                        None => continue,
                    },
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match events.recv() {
                    Ok(ev) => ev,
                    Err(_) => return,
                }
            };
            match ev {
                HotkeyEvent::Down(t) => {
                    if down_at.take().is_some() {
                        // The previous hold's release was never seen (and the poll couldn't
                        // tell, e.g. a swallowed F13-F24 key). Its audio may span minutes of
                        // unrelated speech: discard it rather than paste it.
                        recorder.stop();
                        tracing::warn!("previous hold was never released; discarded it");
                        self.hub.update(|s| s.recording = false);
                    }
                    down_at = self.start_recording(&mut recorder, t);
                }
                HotkeyEvent::Up(released) => {
                    let Some(down) = down_at.take() else { continue };
                    self.finish_recording(&mut recorder, down, released);
                }
                HotkeyEvent::Cancel => {
                    if down_at.take().is_some() {
                        recorder.stop();
                        tracing::info!("dictation cancelled (another key was pressed)");
                        self.hub.update(|s| s.recording = false);
                    }
                }
            }
        }
    }

    /// Called every [`RELEASE_POLL`] while recording with no event: returns a synthesized
    /// release if the key turns out not to be down any more.
    fn missed_release(&self, events: &Receiver<HotkeyEvent>) -> Option<HotkeyEvent> {
        if agt_platform::key_down(self.key) != Some(false) {
            return None; // still held, or can't tell
        }
        // The hook runs before the key state updates, so a real release would normally be
        // queued already; give a racing one a moment to arrive.
        if let Ok(ev) = events.recv_timeout(Duration::from_millis(20)) {
            return Some(ev);
        }
        tracing::warn!(
            "{} is no longer down but no release was seen; finishing the dictation and re-installing the keyboard hook",
            self.key.name
        );
        self.reset.reset();
        Some(HotkeyEvent::Up(Instant::now()))
    }

    fn start_recording(&self, recorder: &mut Recorder, t: Instant) -> Option<Instant> {
        if !self.hub.engine_ready() {
            if self.hub.engine_failed() {
                let _ = self.to_stt.send(ToStt::RetryLoad);
            }
            tracing::info!(
                "{} pressed but the model isn't ready; ignoring",
                self.key.name
            );
            return None;
        }
        match recorder.start() {
            Ok(()) => {
                let _ = self.to_paste.send(ToPaste::Snapshot);
                self.hub.update(|s| s.recording = true);
                tracing::debug!(
                    "recording (key down → mic started {:.1} ms)",
                    ms(Instant::now() - t)
                );
                Some(t)
            }
            Err(e) => {
                tracing::error!("cannot start microphone: {e}");
                self.hub
                    .update(|s| s.error = Some("microphone unavailable (see log)".into()));
                None
            }
        }
    }

    fn finish_recording(&self, recorder: &mut Recorder, down: Instant, released: Instant) {
        let rec = recorder.stop();
        let held = released.saturating_duration_since(down);
        tracing::debug!(
            "recorded {:.2} s from the microphone (rms {:.4})",
            rec.samples.len() as f64 / SAMPLE_RATE as f64,
            (rec.samples.iter().map(|x| x * x).sum::<f32>() / rec.samples.len().max(1) as f32)
                .sqrt()
        );
        if let Some(d) = rec.first_audio {
            tracing::debug!("first audio {:.1} ms after recording started", ms(d));
        }
        if held < Duration::from_millis(self.config.hotkey.min_hold_ms) {
            tracing::info!(
                "hold of {} ms is shorter than min_hold_ms; ignored",
                held.as_millis()
            );
            self.hub.update(|s| s.recording = false);
            return;
        }
        let audio = self.test_wav.clone().unwrap_or(rec.samples);
        self.hub.update(|s| {
            s.recording = false;
            s.pending += 1;
        });
        let _ = self.to_stt.send(ToStt::Transcribe(Job { audio, released }));
    }
}

fn load_engine(config: &Config, hub: &Hub) -> Option<Box<dyn SttEngine>> {
    let t = Instant::now();
    let mut last_pct = u64::MAX;
    let loaded = agt_stt::load(&config.stt, &mut |p| {
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
        hub.update(|s| s.engine = Engine::Loading("Loading model...".into()));
        engine.warmup()?;
        Ok(engine)
    });
    match loaded {
        Ok(engine) => {
            let name = engine.name();
            tracing::info!("{name} ready ({:.1} s)", t.elapsed().as_secs_f64());
            let note = name
                .contains(agt_stt::parakeet::FALLBACK_SUFFIX)
                .then(|| "GPU unavailable, using the CPU".to_owned());
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

fn stt_thread(config: Config, hub: Hub, jobs: Receiver<ToStt>, to_paste: Sender<ToPaste>) {
    let mut engine = load_engine(&config, &hub);
    let opts = TranscribeOpts::default();
    for msg in jobs {
        let job = match msg {
            ToStt::RetryLoad => {
                if engine.is_none() {
                    hub.update(|s| s.engine = Engine::Loading("Loading model...".into()));
                    engine = load_engine(&config, &hub);
                }
                continue;
            }
            ToStt::Transcribe(job) => job,
        };
        let Some(engine) = engine.as_mut() else {
            hub.update(|s| s.pending -= 1);
            continue;
        };
        let stt_start = Instant::now();
        let result = engine.transcribe(&job.audio, &opts);
        let stt_end = Instant::now();
        let audio_secs = job.audio.len() as f64 / SAMPLE_RATE as f64;
        match result {
            Ok(t) => {
                let text = finish_text(&t.text, config.paste.trailing_space);
                if text.is_empty() {
                    tracing::info!(
                        "no speech recognized in {audio_secs:.1} s of audio (stt {:.0} ms)",
                        ms(stt_end - stt_start)
                    );
                    hub.update(|s| s.pending -= 1);
                } else {
                    let _ = to_paste.send(ToPaste::Paste {
                        text,
                        audio_secs,
                        released: job.released,
                        stt_start,
                        stt_end,
                    });
                }
            }
            Err(e) => {
                tracing::error!("transcription failed: {e}");
                hub.update(|s| {
                    s.pending -= 1;
                    s.error = Some("transcription failed (see log)".into());
                });
            }
        }
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
    in_flight: Option<Receiver<Option<ClipboardSnapshot>>>,
}

impl Saver {
    /// Starts saving the clipboard unless a save is already running.
    fn start(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        spawn("clipboard-save", move || {
            let _ = tx.send(snapshot());
        });
        self.in_flight = Some(rx);
    }

    /// Picks up a finished save, waiting at most `wait`.
    fn collect(&mut self, wait: Duration) {
        if let Some(rx) = &self.in_flight {
            match rx.recv_timeout(wait) {
                Ok(s) => {
                    self.saved = s;
                    self.in_flight = None;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.in_flight = None,
            }
        }
    }
}

fn paste_thread(config: Config, hub: Hub, rx: Receiver<ToPaste>) {
    let restore_after = Duration::from_millis(config.paste.restore_clipboard_ms);
    let restore = !restore_after.is_zero();
    let mut saver = Saver::default();
    // Our paste that still has to be undone, and when.
    let mut pending_restore: Option<(PasteToken, Instant)> = None;

    loop {
        let msg = match &pending_restore {
            Some((_, at)) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(ToPaste::Snapshot) => {
                // While our transcript is still on the clipboard, the saved snapshot already
                // holds the user's data; saving now would save our own text instead.
                if restore && pending_restore.is_none() {
                    saver.saved = None;
                    saver.start();
                }
            }
            Ok(ToPaste::Paste {
                text,
                audio_secs,
                released,
                stt_start,
                stt_end,
            }) => {
                let paste_start = Instant::now();
                if restore && pending_restore.is_none() {
                    saver.collect(SNAPSHOT_WAIT);
                    // The user copied something during the hold (or no save ran): save again.
                    if saver.saved.as_ref().is_none_or(ClipboardSnapshot::is_stale) {
                        saver.saved = None;
                        saver.start();
                        saver.collect(SNAPSHOT_WAIT);
                    }
                    if saver.saved.is_none() {
                        tracing::warn!("clipboard not saved in time; it won't be restored");
                    }
                }
                let result = agt_platform::set_clipboard_text(&text).and_then(|token| {
                    match agt_platform::send_paste() {
                        Ok(()) => Ok(token),
                        Err(e) => Err((e, Some(token))),
                    }
                    .map_err(|(e, token)| {
                        // Don't leave the user's clipboard replaced by text that went nowhere.
                        if let (Some(token), Some(snap)) = (token, saver.saved.take()) {
                            if let Err(re) = agt_platform::restore_clipboard(&snap, &token) {
                                tracing::warn!("cannot restore clipboard: {re}");
                            }
                            pending_restore = None;
                        }
                        e
                    })
                });
                let pasted = Instant::now();
                hub.update(|s| {
                    s.pending -= 1;
                    s.error = result
                        .as_ref()
                        .err()
                        .map(|_| "paste failed (admin window?); transcript is in the log".into());
                });
                match result {
                    Ok(token) => {
                        tracing::info!(
                            "dictation: audio {audio_secs:.2} s | release→stt {:.0} ms | stt {:.0} ms | paste {:.0} ms | release→paste {:.0} ms | {} chars",
                            ms(stt_start - released),
                            ms(stt_end - stt_start),
                            ms(pasted - paste_start),
                            ms(pasted - released),
                            text.chars().count()
                        );
                        tracing::debug!("pasted {text:?}");
                        if restore {
                            pending_restore = Some((token, pasted + restore_after));
                        }
                    }
                    Err(e) => {
                        tracing::error!("paste failed: {e}");
                        tracing::warn!("unpasted transcript: {text:?}");
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
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

fn snapshot() -> Option<ClipboardSnapshot> {
    let t = Instant::now();
    match agt_platform::snapshot_clipboard() {
        Ok(s) => {
            tracing::debug!("clipboard saved in {:.1} ms", ms(t.elapsed()));
            Some(s)
        }
        Err(e) => {
            tracing::warn!("cannot save clipboard (it won't be restored): {e}");
            None
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_normalized() {
        assert_eq!(finish_text("  Hello,\n world.  ", true), "Hello, world. ");
        assert_eq!(finish_text("Hi", false), "Hi");
        assert_eq!(finish_text("   ", true), "");
    }

    #[test]
    fn status_text() {
        assert_eq!(Status::Ready(None).to_string(), "Ready");
        assert_eq!(
            Status::Ready(Some("GPU unavailable, using the CPU".into())).to_string(),
            "Ready (GPU unavailable, using the CPU)"
        );
    }
}
