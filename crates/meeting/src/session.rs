//! A recording meeting: the microphone ("Me") and what the computer plays ("Them"), cut live
//! into segments, transcribed, and written to `transcript.jsonl` as [`Line`]s.
//!
//! One `meeting` thread owns everything that changes: both tracks' [`Cutter`]s and WAV
//! writers, the lines being put together, and the captures (it restarts them). Everything
//! else only sends it messages: the capture callbacks their audio, stamped with when it
//! arrived; the [`Transcriber`] each segment's text; [`Session`]'s methods pause, resume and
//! stop. So nothing is locked, and the audio callbacks never wait.
//!
//! If the `meeting` thread panics anyway (a bug), the meeting still ends properly: what was
//! written stays, `ended_at` and `meeting.md` are saved, and [`Event::Error`] then
//! [`Event::Stopped`] go out, so the app never waits for a meeting that can't finish.
//!
//! The system capture is started on a `meeting-system-start` thread, which sends the result
//! back the same way, at most one at a time, restarts included. On macOS that start can block
//! for as long as the "System Audio Recording" prompt is unanswered (forever, on a locked
//! screen): Core Audio holds the call. The meeting doesn't wait for it: the mic records, the
//! system track reports [`SystemAudio::Silent`] after [`SessionConfig::system_wait`], and a
//! start that returns after the meeting ended is dropped at once.
//!
//! # The meeting clock
//!
//! Every line's `start_ms` is on one clock: wall time since the meeting started, pauses
//! included. A track's sample count is not that clock: WASAPI loopback delivers no packets at
//! all while nothing plays, and a capture restarted after a device change misses a moment. So
//! each track keeps its own position on the meeting clock, and a chunk that arrives more than
//! [`GAP`] later than that position says it should is preceded by zeros for the difference,
//! in the cutter and the WAV file alike. A track that runs ahead (its device's clock a little
//! fast) is left alone: no audio is ever dropped to fit the clock. A pause ends both cutters'
//! segments (they are transcribed) and drops what arrives meanwhile; resuming starts new
//! cutters at the current time, so lines after a pause still say when they were said, and the
//! WAV files get silence for the pause (they stay on the meeting clock too).
//!
//! Digital silence (exact zeros: the macOS tap while nothing plays or without the permission,
//! a muted mic, the zeros filling a gap) before a cutter's first sound isn't fed to it at all.
//! [`Cutter`] keeps everything until it hears speech, so the first line would otherwise start
//! at 0:00 however much later it was said, and minutes of zeros would be transcribed. A
//! segment of nothing but zeros isn't transcribed either, nor one the cutter heard no speech in
//! ([`Segment::speech`]: room noise, a quiet stretch), which the model would only turn into
//! made-up words ("Okay.", "Thank you."); either still counts as transcribed, for the echo
//! check. (Speech too quiet to stand out from the room it's in goes with it: the cutter tells
//! speech by a clear rise over the noise floor.) A real mic's room noise isn't zeros, so a
//! line's times are those of the speech inside its segment ([`speech_span`]), not of the
//! segment.
//!
//! # Echo
//!
//! On speakers the mic hears the other side too. Them lines go out as soon as they are
//! transcribed; a Me line waits until it can be compared with what Them said around it
//! ([`echo::is_echo`]), and is dropped if it is mostly the same words while Them was playing.
//! It waits until the Them track is transcribed past its end, or Them was quiet over it (and
//! [`QUIET_AROUND`] either side: nothing to echo), or [`SessionConfig::echo_wait`] passed. Me
//! lines go out in the order they were said. Only the last minute of Them's lines and loudness
//! is kept for this (more only while an older Me line still waits), so a two-hour meeting needs
//! no more memory than a short one.

use crate::{Line, Meeting, Speaker, Store, Transcriber, echo};
use agt_audio::{SAMPLE_RATE, wav::WavWriter};
use agt_stt::chunk::{Cut, CutConfig, Cutter, Segment};
use serde::{Serialize, Serializer};
use std::{
    any::Any,
    collections::{BTreeMap, VecDeque},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A track this far behind the meeting clock missed packets (WASAPI loopback while nothing
/// plays): the gap is filled with zeros. Well over a callback's audio and its jitter.
const GAP: Duration = Duration::from_millis(200);
/// Them's loudness is kept per block of this many samples (100 ms).
const BLOCK: u64 = SAMPLE_RATE as u64 / 10;
/// A block at least this loud (RMS, -50 dBFS) means Them was playing: far below speech on a
/// call, above what noise suppression leaves of the other side's room.
const LOUD: f64 = 0.003_16;
/// Them quiet over a Me line and this long either side: the line can't be an echo.
const QUIET_AROUND: Duration = Duration::from_secs(1);
/// A Me line is compared with the Them lines this close to it ([`echo::is_echo`]: ±3 s).
const NEARBY: Duration = Duration::from_secs(3);
/// Them's lines and loudness are kept this long for the echo check (see the module docs).
const ECHO_KEEP: Duration = Duration::from_secs(60);
/// After a stop, segments still being transcribed are waited for this long at most.
const STOP_WAIT: Duration = Duration::from_secs(60);
/// A 10 ms block sounds like speech ([`speech_span`]) this far above its segment's quiet floor
/// (dB), and ...
const SPEECH_OVER_FLOOR_DB: f32 = 15.0;
/// ... at least this loud (dBFS).
const SPEECH_MIN_DBFS: f32 = -55.0;
/// Silence, to fill gaps a second at a time.
static ZEROS: [f32; SAMPLE_RATE as usize] = [0.0; SAMPLE_RATE as usize];

/// One of a meeting's two audio tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Track {
    /// The microphone: "Me".
    Mic,
    /// What the computer plays: "Them".
    System,
}

impl Track {
    const ALL: [Track; 2] = [Track::Mic, Track::System];

    fn index(self) -> usize {
        self as usize
    }

    /// Its WAV file's name, with `save_audio`.
    fn file(self) -> &'static str {
        match self {
            Track::Mic => "mic.wav",
            Track::System => "system.wav",
        }
    }

    /// For logs and errors.
    fn label(self) -> &'static str {
        match self {
            Track::Mic => "microphone",
            Track::System => "system audio",
        }
    }
}

/// How the "Them" track is doing, for the UI. Serialized as `"ok"`, `"unsupported"` or
/// `"silent"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemAudio {
    Ok,
    /// Not recording, and why: it couldn't be started (macOS before 14.2, no output device),
    /// or was lost and can't be restarted yet. The meeting goes on with the mic only.
    Unsupported(String),
    /// Nothing but exact zeros for [`SessionConfig::silent_after`] since it started, or its
    /// start has been waiting for [`SessionConfig::system_wait`]. On macOS both usually mean
    /// the "System Audio Recording" permission is missing or not answered yet: the tap then
    /// delivers silence (or the start waits for the prompt), not an error. Back to `Ok` at the
    /// first sound. Also a capture that should deliver all the time and (almost) doesn't
    /// ([`SessionConfig::system_delivers_continuously`]), until buffers come again.
    Silent,
}

impl Serialize for SystemAudio {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            SystemAudio::Ok => "ok",
            SystemAudio::Unsupported(_) => "unsupported",
            SystemAudio::Silent => "silent",
        })
    }
}

/// Where a capture's audio goes: 16 kHz mono chunks as they arrive, on the capture's thread.
pub type Sink = Box<dyn FnMut(&[f32]) + Send>;

/// Starts the captures behind the two tracks: [`DeviceCaptures`](crate::DeviceCaptures) on
/// this machine's devices, a fake in tests.
pub trait CaptureFactory: Send + Sync {
    /// Starts capturing `track`, its audio going to `sink` until the handle is dropped.
    fn start(&self, track: Track, sink: Sink) -> Result<Box<dyn CaptureHandle>, String>;
}

/// A running capture; dropping it stops it.
pub trait CaptureHandle: Send {
    /// It stopped delivering (device unplugged, audio service restarted).
    fn failed(&self) -> bool;
    /// It's on a device other than the one it would open now (the default device changed).
    /// May enumerate devices: asked at most once per [`SessionConfig::poll`].
    fn stale(&self) -> bool;
}

/// How a [`Session`] records.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Also keep the audio: `mic.wav` and `system.wav` in the meeting's folder, on the meeting
    /// clock (a pause or a gap is silence in them).
    pub save_audio: bool,
    /// How each track is cut into segments to transcribe. The app passes its meeting cut
    /// config (short pauses cut short segments, so lines come within a second or two of each
    /// pause), not dictation's [`CutConfig::live`]. With a [`PauseCut`](agt_stt::chunk::PauseCut),
    /// segments the cutter heard no speech in aren't transcribed (see the module docs).
    pub cut: CutConfig,
    /// While set (a dictation records), the mic track is silence: the dictation is not part of
    /// the meeting.
    pub mic_muted: Arc<AtomicBool>,
    /// A Me line waits at most this long for the echo check (30 s).
    pub echo_wait: Duration,
    /// How often the captures are checked (and restarted) and [`Event::Status`] is sent (1 s).
    pub poll: Duration,
    /// [`SystemAudio::Silent`] after this much audio of nothing but zeros (60 s).
    pub silent_after: Duration,
    /// [`SystemAudio::Silent`] too while the system capture's start has been waiting this
    /// long (3 s): macOS holds it until the user answers the permission prompt.
    pub system_wait: Duration,
    /// The system capture delivers audio all the time, silence included (the macOS tap), so
    /// one that has delivered less than a tenth of the time it has run for, over the last
    /// [`silent_after`](Self::silent_after), is broken: [`SystemAudio::Silent`]. A tap can
    /// start fine and then never deliver a buffer. Off where nothing comes while nothing
    /// plays (WASAPI loopback): there a quiet call looks the same. Off by default; the app sets
    /// it for its platform.
    pub system_delivers_continuously: bool,
}

impl SessionConfig {
    /// No audio files; the default waits and poll.
    pub fn new(cut: CutConfig, mic_muted: Arc<AtomicBool>) -> SessionConfig {
        SessionConfig {
            save_audio: false,
            cut,
            mic_muted,
            echo_wait: Duration::from_secs(30),
            poll: Duration::from_secs(1),
            silent_after: Duration::from_secs(60),
            system_wait: Duration::from_secs(3),
            system_delivers_continuously: false,
        }
    }
}

