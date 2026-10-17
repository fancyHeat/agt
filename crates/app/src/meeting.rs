//! Meeting notes in the app: owns the recording [`Session`], the meeting folders' [`Store`],
//! the AI notes jobs and the "Meeting detected" prompt, and tells the windows
//! ([`crate::meeting_ui`]) what to show. Everything here runs on the UI thread; the session,
//! the summary jobs, the start and the detection poll post back as [`Msg`].
//!
//! - **One meeting at a time**, from the tray, the pill, the notes window or the command line
//!   ([`crate::control`]). A start from the tray also opens the notes window on it; the others
//!   don't (an open notes window switches to it). [`Session::start`] opens both captures, which
//!   can take a moment, so it runs on a `meeting-start` thread.
//! - While a meeting records or finishes, dictation doesn't duck the volume
//!   ([`Pipeline::set_meeting_active`]), the meeting's mic track is silence while a dictation
//!   records ([`Pipeline::dictating`]), and AGT never restarts for new settings or an update
//!   ([`Meetings::busy`], which counts AI notes being written too). Quitting stops it and waits
//!   for its last lines, and gives AI notes a few seconds ([`Meetings::shutdown`]).
//! - **AI notes** run the user's Claude Code CLI on a `meeting-summary` thread
//!   ([`summary::run`]): when the user clicks Generate notes, and with `meeting.auto_summary`
//!   on, by themselves after a stop when the transcript has [`AUTO_SUMMARY_WORDS`]
//!   ([`auto_summary`]). The notes page always gets an answer, done or the reason why not.
//!   That is the only time text leaves the machine.
//! - **What goes wrong reaches the user**: a session's errors and a start that failed (or was
//!   refused: the speech model didn't load) go to the notes page as a one-line notice, and
//!   flash the pill ([`Meetings::notice`]). The last notice per meeting is kept and given to a
//!   notes window that opens later ([`Notices`]). A start that fails from the tray or the
//!   pill's Take notes opens the notes window, on the list, to say why. "Speech model still
//!   loading" goes away once it has loaded.
//! - **Meetings AGT never got to stop** (it was killed or crashed) are put right once at
//!   launch, on a `meeting-recover` thread ([`Store::recover`]).
//! - **Detection** (`meeting.detect`, macOS): a `meeting-detect` thread lists the apps using
//!   the microphone every [`DETECT_POLL`] while no meeting records; [`Detector`] decides when
//!   the pill offers to take notes.

use crate::{
    AppEvent,
    control::{Command, Reply},
    meeting_ui::{
        FromNotes, FromPill, Live, MeetingOut, PillState, PillView, ToNotes, Ui, UiEvent,
    },
    pipeline::{Pipeline, SpeechModel},
};
use agt_core::{
    Config,
    config::{MeetingConfig, MeetingSummary},
    paths,
    stt::SAMPLE_RATE,
};
use agt_meeting::{
    CaptureFactory, DeviceCaptures, Event, Line, Session, SessionConfig, Store, SystemAudio,
    summary,
};
use agt_stt::chunk::{CutConfig, PauseCut};
use std::{
    collections::HashMap,
    fmt, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use winit::{
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoopProxy},
    window::WindowId,
};

/// A summary run is stopped after this long.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(180);
/// Quitting waits this long for AI notes being written, then stops the CLI (nothing is left
/// running) and drops them.
const SUMMARY_QUIT_WAIT: Duration = Duration::from_secs(3);
/// With `meeting.auto_summary` on, the AI notes are written by themselves after a stop if the
/// transcript has this many words (less is a test or a false start, not worth a Claude call).
const AUTO_SUMMARY_WORDS: usize = 30;
/// How often the detection thread lists the apps using the microphone.
const DETECT_POLL: Duration = Duration::from_secs(2);
/// A meeting app using the mic this long without a break is a call: offer to take notes.
const SUSTAIN: Duration = Duration::from_secs(8);
/// The offer goes away by itself after this long.
const PROMPT_FOR: Duration = Duration::from_secs(20);
/// A dismissed (or ignored) app is not offered again until it has been off the mic this long:
/// not again during the same call, even if it lets go of the mic for a moment.
const COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// The system audio permission is asked again at most this often (an XPC call to TCC):
/// a status comes every second.
const PERMISSION_REFRESH: Duration = Duration::from_secs(10);

const AI_OFF: &str =
    "AI notes are off. Turn them on with summary = \"claude\" under [meeting] in config.toml.";
const MODEL_LOADING: &str = "Speech model still loading; transcription starts when it's ready";
const NO_CLAUDE: &str =
    "Claude Code isn't installed. Install it, or set meeting.claude_path in config.toml.";
const FAILED_LABEL: &str = "Meeting notes failed to start - see log";

/// How a meeting's tracks are cut into segments to transcribe. Not dictation's
/// [`CutConfig::live`]: its 8 s minimum segment keeps a short turn ("Yeah, sounds good.") in
/// the cutter until 8 s of speech pile up, so Them lines would show up late, and the Me lines
/// around them would wait the whole echo hold. Meetings trade a little accuracy (less context
/// per segment) for lines within a second or two of each pause: a cut at every 0.8 s pause
/// once a segment has 3 s, and at most 20 s.
fn meeting_cut() -> CutConfig {
    const MAX_SECS: f32 = 20.0;
    let secs = |s: f32| (s * SAMPLE_RATE as f32) as usize;
    CutConfig {
        max_len: secs(MAX_SECS),
        search_len: secs(agt_stt::CHUNK_SEARCH_SECS.min(MAX_SECS / 2.0)),
        pause: Some(PauseCut {
            len: secs(0.8),
            min_segment: secs(3.0),
            pad: secs(0.25),
            trim: true,
        }),
    }
}

/// Whether a start opens the notes window, by where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenNotes {
    /// The tray: on the meeting, or on the list saying why it didn't start.
    Always,
    /// The pill's Take notes: only to say why it didn't start (the pill goes away then, and
    /// its flash with it).
    OnFailure,
    /// The notes window (it's open) and the command line (its answer says why).
    Never,
}

impl OpenNotes {
    fn on_start(self) -> bool {
        self == OpenNotes::Always
    }

    fn on_failure(self) -> bool {
        self != OpenNotes::Never
    }
}

/// What other threads (and the pages) post about meetings, as [`AppEvent::Meeting`].
pub enum Msg {
    /// From the recording session's thread.
    Session(Event),
    /// [`Session::start`] returned, on the `meeting-start` thread.
    Started(Result<Session, String>),
    /// AI notes for `id`: `(markdown, by)` (already saved), or the message to show.
    Summary {
        id: String,
        result: Result<(String, String), String>,
    },
    /// The meeting apps using the microphone now (detection).
    MicUsers(Vec<String>),
    /// These meetings, which AGT never got to stop, were put right at launch.
    Recovered(Vec<String>),
    /// `agt meeting <command>`.
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "the command line's socket is unix only")
    )]
    Control(Command, Reply),
    Ui(UiEvent),
}

