//! The dictation pipeline: hotkey → microphone → speech-to-text → paste.
//!
//! Threads (none of them is the UI thread, and nothing blocks the hook thread):
//! - `hotkey` (agt-platform): low-level keyboard hook; forwards hold events to `audio`.
//! - `audio`: owns the microphone. Starts recording on key down, and on release hands the
//!   samples to `stt`. Short taps and cancelled holds are dropped here.
//! - `stt`: downloads/loads the model at start-up, then transcribes jobs in order. A new
//!   recording may start while the previous one is still being transcribed: jobs queue up
//!   and are pasted in order (nothing is dropped).
//! - `paste`: snapshots the clipboard when a recording starts (so it's off the critical
//!   path), pastes, and restores the clipboard `paste.restore_clipboard_ms` later.
//!
//! Status changes are reported through a callback (the app forwards them to the tray).

use agt_audio::Recorder;
use agt_core::{
    Config,
    stt::{SAMPLE_RATE, SttEngine, TranscribeOpts},
};
use agt_platform::{ClipboardSnapshot, HotkeyEvent, HotkeyHandle, Key, PasteToken};
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

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loading(String),
    Ready,
    Recording,
    Transcribing,
    Error(String),
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Loading(what) => write!(f, "{what}"),
            Status::Ready => write!(f, "Ready"),
            Status::Recording => write!(f, "Recording..."),
            Status::Transcribing => write!(f, "Transcribing..."),
            Status::Error(e) => write!(f, "Error: {e}"),
        }
    }
}

enum Engine {
    Loading(String),
    Ready,
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
                (Engine::Failed(e), _) | (Engine::Ready, Some(e)) => Status::Error(e.clone()),
                (Engine::Ready, None) => Status::Ready,
            }
        };
        if s.last.as_ref() != Some(&status) {
            tracing::debug!("status: {status}");
            s.last = Some(status.clone());
            (self.notify)(status);
        }
    }

    fn engine_ready(&self) -> bool {
        matches!(self.state.lock().unwrap().engine, Engine::Ready)
    }

    fn engine_failed(&self) -> bool {
        matches!(self.state.lock().unwrap().engine, Engine::Failed(_))
    }
}

/// Keeps the pipeline running; dropping it stops the hotkey.
pub struct Pipeline {
    _hotkey: HotkeyHandle,
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

    let test_wav = load_test_wav();
    let (h, cfg) = (hub.clone(), config.clone());
    let to_paste2 = to_paste.clone();
    spawn("audio", move || {
        audio_thread(cfg, key, h, audio_rx, to_stt, to_paste2, test_wav)
    });

    let (h, cfg) = (hub.clone(), config.clone());
    spawn("stt", move || stt_thread(cfg, h, stt_rx, to_paste));

    let (h, cfg) = (hub.clone(), config.clone());
    spawn("paste", move || paste_thread(cfg, h, paste_rx));

    hub.update(|_| {});
    Ok(Pipeline { _hotkey: hotkey })
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

fn audio_thread(
    config: Config,
    key: Key,
    hub: Hub,
    events: Receiver<HotkeyEvent>,
    to_stt: Sender<ToStt>,
    to_paste: Sender<ToPaste>,
    test_wav: Option<Vec<f32>>,
) {
    let mut recorder = Recorder::new(config.audio.keep_mic_open);
    if let Err(e) = recorder.prepare() {
        tracing::warn!("microphone not available yet: {e}");
    }
    let min_hold = Duration::from_millis(config.hotkey.min_hold_ms);
    let mut down_at: Option<Instant> = None;

    for ev in events {
        match ev {
            HotkeyEvent::Down(t) => {
                if !hub.engine_ready() {
                    if hub.engine_failed() {
                        let _ = to_stt.send(ToStt::RetryLoad);
                    }
                    tracing::info!("{} pressed but the model isn't ready; ignoring", key.name);
                    continue;
                }
                match recorder.start() {
                    Ok(()) => {
                        down_at = Some(t);
                        let _ = to_paste.send(ToPaste::Snapshot);
                        hub.update(|s| s.recording = true);
                        tracing::debug!(
                            "recording (key down → mic started {:.1} ms)",
                            (Instant::now() - t).as_secs_f64() * 1e3
                        );
                    }
                    Err(e) => {
                        tracing::error!("cannot start microphone: {e}");
                        hub.update(|s| s.error = Some("microphone unavailable (see log)".into()));
                    }
                }
            }
            HotkeyEvent::Up(released) => {
                let Some(down) = down_at.take() else { continue };
                let rec = recorder.stop();
                let held = released.saturating_duration_since(down);
                tracing::debug!(
                    "recorded {:.2} s from the microphone (rms {:.4})",
                    rec.samples.len() as f64 / SAMPLE_RATE as f64,
                    (rec.samples.iter().map(|x| x * x).sum::<f32>()
                        / rec.samples.len().max(1) as f32)
                        .sqrt()
                );
                if let Some(d) = rec.first_audio {
                    tracing::debug!(
                        "first audio {:.1} ms after recording started",
                        d.as_secs_f64() * 1e3
                    );
                }
                if held < min_hold {
                    tracing::info!(
                        "hold of {} ms is shorter than min_hold_ms; ignored",
                        held.as_millis()
                    );
                    hub.update(|s| s.recording = false);
                    continue;
                }
                let audio = test_wav.clone().unwrap_or(rec.samples);
                hub.update(|s| {
                    s.recording = false;
                    s.pending += 1;
                });
                let _ = to_stt.send(ToStt::Transcribe(Job { audio, released }));
            }
            HotkeyEvent::Cancel => {
                if down_at.take().is_some() {
                    recorder.stop();
                    tracing::info!("dictation cancelled (another key was pressed)");
                    hub.update(|s| s.recording = false);
                }
            }
        }
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
            tracing::info!(
                "{} ready ({:.1} s)",
                engine.name(),
                t.elapsed().as_secs_f64()
            );
            hub.update(|s| s.engine = Engine::Ready);
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

fn paste_thread(config: Config, hub: Hub, rx: Receiver<ToPaste>) {
    let restore_after = Duration::from_millis(config.paste.restore_clipboard_ms);
    let restore = !restore_after.is_zero();
    // The user's clipboard, saved at the start of a recording.
    let mut saved: Option<ClipboardSnapshot> = None;
    // Our paste that still has to be undone, and when.
    let mut pending_restore: Option<(PasteToken, Instant)> = None;

    loop {
        let msg = match &pending_restore {
            Some((_, at)) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(ToPaste::Snapshot) => {
                // While our transcript is still on the clipboard, `saved` already holds the
                // user's data; snapshotting now would save our own text instead.
                if restore && pending_restore.is_none() {
                    saved = snapshot();
                }
            }
            Ok(ToPaste::Paste {
                text,
                audio_secs,
                released,
                stt_start,
                stt_end,
            }) => {
                if restore && saved.is_none() && pending_restore.is_none() {
                    saved = snapshot();
                }
                let paste_start = Instant::now();
                let result = agt_platform::paste_text(&text);
                let pasted = Instant::now();
                hub.update(|s| {
                    s.pending -= 1;
                    s.error = result
                        .as_ref()
                        .err()
                        .map(|_| "paste failed (see log)".into());
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
                    Err(e) => tracing::error!("paste failed: {e}"),
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let (Some((token, _)), Some(snap)) = (pending_restore.take(), saved.take()) {
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
}