/// What a [`Session`] reports, on its `meeting` thread. `id` is the meeting's.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The meeting's folder and `meeting.json` exist, and it records. Always the first event.
    Started(Meeting),
    /// A final line, already in `transcript.jsonl`. Ids go up in the order lines come; Me lines
    /// come later than Them lines said at the same time (they wait for the echo check).
    Line { id: String, line: Line },
    /// Every [`SessionConfig::poll`] while recording, and at once on pause, resume and stop
    /// (`recording` false: what's left is being transcribed). `elapsed_ms` includes pauses.
    Status {
        id: String,
        recording: bool,
        paused: bool,
        elapsed_ms: u64,
        system: SystemAudio,
    },
    /// A Me line was dropped as an echo of Them; `dropped` so far in this meeting.
    Echo { id: String, dropped: u64 },
    /// Everything is transcribed and written (`ended_at`, `meeting.md`). Always the last.
    Stopped { id: String },
    /// Something the user should know went wrong (the mic was lost, a segment couldn't be
    /// transcribed, a file couldn't be written); the meeting goes on. A message repeated on
    /// every segment is sent once.
    Error { id: String, message: String },
}

/// A meeting being recorded. Dropping it stops it, like [`stop`](Self::stop).
pub struct Session {
    id: String,
    t0: Instant,
    /// Paused; held while the pause or resume is sent, so the thread gets them in the order
    /// the calls happened.
    paused: Mutex<bool>,
    tx: Sender<Msg>,
}

impl Session {
    /// Starts recording a new meeting: both captures, its folder in `store`, the `meeting`
    /// thread. Events go to `notify` on that thread, [`Event::Started`] first.
    ///
    /// Fails, without making a folder, if the microphone can't be started. The system audio
    /// starts on a thread of its own, so this never waits for it (see the module docs); if it
    /// can't start, the meeting records the mic only ([`SystemAudio::Unsupported`]).
    pub fn start(
        store: Store,
        cfg: SessionConfig,
        captures: Arc<dyn CaptureFactory>,
        transcriber: Arc<dyn Transcriber>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Session, String> {
        let t0 = Instant::now();
        let started_at = unix_ms();
        let (tx, rx) = mpsc::channel();
        let muted = Some(cfg.mic_muted.clone());
        let mic = captures
            .start(Track::Mic, sink(Track::Mic, 0, &tx, muted))
            .map_err(|e| format!("Couldn't start the microphone: {e}"))?;
        let meeting = store
            .create(started_at)
            .map_err(|e| format!("Couldn't create the meeting's folder: {e}"))?;
        let deps = Deps {
            store,
            cfg,
            captures,
            transcriber,
            notify,
        };
        let mut w = Worker::new(&meeting, t0, deps, tx.clone());
        w.install(Track::Mic, mic);
        // Not retried if this first start fails: the OS can't, or won't.
        w.start_capture(Track::System, Instant::now());
        let mut errors = Vec::new();
        let mut meeting = meeting;
        if w.cfg.save_audio {
            match w.open_wav(Track::Mic) {
                Ok(m) => meeting = m,
                Err(e) => errors.push(e),
            }
        }
        let id = meeting.id.clone();
        tracing::info!("meeting {id} started; starting the system audio");
        let rescue = Rescue {
            id: id.clone(),
            started_at: meeting.started_at,
            t0,
            store: w.store.clone(),
            notify: w.notify.clone(),
            stopped: w.stopped.clone(),
        };
        thread::Builder::new()
            .name("meeting".into())
            .spawn(move || {
                let run = AssertUnwindSafe(|| w.run(rx, meeting, errors));
                if panic::catch_unwind(run).is_err() {
                    rescue.finish();
                }
            })
            .map_err(|e| format!("Couldn't start the meeting thread: {e}"))?;
        Ok(Session {
            id,
            t0,
            paused: Mutex::new(false),
            tx,
        })
    }

    /// The meeting's id (its folder's name).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Stops recording until [`resume`](Self::resume): what both tracks hold so far is
    /// transcribed, what they hear meanwhile is dropped. The captures keep running, so
    /// resuming is instant.
    pub fn pause(&self) {
        let mut paused = self.paused.lock().unwrap_or_else(PoisonError::into_inner);
        if !*paused {
            *paused = true;
            let _ = self.tx.send(Msg::Pause);
        }
    }

    pub fn resume(&self) {
        let mut paused = self.paused.lock().unwrap_or_else(PoisonError::into_inner);
        if *paused {
            *paused = false;
            let _ = self.tx.send(Msg::Resume(Instant::now()));
        }
    }

    pub fn paused(&self) -> bool {
        *self.paused.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Time since the start, pauses included: the clock of the lines' `start_ms`.
    pub fn elapsed_ms(&self) -> u64 {
        elapsed_ms(self.t0, Instant::now())
    }

    /// Stops recording, and returns at once. The `meeting` thread then transcribes what is
    /// left (waiting for it a minute at most), sets `ended_at`, writes `meeting.md` and sends
    /// [`Event::Stopped`].
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The thread only ends after a Stop, so it is there to get this one.
        let _ = self.tx.send(Msg::Stop(Instant::now()));
    }
}

/// To the `meeting` thread.
enum Msg {
    /// A capture's audio, and when it arrived. `generation`: which capture of the track sent
    /// it (see [`TrackState::generation`]).
    Audio {
        track: Track,
        generation: u64,
        at: Instant,
        samples: Vec<f32>,
    },
    /// A capture start's result (from a `meeting-system-start` thread).
    Started {
        track: Track,
        generation: u64,
        result: Result<Box<dyn CaptureHandle>, String>,
    },
    /// The text of segment `seq` (see [`TrackState::seq`]).
    Text {
        track: Track,
        seq: u64,
        result: Result<String, String>,
    },
    Pause,
    /// When [`Session::resume`] was called.
    Resume(Instant),
    /// When [`Session::stop`] was called.
    Stop(Instant),
}

/// A capture's callback: sends its audio to the `meeting` thread with the time it arrived
/// (zeros instead while `muted`). Cheap: one copy and a channel send.
fn sink(track: Track, generation: u64, tx: &Sender<Msg>, muted: Option<Arc<AtomicBool>>) -> Sink {
    let tx = tx.clone();
    Box::new(move |audio| {
        let at = Instant::now();
        let samples = match &muted {
            Some(m) if m.load(Ordering::Relaxed) => vec![0.0; audio.len()],
            _ => audio.to_vec(),
        };
        // Fails only once the meeting is over.
        let _ = tx.send(Msg::Audio {
            track,
            generation,
            at,
            samples,
        });
    })
}

/// What a panicked `meeting` thread still needs to end its meeting: [`Rescue::finish`].
struct Rescue {
    id: String,
    started_at: i64,
    t0: Instant,
    store: Store,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    /// [`Event::Stopped`] went out already (the panic came after it).
    stopped: Arc<AtomicBool>,
}

impl Rescue {
    /// The meeting thread panicked (the panic hook has logged where): everything it held is
    /// gone (the captures stopped as it unwound, its WAV files were finished on the way, the
    /// segments still being transcribed are lost), but what it wrote stays. Ends the meeting
    /// now with that: `ended_at` (unless it was set), `meeting.md`, then [`Event::Error`] and
    /// [`Event::Stopped`], so the app goes back to idle instead of waiting for a stop that
    /// never comes.
    fn finish(self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        // Not the panic's message: a formatted one can hold anything, even what was said.
        tracing::error!(
            "meeting {}: the recording stopped on an error; saving what there is",
            self.id
        );
        let ended_at = self.started_at + elapsed_ms(self.t0, Instant::now()) as i64;
        if let Err(e) = self.store.update(&self.id, |m| {
            m.ended_at.get_or_insert(ended_at);
        }) {
            tracing::error!("cannot save meeting {}: {e}", self.id);
        }
        if let Err(e) = self.store.write_markdown(&self.id) {
            tracing::error!("cannot write meeting {}'s meeting.md: {e}", self.id);
        }
        let id = self.id.clone();
        let message = "Meeting notes stopped because of an error in AGT. What was transcribed \
                       until then is saved."
            .to_owned();
        (self.notify)(Event::Error {
            id: id.clone(),
            message,
        });
        (self.notify)(Event::Stopped { id });
    }
}

/// What the `meeting` thread gets from [`Session::start`].
struct Deps {
    store: Store,
    cfg: SessionConfig,
    captures: Arc<dyn CaptureFactory>,
    transcriber: Arc<dyn Transcriber>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
}

/// Where something is on the meeting clock, in samples (`end` excluded).
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u64,
    end: u64,
}

impl Span {
    /// Widened by `d` either side.
    fn around(self, d: Duration) -> Span {
        let d = samples_in(d);
        Span {
            start: self.start.saturating_sub(d),
            end: self.end + d,
        }
    }

    fn overlaps(self, other: Span) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// A Me line waiting for the echo check, since `since`.
struct Held {
    span: Span,
    text: String,
    since: Instant,
}

/// One track's capture, cutter and file.
struct TrackState {
    capture: Option<Box<dyn CaptureHandle>>,
    /// Bumped on every start: audio still queued from a capture that was replaced is ignored.
    generation: u64,
    /// Since when a start (of `generation`) is on its way; at most one at a time.
    starting: Option<Instant>,
    /// Start it again when it's missing. Not after the system audio failed to start at all:
    /// the OS can't, or won't, and asking again every second wouldn't change that.
    retry: bool,
    /// Why the last start failed (reported once).
    error: Option<String>,
    cutter: Cutter,
    /// Meeting-clock position (samples) of the track's next sample: everything before it was
    /// fed, filled with zeros, or dropped during a pause.
    pos: u64,
    /// Meeting-clock position of the cutter's first sample; `None` while it has had only zeros.
    base: Option<u64>,
    wav: Option<WavWriter>,
    /// The WAV file was made (or failed): the system track's is made at its first audio.
    wav_opened: bool,
    /// Segments cut so far: the last one's number.
    seq: u64,
    /// Segments being transcribed, by number.
    pending: BTreeMap<u64, Span>,
    /// Segments not counted in `done` yet, in order (`None`: a cutter's end, at a pause or the
    /// stop), each with how far the track is transcribed once it and those before it are.
    order: VecDeque<(Option<u64>, u64)>,
    /// Transcribed up to here: every word before it has its text (or there was none).
    done: u64,
    /// Samples the current capture delivered, and whether one wasn't exactly zero
    /// ([`SystemAudio::Silent`]).
    delivered: u64,
    sound: bool,
    /// `delivered` at each poll since the current capture was installed, for
    /// [`SessionConfig::system_delivers_continuously`]: just over the last `silent_after`.
    deliveries: VecDeque<(Instant, u64)>,
}

impl TrackState {
    fn new(cut: &CutConfig) -> TrackState {
        TrackState {
            capture: None,
            generation: 0,
            starting: None,
            retry: false,
            error: None,
            cutter: Cutter::new(cut.clone()),
            pos: 0,
            base: None,
            wav: None,
            wav_opened: false,
            seq: 0,
            pending: BTreeMap::new(),
            order: VecDeque::new(),
            done: 0,
            delivered: 0,
            sound: false,
            deliveries: VecDeque::new(),
        }
    }