/// Leaves out what was said: a debug log of an event must not hold the transcript.
impl fmt::Debug for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Msg::Session(Event::Line { id, line }) => write!(f, "Line({id}, {})", line.id),
            Msg::Session(e) => write!(f, "Session({:?})", std::mem::discriminant(e)),
            Msg::Started(Ok(s)) => write!(f, "Started({})", s.id()),
            Msg::Started(Err(e)) => write!(f, "Started(Err({e}))"),
            Msg::Summary { id, result } => write!(f, "Summary({id}, ok: {})", result.is_ok()),
            Msg::MicUsers(apps) => write!(f, "MicUsers({apps:?})"),
            Msg::Recovered(ids) => write!(f, "Recovered({ids:?})"),
            Msg::Control(c, _) => write!(f, "Control({c:?})"),
            Msg::Ui(UiEvent::Notes(m)) => write!(f, "Notes({:?})", std::mem::discriminant(m)),
            Msg::Ui(e) => write!(f, "Ui({e:?})"),
        }
    }
}

enum Phase {
    Idle,
    /// [`Session::start`] is running; `open` says whether the notes window opens after.
    Starting {
        open: OpenNotes,
    },
    Recording(Session),
    /// Stopped; the last segments are being transcribed until [`Event::Stopped`].
    Finishing(String),
}

pub struct Meetings {
    cfg: MeetingConfig,
    /// `audio.device`.
    mic_device: String,
    store: Store,
    /// The tracks' levels, shared by every session's captures.
    captures: DeviceCaptures,
    proxy: EventLoopProxy<AppEvent>,
    phase: Phase,
    /// The current meeting's latest state, for the notes window.
    live: Option<Live>,
    /// The tracks are being metered (recording, not paused).
    metering: bool,
    /// `agt meeting start` / `stop` waiting for the meeting to start / finish.
    start_waiters: Vec<Reply>,
    stop_waiters: Vec<Reply>,
    /// Meetings whose AI notes are being written.
    summaries: HashMap<String, SummaryJob>,
    detector: Detector,
    /// The app the "Meeting detected" prompt is about, while it shows.
    prompt: Option<String>,
    /// Not idle: the detection thread doesn't poll.
    busy: Arc<AtomicBool>,
    /// [`Event::Stopped`]'s id, also sent here by the session itself: after the event loop
    /// has exited, this is how [`Self::shutdown`] hears of it.
    stopped: Receiver<String>,
    stopped_tx: Sender<String>,
    /// The last start failed (shown on the tray item until the next one).
    start_failed: bool,
    /// The last notice per meeting, for a notes window that opens later.
    notices: Notices,
    /// The meeting whose notice says the speech model is still loading, until it has.
    loading_notice: Option<String>,
    /// Quitting: nothing new starts.
    exiting: bool,
    permission: PermissionCache,
    ui: Ui,
}

impl Meetings {
    /// With `config.meeting` (and `config.audio.device` for the mic). Starts the detection
    /// thread if `meeting.detect` is on (macOS).
    pub fn new(config: &Config, proxy: EventLoopProxy<AppEvent>) -> Meetings {
        let busy = Arc::new(AtomicBool::new(false));
        if config.meeting.detect && cfg!(target_os = "macos") {
            spawn_detection(proxy.clone(), busy.clone());
        }
        let store = Store::new(paths::history_dir().join("meetings"));
        // Nothing can have started yet: every meeting without an end is one AGT never stopped.
        spawn_recovery(store.clone(), unix_ms(), proxy.clone());
        let (stopped_tx, stopped) = mpsc::channel();
        Meetings {
            cfg: config.meeting.clone(),
            mic_device: config.audio.device.clone(),
            store,
            captures: DeviceCaptures::default(),
            ui: Ui::new(proxy.clone()),
            proxy,
            phase: Phase::Idle,
            live: None,
            metering: false,
            start_waiters: Vec::new(),
            stop_waiters: Vec::new(),
            summaries: HashMap::new(),
            detector: Detector::default(),
            prompt: None,
            busy,
            stopped,
            stopped_tx,
            start_failed: false,
            notices: Notices::default(),
            loading_notice: None,
            exiting: false,
            permission: PermissionCache::default(),
        }
    }

    /// A meeting is starting, recording or finishing, or AI notes are being written: AGT must
    /// not restart (the app counts a change of this as a status change, so a waiting restart
    /// goes ahead once both are done).
    pub fn busy(&self) -> bool {
        self.in_meeting() || !self.summaries.is_empty()
    }

    fn in_meeting(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// The tray item's label, and whether it can be clicked.
    pub fn tray_label(&self) -> (&'static str, bool) {
        match self.phase {
            Phase::Idle if self.start_failed => (FAILED_LABEL, true),
            Phase::Idle => ("Start meeting notes", true),
            Phase::Starting { .. } => ("Starting meeting notes...", false),
            Phase::Recording(_) => ("Stop meeting notes", true),
            Phase::Finishing(_) => ("Finishing meeting notes...", false),
        }
    }

    /// The tray's start/stop item.
    pub fn toggle(&mut self, event_loop: &ActiveEventLoop, pipeline: Option<&Pipeline>) {
        match self.phase {
            Phase::Idle => self.start(event_loop, pipeline, OpenNotes::Always, None),
            Phase::Recording(_) => self.stop(None),
            Phase::Starting { .. } | Phase::Finishing(_) => {}
        }
    }

    /// "Meeting notes...", the pill's Notes: the notes window, on the live meeting if one
    /// records, else on the list. A window opened for the first time also gets the notices
    /// sent while there was none.
    pub fn open_window(&mut self, event_loop: &ActiveEventLoop) {
        let new = !self.ui.notes_exists();
        if !self.ui.show_notes(event_loop) {
            return;
        }
        self.send_list();
        if let Some(id) = self.current_id().map(str::to_owned) {
            self.send_meeting(&id);
        }
        if new {
            self.send_notices();
        }
    }

    /// The speech model's state changed (or may have): a meeting waiting for it hears so.
    pub fn speech_model(&mut self, model: &SpeechModel) {
        let Some(id) = self.loading_notice.clone() else {
            return;
        };
        match model {
            SpeechModel::Loading => {}
            SpeechModel::Ready => self.model_ready(&id),
            SpeechModel::Failed(why) => {
                // Replaces the loading notice: transcription won't start after all.
                self.loading_notice = None;
                self.notice(Some(&id), format!("Speech model failed to load: {why}"));
            }
        }
    }

    /// Takes back meeting `id`'s "still loading" notice, if it's still the one showing.
    fn model_ready(&mut self, id: &str) {
        if self.loading_notice.as_deref() == Some(id) {
            self.loading_notice = None;
            self.clear_notice(Some(id), MODEL_LOADING);
        }
    }

    pub fn handle(&mut self, msg: Msg, event_loop: &ActiveEventLoop, pipeline: Option<&Pipeline>) {
        match msg {
            Msg::Session(e) => self.session_event(e, pipeline),
            Msg::Started(result) => self.started(result, event_loop, pipeline),
            Msg::Summary { id, result } => {
                self.summaries.remove(&id);
                self.ui.send_notes(&ToNotes::summary(&id, result));
            }
            Msg::MicUsers(using) => self.mic_users(event_loop, &using),
            Msg::Recovered(ids) => {
                // Their end times and files changed: an open list shows them as they are now.
                if !ids.is_empty() && self.ui.notes_exists() {
                    self.send_list();
                }
            }
            Msg::Control(Command::Start, reply) => {
                self.start(event_loop, pipeline, OpenNotes::Never, Some(reply))
            }
            Msg::Control(Command::Stop, reply) => self.stop(Some(reply)),
            Msg::Control(Command::Status, reply) => {
                let _ = reply.send(self.status_line());
            }
            Msg::Ui(UiEvent::Notes(m)) => self.notes_message(m, event_loop, pipeline),
            Msg::Ui(UiEvent::Pill(m)) => self.pill_message(m, event_loop, pipeline),
            Msg::Ui(UiEvent::PillLoaded) => self.ui.pill_loaded(),
        }
    }

    /// Returns whether the event was for one of the meeting windows.
    pub fn window_event(&mut self, id: WindowId, event: &WindowEvent) -> bool {
        self.ui.window_event(id, event)
    }

    pub fn next_tick(&self) -> Option<Instant> {
        self.ui.next_tick()
    }

    pub fn tick(&mut self, now: Instant) {
        self.ui.tick(now)
    }

    // --------------------------------------------------------------- start and stop

    fn current_id(&self) -> Option<&str> {
        match &self.phase {
            Phase::Recording(s) => Some(s.id()),
            Phase::Finishing(id) => Some(id),
            Phase::Idle | Phase::Starting { .. } => None,
        }
    }

    fn start(
        &mut self,
        event_loop: &ActiveEventLoop,
        pipeline: Option<&Pipeline>,
        open: OpenNotes,
        reply: Option<Reply>,
    ) {
        let answer = |reply: Option<Reply>, text: String| {
            if let Some(r) = reply {
                let _ = r.send(text);
            }
        };
        match &self.phase {
            Phase::Recording(s) => {
                let id = s.id().to_owned();
                if open.on_start() {
                    self.open_window(event_loop);
                }
                return answer(reply, format!("already recording {id}"));
            }
            Phase::Starting { .. } => {
                self.start_waiters.extend(reply);
                return;
            }
            Phase::Finishing(id) => {
                let text = format!("error: still finishing {id}; try again in a moment");
                return answer(reply, text);
            }
            Phase::Idle => {}
        }
        if self.exiting {
            return answer(reply, "error: AGT is quitting".into());
        }
        let Some(pipeline) = pipeline else {
            tracing::error!("cannot start meeting notes: dictation (its transcription) is off");
            let why = "AGT can't transcribe right now (see the tray and the log)".to_owned();
            self.fail_start(event_loop, open, &why);
            return answer(reply, format!("error: {why}"));
        };
        if let Err(why) = start_check(&pipeline.speech_model()) {
            tracing::error!("not starting meeting notes: {why}");
            self.fail_start(event_loop, open, &why);
            return answer(reply, format!("error: {why}"));
        }
        let mut cfg = SessionConfig::new(meeting_cut(), pipeline.dictating());
        cfg.save_audio = self.cfg.save_audio;
        // The macOS tap delivers silence too; WASAPI loopback nothing while nothing plays.
        cfg.system_delivers_continuously = cfg!(target_os = "macos");
        let captures: Arc<dyn CaptureFactory> = Arc::new(DeviceCaptures {
            mic_device: self.mic_device.clone(),
            ..self.captures.clone()
        });
        let transcriber = pipeline.transcriber();
        let (proxy, stopped) = (self.proxy.clone(), self.stopped_tx.clone());
        let notify: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |e: Event| {
            if let Event::Stopped { id } = &e {
                let _ = stopped.send(id.clone());
            }
            let _ = proxy.send_event(AppEvent::Meeting(Msg::Session(e)));
        });
        let (store, proxy) = (self.store.clone(), self.proxy.clone());
        let spawned = thread::Builder::new()
            .name("meeting-start".into())
            .spawn(move || {
                let result = Session::start(store, cfg, captures, transcriber, notify);
                // If the event loop is gone, the session is dropped here, which stops it.
                let _ = proxy.send_event(AppEvent::Meeting(Msg::Started(result)));
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start meeting notes: {e}");
            let why = format!("Meeting notes didn't start: {e}");
            self.fail_start(event_loop, open, &why);
            return answer(reply, format!("error: {e}"));
        }
        pipeline.set_meeting_active(true);
        self.phase = Phase::Starting { open };
        self.start_waiters.extend(reply);
        self.busy.store(true, Ordering::Relaxed);
        self.detector.meeting_started();
    }

    fn started(
        &mut self,
        result: Result<Session, String>,
        event_loop: &ActiveEventLoop,
        pipeline: Option<&Pipeline>,
    ) {
        let open = match self.phase {
            Phase::Starting { open } => open,
            _ => OpenNotes::Never,
        };
        let session = match result {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("meeting notes didn't start: {e}");
                self.phase = Phase::Idle;
                self.busy.store(false, Ordering::Relaxed);
                if let Some(p) = pipeline {
                    p.set_meeting_active(false);
                }
                for r in self.start_waiters.drain(..) {
                    let _ = r.send(format!("error: {e}"));
                }
                // `e` says what: "Couldn't start the microphone: ...".
                return self.fail_start(event_loop, open, &e);
            }
        };
        let id = session.id().to_owned();
        self.start_failed = false;
        self.prompt = None;
        // A failed start's notice is old news (the page drops it too).
        self.notices.remove(None);
        // Until the session's first status, a second from now.
        let permission = self.permission();
        self.live = Some(Live::new(
            true,
            false,
            session.elapsed_ms(),
            &SystemAudio::Ok,
            permission,
        ));
        let elapsed_ms = session.elapsed_ms();
        self.phase = Phase::Recording(session);
        for r in self.start_waiters.drain(..) {
            let _ = r.send(format!("started {id}"));
        }
        if self.exiting {
            return self.stop(None);
        }
        self.set_metering(true);
        self.ui.set_pill(
            event_loop,
            Some(PillView {
                state: PillState::Recording,
                elapsed_ms,
                app: None,
            }),
        );
        if open.on_start() {
            self.open_window(event_loop);
        } else if self.ui.notes_exists() {
            self.send_meeting(&id);
        }
        // Its segments wait for the model meanwhile; the notice goes once it's ready.
        if pipeline.is_some_and(|p| p.speech_model() == SpeechModel::Loading) {
            self.notice(Some(&id), MODEL_LOADING.into());
            self.loading_notice = Some(id);
        }
    }