    /// Notes what the capture delivered by `now`, keeping just enough notes to look `window`
    /// back.
    fn note_delivery(&mut self, now: Instant, window: Duration) {
        self.deliveries.push_back((now, self.delivered));
        while self
            .deliveries
            .get(1)
            .is_some_and(|&(at, _)| now.saturating_duration_since(at) >= window)
        {
            self.deliveries.pop_front();
        }
    }

    /// Over the last `window` (at least) of the current capture, it delivered less than a
    /// tenth of the audio that much time holds: it runs, but (almost) no buffers come.
    fn starved(&self, now: Instant, window: Duration) -> bool {
        let Some(&(since, then)) = self.deliveries.front() else {
            return false;
        };
        let ran = now.saturating_duration_since(since);
        ran >= window && self.delivered.saturating_sub(then) * 10 < samples_in(ran)
    }
}

/// Them's loudness on the meeting clock: which [`BLOCK`]s were [`LOUD`].
#[derive(Default)]
struct Levels {
    /// The loud blocks' numbers, ascending.
    loud: VecDeque<u64>,
    /// The block being summed: its number, its sum of squares and samples so far.
    block: u64,
    sum: f64,
    n: u64,
}

impl Levels {
    /// Adds `samples`, which start at `pos`.
    fn feed(&mut self, mut pos: u64, mut samples: &[f32]) {
        while !samples.is_empty() {
            let block = pos / BLOCK;
            if block != self.block {
                self.close();
                self.block = block;
            }
            let n = ((block + 1) * BLOCK - pos).min(samples.len() as u64) as usize;
            self.sum += samples[..n]
                .iter()
                .map(|&x| f64::from(x) * f64::from(x))
                .sum::<f64>();
            self.n += n as u64;
            pos += n as u64;
            samples = &samples[n..];
        }
    }

    /// Ends the block being summed.
    fn close(&mut self) {
        if self.loud_now() {
            self.loud.push_back(self.block);
        }
        (self.sum, self.n) = (0.0, 0);
    }

    fn loud_now(&self) -> bool {
        self.n > 0 && (self.sum / self.n as f64).sqrt() >= LOUD
    }

    /// Them played somewhere in `span`.
    fn loud_in(&self, span: Span) -> bool {
        let (first, last) = (span.start / BLOCK, span.end.saturating_sub(1) / BLOCK);
        let blocks = first..=last;
        (self.loud_now() && blocks.contains(&self.block))
            || self
                .loud
                .iter()
                .rev()
                .take_while(|b| **b >= first)
                .any(|b| blocks.contains(b))
    }

    /// Forgets the blocks that end before `pos`.
    fn forget(&mut self, pos: u64) {
        while self.loud.front().is_some_and(|b| (b + 1) * BLOCK <= pos) {
            self.loud.pop_front();
        }
    }
}

/// The `meeting` thread.
struct Worker {
    id: String,
    started_at: i64,
    t0: Instant,
    store: Store,
    cfg: SessionConfig,
    captures: Arc<dyn CaptureFactory>,
    transcriber: Arc<dyn Transcriber>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    /// For new captures' audio and the transcriber's answers.
    tx: Sender<Msg>,
    tracks: [TrackState; 2],
    /// The system track delivered audio at some point: until then there is nothing to echo.
    system_ran: bool,
    paused: bool,
    /// From the stop on: `ended_at`, and until when the last segments are waited for.
    stopping: Option<(i64, Instant)>,
    /// Me lines waiting for the echo check, in the order they were said.
    held: VecDeque<Held>,
    /// Them's recent lines and loudness, for the echo check.
    them_lines: VecDeque<(Span, String)>,
    them_levels: Levels,
    /// Lines so far (the last one's id), and Me lines dropped as echo.
    lines: u64,
    echoes: u64,
    /// The last error sent: one that repeats on every segment is sent once.
    last_error: Option<String>,
    /// Set as [`Event::Stopped`] goes out (see [`Rescue`]).
    stopped: Arc<AtomicBool>,
}

impl Worker {
    fn new(meeting: &Meeting, t0: Instant, deps: Deps, tx: Sender<Msg>) -> Worker {
        let tracks = [
            TrackState::new(&deps.cfg.cut),
            TrackState::new(&deps.cfg.cut),
        ];
        Worker {
            id: meeting.id.clone(),
            started_at: meeting.started_at,
            t0,
            store: deps.store,
            cfg: deps.cfg,
            captures: deps.captures,
            transcriber: deps.transcriber,
            notify: deps.notify,
            tx,
            tracks,
            system_ran: false,
            paused: false,
            stopping: None,
            held: VecDeque::new(),
            them_lines: VecDeque::new(),
            them_levels: Levels::default(),
            lines: 0,
            echoes: 0,
            last_error: None,
            stopped: Arc::default(),
        }
    }

    /// `save_audio`: makes `track`'s WAV file (once), with silence up to where the track is
    /// now so it stays on the meeting clock, and lists it in `meeting.audio`. The mic's is made
    /// at the start; the system track's at its first audio, which may be much later.
    fn open_wav(&mut self, track: Track) -> Result<Meeting, String> {
        let t = &mut self.tracks[track.index()];
        t.wav_opened = true;
        let path = self.store.dir(&self.id).join(track.file());
        let fail = |e| format!("Couldn't save the {} audio: {e}", track.label());
        let mut wav = WavWriter::create(&path).map_err(fail)?;
        let mut n = t.pos;
        while n > 0 {
            let k = n.min(ZEROS.len() as u64);
            wav.write(&ZEROS[..k as usize]).map_err(fail)?;
            n -= k;
        }
        t.wav = Some(wav);
        let file = track.file().to_owned();
        self.store
            .update(&self.id, |m| m.audio.push(file))
            .map_err(|e| format!("Couldn't list the audio files: {e}"))
    }