    /// A start that failed (or was refused), `why` said plainly: on the tray item, and as a
    /// notice in the notes window, opened for it (on the list) if the start came from the tray
    /// or the pill. A "Meeting detected" prompt goes away, and isn't offered again for the
    /// same call.
    fn fail_start(&mut self, event_loop: &ActiveEventLoop, open: OpenNotes, why: &str) {
        self.start_failed = true;
        if self.prompt.take().is_some() {
            self.detector.dismiss();
            self.ui.hide_pill();
        }
        // First, so a window opened just now gets it with the rest (see `open_window`).
        self.notice(None, why.into());
        if open.on_failure() {
            self.open_window(event_loop);
        }
    }

    /// Tells the user `text` (something went wrong, or is worth knowing): a line in the notes
    /// window (`id`: the meeting it's about), and a flash of the pill if it shows. Kept as the
    /// last notice about `id`, for a notes window that opens later.
    fn notice(&mut self, id: Option<&str>, text: String) {
        self.ui.flash_pill(&text);
        self.notices.set(id, &text);
        self.ui.send_notes(&ToNotes::Notice {
            id: id.map(str::to_owned),
            text,
        });
    }

    /// Takes back the notice `text` about `id`, if it's still the last one.
    fn clear_notice(&mut self, id: Option<&str>, text: &str) {
        if self.notices.clear(id, text) {
            self.ui.send_notes(&ToNotes::ClearNotice {
                id: id.map(str::to_owned),
                text: text.to_owned(),
            });
        }
    }

    /// Every kept notice, to a notes page that has none of them (new, or reloaded).
    fn send_notices(&mut self) {
        for msg in self.notices.messages() {
            self.ui.send_notes(&msg);
        }
    }

    /// [`agt_platform::system_audio_permission`], asked again at most every
    /// [`PERMISSION_REFRESH`].
    fn permission(&mut self) -> Option<bool> {
        self.permission
            .get(Instant::now(), agt_platform::system_audio_permission)
    }

    fn stop(&mut self, reply: Option<Reply>) {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Recording(session) => {
                let id = session.id().to_owned();
                session.stop();
                self.phase = Phase::Finishing(id.clone());
                self.stop_waiters.extend(reply);
                self.set_metering(false);
                if let Some(live) = &mut self.live {
                    live.recording = false;
                }
                self.show_live(&id);
            }
            Phase::Finishing(id) => {
                self.phase = Phase::Finishing(id);
                self.stop_waiters.extend(reply);
            }
            phase @ Phase::Starting { .. } => {
                self.phase = phase;
                if let Some(r) = reply {
                    let _ = r.send("error: still starting; try again in a moment".into());
                }
            }
            Phase::Idle => {
                if let Some(r) = reply {
                    let _ = r.send("not recording".into());
                }
            }
        }
    }

    fn status_line(&self) -> String {
        match &self.phase {
            Phase::Idle => "idle".into(),
            Phase::Starting { .. } => "starting".into(),
            Phase::Recording(s) => format!("recording {} {}", s.id(), s.elapsed_ms() / 1000),
            Phase::Finishing(id) => format!("finishing {id}"),
        }
    }

    fn set_metering(&mut self, on: bool) {
        if self.metering != on {
            self.metering = on;
            self.ui.set_meters(on.then(|| {
                (
                    self.captures.mic_level.clone(),
                    self.captures.system_level.clone(),
                )
            }));
        }
    }

    /// The pill and the notes window, from [`Self::live`].
    fn show_live(&mut self, id: &str) {
        let Some(live) = self.live.clone() else {
            return;
        };
        let state = if !live.recording || matches!(self.phase, Phase::Finishing(_)) {
            PillState::Finishing
        } else if live.paused {
            PillState::Paused
        } else {
            PillState::Recording
        };
        self.ui.update_pill(PillView {
            state,
            elapsed_ms: live.elapsed_ms,
            app: None,
        });
        self.ui.send_notes(&ToNotes::Live {
            id: id.into(),
            live: Some(live),
        });
    }

    fn session_event(&mut self, e: Event, pipeline: Option<&Pipeline>) {
        match e {
            Event::Started(m) => tracing::debug!("meeting {} recording", m.id),
            Event::Line { id, line } => {
                // Something was transcribed: the model has loaded.
                self.model_ready(&id);
                self.ui.send_notes(&ToNotes::Line { id, line });
            }
            Event::Status {
                id,
                recording,
                paused,
                elapsed_ms,
                system,
            } => {
                if self.current_id() != Some(id.as_str()) {
                    return;
                }
                // In case the app heard nothing of it (its status line didn't change).
                if self.loading_notice.is_some()
                    && let Some(p) = pipeline
                {
                    self.speech_model(&p.speech_model());
                }
                let permission = self.permission();
                self.live = Some(Live::new(
                    recording, paused, elapsed_ms, &system, permission,
                ));
                let recording = matches!(self.phase, Phase::Recording(_));
                self.set_metering(recording && !paused);
                self.show_live(&id);
            }
            Event::Echo { id, dropped } => {
                tracing::debug!("meeting {id}: {dropped} lines dropped as echo so far")
            }
            Event::Error { id, message } => {
                tracing::warn!("meeting {id}: {message}");
                self.notice(Some(&id), message);
            }
            Event::Stopped { id } => self.stopped(id, pipeline),
        }
    }

    fn stopped(&mut self, id: String, pipeline: Option<&Pipeline>) {
        // Only the quit path reads these; keep the channel empty meanwhile.
        while self.stopped.try_recv().is_ok() {}
        tracing::info!("meeting {id} stopped");
        if self.current_id() == Some(id.as_str()) {
            self.phase = Phase::Idle;
            self.live = None;
            self.busy.store(false, Ordering::Relaxed);
            self.set_metering(false);
            self.ui.hide_pill();
            if let Some(p) = pipeline {
                p.set_meeting_active(false);
            }
            for r in self.stop_waiters.drain(..) {
                let _ = r.send(format!("stopped {id}"));
            }
        }
        self.ui.send_notes(&ToNotes::Live {
            id: id.clone(),
            live: None,
        });
        let count = || match self.store.lines(&id) {
            Ok(lines) => Some(words(&lines)),
            Err(e) => {
                tracing::warn!("cannot read meeting {id}'s transcript: {e}");
                None
            }
        };
        // Off (the default), the page shows Generate notes: nothing is sent without a click.
        if auto_summary(&self.cfg, self.exiting, count) {
            self.summarize(id);
        }
    }

    // --------------------------------------------------------------- AI notes

    /// Writes the AI notes for `id` on a worker thread; the notes page hears "running" now
    /// and the result later (or why not right away).
    fn summarize(&mut self, id: String) {
        if self.cfg.summary == MeetingSummary::Off {
            return self
                .ui
                .send_notes(&ToNotes::summary(&id, Err(AI_OFF.into())));
        }
        self.ui.send_notes(&ToNotes::running(&id));
        if self.summaries.contains_key(&id) || self.exiting {
            return; // already being written (or quitting: never started)
        }
        let (store, proxy) = (self.store.clone(), self.proxy.clone());
        let (claude_path, model) = (self.cfg.claude_path.clone(), self.cfg.claude_model.clone());
        let cancel = summary::Cancel::default();
        let (job, stop) = (id.clone(), cancel.clone());
        let spawned = thread::Builder::new()
            .name("meeting-summary".into())
            .spawn(move || {
                let result = write_summary(&store, &claude_path, &model, &job, &stop);
                let msg = Msg::Summary { id: job, result };
                let _ = proxy.send_event(AppEvent::Meeting(msg));
            });
        match spawned {
            Ok(thread) => {
                self.summaries.insert(id, SummaryJob { cancel, thread });
            }
            Err(e) => {
                let error = format!("Couldn't start writing the notes: {e}");
                self.ui.send_notes(&ToNotes::summary(&id, Err(error)));
            }
        }
    }

    // --------------------------------------------------------------- the notes window

    fn notes_message(&mut self, m: FromNotes, event_loop: &ActiveEventLoop, p: Option<&Pipeline>) {
        match m {
            FromNotes::Ready => {
                // A page that loaded with nothing queued (it reloaded) starts over.
                if !self.ui.notes_ready() {
                    self.send_list();
                    if let Some(id) = self.current_id().map(str::to_owned) {
                        self.send_meeting(&id);
                    }
                    self.send_notices();
                }
            }
            FromNotes::Start => self.start(event_loop, p, OpenNotes::Never, None),
            FromNotes::Stop => self.stop(None),
            FromNotes::Pause => self.pause(true),
            FromNotes::Resume => self.pause(false),
            FromNotes::Notes { id, notes } => self.save(&id, |m| m.notes = notes),
            FromNotes::Title { id, title } => {
                let title = title.trim().to_owned();
                if !title.is_empty() {
                    self.save(&id, |m| m.title = title);
                }
            }
            FromNotes::Summarize { id } => {
                if self.store.load(&id).is_ok() {
                    self.summarize(id);
                } else {
                    let error = "This meeting's folder is gone.".to_owned();
                    self.ui.send_notes(&ToNotes::summary(&id, Err(error)));
                }
            }
            FromNotes::Copy { text, id } => self.copy(text, id),
            FromNotes::OpenFolder { id } => {
                if self.store.load(&id).is_ok() {
                    crate::open_folder(&self.store.dir(&id));
                }
            }
            FromNotes::List => self.send_list(),
            FromNotes::Open { id } => self.send_meeting(&id),
            FromNotes::OpenSettings => agt_platform::open_system_audio_settings(),
        }
    }

    fn pause(&mut self, pause: bool) {
        // The session reports the change at once (a status), which updates both windows.
        if let Phase::Recording(s) = &self.phase {
            if pause { s.pause() } else { s.resume() }
        }
    }

    /// Changes `meeting.json` (ids the store doesn't know are ignored), and `meeting.md` if
    /// the meeting isn't recording (the session writes it when it stops).
    fn save(&mut self, id: &str, f: impl FnOnce(&mut agt_meeting::Meeting)) {
        match self.store.update(id, f) {
            Ok(_) => {
                if self.current_id() != Some(id)
                    && let Err(e) = self.store.write_markdown(id)
                {
                    tracing::warn!("cannot write meeting {id}'s meeting.md: {e}");
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                tracing::debug!("ignoring an edit of unknown meeting {id:?}");
            }
            Err(e) => tracing::error!("cannot save meeting {id}: {e}"),
        }
    }

    fn copy(&self, text: Option<String>, id: Option<String>) {
        let text = match (text, id) {
            (Some(text), _) => text,
            (None, Some(id)) => match (self.store.load(&id), self.store.lines(&id)) {
                (Ok(m), Ok(lines)) => agt_meeting::markdown(&m, &lines),
                (Err(e), _) | (_, Err(e)) => {
                    return tracing::warn!("cannot copy meeting {id}: {e}");
                }
            },
            (None, None) => return,
        };
        // A copy to keep, like any app's Copy: not marked transient like a dictation's paste.
        if let Err(e) = agt_platform::copy_text(&text) {
            tracing::error!("cannot copy the meeting notes: {e}");
        }
    }

    /// The meetings list, and the live meeting's state (the list shows it).
    fn send_list(&mut self) {
        self.ui.send_notes(&ToNotes::Meetings {
            list: self.store.list(),
        });
        if let (Some(id), Some(live)) = (self.current_id(), self.live.clone()) {
            let msg = ToNotes::Live {
                id: id.into(),
                live: Some(live),
            };
            self.ui.send_notes(&msg);
        }
    }

    /// Shows meeting `id` in the notes window, live if it's the one recording.
    fn send_meeting(&mut self, id: &str) {
        let m = match self.store.load(id) {
            Ok(m) => m,
            Err(e) => return tracing::warn!("cannot open meeting {id:?}: {e}"),
        };
        let lines = self.store.lines(id).unwrap_or_else(|e| {
            tracing::warn!("cannot read meeting {id}'s transcript: {e}");
            Vec::new()
        });
        let live = if self.current_id() == Some(id) {
            self.live.clone()
        } else {
            None
        };
        self.ui.send_notes(&ToNotes::Meeting {
            meeting: MeetingOut::new(m, lines),
            live,
        });
        if self.summaries.contains_key(id) {
            self.ui.send_notes(&ToNotes::running(id));
        }
    }

    // --------------------------------------------------------------- the pill

    fn pill_message(&mut self, m: FromPill, event_loop: &ActiveEventLoop, p: Option<&Pipeline>) {
        match m {
            FromPill::OpenNotes => self.open_window(event_loop),
            FromPill::Stop => self.stop(None),
            FromPill::Pause => self.pause(true),
            FromPill::Resume => self.pause(false),
            FromPill::Start => self.start(event_loop, p, OpenNotes::OnFailure, None),
            FromPill::Dismiss => {
                self.detector.dismiss();
                if self.prompt.take().is_some() {
                    self.ui.hide_pill();
                }
            }
            FromPill::Hover { on } => self.ui.pill_hover(on),
            FromPill::Drag => self.ui.pill_drag(),
        }
    }

    fn mic_users(&mut self, event_loop: &ActiveEventLoop, using: &[String]) {
        if self.in_meeting() || self.exiting {
            return;
        }
        match self.detector.observe(Instant::now(), using) {
            Some(Decision::Show(app)) => {
                tracing::info!("{app} is using the microphone: offering to take notes");
                self.prompt = Some(app.clone());
                let view = PillView {
                    state: PillState::Prompt,
                    elapsed_ms: 0,
                    app: Some(app),
                };
                self.ui.set_pill(event_loop, Some(view));
            }
            Some(Decision::Hide) if self.prompt.take().is_some() => self.ui.hide_pill(),
            Some(Decision::Hide) | None => {}
        }
    }

    // --------------------------------------------------------------- quitting

    /// Before the event loop exits: stops a recording meeting (it finishes on its own thread)
    /// and hides the windows. Returns whether the notes page may still send an edit, so the
    /// loop should run a moment longer.
    pub fn prepare_exit(&mut self) -> bool {
        self.exiting = true;
        if let Phase::Recording(_) = self.phase {
            self.stop(None);
        }
        self.ui.prepare_exit()
    }

    /// Once the event loop is exiting (nothing posted to it is handled any more): stops a
    /// meeting still recording and waits up to `wait` for its last lines and files (if they
    /// don't come, the meeting is marked ended now and its `meeting.md` written from what's
    /// there), and gives AI notes being written [`SUMMARY_QUIT_WAIT`] from now to finish before
    /// their CLI is stopped.
    pub fn shutdown(&mut self, wait: Duration) {
        let summaries_until = Instant::now() + SUMMARY_QUIT_WAIT;
        self.finish_meeting(wait);
        self.finish_summaries(summaries_until);
    }

    /// AI notes still being written: done by `until`, or stopped (the CLI and everything it
    /// started killed) and lost.
    fn finish_summaries(&mut self, until: Instant) {
        const POLL: Duration = Duration::from_millis(25);
        if self.summaries.is_empty() {
            return;
        }
        tracing::info!("waiting for AI notes being written");
        let running = |s: &HashMap<String, SummaryJob>| s.values().any(|j| !j.thread.is_finished());
        while running(&self.summaries) && Instant::now() < until {
            thread::sleep(POLL);
        }
        for (id, job) in &self.summaries {
            if !job.thread.is_finished() {
                tracing::warn!("AI notes for meeting {id} not done at quit; stopping claude");
                job.cancel.cancel();
            }
        }
        // The CLI is killed within a poll of the cancel; a second is plenty.
        let until = Instant::now() + Duration::from_secs(1);
        while running(&self.summaries) && Instant::now() < until {
            thread::sleep(POLL);
        }
        self.summaries.clear();
    }

    fn finish_meeting(&mut self, wait: Duration) {
        let id = match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Recording(session) => {
                let id = session.id().to_owned();
                session.stop();
                id
            }
            Phase::Finishing(id) => id,
            // A session still starting is dropped with the event it arrives in, which stops
            // it; there's no meeting to wait for yet.
            Phase::Idle | Phase::Starting { .. } => return,
        };
        tracing::info!("waiting up to {wait:?} for meeting {id} to finish");
        let deadline = Instant::now() + wait;
        loop {
            match self
                .stopped
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(stopped) if stopped == id => return,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        tracing::warn!("meeting {id} didn't finish in time; saving it as it is");
        let now = unix_ms();
        if let Err(e) = self.store.update(&id, |m| {
            m.ended_at.get_or_insert(now);
        }) {
            tracing::error!("cannot save meeting {id}: {e}");
        }
        if let Err(e) = self.store.write_markdown(&id) {
            tracing::error!("cannot write meeting {id}'s meeting.md: {e}");
        }
    }
}