    fn run(mut self, rx: Receiver<Msg>, meeting: Meeting, errors: Vec<String>) {
        self.emit(Event::Started(meeting));
        for e in errors {
            self.error(e);
        }
        // A zero poll would spin.
        let poll = self.cfg.poll.max(Duration::from_millis(10));
        let mut tick = Instant::now() + poll;
        loop {
            let now = Instant::now();
            let wake = match self.stopping {
                Some((_, deadline)) => {
                    if self.transcribing() == 0 || now >= deadline {
                        break;
                    }
                    deadline
                }
                None => {
                    if now >= tick {
                        self.tick(now);
                        tick += poll;
                        if tick <= now {
                            tick = now + poll; // fell behind: no burst of ticks
                        }
                    }
                    tick
                }
            };
            match rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(msg) => self.handle(msg, Instant::now()),
                Err(RecvTimeoutError::Timeout) => {}
                // Can't happen while `self.tx` lives.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.finish(Instant::now());
    }

    fn handle(&mut self, msg: Msg, now: Instant) {
        match msg {
            Msg::Audio {
                track,
                generation,
                at,
                samples,
            } => self.audio(track, generation, at, &samples),
            Msg::Started {
                track,
                generation,
                result,
            } => self.started(track, generation, result),
            Msg::Text { track, seq, result } => self.text(track, seq, result, now),
            Msg::Pause => self.pause(now),
            Msg::Resume(at) => self.resume(at, now),
            Msg::Stop(at) => self.stop(at, now),
        }
        self.release(now, false);
    }

    /// Every [`SessionConfig::poll`]: restarts lost captures, reports the status, forgets old
    /// Them history, and puts out Me lines that waited long enough.
    fn tick(&mut self, now: Instant) {
        for track in Track::ALL {
            self.check(track, now);
        }
        let system = &mut self.tracks[Track::System.index()];
        if self.cfg.system_delivers_continuously && system.capture.is_some() {
            system.note_delivery(now, self.cfg.silent_after);
        }
        self.status(now, true);
        self.forget();
        self.release(now, false);
    }

    fn audio(&mut self, track: Track, generation: u64, at: Instant, samples: &[f32]) {
        let t = &mut self.tracks[track.index()];
        if generation != t.generation || self.stopping.is_some() {
            return;
        }
        t.delivered += samples.len() as u64;
        t.sound = t.sound || samples.iter().any(|&x| x != 0.0);
        self.system_ran |= track == Track::System;
        if self.paused {
            return;
        }
        if self.cfg.save_audio
            && !self.tracks[track.index()].wav_opened
            && let Err(e) = self.open_wav(track)
        {
            self.error(e);
        }
        // The chunk ends about when it arrived.
        let due = self.clock(at).saturating_sub(samples.len() as u64);
        let pos = self.tracks[track.index()].pos;
        if due > pos + samples_in(GAP) {
            self.pad(track, due - pos);
        }
        self.feed(track, samples);
    }

    /// Feeds `n` zeros: a gap in the track's audio.
    fn pad(&mut self, track: Track, mut n: u64) {
        while n > 0 {
            let k = n.min(ZEROS.len() as u64);
            self.feed(track, &ZEROS[..k as usize]);
            n -= k;
        }
    }

    /// Adds audio at the track's position: to its WAV file, Them's loudness and the cutter.
    fn feed(&mut self, track: Track, samples: &[f32]) {
        let i = track.index();
        if let Some(wav) = &mut self.tracks[i].wav
            && let Err(e) = wav.write(samples)
        {
            self.tracks[i].wav = None;
            self.error(format!("Couldn't save the {} audio: {e}", track.label()));
        }
        let t = &mut self.tracks[i];
        if track == Track::System {
            self.them_levels.feed(t.pos, samples);
        }
        // Zeros before the cutter's first sound only move where it starts.
        let skip = match t.base {
            Some(_) => 0,
            None => {
                let k = samples
                    .iter()
                    .position(|&x| x != 0.0)
                    .unwrap_or(samples.len());
                if k < samples.len() {
                    t.base = Some(t.pos + k as u64);
                }
                k
            }
        };
        let segments = match &samples[skip..] {
            [] => Vec::new(),
            rest => t.cutter.push(rest),
        };
        t.pos += samples.len() as u64;
        for seg in segments {
            self.segment(track, seg);
        }
    }

    /// Ends both cutters' segments (pause, stop): what they hold is transcribed, and new
    /// cutters start at the next sound.
    fn flush(&mut self) {
        for track in Track::ALL {
            let i = track.index();
            let fresh = Cutter::new(self.cfg.cut.clone());
            let cutter = std::mem::replace(&mut self.tracks[i].cutter, fresh);
            for seg in cutter.finish() {
                self.segment(track, seg);
            }
            let t = &mut self.tracks[i];
            t.order.push_back((None, t.pos));
            t.base = None;
            self.advance(track);
        }
    }

    /// Sends a finished segment to the transcriber, unless it is all zeros or holds no speech
    /// ([`Segment::speech`], always true without a pause cut). A segment that isn't sent is
    /// done at once: the track is transcribed past it all the same.
    fn segment(&mut self, track: Track, seg: Segment) {
        let t = &mut self.tracks[track.index()];
        let base = t.base.unwrap_or(t.pos);
        let start = base + seg.start as u64;
        // Everything before this is in this segment or an earlier one, or is silence the
        // cutter dropped: a pause cut comes a pause after the speech, the end after all of it.
        // (A pause trimmed inside the segment makes `start + len` a little early: safe here.)
        let done = match seg.cut {
            Cut::Pause | Cut::End => base + seg.at as u64,
            Cut::Limit => start + seg.audio.len() as u64,
        };
        t.seq += 1;
        let seq = t.seq;
        t.order.push_back((Some(seq), done));
        if !seg.speech || seg.audio.iter().all(|&x| x == 0.0) {
            if !seg.speech {
                tracing::debug!(
                    "{}: no speech in {} ms at {} ms, not transcribed",
                    track.label(),
                    ms(seg.audio.len() as u64),
                    ms(start)
                );
            }
            self.advance(track);
            return;
        }
        // The line's times: where the speech is in it, not where the cutter began keeping audio.
        let (from, to) = speech_span(&seg.audio);
        let span = Span {
            start: start + from as u64,
            end: start + to as u64,
        };
        t.pending.insert(seq, span);
        let tx = self.tx.clone();
        self.transcriber.transcribe(
            seg.audio,
            Box::new(move |result| {
                // Fails only after the stop gave up waiting for it.
                let _ = tx.send(Msg::Text { track, seq, result });
            }),
        );
    }

    /// Moves `done` past the segments at the front of `order` that are transcribed.
    fn advance(&mut self, track: Track) {
        let t = &mut self.tracks[track.index()];
        while let Some(&(seq, done)) = t.order.front() {
            if seq.is_some_and(|s| t.pending.contains_key(&s)) {
                break;
            }
            t.done = t.done.max(done);
            t.order.pop_front();
        }
    }

    fn text(&mut self, track: Track, seq: u64, result: Result<String, String>, now: Instant) {
        let Some(span) = self.tracks[track.index()].pending.remove(&seq) else {
            return;
        };
        self.advance(track);
        let text = match result {
            Ok(text) => text.trim().to_owned(),
            Err(e) => {
                self.error(format!("Couldn't transcribe part of the meeting: {e}"));
                return;
            }
        };
        if text.is_empty() {
            return;
        }
        match track {
            Track::System => {
                self.them_lines.push_back((span, text.clone()));
                self.emit_line(Speaker::Them, span, text);
            }
            Track::Mic => {
                let at = self.held.partition_point(|h| h.span.start <= span.start);
                self.held.insert(
                    at,
                    Held {
                        span,
                        text,
                        since: now,
                    },
                );
            }
        }
    }

    /// Puts out the Me lines whose echo check can be made now (all of them at the end), in the
    /// order they were said.
    fn release(&mut self, now: Instant, all: bool) {
        while let Some(h) = self.held.front() {
            let quiet = h.span.around(QUIET_AROUND);
            let playing = self.them_levels.loud_in(quiet);
            let ready = all
                || !self.system_ran
                || self.tracks[Track::System.index()].done >= h.span.end
                || (!playing && self.them_known(now) >= quiet.end)
                || now.saturating_duration_since(h.since) >= self.cfg.echo_wait;
            if !ready {
                return;
            }
            let near = h.span.around(NEARBY);
            let them: Vec<&str> = self
                .them_lines
                .iter()
                .filter(|(span, _)| span.overlaps(near))
                .map(|(_, text)| text.as_str())
                .collect();
            let echo = echo::is_echo(&h.text, &them, playing);
            let Some(h) = self.held.pop_front() else {
                return;
            };
            if echo {
                self.echoes += 1;
                tracing::debug!("dropped a Me line at {} ms as echo", ms(h.span.start));
                self.emit(Event::Echo {
                    id: self.id.clone(),
                    dropped: self.echoes,
                });
            } else {
                self.emit_line(Speaker::Me, h.span, h.text);
            }
        }
    }

    /// How far Them's audio is known: what it delivered, and since no packets come while
    /// nothing plays, the clock less [`GAP`].
    fn them_known(&self, now: Instant) -> u64 {
        let pos = self.tracks[Track::System.index()].pos;
        pos.max(self.clock(now).saturating_sub(samples_in(GAP)))
    }

    /// Numbers the line, appends it to `transcript.jsonl`, then sends it.
    fn emit_line(&mut self, speaker: Speaker, span: Span, text: String) {
        self.lines += 1;
        let line = Line {
            id: self.lines,
            speaker,
            start_ms: ms(span.start),
            end_ms: ms(span.end),
            text,
        };
        if let Err(e) = self.store.append_line(&self.id, &line) {
            self.error(format!("Couldn't save the transcript: {e}"));
        }
        self.emit(Event::Line {
            id: self.id.clone(),
            line,
        });
    }

    fn pause(&mut self, now: Instant) {
        if self.paused || self.stopping.is_some() {
            return;
        }
        self.paused = true;
        self.flush();
        self.status(now, true);
    }

    fn resume(&mut self, at: Instant, now: Instant) {
        if !self.paused || self.stopping.is_some() {
            return;
        }
        self.paused = false;
        // Both tracks go on from now: the new cutters skip the zeros, the files keep them.
        let clock = self.clock(at);
        for track in Track::ALL {
            let pos = self.tracks[track.index()].pos;
            if clock > pos {
                self.pad(track, clock - pos);
            }
        }
        self.status(now, true);
    }

    fn stop(&mut self, at: Instant, now: Instant) {
        if self.stopping.is_some() {
            return;
        }
        self.status(now, false);
        let ended_at = self.started_at + elapsed_ms(self.t0, at) as i64;
        self.stopping = Some((ended_at, now + STOP_WAIT));
        for t in &mut self.tracks {
            t.capture = None;
        }
        self.flush();
        for track in Track::ALL {
            if let Some(wav) = self.tracks[track.index()].wav.take()
                && let Err(e) = wav.finish()
            {
                self.error(format!("Couldn't save the {} audio: {e}", track.label()));
            }
        }
        tracing::info!(
            "meeting {} stopping; {} segment(s) still being transcribed",
            self.id,
            self.transcribing()
        );
    }

    /// After the stop: the last lines, `ended_at`, `meeting.md`, [`Event::Stopped`].
    fn finish(mut self, now: Instant) {
        let left = self.transcribing();
        if left > 0 {
            tracing::warn!(
                "meeting {}: {left} segment(s) still not transcribed after {STOP_WAIT:?}; \
                 stopping without them",
                self.id
            );
        }
        self.release(now, true);
        let ended_at = match self.stopping {
            Some((ended_at, _)) => ended_at,
            None => self.started_at + elapsed_ms(self.t0, now) as i64,
        };
        if let Err(e) = self.store.update(&self.id, |m| m.ended_at = Some(ended_at)) {
            self.error(format!("Couldn't save the meeting: {e}"));
        }
        if let Err(e) = self.store.write_markdown(&self.id) {
            self.error(format!("Couldn't write meeting.md: {e}"));
        }
        tracing::info!(
            "meeting {} stopped: {} line(s), {} echo line(s) dropped",
            self.id,
            self.lines,
            self.echoes
        );
        self.stopped.store(true, Ordering::SeqCst);
        self.emit(Event::Stopped {
            id: self.id.clone(),
        });
    }

    /// Restarts `track`'s capture if it failed or its device changed, or starts it if it is
    /// missing.
    fn check(&mut self, track: Track, now: Instant) {
        let t = &mut self.tracks[track.index()];
        let restart = match &t.capture {
            Some(c) if c.failed() => {
                tracing::warn!("{} capture failed; restarting it", track.label());
                true
            }
            Some(c) if c.stale() => {
                tracing::info!("{} device changed; restarting the capture", track.label());
                true
            }
            Some(_) => false,
            None => t.retry && t.starting.is_none(),
        };
        if restart {
            // The old one goes first: it may hold the device.
            t.capture = None;
            self.start_capture(track, now);
        }
    }

    /// Starts a new capture of `track`: the mic here and now, the system audio on a thread of
    /// its own (see the module docs), its result coming back as [`Msg::Started`].
    fn start_capture(&mut self, track: Track, now: Instant) {
        let t = &mut self.tracks[track.index()];
        t.generation += 1;
        t.delivered = 0;
        t.sound = false;
        t.deliveries.clear();
        let generation = t.generation;
        let muted = (track == Track::Mic).then(|| self.cfg.mic_muted.clone());
        let sink = sink(track, generation, &self.tx, muted);
        if track == Track::Mic {
            let result = self.captures.start(track, sink);
            self.started(track, generation, result);
            return;
        }
        t.starting = Some(now);
        let (captures, tx) = (self.captures.clone(), self.tx.clone());
        let spawned = thread::Builder::new()
            .name("meeting-system-start".into())
            .spawn(move || {
                // A panic in there must not leave the track waiting for it forever.
                let start = AssertUnwindSafe(|| captures.start(track, sink));
                let result = panic::catch_unwind(start)
                    .unwrap_or_else(|p| Err(format!("starting it crashed: {}", panic_text(&*p))));
                // The meeting may be over by now: then this fails, and the capture is dropped
                // right here.
                let _ = tx.send(Msg::Started {
                    track,
                    generation,
                    result,
                });
            });
        if let Err(e) = spawned {
            let result = Err(format!("couldn't start a thread for it: {e}"));
            self.started(track, generation, result);
        }
    }

    /// A start's result. One that comes after the stop (or after another start replaced it)
    /// is dropped, which stops it.
    fn started(
        &mut self,
        track: Track,
        generation: u64,
        result: Result<Box<dyn CaptureHandle>, String>,
    ) {
        let i = track.index();
        if generation != self.tracks[i].generation || self.stopping.is_some() {
            return;
        }
        self.tracks[i].starting = None;
        match result {
            Ok(capture) => {
                if self.tracks[i].error.take().is_some() {
                    tracing::info!("{} capture is back", track.label());
                    // So losing it again is reported again.
                    self.last_error = None;
                }
                self.install(track, capture);
            }
            Err(e) => {
                if self.tracks[i].error.as_ref() != Some(&e) {
                    if self.tracks[i].retry {
                        tracing::warn!("can't restart the {} capture: {e}", track.label());
                    } else {
                        tracing::warn!("no {}, recording without it: {e}", track.label());
                    }
                    if track == Track::Mic {
                        self.error(format!("The microphone stopped: {e}"));
                    }
                }
                self.tracks[i].error = Some(e);
            }
        }
    }

    /// `capture` now delivers `track` (with the current `generation`).
    fn install(&mut self, track: Track, capture: Box<dyn CaptureHandle>) {
        let t = &mut self.tracks[track.index()];
        t.capture = Some(capture);
        t.retry = true;
        t.deliveries.clear();
        t.deliveries.push_back((Instant::now(), t.delivered));
    }

    fn system_audio(&self, now: Instant) -> SystemAudio {
        let t = &self.tracks[Track::System.index()];
        match (&t.capture, t.starting, &t.error) {
            // Trying again after a failed start: still not recording, for that reason.
            (None, Some(_), Some(e)) => SystemAudio::Unsupported(e.clone()),
            (None, Some(since), None)
                if now.saturating_duration_since(since) >= self.cfg.system_wait =>
            {
                SystemAudio::Silent
            }
            (None, Some(_), None) => SystemAudio::Ok,
            (None, None, e) => SystemAudio::Unsupported(e.clone().unwrap_or_default()),
            (Some(_), ..) if !t.sound && t.delivered >= samples_in(self.cfg.silent_after) => {
                SystemAudio::Silent
            }
            (Some(_), ..)
                if self.cfg.system_delivers_continuously
                    && t.starved(now, self.cfg.silent_after) =>
            {
                SystemAudio::Silent
            }
            (Some(_), ..) => SystemAudio::Ok,
        }
    }

    fn status(&self, now: Instant, recording: bool) {
        self.emit(Event::Status {
            id: self.id.clone(),
            recording,
            paused: self.paused,
            elapsed_ms: elapsed_ms(self.t0, now),
            system: self.system_audio(now),
        });
    }

    /// Forgets Them's lines and loudness older than [`ECHO_KEEP`], unless a Me line still to
    /// be checked needs them.
    fn forget(&mut self) {
        let them = &self.tracks[Track::System.index()];
        let mut keep = them.pos.saturating_sub(samples_in(ECHO_KEEP));
        let me = &self.tracks[Track::Mic.index()];
        let waiting = self.held.iter().map(|h| h.span.start);
        if let Some(oldest) = waiting.chain(me.pending.values().map(|s| s.start)).min() {
            keep = keep.min(oldest.saturating_sub(samples_in(NEARBY)));
        }
        self.them_lines.retain(|(span, _)| span.end >= keep);
        self.them_levels.forget(keep);
    }

    fn transcribing(&self) -> usize {
        self.tracks.iter().map(|t| t.pending.len()).sum()
    }

    /// Logs `message` and sends it, unless it was the last one sent.
    fn error(&mut self, message: String) {
        tracing::warn!("meeting {}: {message}", self.id);
        if self.last_error.as_ref() != Some(&message) {
            self.emit(Event::Error {
                id: self.id.clone(),
                message: message.clone(),
            });
            self.last_error = Some(message);
        }
    }

    fn emit(&self, event: Event) {
        (self.notify)(event);
    }

    /// Where `at` is on the meeting clock, in samples.
    fn clock(&self, at: Instant) -> u64 {
        samples_in(at.saturating_duration_since(self.t0))
    }
}

/// Where the speech is in a segment's audio, as sample offsets: from the first 10 ms block
/// that sounds like speech to the end of the last one; all of it if none does. Sounds like
/// speech: at least [`SPEECH_OVER_FLOOR_DB`] above the segment's quiet floor (its
/// 10th-percentile block level), and at least [`SPEECH_MIN_DBFS`].
///
/// [`Cutter`] keeps a real mic's room noise until it hears speech (and a long quiet stretch is
/// cut only at its length limit), so a segment can begin well before anyone spoke. A line's
/// times say when the words were.
fn speech_span(audio: &[f32]) -> (usize, usize) {
    const STEP: usize = SAMPLE_RATE as usize / 100;
    let levels: Vec<f32> = audio
        .chunks(STEP)
        .map(|b| {
            let power = b.iter().map(|&x| x * x).sum::<f32>() / b.len() as f32;
            10.0 * (power + 1e-12).log10()
        })
        .collect();
    let mut sorted = levels.clone();
    sorted.sort_unstable_by(f32::total_cmp);
    let Some(&floor) = sorted.get(sorted.len() / 10) else {
        return (0, 0);
    };
    let speech = |level: &f32| *level >= floor + SPEECH_OVER_FLOOR_DB && *level >= SPEECH_MIN_DBFS;
    match (
        levels.iter().position(speech),
        levels.iter().rposition(speech),
    ) {
        (Some(first), Some(last)) => (first * STEP, ((last + 1) * STEP).min(audio.len())),
        _ => (0, audio.len()),
    }
}

/// What a panic said, if it said it with a string.
fn panic_text(payload: &(dyn Any + Send)) -> &str {
    match payload.downcast_ref::<&str>() {
        Some(text) => text,
        None => payload.downcast_ref::<String>().map_or("?", String::as_str),
    }
}

/// `d` in samples.
fn samples_in(d: Duration) -> u64 {
    (d.as_micros() * u128::from(SAMPLE_RATE) / 1_000_000) as u64
}

/// Samples to milliseconds.
fn ms(samples: u64) -> u64 {
    samples * 1000 / u64::from(SAMPLE_RATE)
}

fn elapsed_ms(t0: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(t0).as_millis() as u64
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agt_stt::chunk::PauseCut;
    use std::{
        f32::consts::TAU,
        fs,
        path::PathBuf,
        sync::{
            Condvar,
            atomic::{AtomicU64, AtomicUsize},
        },
    };

    const SR: usize = SAMPLE_RATE as usize;

    fn secs(s: f32) -> usize {
        (s * SR as f32).round() as usize
    }

    /// What the fake transcriber can hear: each phrase is a tone of its own loudness ([`say`]).
    const PHRASES: [&str; 4] = [
        "hello from the other side",
        "this is me talking",
        "one more thing",
        "see you on friday",
    ];
    const THEM: &str = PHRASES[0];
    const ME: &str = PHRASES[1];
    const MORE: &str = PHRASES[2];
    const LATER: &str = PHRASES[3];

    /// `phrase` said for `s` seconds: a 220 Hz tone, 0.2 loud for the first phrase, 0.3 for the
    /// second, and so on.
    fn say(phrase: &str, s: f32) -> Vec<f32> {
        let i = PHRASES.iter().position(|p| *p == phrase).unwrap();
        let amp = 0.2 + 0.1 * i as f32;
        (0..secs(s))
            .map(|n| amp * (TAU * 220.0 * n as f32 / SR as f32).sin())
            .collect()
    }

    fn quiet(s: f32) -> Vec<f32> {
        vec![0.0; secs(s)]
    }

    /// The phrase a segment holds, by its loudest sample; "" for silence.
    fn hear(audio: &[f32]) -> String {
        let peak = audio.iter().fold(0f32, |m, x| m.max(x.abs()));
        if peak < 0.1 {
            return String::new();
        }
        let i = ((peak - 0.2) / 0.1).round() as usize;
        PHRASES
            .get(i)
            .map_or_else(|| format!("? {peak}"), |p| p.to_string())
    }

    type Done = Box<dyn FnOnce(Result<String, String>) + Send>;

    /// Transcribes in order on a thread of its own, a little later, like the app's `stt` thread.
    struct Stt(Sender<(Vec<f32>, Done)>);

    impl Stt {
        fn new() -> Arc<Stt> {
            let (tx, rx) = mpsc::channel::<(Vec<f32>, Done)>();
            thread::Builder::new()
                .name("fake stt".into())
                .spawn(move || {
                    for (audio, done) in rx {
                        thread::sleep(Duration::from_millis(20));
                        done(Ok(hear(&audio)));
                    }
                })
                .unwrap();
            Arc::new(Stt(tx))
        }
    }

    impl Transcriber for Stt {
        fn transcribe(&self, audio: Vec<f32>, done: Done) {
            self.0.send((audio, done)).unwrap();
        }
    }

    /// Transcribes at once, inside `transcribe` (as the app's does once its thread is gone).
    struct AtOnce;

    impl Transcriber for AtOnce {
        fn transcribe(&self, audio: Vec<f32>, done: Done) {
            done(Ok(hear(&audio)));
        }
    }

    /// Transcribes at once like [`AtOnce`], but makes words up for audio it can't hear that
    /// isn't silence, as a speech model does with room noise.
    struct MakesUp;

    impl Transcriber for MakesUp {
        fn transcribe(&self, audio: Vec<f32>, done: Done) {
            let text = match hear(&audio) {
                heard if heard.is_empty() && audio.iter().any(|&x| x != 0.0) => "thank you".into(),
                heard => heard,
            };
            done(Ok(text));
        }
    }

    /// Panics on the meeting thread, which calls it.
    struct Panics;

    impl Transcriber for Panics {
        fn transcribe(&self, _: Vec<f32>, _: Done) {
            panic!("fake transcriber crashed");
        }
    }

    /// Fake devices: the test plays audio into whichever captures are running.
    #[derive(Default)]
    struct Devices {
        sinks: Mutex<[Option<(u64, Sink)>; 2]>,
        /// Every `start`, in order.
        starts: Mutex<Vec<Track>>,
        next: AtomicU64,
        /// The track's capture reports stale once (its default device changed).
        stale: [AtomicBool; 2],
        /// The system track can't start, and why.
        no_system: Mutex<Option<String>>,
        /// The mic can't start (it was unplugged), and why.
        no_mic: Mutex<Option<String>>,
        /// The next `start(System)` panics.
        panic_system: AtomicBool,
        /// While set, `start(System)` waits for [`Devices::release_system`], like a start
        /// macOS holds until the user answers the permission prompt.
        hold_system: Mutex<bool>,
        released: Condvar,
        /// `start(System)` returned, Ok or not.
        system_started: AtomicBool,
        /// Captures dropped, per track.
        dropped: [AtomicUsize; 2],
    }

    impl Devices {
        /// Their system audio start waits for [`release_system`](Self::release_system).
        fn held() -> Devices {
            Devices {
                hold_system: Mutex::new(true),
                ..Devices::default()
            }
        }

        fn release_system(&self) {
            *self.hold_system.lock().unwrap() = false;
            self.released.notify_all();
        }

        fn running(&self, track: Track) -> bool {
            self.sinks.lock().unwrap()[track.index()].is_some()
        }

        fn dropped(&self, track: Track) -> usize {
            self.dropped[track.index()].load(Ordering::SeqCst)
        }

        /// Plays both tracks side by side, 100 ms at a time, `speed` times real time.
        fn play(&self, mic: &[f32], system: &[f32], speed: f32) {
            let step = SR / 10;
            for k in 0..mic.len().max(system.len()).div_ceil(step) {
                for (track, audio) in [(Track::Mic, mic), (Track::System, system)] {
                    let part =
                        &audio[(k * step).min(audio.len())..((k + 1) * step).min(audio.len())];
                    let mut sinks = self.sinks.lock().unwrap();
                    if let (false, Some((_, sink))) = (part.is_empty(), &mut sinks[track.index()]) {
                        sink(part);
                    }
                }
                thread::sleep(Duration::from_secs_f32(0.1 / speed));
            }
        }

        fn starts(&self) -> Vec<Track> {
            self.starts.lock().unwrap().clone()
        }
    }

    struct Fake(Arc<Devices>);

    impl CaptureFactory for Fake {
        fn start(&self, track: Track, sink: Sink) -> Result<Box<dyn CaptureHandle>, String> {
            self.0.starts.lock().unwrap().push(track);
            if track == Track::System && self.0.panic_system.swap(false, Ordering::SeqCst) {
                panic!("fake tap crashed");
            }
            if track == Track::System {
                let mut held = self.0.hold_system.lock().unwrap();
                while *held {
                    held = self.0.released.wait(held).unwrap();
                }
            }
            let refused = match track {
                Track::Mic => self.0.no_mic.lock().unwrap().clone(),
                Track::System => self.0.no_system.lock().unwrap().clone(),
            };
            let started = match refused {
                Some(why) => Err(why),
                None => {
                    let generation = self.0.next.fetch_add(1, Ordering::SeqCst);
                    self.0.sinks.lock().unwrap()[track.index()] = Some((generation, sink));
                    Ok(Box::new(FakeCapture {
                        devices: self.0.clone(),
                        track,
                        generation,
                    }) as Box<dyn CaptureHandle>)
                }
            };
            if track == Track::System {
                self.0.system_started.store(true, Ordering::SeqCst);
            }
            started
        }
    }

    struct FakeCapture {
        devices: Arc<Devices>,
        track: Track,
        generation: u64,
    }

    impl CaptureHandle for FakeCapture {
        fn failed(&self) -> bool {
            false
        }

        fn stale(&self) -> bool {
            self.devices.stale[self.track.index()].swap(false, Ordering::SeqCst)
        }
    }

    impl Drop for FakeCapture {
        fn drop(&mut self) {
            self.devices.dropped[self.track.index()].fetch_add(1, Ordering::SeqCst);
            let mut sinks = self.devices.sinks.lock().unwrap();
            let slot = &mut sinks[self.track.index()];
            if slot.as_ref().is_some_and(|(g, _)| *g == self.generation) {
                *slot = None;
            }
        }
    }

    /// A folder for one test, removed when it ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir()
                .join(format!("agt-meeting-session-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Cut at 1 s pauses once there are 2 s, poll every 50 ms.
    fn config() -> SessionConfig {
        let cut = CutConfig {
            max_len: secs(10.0),
            search_len: secs(2.0),
            pause: Some(PauseCut {
                len: secs(1.0),
                min_segment: secs(2.0),
                pad: secs(0.25),
                trim: true,
            }),
        };
        SessionConfig {
            poll: Duration::from_millis(50),
            ..SessionConfig::new(cut, Arc::default())
        }
    }

    /// A session on fake devices and the fake transcriber, and what it reported.
    struct Run {
        devices: Arc<Devices>,
        store: Store,
        id: String,
        session: Option<Session>,
        events: Receiver<Event>,
        /// Every event so far.
        seen: Vec<Event>,
        _dir: TempDir,
    }

    impl Run {
        fn start(name: &str, devices: Devices, cfg: SessionConfig) -> Run {
            Run::start_with(name, devices, cfg, Stt::new())
        }

        /// [`Run::start`] with another transcriber.
        fn start_with(
            name: &str,
            devices: Devices,
            cfg: SessionConfig,
            transcriber: Arc<dyn Transcriber>,
        ) -> Run {
            let dir = TempDir::new(name);
            let store = Store::new(dir.0.clone());
            let held = *devices.hold_system.lock().unwrap();
            let devices = Arc::new(devices);
            if held {
                // A start that blocks on it must fail the test, not hang it.
                let devices = devices.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(5));
                    devices.release_system();
                });
            }
            let (tx, events) = mpsc::channel();
            let session = Session::start(
                store.clone(),
                cfg,
                Arc::new(Fake(devices.clone())),
                transcriber,
                Arc::new(move |e| {
                    let _ = tx.send(e);
                }),
            )
            .unwrap();
            if !held {
                // The system capture starts on a thread of its own: let it, so both tracks play.
                wait_until("the system start", || {
                    devices.system_started.load(Ordering::SeqCst)
                });
            }
            Run {
                devices,
                store,
                id: session.id().to_owned(),
                session: Some(session),
                events,
                seen: Vec::new(),
                _dir: dir,
            }
        }

        fn session(&self) -> &Session {
            self.session.as_ref().unwrap()
        }

        /// Plays both tracks at 20x real time.
        fn play(&self, mic: &[f32], system: &[f32]) {
            self.devices.play(mic, system, 20.0);
        }

        /// Waits (10 s at most) for an event `f` accepts.
        fn wait(&mut self, what: &str, f: impl Fn(&Event) -> bool) -> Event {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.events.recv_timeout(left) {
                    Ok(e) => {
                        self.seen.push(e.clone());
                        if f(&e) {
                            return e;
                        }
                    }
                    Err(err) => panic!("no {what} ({err}); got {:#?}", self.seen),
                }
            }
        }