/// AI notes being written on a `meeting-summary` thread.
struct SummaryJob {
    /// Stops the CLI (on quit).
    cancel: summary::Cancel,
    thread: thread::JoinHandle<()>,
}

/// [`Msg::Summary`]'s result for meeting `id`, saved in `meeting.json` and `meeting.md`.
fn write_summary(
    store: &Store,
    claude_path: &str,
    model: &str,
    id: &str,
    cancel: &summary::Cancel,
) -> Result<(String, String), String> {
    let claude = summary::find_claude(claude_path).ok_or(NO_CLAUDE)?;
    let m = store
        .load(id)
        .map_err(|e| format!("Couldn't read this meeting: {e}"))?;
    let lines = store
        .lines(id)
        .map_err(|e| format!("Couldn't read the transcript: {e}"))?;
    let started = Instant::now();
    let dir = store.dir(id);
    let markdown =
        summary::run_cancellable(&claude, model, &m, &lines, &dir, SUMMARY_TIMEOUT, cancel)
            .map_err(|raw| {
                // Not `raw`: the CLI's message can quote the meeting.
                let kind = summary::failure_kind(&raw);
                tracing::warn!("AI notes for meeting {id} failed: {kind}");
                summary::friendly_error(&raw)
            })?;
    tracing::info!(
        "AI notes for meeting {id} written in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    let by = match model.trim() {
        "" => "Claude".to_owned(),
        model => format!("Claude ({model})"),
    };
    let (md, author) = (markdown.clone(), by.clone());
    store
        .update(id, move |m| {
            m.summary = Some(md);
            m.summary_by = Some(author);
        })
        .map_err(|e| format!("Couldn't save the notes: {e}"))?;
    if let Err(e) = store.write_markdown(id) {
        tracing::warn!("cannot write meeting {id}'s meeting.md: {e}");
    }
    Ok((markdown, by))
}

/// Whether a meeting that just stopped gets its AI notes by itself: only with
/// `meeting.auto_summary` on (otherwise its transcript and the user's notes stay on the machine
/// until Generate notes is clicked), AI notes on, not while quitting, and with a transcript of
/// at least [`AUTO_SUMMARY_WORDS`] (`words`, read only when everything else says yes; `None`
/// if it can't be read).
fn auto_summary(cfg: &MeetingConfig, exiting: bool, words: impl FnOnce() -> Option<usize>) -> bool {
    cfg.auto_summary
        && cfg.summary == MeetingSummary::Claude
        && !exiting
        && words().is_some_and(|n| n >= AUTO_SUMMARY_WORDS)
}

/// Whether a meeting can start with the speech model as it is: not if it failed to load
/// (nothing would be transcribed; the error says so). While it loads it can: the segments
/// wait for it ([`MODEL_LOADING`] says so once it started).
fn start_check(model: &SpeechModel) -> Result<(), String> {
    match model {
        SpeechModel::Failed(why) => Err(format!("Speech model failed to load: {why}")),
        SpeechModel::Loading | SpeechModel::Ready => Ok(()),
    }
}

/// The last notice about each meeting (and about none: a start that failed), in the order
/// they came, for a notes page that wasn't there when they were sent: the window opens later
/// (a failed start from the command line, a lost microphone while it was never opened), or the
/// page reloaded. A page that was there keeps them itself, dismissals included. One per
/// meeting, a short line each: a long-running AGT keeps a few.
#[derive(Debug, Default)]
struct Notices(Vec<(Option<String>, String)>);

impl Notices {
    /// `text` is the last notice about `id` now.
    fn set(&mut self, id: Option<&str>, text: &str) {
        self.remove(id);
        self.0.push((id.map(str::to_owned), text.to_owned()));
    }

    /// Forgets the notice about `id`, whatever it says.
    fn remove(&mut self, id: Option<&str>) {
        self.0.retain(|(about, _)| about.as_deref() != id);
    }

    /// Forgets the notice about `id` if it still says `text`. Returns whether it did.
    fn clear(&mut self, id: Option<&str>, text: &str) -> bool {
        let before = self.0.len();
        self.0
            .retain(|(about, said)| !(about.as_deref() == id && said == text));
        self.0.len() < before
    }