        /// Stops; every event, once the meeting thread is done.
        fn stop(&mut self) -> Vec<Event> {
            self.session.take().unwrap().stop();
            self.wait("Stopped", |e| matches!(e, Event::Stopped { .. }));
            // The thread ends right after it (dropping `notify`): nothing comes after Stopped.
            match self.events.recv_timeout(Duration::from_secs(10)) {
                Err(RecvTimeoutError::Disconnected) => {}
                other => panic!("after Stopped: {other:?}"),
            }
            self.seen.clone()
        }
    }

    fn lines(events: &[Event]) -> Vec<Line> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Line { line, .. } => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    fn texts(lines: &[Line]) -> Vec<(Speaker, &str)> {
        lines.iter().map(|l| (l.speaker, l.text.as_str())).collect()
    }

    fn is_line(e: &Event, speaker: Speaker) -> bool {
        matches!(e, Event::Line { line, .. } if line.speaker == speaker)
    }

    fn near(ms: u64, want: u64) -> bool {
        ms.abs_diff(want) <= 300
    }

    /// Waits (10 s at most) until `f` holds.
    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "no {what} in 10 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Steady white noise at `dbfs` (RMS), the same every time.
    fn noise(dbfs: f32, s: f32) -> Vec<f32> {
        // Uniform in [-a, a] has an RMS of a / sqrt(3).
        let a = 10f32.powf(dbfs / 20.0) * 3f32.sqrt();
        let mut x: u32 = 0x1234_5678;
        (0..secs(s))
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                a * ((x >> 8) as f32 / (1 << 23) as f32 - 1.0)
            })
            .collect()
    }

    /// A 220 Hz tone at `dbfs` (RMS).
    fn tone(dbfs: f32, s: f32) -> Vec<f32> {
        let amp = 10f32.powf(dbfs / 20.0) * 2f32.sqrt();
        (0..secs(s))
            .map(|n| amp * (TAU * 220.0 * n as f32 / SR as f32).sin())
            .collect()
    }

    #[test]
    fn speech_span_skips_leading_noise() {
        let audio = [noise(-60.0, 2.0), tone(-20.0, 1.0), noise(-60.0, 1.0)].concat();
        let (start, end) = speech_span(&audio);
        assert!(start.abs_diff(32_000) <= 160, "{start}");
        assert!(end.abs_diff(48_000) <= 160, "{end}");
        // Speech to the end.
        let audio = [noise(-60.0, 2.0), tone(-20.0, 1.0)].concat();
        let (start, end) = speech_span(&audio);
        assert!(start.abs_diff(32_000) <= 160, "{start}");
        assert_eq!(end, audio.len());
    }

    #[test]
    fn speech_span_all_quiet() {
        for audio in [
            noise(-60.0, 3.0),
            quiet(3.0),
            // Steady from end to end: nothing stands out.
            tone(-20.0, 3.0),
            // Stands out, but too faint to be speech.
            [noise(-85.0, 2.0), tone(-60.0, 1.0)].concat(),
            // Not even a whole block.
            noise(-20.0, 0.005),
        ] {
            assert_eq!(speech_span(&audio), (0, audio.len()));
        }
        assert_eq!(speech_span(&[]), (0, 0));
    }

    #[test]
    fn lines_from_both_tracks() {
        let mut run = Run::start("both", Devices::default(), config());
        run.play(
            &[quiet(5.0), say(ME, 3.0), quiet(2.0)].concat(),
            &[say(THEM, 3.0), quiet(7.0)].concat(),
        );
        // Them was quiet around the Me line, so it comes without waiting for the stop.
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        let events = run.stop();
        let lines = lines(&events);
        assert_eq!(
            texts(&lines),
            [(Speaker::Them, THEM), (Speaker::Me, ME)],
            "{events:#?}"
        );
        assert_eq!((lines[0].id, lines[1].id), (1, 2));
        assert!(near(lines[0].start_ms, 0), "{lines:#?}");
        assert!(near(lines[1].start_ms, 5000), "{lines:#?}");
        assert!(near(lines[1].end_ms, 8250), "{lines:#?}");
    }

    #[test]
    fn echo_is_dropped() {
        let mut run = Run::start("echo", Devices::default(), config());
        let talk = [say(THEM, 3.0), quiet(3.0)].concat();
        run.play(&talk, &talk);
        run.wait("the echo", |e| matches!(e, Event::Echo { dropped: 1, .. }));
        let events = run.stop();
        assert_eq!(
            texts(&lines(&events)),
            [(Speaker::Them, THEM)],
            "{events:#?}"
        );
    }

    #[test]
    fn crosstalk_is_not_echo() {
        let mut run = Run::start("crosstalk", Devices::default(), config());
        run.play(
            &[say(ME, 3.0), quiet(3.0)].concat(),
            &[say(THEM, 3.0), quiet(3.0)].concat(),
        );
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        let events = run.stop();
        assert!(!events.iter().any(|e| matches!(e, Event::Echo { .. })));
        assert_eq!(
            texts(&lines(&events)),
            [(Speaker::Them, THEM), (Speaker::Me, ME)]
        );
    }

    #[test]
    fn noise_without_speech_is_not_transcribed() {
        let mut run = Run::start_with("noise", Devices::default(), config(), Arc::new(MakesUp));
        // Them: 14 s of steady noise and nothing said: a limit cut at 10 s, the rest at the
        // stop, neither with speech. Me: a phrase in the middle of it.
        run.play(
            &[quiet(2.0), say(ME, 3.0), quiet(9.0)].concat(),
            &noise(-40.0, 14.0),
        );
        // Them plays all along, so the Me line waits until Them is transcribed past it: done
        // at the limit cut, which never went to the transcriber.
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        let events = run.stop();
        assert_eq!(
            texts(&lines(&events)),
            [(Speaker::Me, ME)],
            "no made-up Them lines: {events:#?}"
        );
    }

    #[test]
    fn a_panic_still_ends_the_meeting() {
        let mut run = Run::start_with("panic", Devices::default(), config(), Arc::new(Panics));
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        // The first segment makes the meeting thread panic.
        run.wait("the error", |e| {
            matches!(e, Event::Error { message, .. } if message.contains("because of an error"))
        });
        let stopped = run.wait("Stopped", |e| matches!(e, Event::Stopped { .. }));
        assert_eq!(stopped, Event::Stopped { id: run.id.clone() });
        // The thread is gone, and nothing comes after Stopped.
        match run.events.recv_timeout(Duration::from_secs(10)) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("after Stopped: {other:?}"),
        }
        // Its captures were stopped, and the meeting is saved as ended.
        wait_until("the mic stopped", || run.devices.dropped(Track::Mic) == 1);
        assert!(!run.devices.running(Track::Mic));
        let m = run.store.load(&run.id).unwrap();
        assert!(m.ended_at.is_some_and(|end| end >= m.started_at), "{m:?}");
        assert!(run.store.dir(&run.id).join("meeting.md").is_file());
        // Stopping the session afterwards is harmless.
        run.session.take().unwrap().stop();
    }

    #[test]
    fn me_line_kept_when_them_silent() {
        let mut run = Run::start("alone", Devices::default(), config());
        run.play(&[say(ME, 3.0), quiet(3.0)].concat(), &quiet(6.0));
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        let events = run.stop();
        assert!(!events.iter().any(|e| matches!(e, Event::Echo { .. })));
        assert_eq!(texts(&lines(&events)), [(Speaker::Me, ME)]);
    }

    #[test]
    fn pause_skips_audio() {
        let mut run = Run::start("pause", Devices::default(), config());
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        run.session().pause();
        assert!(run.session().paused());
        run.play(&[say(MORE, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        run.session().resume();
        assert!(!run.session().paused());
        run.play(&[say(LATER, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        let events = run.stop();
        let lines = lines(&events);
        assert_eq!(
            texts(&lines),
            [(Speaker::Me, ME), (Speaker::Me, LATER)],
            "{events:#?}"
        );
        // Still on the meeting clock after the pause.
        assert!(lines[1].start_ms >= lines[0].end_ms.max(4700), "{lines:#?}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Status { paused: true, .. }))
        );
    }

    #[test]
    fn muted_mic_produces_no_me_lines() {
        let cfg = config();
        let muted = cfg.mic_muted.clone();
        let mut run = Run::start("muted", Devices::default(), cfg);
        // A dictation while the user says something.
        muted.store(true, Ordering::SeqCst);
        run.play(&[say(MORE, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        muted.store(false, Ordering::SeqCst);
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        let lines = lines(&run.stop());
        assert_eq!(texts(&lines), [(Speaker::Me, ME)]);
        assert!(near(lines[0].start_ms, 5000), "{lines:#?}");
    }

    #[test]
    fn restarts_a_stale_capture() {
        let mut run = Run::start("stale", Devices::default(), config());
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &quiet(5.0));
        run.devices.stale[Track::System.index()].store(true, Ordering::SeqCst);
        wait_until("the restart", || {
            run.devices.starts().len() == 3 && run.devices.running(Track::System)
        });
        run.play(&quiet(5.0), &[say(THEM, 3.0), quiet(2.0)].concat());
        let events = run.stop();
        assert_eq!(
            run.devices.starts(),
            [Track::Mic, Track::System, Track::System]
        );
        let lines = lines(&events);
        assert_eq!(
            texts(&lines),
            [(Speaker::Me, ME), (Speaker::Them, THEM)],
            "{events:#?}"
        );
        assert!(near(lines[1].start_ms, 5000), "{lines:#?}");
    }

    #[test]
    fn system_unsupported_runs_mic_only() {
        let devices = Devices {
            no_system: Mutex::new(Some("no system audio here".into())),
            ..Devices::default()
        };
        let mut run = Run::start("unsupported", devices, config());
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &[]);
        // Nothing to wait for: there is no Them track to echo.
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        let events = run.stop();
        let systems: Vec<&SystemAudio> = events
            .iter()
            .filter_map(|e| match e {
                Event::Status { system, .. } => Some(system),
                _ => None,
            })
            .collect();
        // From the start's answer on (it comes from a thread of its own).
        let unsupported = SystemAudio::Unsupported("no system audio here".into());
        let after: Vec<_> = systems
            .iter()
            .skip_while(|s| ***s == SystemAudio::Ok)
            .collect();
        assert!(!after.is_empty(), "{systems:?}");
        assert!(after.iter().all(|s| ***s == unsupported), "{systems:?}");
        assert_eq!(texts(&lines(&events)), [(Speaker::Me, ME)]);
        // Not tried again: the OS said no.
        assert_eq!(run.devices.starts(), [Track::Mic, Track::System]);
    }

    #[test]
    fn first_line_timestamp_follows_speech_not_silence() {
        let mut run = Run::start("onset", Devices::default(), config());
        // A real mic: never exact zeros, so the cutter keeps the 6 s of room noise.
        let mic = [noise(-60.0, 6.0), say(ME, 3.0), noise(-60.0, 2.0)].concat();
        run.play(&mic, &quiet(11.0));
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        let lines = lines(&run.stop());
        assert_eq!(texts(&lines), [(Speaker::Me, ME)]);
        assert!(near(lines[0].start_ms, 6000), "{lines:#?}");
        assert!(near(lines[0].end_ms, 9000), "{lines:#?}");
    }

    #[test]
    fn system_start_does_not_block_the_meeting() {
        let cfg = SessionConfig {
            system_wait: Duration::from_millis(300),
            ..config()
        };
        let t = Instant::now();
        let mut run = Run::start("system-wait", Devices::held(), cfg);
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "start: {:?}",
            t.elapsed()
        );
        wait_until("the system start", || {
            run.devices.starts().contains(&Track::System)
        });
        // The mic records meanwhile.
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &[]);
        run.wait("the Me line", |e| is_line(e, Speaker::Me));
        run.wait("Silent while it waits", |e| {
            matches!(
                e,
                Event::Status {
                    system: SystemAudio::Silent,
                    ..
                }
            )
        });
        run.devices.release_system();
        wait_until("the system capture", || run.devices.running(Track::System));
        run.wait("Ok once it runs", |e| {
            matches!(
                e,
                Event::Status {
                    system: SystemAudio::Ok,
                    ..
                }
            )
        });
        let at = run.session().elapsed_ms();
        run.play(&quiet(5.0), &[say(THEM, 3.0), quiet(2.0)].concat());
        let lines = lines(&run.stop());
        assert_eq!(
            texts(&lines),
            [(Speaker::Me, ME), (Speaker::Them, THEM)],
            "{lines:#?}"
        );
        // On the meeting clock: the late track starts with silence up to when it began.
        let start = lines[1].start_ms;
        assert!(
            start + 300 >= at && start <= at + 300,
            "{start} ms, played at {at} ms"
        );
    }

    #[test]
    fn failing_system_restart_reports_unsupported() {
        let mut run = Run::start("system-lost", Devices::default(), config());
        // The only output device is unplugged: every restart fails.
        *run.devices.no_system.lock().unwrap() = Some("no output device".into());
        run.devices.stale[Track::System.index()].store(true, Ordering::SeqCst);
        let lost = SystemAudio::Unsupported("no output device".into());
        let is_lost = |e: &Event| matches!(e, Event::Status { system, .. } if *system == lost);
        run.wait("Unsupported", is_lost);
        // Every poll tries again, and fails again: never Ok meanwhile.
        for _ in 0..5 {
            let status = run.wait("a Status", |e| matches!(e, Event::Status { .. }));
            assert!(is_lost(&status), "{status:?}");
        }
        run.stop();
        assert!(
            run.devices.starts().len() >= 6,
            "{:?}",
            run.devices.starts()
        );
    }

    #[test]
    fn system_start_panic_is_unsupported() {
        let mut run = Run::start("system-panic", Devices::default(), config());
        // The restart's start panics on its thread: the track mustn't wait for it forever.
        run.devices.panic_system.store(true, Ordering::SeqCst);
        run.devices.stale[Track::System.index()].store(true, Ordering::SeqCst);
        run.wait("Unsupported", |e| {
            matches!(
                e,
                Event::Status { system: SystemAudio::Unsupported(why), .. }
                    if why.contains("fake tap crashed")
            )
        });
        // The next start works.
        wait_until("the system capture", || run.devices.running(Track::System));
        run.play(&quiet(5.0), &[say(THEM, 3.0), quiet(2.0)].concat());
        let lines = lines(&run.stop());
        assert_eq!(texts(&lines), [(Speaker::Them, THEM)]);
    }

    #[test]
    fn system_restarts_one_at_a_time() {
        let mut run = Run::start("system-once", Devices::default(), config());
        // The device changes, and the new start hangs (the prompt again).
        *run.devices.hold_system.lock().unwrap() = true;
        run.devices.stale[Track::System.index()].store(true, Ordering::SeqCst);
        wait_until("the restart", || run.devices.starts().len() == 3);
        // Polls go on meanwhile (every 50 ms); none starts another.
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            run.devices.starts(),
            [Track::Mic, Track::System, Track::System]
        );
        run.devices.release_system();
        wait_until("the system capture", || run.devices.running(Track::System));
        run.play(&quiet(5.0), &[say(THEM, 3.0), quiet(2.0)].concat());
        let lines = lines(&run.stop());
        assert_eq!(texts(&lines), [(Speaker::Them, THEM)]);
        assert_eq!(run.devices.starts().len(), 3);
    }

    #[test]
    fn stop_while_system_start_pending() {
        let t = Instant::now();
        let mut run = Run::start("system-stop", Devices::held(), config());
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "start: {:?}",
            t.elapsed()
        );
        wait_until("the system start", || {
            run.devices.starts().contains(&Track::System)
        });
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &[]);
        let t = Instant::now();
        let events = run.stop();
        // The start may never return: Stopped doesn't wait for it.
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "stop: {:?}",
            t.elapsed()
        );
        assert_eq!(texts(&lines(&events)), [(Speaker::Me, ME)]);
        assert_eq!(run.devices.dropped(Track::System), 0);
        // When it finally returns, the meeting is over: its capture is dropped at once.
        run.devices.release_system();
        wait_until("the late capture dropped", || {
            run.devices.dropped(Track::System) == 1
        });
        assert!(!run.devices.running(Track::System));
    }

    #[test]
    fn stop_flushes_and_reports_stopped_last() {
        let mut run = Run::start("stop", Devices::default(), config());
        // Both still talking when it stops: neither cutter has cut anything yet.
        run.play(&say(ME, 3.0), &say(THEM, 3.0));
        let events = run.stop();
        assert!(
            matches!(events.first(), Some(Event::Started(_))),
            "{events:#?}"
        );
        assert!(matches!(events.last(), Some(Event::Stopped { id }) if *id == run.id));
        let stopped = events.iter().filter(|e| matches!(e, Event::Stopped { .. }));
        assert_eq!(stopped.count(), 1);
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Status {
                recording: false,
                ..
            }
        )));
        let lines = lines(&events);
        assert_eq!(
            texts(&lines),
            [(Speaker::Them, THEM), (Speaker::Me, ME)],
            "{events:#?}"
        );
        assert_eq!(run.store.lines(&run.id).unwrap(), lines);
        let m = run.store.load(&run.id).unwrap();
        assert!(m.ended_at.is_some_and(|end| end >= m.started_at), "{m:?}");
        let md = fs::read_to_string(run.store.dir(&run.id).join("meeting.md")).unwrap();
        assert!(md.contains(THEM) && md.contains(ME), "{md}");
    }

    #[test]
    fn saves_audio_when_asked() {
        let cfg = SessionConfig {
            save_audio: true,
            ..config()
        };
        let mut run = Run::start("audio", Devices::default(), cfg);
        let mic = [say(ME, 2.0), quiet(1.33)].concat();
        let system = [say(THEM, 1.5), quiet(1.17)].concat();
        run.play(&mic, &system);
        let events = run.stop();
        let Some(Event::Started(started)) = events.first() else {
            panic!("{events:#?}");
        };
        // system.wav is listed once the system audio runs: its start may take a while.
        assert_eq!(started.audio, ["mic.wav"]);
        let m = run.store.load(&run.id).unwrap();
        assert_eq!(m.audio, ["mic.wav", "system.wav"]);
        for (file, pushed) in [("mic.wav", &mic), ("system.wav", &system)] {
            let path = run.store.dir(&run.id).join(file);
            let back = agt_audio::wav::read_16k_mono(&path).unwrap();
            assert_eq!(back.len(), pushed.len(), "{file}");
            assert!(
                back.iter().zip(pushed).all(|(a, b)| (a - b).abs() < 1e-3),
                "{file}"
            );
        }
    }

    #[test]
    fn silent_gaps_are_padded() {
        let cfg = SessionConfig {
            save_audio: true,
            ..config()
        };
        let mut run = Run::start("gaps", Devices::default(), cfg);
        // The system track keeps up with the clock for a second, then gets nothing for 3 s,
        // like WASAPI loopback while nothing plays.
        run.devices.play(&[], &quiet(1.0), 1.0);
        thread::sleep(Duration::from_secs(3));
        let gap_end = run.session().elapsed_ms();
        run.play(&[], &[say(THEM, 3.0), quiet(2.0)].concat());
        let events = run.stop();
        let lines = lines(&events);
        assert_eq!(texts(&lines), [(Speaker::Them, THEM)], "{events:#?}");
        // Said about 4 s in (its first chunk ends when it arrives), not 1 s in.
        let start = lines[0].start_ms;
        assert!(
            start >= 3_700 && start + 300 >= gap_end && start <= gap_end + 300,
            "{start} ms, gap ended at {gap_end} ms"
        );
        // The gap is in the file too, as silence.
        let path = run.store.dir(&run.id).join("system.wav");
        let wav_ms = (agt_audio::wav::read_16k_mono(&path).unwrap().len() * 1000 / SR) as u64;
        assert!(near(wav_ms, gap_end + 5000), "{wav_ms} ms");
    }

    #[test]
    fn zero_system_audio_reports_silent() {
        let cfg = SessionConfig {
            silent_after: Duration::from_secs(2),
            ..config()
        };
        let mut run = Run::start("silent", Devices::default(), cfg);
        // macOS without the permission: the tap delivers zeros, not an error.
        run.play(&quiet(3.0), &quiet(3.0));
        run.wait("Silent", |e| {
            matches!(
                e,
                Event::Status {
                    system: SystemAudio::Silent,
                    ..
                }
            )
        });
        run.play(&quiet(1.0), &say(THEM, 1.0));
        run.wait("Ok again", |e| {
            matches!(
                e,
                Event::Status {
                    system: SystemAudio::Ok,
                    ..
                }
            )
        });
        run.stop();
    }

    fn is_system(e: &Event, want: &SystemAudio) -> bool {
        matches!(e, Event::Status { system, .. } if system == want)
    }

    #[test]
    fn starved_system_capture_reports_silent() {
        let cfg = SessionConfig {
            silent_after: Duration::from_millis(400),
            system_delivers_continuously: true,
            ..config()
        };
        let mut run = Run::start("starved", Devices::default(), cfg);
        // The macOS tap started fine, but its stream never delivers a buffer: the fake system
        // capture's sink is never called.
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &[]);
        run.wait("Silent", |e| is_system(e, &SystemAudio::Silent));
        // Buffers start coming: back to Ok.
        run.play(&[], &[say(THEM, 3.0), quiet(2.0)].concat());
        run.wait("Ok again", |e| is_system(e, &SystemAudio::Ok));
        let lines = lines(&run.stop());
        assert_eq!(
            texts(&lines),
            [(Speaker::Me, ME), (Speaker::Them, THEM)],
            "{lines:#?}"
        );
    }

    #[test]
    fn loopback_without_packets_is_not_silent() {
        // WASAPI loopback: nothing comes while nothing plays, and that's a quiet call, not a
        // broken capture.
        let cfg = SessionConfig {
            silent_after: Duration::from_millis(300),
            ..config()
        };
        let mut run = Run::start("loopback", Devices::default(), cfg);
        run.play(&[say(ME, 3.0), quiet(2.0)].concat(), &[]);
        thread::sleep(Duration::from_millis(900));
        let events = run.stop();
        assert!(
            !events.iter().any(|e| is_system(e, &SystemAudio::Silent)),
            "{events:#?}"
        );
    }

    #[test]
    fn repeated_mic_loss_is_reported_again() {
        let mut run = Run::start("mic-loss", Devices::default(), config());
        let lose_mic = |run: &mut Run| {
            *run.devices.no_mic.lock().unwrap() = Some("unplugged".into());
            run.devices.stale[Track::Mic.index()].store(true, Ordering::SeqCst);
            run.wait(
                "the mic error",
                |e| matches!(e, Event::Error { message, .. } if message.contains("unplugged")),
            );
        };
        lose_mic(&mut run);
        // Plugged back in: a poll starts it again.
        *run.devices.no_mic.lock().unwrap() = None;
        wait_until("the mic back", || run.devices.running(Track::Mic));
        // Lost again: the user hears of it again (the same message isn't held back).
        lose_mic(&mut run);
        run.stop();
    }

    #[test]
    fn a_session_can_be_shared() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Session>();
    }

    #[test]
    fn system_audio_serializes_as_a_word() {
        let json = |s: SystemAudio| serde_json::to_string(&s).unwrap();
        assert_eq!(json(SystemAudio::Ok), "\"ok\"");
        assert_eq!(
            json(SystemAudio::Unsupported("why".into())),
            "\"unsupported\""
        );
        assert_eq!(json(SystemAudio::Silent), "\"silent\"");
    }

    /// Minutes of the other side talking keep about a minute of it for the echo check.
    #[test]
    fn echo_history_stays_bounded() {
        let dir = TempDir::new("bounded");
        let store = Store::new(dir.0.clone());
        let meeting = store.create(unix_ms()).unwrap();
        let (tx, rx) = mpsc::channel();
        let deps = Deps {
            store,
            cfg: config(),
            captures: Arc::new(Fake(Arc::default())),
            transcriber: Arc::new(AtOnce),
            notify: Arc::new(|_| {}),
        };
        let t0 = Instant::now();
        let mut w = Worker::new(&meeting, t0, deps, tx);
        // Three minutes, in real time on the meeting clock: a 3 s line every 5 s.
        let talk = [say(THEM, 3.0), quiet(2.0)].concat();
        let mut fed = 0;
        for _ in 0..36 {
            for part in talk.chunks(SR / 10) {
                fed += part.len();
                let now = t0 + Duration::from_secs_f64(fed as f64 / SR as f64);
                let samples = part.to_vec();
                let msg = Msg::Audio {
                    track: Track::System,
                    generation: 0,
                    at: now,
                    samples,
                };
                w.handle(msg, now);
                while let Ok(msg) = rx.try_recv() {
                    w.handle(msg, now);
                }
                if fed % SR == 0 {
                    w.tick(now);
                }
            }
        }
        assert_eq!(w.lines, 36);
        // The last 60 s: a line every 5 s, 3 loud seconds in every 5.
        let lines = w.them_lines.len();
        assert!((11..=14).contains(&lines), "{lines} lines");
        let loud = w.them_levels.loud.len();
        assert!((350..=400).contains(&loud), "{loud} blocks");
    }
}