    /// Them as messages to the notes page, oldest first (the page shows the newest).
    fn messages(&self) -> Vec<ToNotes> {
        self.0
            .iter()
            .map(|(id, text)| ToNotes::Notice {
                id: id.clone(),
                text: text.clone(),
            })
            .collect()
    }
}

/// A value asked for at most every [`PERMISSION_REFRESH`].
#[derive(Debug, Default)]
struct PermissionCache {
    /// When it was last asked, and its answer.
    last: Option<(Instant, Option<bool>)>,
}

impl PermissionCache {
    /// The answer from `ask`, asked again only if the last one is [`PERMISSION_REFRESH`] old.
    fn get(&mut self, now: Instant, ask: impl FnOnce() -> Option<bool>) -> Option<bool> {
        match self.last {
            Some((at, value)) if now.saturating_duration_since(at) < PERMISSION_REFRESH => value,
            _ => {
                let value = ask();
                self.last = Some((now, value));
                value
            }
        }
    }
}

/// Words in the transcript.
fn words(lines: &[Line]) -> usize {
    lines
        .iter()
        .map(|l| l.text.split_whitespace().count())
        .sum()
}

/// Puts right, once, the meetings AGT never got to stop ([`Store::recover`]; those started
/// before `launched`, unix ms), on a `meeting-recover` thread: it reads every meeting folder
/// and may patch WAV files. Tells the app which ones, so an open list shows them as they are.
fn spawn_recovery(store: Store, launched: i64, proxy: EventLoopProxy<AppEvent>) {
    let spawned = thread::Builder::new()
        .name("meeting-recover".into())
        .spawn(move || {
            let ids = store.recover(launched);
            if !ids.is_empty() {
                let _ = proxy.send_event(AppEvent::Meeting(Msg::Recovered(ids)));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("not checking for meetings left unfinished: {e}");
    }
}

/// Now, in unix ms.
fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Lists the meeting apps using the microphone every [`DETECT_POLL`] while `busy` is clear,
/// and posts them; only while some do, or when that just changed (nothing to decide on an
/// empty list twice).
fn spawn_detection(proxy: EventLoopProxy<AppEvent>, busy: Arc<AtomicBool>) {
    let spawned = thread::Builder::new()
        .name("meeting-detect".into())
        .spawn(move || {
            // `None`: unknown (at start, after a meeting), so the next list is sent.
            let mut last: Option<Vec<String>> = None;
            loop {
                thread::sleep(DETECT_POLL);
                if busy.load(Ordering::Relaxed) {
                    last = None;
                    continue;
                }
                let mut using: Vec<String> = agt_platform::mic_users()
                    .iter()
                    .filter_map(|u| agt_platform::meeting_app_name(&u.bundle_id))
                    .map(str::to_owned)
                    .collect();
                using.sort();
                using.dedup();
                if using.is_empty() && last.as_deref() == Some(&[]) {
                    continue;
                }
                last = Some(using.clone());
                if proxy
                    .send_event(AppEvent::Meeting(Msg::MicUsers(using)))
                    .is_err()
                {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("no meeting detection: {e}");
    }
}

/// Whether to offer to take notes, from the meeting apps using the microphone over time.
/// Pure: fed the time and the apps (by name, so Chrome's helper processes are one app).
#[derive(Debug, Default)]
pub struct Detector {
    /// Meeting apps using the mic, and since when without a break.
    using: HashMap<String, Instant>,
    /// Apps not to offer for: dismissed, ignored for [`PROMPT_FOR`], or on the mic when a
    /// meeting started. Until they have been off the mic for [`COOLDOWN`] (since when; `None`
    /// while still on it).
    quiet: HashMap<String, Option<Instant>>,
    /// The offer showing: its app, and since when.
    prompt: Option<(String, Instant)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Show the prompt: this app has been using the mic for [`SUSTAIN`].
    Show(String),
    /// Hide it: its app let go of the mic, or it was up for [`PROMPT_FOR`].
    Hide,
}

impl Detector {
    /// The meeting apps using the mic at `now` (one look; not every look needs to be passed,
    /// only one each time the list changes, and while it isn't empty).
    pub fn observe(&mut self, now: Instant, using: &[String]) -> Option<Decision> {
        let on = |app: &str| using.iter().any(|u| u == app);
        self.quiet.retain(|app, off_since| {
            // Off the mic long enough (up to now, whatever it does now): a new call.
            if off_since.is_some_and(|t| now.duration_since(t) >= COOLDOWN) {
                return false;
            }
            if on(app) {
                *off_since = None;
            } else if off_since.is_none() {
                *off_since = Some(now);
            }
            true
        });
        self.using.retain(|app, _| on(app));
        for app in using {
            self.using.entry(app.clone()).or_insert(now);
        }
        if let Some((app, shown)) = &self.prompt {
            if !on(app) {
                self.prompt = None;
                return Some(Decision::Hide);
            }
            if now.duration_since(*shown) >= PROMPT_FOR {
                self.quiet.insert(app.clone(), None);
                self.prompt = None;
                return Some(Decision::Hide);
            }
            return None;
        }
        let app = using.iter().find(|&app| {
            !self.quiet.contains_key(app)
                && self
                    .using
                    .get(app)
                    .is_some_and(|&since| now.duration_since(since) >= SUSTAIN)
        })?;
        self.prompt = Some((app.clone(), now));
        Some(Decision::Show(app.clone()))
    }

    /// The prompt's x. Returns whether there was one.
    pub fn dismiss(&mut self) -> bool {
        match self.prompt.take() {
            Some((app, _)) => {
                self.quiet.insert(app, None);
                true
            }
            None => false,
        }
    }

    /// A meeting started (however): the apps on the mic now are that meeting, not a new one
    /// to offer once it ends. The polling stops meanwhile, so "since when" starts over after.
    pub fn meeting_started(&mut self) {
        for app in self.using.drain().map(|(app, _)| app) {
            self.quiet.insert(app, None);
        }
        if let Some((app, _)) = self.prompt.take() {
            self.quiet.insert(app, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn using(apps: &[&str]) -> Vec<String> {
        apps.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn detection_policy() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let chrome = using(&["Google Chrome"]);
        let none = using(&[]);
        let mut d = Detector::default();
        assert_eq!(d.observe(at(0), &chrome), None);
        assert_eq!(d.observe(at(6), &chrome), None, "not 8 s yet");
        assert_eq!(
            d.observe(at(8), &chrome),
            Some(Decision::Show("Google Chrome".into()))
        );
        assert_eq!(d.observe(at(10), &chrome), None, "shown once");
        // Hidden when the app stops using the mic; it isn't quieted for that.
        assert_eq!(d.observe(at(12), &none), Some(Decision::Hide));
        assert_eq!(d.observe(at(14), &chrome), None);
        assert_eq!(
            d.observe(at(20), &chrome),
            None,
            "a gap starts the 8 s over"
        );
        assert_eq!(
            d.observe(at(22), &chrome),
            Some(Decision::Show("Google Chrome".into()))
        );
        // Times out after 20 s, then stays quiet while the app keeps the mic ...
        assert_eq!(d.observe(at(40), &chrome), None);
        assert_eq!(d.observe(at(42), &chrome), Some(Decision::Hide));
        assert_eq!(d.observe(at(600), &chrome), None);
        // ... and for 10 minutes after it let go of it.
        assert_eq!(d.observe(at(700), &none), None);
        assert_eq!(d.observe(at(900), &chrome), None, "back before 10 minutes");
        assert_eq!(d.observe(at(1000), &none), None);
        assert_eq!(d.observe(at(1599), &chrome), None, "9:59 off the mic");
        assert_eq!(d.observe(at(1600), &none), None);
        assert_eq!(d.observe(at(2200), &chrome), None, "10 minutes: a new call");
        assert_eq!(
            d.observe(at(2208), &chrome),
            Some(Decision::Show("Google Chrome".into()))
        );
        // Dismissed (x): hidden and quiet the same way.
        assert!(d.dismiss());
        assert!(!d.dismiss(), "nothing to dismiss");
        assert_eq!(d.observe(at(2400), &chrome), None);
        // Another app is its own matter.
        let both = using(&["Google Chrome", "Zoom"]);
        assert_eq!(d.observe(at(2402), &both), None);
        assert_eq!(
            d.observe(at(2410), &both),
            Some(Decision::Show("Zoom".into()))
        );
        // Taking notes quiets every app using the mic, and drops the prompt.
        d.meeting_started();
        assert_eq!(d.observe(at(4000), &both), None);
        assert_eq!(d.observe(at(4100), &both), None);
        assert_eq!(d.observe(at(4101), &none), None);
        assert_eq!(d.observe(at(4701), &using(&["Zoom"])), None);
        assert_eq!(
            d.observe(at(4709), &using(&["Zoom"])),
            Some(Decision::Show("Zoom".into()))
        );
    }

    #[test]
    fn ai_notes_wait_for_a_click_unless_auto_summary() {
        let cfg = |auto_summary, summary| MeetingConfig {
            auto_summary,
            summary,
            ..MeetingConfig::default()
        };
        let long = || Some(AUTO_SUMMARY_WORDS);
        // The default: nothing goes to Claude by itself, and the transcript isn't even read.
        let defaults = MeetingConfig::default();
        assert!(!auto_summary(&defaults, false, || panic!(
            "read the transcript"
        )));
        assert!(!auto_summary(
            &cfg(false, MeetingSummary::Claude),
            false,
            long
        ));
        // Turned on: after a meeting long enough to be worth it.
        let on = cfg(true, MeetingSummary::Claude);
        assert!(auto_summary(&on, false, long));
        assert!(!auto_summary(&on, false, || Some(AUTO_SUMMARY_WORDS - 1)));
        assert!(!auto_summary(&on, false, || None), "transcript unreadable");
        assert!(!auto_summary(&on, true, long), "quitting");
        assert!(!auto_summary(&cfg(true, MeetingSummary::Off), false, long));
    }

    #[test]
    fn notices_keep_the_last_per_meeting() {
        let said = |n: &Notices| -> Vec<(Option<String>, String)> {
            n.messages()
                .into_iter()
                .map(|m| match m {
                    ToNotes::Notice { id, text } => (id, text),
                    other => panic!("not a notice: {other:?}"),
                })
                .collect()
        };
        let notice = |id: Option<&str>, text: &str| (id.map(str::to_owned), text.to_owned());
        let mut n = Notices::default();
        n.set(Some("m1"), "The microphone stopped");
        n.set(None, "Speech model failed to load");
        n.set(Some("m1"), MODEL_LOADING);
        n.set(Some("m2"), "Couldn't save the transcript");
        // One per meeting (and one about none), oldest first.
        assert_eq!(
            said(&n),
            [
                notice(None, "Speech model failed to load"),
                notice(Some("m1"), MODEL_LOADING),
                notice(Some("m2"), "Couldn't save the transcript"),
            ]
        );
        // Taken back only while it still says the same.
        assert!(!n.clear(Some("m1"), "The microphone stopped"));
        assert!(n.clear(Some("m1"), MODEL_LOADING));
        assert!(!n.clear(Some("m1"), MODEL_LOADING));
        // A meeting that started: the failed start is old news.
        n.remove(None);
        assert_eq!(
            said(&n),
            [notice(Some("m2"), "Couldn't save the transcript")]
        );
    }

    #[test]
    fn tray_and_pill_starts_open_the_notes_window_to_say_why_not() {
        assert!(OpenNotes::Always.on_start() && OpenNotes::Always.on_failure());
        // The pill's Take notes: the window only if it didn't start.
        assert!(!OpenNotes::OnFailure.on_start() && OpenNotes::OnFailure.on_failure());
        assert!(!OpenNotes::Never.on_start() && !OpenNotes::Never.on_failure());
    }

    #[test]
    fn start_needs_a_speech_model() {
        assert_eq!(start_check(&SpeechModel::Ready), Ok(()));
        // Loading: the segments wait for it.
        assert_eq!(start_check(&SpeechModel::Loading), Ok(()));
        assert_eq!(
            start_check(&SpeechModel::Failed("model unavailable, see log".into())),
            Err("Speech model failed to load: model unavailable, see log".into())
        );
    }

    #[test]
    fn permission_asked_at_most_every_10_s() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let asked = std::cell::Cell::new(0);
        let ask = |answer: Option<bool>| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                answer
            }
        };
        let mut cache = PermissionCache::default();
        assert_eq!(cache.get(at(0), ask(None)), None);
        // Every status for the next 10 s gets the same answer without asking.
        assert_eq!(cache.get(at(1), ask(Some(true))), None);
        assert_eq!(cache.get(at(9), ask(Some(true))), None);
        assert_eq!(asked.get(), 1);
        assert_eq!(cache.get(at(10), ask(Some(true))), Some(true));
        assert_eq!(cache.get(at(15), ask(Some(false))), Some(true));
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn meeting_cut_is_short() {
        let c = meeting_cut();
        assert_eq!(c.max_len, 20 * 16_000);
        assert_eq!(c.search_len, 5 * 16_000);
        let p = c.pause.unwrap();
        assert_eq!((p.len, p.min_segment, p.pad), (12_800, 48_000, 4_000));
        assert!(p.trim);
    }

    #[test]
    fn transcript_word_count() {
        use agt_meeting::{Line, Speaker};
        let line = |text: &str| Line {
            id: 0,
            speaker: Speaker::Me,
            start_ms: 0,
            end_ms: 0,
            text: text.into(),
        };
        assert_eq!(words(&[]), 0);
        assert_eq!(
            words(&[line("Hello there,  team"), line(" "), line("ok\nthen")]),
            5
        );
    }
}
