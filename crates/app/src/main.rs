// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logging;
mod pipeline;
mod settings;
mod tray;
mod update;

use agt_core::{Config, config::OverlayPosition, paths};
use settings::Action;
use std::{
    fs::{File, TryLockError},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use tray_icon::{TrayIconEvent, menu::MenuEvent};
use velopack::{VelopackApp, VelopackAsset};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::WindowId,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long dictation must be idle ([`pipeline::Status::Ready`], no status change) before an
/// update restart: covers the clipboard restore after a paste and back-to-back dictations.
const RESTART_SETTLE: Duration = Duration::from_secs(5);

/// Set for the instance a settings restart starts, so it waits for this one to exit and
/// release the instance lock (for up to [`RELAUNCH_WAIT`]) instead of quitting at once.
const RELAUNCH_ENV: &str = "AGT_RELAUNCH";
const RELAUNCH_WAIT: Duration = Duration::from_secs(15);

/// How often `config.toml` is checked for changes made outside AGT (one `stat`; a file
/// watcher crate isn't worth it for this).
const CONFIG_POLL: Duration = Duration::from_secs(2);

/// Everything other threads post to the UI thread, via `EventLoopProxy<AppEvent>`.
#[derive(Debug)]
enum AppEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Pipeline(pipeline::Event),
    Update(UpdateEvent),
    /// `config.toml` was modified (or appeared, or went away).
    ConfigFile,
}

/// Why AGT is waiting to restart.
enum Restart {
    /// Into a downloaded update ("Update now"); it applies changed settings as well.
    Update(VelopackAsset),
    /// To apply changed settings: the file differs from what the pipeline runs with.
    Settings,
}

/// Progress of an "Update now" check, from its worker thread.
#[derive(Debug)]
enum UpdateEvent {
    /// Found this version; downloading it.
    Downloading(String),
    /// The update ready to apply (`None`: up to date), or the error.
    Done(Result<Option<VelopackAsset>, String>),
    /// [`RESTART_SETTLE`] has passed since change number `.0` (see [`App::changes`]).
    Settled(u64),
}

/// How long exiting waits for the telemetry writer to commit the last records.
const TELEMETRY_FLUSH: Duration = Duration::from_secs(2);

fn main() {
    // `agt stats`: a report from the telemetry database; doesn't start the app.
    if std::env::args().nth(1).as_deref() == Some("stats") {
        return print_stats();
    }

    let config_path = paths::config_file();
    let config = Config::load_or_create(&config_path);
    let log_level = config.as_ref().map_or("info", |c| &c.log_level).to_owned();
    // Events are queued from here on, and written once the database is opened below.
    logging::init(&log_level);

    // One instance only (Phase 1 adds global hotkeys; two instances would both paste).
    // Held for the life of the process; the OS releases it on exit.
    let relaunched = std::env::var_os(RELAUNCH_ENV).is_some();
    let wait = if relaunched {
        RELAUNCH_WAIT
    } else {
        Duration::ZERO
    };
    let Some(_instance_lock) = acquire_instance_lock(wait) else {
        eprintln!("agt: AGT is already running; exiting");
        return;
    };

    // Handles Velopack install/update hooks (may exit) and applies downloaded updates.
    #[allow(unused_mut)]
    let mut velopack = VelopackApp::build();
    // The usage history lives outside the install folder so reinstalls keep it (see
    // `paths`); uninstalling still removes it. Models are left, as before.
    #[cfg(windows)]
    {
        velopack = velopack.on_before_uninstall_fast_callback(|_| {
            let history = paths::history_dir();
            if history != paths::data_dir() {
                let _ = std::fs::remove_dir_all(history);
            }
        });
    }
    velopack.run();

    tracing::info!(
        "AGT {VERSION} starting; config at {}",
        config_path.display()
    );
    let file = config.map_err(|e| e.to_string());
    let (config, config_error) = match &file {
        Ok(c) => (c.clone(), false),
        Err(e) => {
            tracing::error!("{e}");
            tracing::error!("running with defaults and updates OFF; the file was not modified");
            (Config::fallback(), true)
        }
    };
    // After the lock and Velopack's hooks, so neither a second instance nor a hook run
    // counts as a session.
    agt_telemetry::global().start(paths::db_file(), session_start(&config, config_error));

    let updater = update::start(&config.update);

    #[allow(unused_mut)]
    let mut builder = EventLoop::<AppEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // Menu-bar-only app: no Dock icon.
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
        // winit activates the app on launch by default, which makes it frontmost: with no
        // window to type into, every keystroke goes nowhere until the user clicks elsewhere
        // (seen after each settings restart and at login).
        builder.with_activate_ignoring_other_apps(false);
    }
    let event_loop = builder.build().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // Without handlers, tray-icon queues events in an unbounded global channel forever.
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(AppEvent::Menu(e));
    }));
    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e| {
        // Move/Enter/Leave fire constantly; only clicks are worth waking the loop for.
        if matches!(e, TrayIconEvent::Click { .. }) {
            let _ = proxy.send_event(AppEvent::Tray(e));
        }
    }));

    watch_config(config_path.clone(), event_loop.create_proxy());

    let proxy = event_loop.create_proxy();
    let pipeline = pipeline::start(&config, move |e| {
        let _ = proxy.send_event(AppEvent::Pipeline(e));
    })
    .map_err(|e| {
        tracing::error!("dictation unavailable: {e}");
        pipeline::Status::Error(e)
    });
    let status = pipeline.as_ref().err().cloned();
    let polish_info = pipeline
        .as_ref()
        .map_or("Polish: off", |p| &p.polish_info)
        .to_owned();

    let mut app = App {
        tray: None,
        proxy: event_loop.create_proxy(),
        updater,
        restart_with: None,
        changes: 0,
        config_path,
        file,
        running: config.clone(),
        config_error,
        polish_info,
        status,
        pipeline: pipeline.ok(),
        overlay: None,
        failures_at_start: 0,
        exit_reason: agt_telemetry::ExitReason::Quit,
        overlay_position: config
            .overlay
            .enabled
            .then_some(match config.overlay.position {
                OverlayPosition::Bottom => agt_overlay::Position::Bottom,
                OverlayPosition::Top => agt_overlay::Position::Top,
            }),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!("event loop error: {e}");
        app.exit_reason = agt_telemetry::ExitReason::Error;
    }
    tracing::info!("AGT exiting");
    agt_telemetry::global().end_session(app.exit_reason, TELEMETRY_FLUSH);
}

/// The session's non-secret settings. Polish: provider, model, style and S1-mini's options
/// only (never the API key, its variable, the URL, which may carry credentials, or a custom
/// prompt).
fn session_start(config: &Config, config_error: bool) -> agt_telemetry::SessionStart {
    let lower = |v: &dyn std::fmt::Debug| format!("{v:?}").to_lowercase();
    let polish = &config.polish;
    let on = polish.enabled;
    let s1 = on && polish.provider == agt_core::config::PolishProvider::S1Mini;
    agt_telemetry::SessionStart {
        app_version: VERSION.into(),
        hotkey: config.hotkey.key.clone(),
        min_hold_ms: config.hotkey.min_hold_ms as i64,
        keep_mic_open: config.audio.keep_mic_open,
        stt_engine: lower(&config.stt.engine),
        stt_model: config.stt.model.clone(),
        stt_precision: lower(&config.stt.precision),
        stt_device: lower(&config.stt.device),
        stt_threads: config.stt.threads as i64,
        polish_enabled: on,
        polish_provider: on.then(|| lower(&polish.provider)),
        polish_model: on.then(|| polish.model().to_owned()),
        polish_style: on.then(|| lower(&polish.style)),
        s1_styling: s1.then(|| lower(&polish.s1_mini.styling)),
        s1_structure: s1.then(|| lower(&polish.s1_mini.structure)),
        s1_context: s1.then(|| lower(&polish.s1_mini.context)),
        s1_device: s1.then(|| lower(&polish.s1_mini.device)),
        restore_clipboard_ms: config.paste.restore_clipboard_ms as i64,
        trailing_space: config.paste.trailing_space,
        overlay_enabled: config.overlay.enabled,
        updates_enabled: config.update.enabled,
        log_level: config.log_level.clone(),
        config_error,
    }
}

/// `agt stats`. Release builds on Windows are GUI-subsystem programs with no console, so
/// attach to the one we were started from (output then interleaves with the shell's prompt,
/// which doesn't wait for GUI programs; `agt stats | more` or `> file` avoids that).
fn print_stats() {
    // Only without a stdout already (redirected to a file or pipe, or a debug build).
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
        };
        if GetStdHandle(STD_OUTPUT_HANDLE).is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    let path = paths::db_file();
    match agt_telemetry::stats::report(&path) {
        Ok(report) => println!("AGT stats ({})\n{report}", path.display()),
        Err(e) => eprintln!("agt stats: {e}"),
    }
}

/// `None` if another instance holds the lock (still, after waiting up to `wait` for it to
/// exit). `Some(None)` if locking failed for another reason; we run anyway rather than
/// refuse to start.
fn acquire_instance_lock(wait: Duration) -> Option<Option<File>> {
    let dir = paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = match File::create(dir.join("agt.lock")) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("cannot create instance lock ({e}); continuing without it");
            return Some(None);
        }
    };
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Some(Some(file)),
            Err(TryLockError::WouldBlock) if start.elapsed() < wait => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Error(e)) => {
                tracing::warn!("cannot take instance lock ({e}); continuing without it");
                return Some(None);
            }
        }
    }
}

/// Posts [`AppEvent::ConfigFile`] whenever `config.toml`'s modification time or size
/// changes (edits in an editor, and AGT's own from the Settings menu).
fn watch_config(path: PathBuf, proxy: EventLoopProxy<AppEvent>) {
    let stamp = move || {
        std::fs::metadata(&path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()))
    };
    let spawned = thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            let mut last = stamp();
            loop {
                thread::sleep(CONFIG_POLL);
                let now = stamp();
                if now != last {
                    last = now;
                    if proxy.send_event(AppEvent::ConfigFile).is_err() {
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("cannot watch config.toml ({e}); changes apply on the next start");
    }
}

/// Starts a new instance with the same arguments; it takes over once this one has exited.
fn relaunch() -> std::io::Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env(RELAUNCH_ENV, "1")
        .spawn()
        .map(drop)
}

struct App {
    tray: Option<tray::Tray>,
    proxy: EventLoopProxy<AppEvent>,
    /// `None` if this isn't a Velopack install.
    updater: Option<update::Updater>,
    /// A restart waiting for dictation to be idle for [`RESTART_SETTLE`] (or for the user to
    /// click "Update now" / "Restart now to apply" again).
    restart_with: Option<Restart>,
    /// Counts pipeline status changes and settings changes: a restart timer is stale if this
    /// moved on.
    changes: u64,
    config_path: PathBuf,
    /// `config.toml` as last read (or its error): what the Settings menu shows.
    file: Result<Config, String>,
    /// What the pipeline was started with (defaults if the file had an error then).
    running: Config,
    /// The file had an error at start-up (so updates are off).
    config_error: bool,
    /// Tray line about LLM polish.
    polish_info: String,
    /// Latest pipeline status, kept so it can be shown once the tray exists.
    status: Option<pipeline::Status>,
    pipeline: Option<pipeline::Pipeline>,
    /// Created at start-up (hidden) when enabled and dictation works.
    overlay: Option<agt_overlay::Overlay>,
    /// `None` if the overlay is turned off.
    overlay_position: Option<agt_overlay::Position>,
    /// The pipeline's failure count when the current dictation started (see
    /// [`pipeline::Event::Status`]).
    failures_at_start: u64,
    /// Recorded in the session when the event loop returns.
    exit_reason: agt_telemetry::ExitReason,
}

impl ApplicationHandler<AppEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // The tray must be created once the event loop is running (required on macOS).
        if cause == StartCause::Init {
            let installed = self.updater.is_some();
            // On macOS (and with AGT_HOME) the history is in the config folder: one item will do.
            let data_folder = paths::history_dir() != paths::config_dir();
            let tray = tray::Tray::new(VERSION, &self.polish_info, installed, data_folder);
            match tray {
                Ok(t) => {
                    if let Some(s) = &self.status {
                        t.set_status(&s.to_string());
                    }
                    self.tray = Some(t);
                    self.show_settings();
                }
                Err(e) => {
                    tracing::error!("failed to create tray icon: {e}");
                    self.exit_reason = agt_telemetry::ExitReason::Error;
                    event_loop.exit();
                }
            }
            // Created hidden now, so showing it on key down is a single present.
            if let (Some(position), Some(_)) = (self.overlay_position, &self.pipeline) {
                match agt_overlay::Overlay::new(position) {
                    Ok(o) => self.overlay = Some(o),
                    Err(e) => tracing::warn!("no overlay: {e}"),
                }
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        // Already restarting into an update (or quitting): don't hand Velopack a second job.
        if event_loop.exiting() {
            return;
        }
        if let AppEvent::Pipeline(e) = &event {
            match e {
                pipeline::Event::Status(s, failures) => {
                    // Overlay first: it's what the user is looking at on key down.
                    self.update_overlay(s, *failures);
                    if let Some(tray) = &self.tray {
                        tray.set_status(&s.to_string());
                    }
                    self.status = Some(s.clone());
                    self.changes += 1;
                    self.start_restart_timer();
                }
                pipeline::Event::Rejected => {
                    if let (Some(o), Some(p)) = (&mut self.overlay, &self.pipeline) {
                        o.flash_error(p.level.get(), Instant::now());
                    }
                }
                pipeline::Event::CopyAvailable(available) => {
                    if let Some(tray) = &self.tray {
                        tray.set_copy_enabled(*available);
                    }
                }
                pipeline::Event::PolishInfo(info) => {
                    if let Some(tray) = &self.tray {
                        tray.set_polish(info);
                    }
                    self.polish_info = info.clone();
                }
            }
            return;
        }
        if let AppEvent::ConfigFile = event {
            return self.config_file_changed();
        }
        if let AppEvent::Menu(e) = &event
            && let Some(action) = self.tray.as_ref().and_then(|t| t.settings.action(&e.id))
        {
            return self.settings_action(action.clone(), event_loop);
        }
        let Some(tray) = &self.tray else { return };
        match event {
            AppEvent::Menu(e) if e.id == tray.copy_last => {
                if let Some(p) = &self.pipeline {
                    p.copy_last_transcript();
                }
            }
            AppEvent::Menu(e) if e.id == tray.update => self.update_now(event_loop),
            AppEvent::Menu(e) if e.id == tray.open_config => open_folder(&paths::config_dir()),
            AppEvent::Menu(e) if tray.privacy.as_ref() == Some(&e.id) => {
                // The page for the first missing permission (or Accessibility, to review).
                let missing = agt_platform::missing_permissions();
                agt_platform::open_permission_settings(
                    missing
                        .first()
                        .copied()
                        .unwrap_or(agt_platform::Permission::Accessibility),
                );
            }
            AppEvent::Menu(e) if Some(&e.id) == tray.open_data.as_ref() => {
                open_folder(&paths::history_dir())
            }
            AppEvent::Menu(e) if e.id == tray.quit => {
                if let Some(u) = &self.updater {
                    update::apply_pending_on_exit(&u.um);
                }
                event_loop.exit();
            }
            AppEvent::Update(UpdateEvent::Downloading(version)) => {
                tray.set_update(&format!("Downloading update {version}..."), false);
            }
            AppEvent::Update(UpdateEvent::Done(Ok(Some(asset)))) => self.restart_when_idle(asset),
            AppEvent::Update(UpdateEvent::Settled(n)) => {
                if n == self.changes
                    && let Some(restart) = self.restart_with.take()
                {
                    self.restart_now(restart, event_loop);
                }
            }
            AppEvent::Update(UpdateEvent::Done(Ok(None))) => {
                tray.set_update(
                    &format!("{} - up to date ({VERSION})", tray::UPDATE_NOW),
                    true,
                );
            }
            AppEvent::Update(UpdateEvent::Done(Err(e))) => {
                tracing::error!("update failed: {e}");
                tray.set_update("Update failed - see log", true);
            }
            AppEvent::Tray(e) => tracing::debug!("tray: {e:?}"),
            AppEvent::Menu(_) | AppEvent::Pipeline(_) | AppEvent::ConfigFile => {}
        }
    }

    /// Draws overlay frames while it's visible. While hidden, the loop just waits for events:
    /// no timer, no wake-ups.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(overlay), Some(pipeline)) = (&mut self.overlay, &self.pipeline) else {
            return;
        };
        if overlay
            .next_frame()
            .is_some_and(|due| Instant::now() >= due)
        {
            overlay.tick(pipeline.level.get(), Instant::now());
        }
        event_loop.set_control_flow(match overlay.next_frame() {
            Some(due) => ControlFlow::WaitUntil(due),
            None => ControlFlow::Wait,
        });
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

impl App {
    /// "Update now": while a restart is waiting, restarts right away (the user's choice);
    /// with an update already downloaded, restarts into it once idle; else checks and
    /// downloads on a worker thread, which reports back as [`AppEvent::Update`].
    fn update_now(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(Restart::Update(_)) = self.restart_with {
            let restart = self.restart_with.take().unwrap();
            return self.restart_now(restart, event_loop);
        }
        let (Some(updater), Some(tray)) = (&self.updater, &self.tray) else {
            return;
        };
        if let Some(asset) = updater.um.get_update_pending_restart() {
            return self.restart_when_idle(asset);
        }
        tray.set_update("Checking for updates...", false);
        let (updater, proxy) = (updater.clone(), self.proxy.clone());
        let spawned = thread::Builder::new()
            .name("update-now".into())
            .spawn(move || {
                let result = update::check_now(&updater, |version| {
                    let _ = proxy
                        .send_event(AppEvent::Update(UpdateEvent::Downloading(version.into())));
                });
                let result = result.map_err(|e| e.to_string());
                let _ = proxy.send_event(AppEvent::Update(UpdateEvent::Done(result)));
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start update check: {e}");
            tray.set_update("Update failed - see log", true);
        }
    }

    /// Restarts into `asset` once dictation has been idle for [`RESTART_SETTLE`]. Until then
    /// the item offers to restart right away (also the way out after an error, which waits
    /// for the user so a failed paste's transcript can still be copied).
    fn restart_when_idle(&mut self, asset: VelopackAsset) {
        if let Some(tray) = &self.tray {
            tray.set_update(&format!("Restart into {} now", asset.Version), true);
        }
        self.restart_with = Some(Restart::Update(asset));
        self.start_restart_timer();
    }

    /// If a restart is waiting and dictation is idle, posts [`UpdateEvent::Settled`] after
    /// [`RESTART_SETTLE`]; any status change in between makes it stale.
    fn start_restart_timer(&self) {
        use pipeline::Status;
        // Idle: ready, or dictation unavailable altogether. For an update not on Error
        // (an error waits for the user, see above); for settings the user just changed, on
        // Error too (a bad setting may be the error). Never while loading: a restart would
        // start a model download over ("Restart now to apply" still does).
        let idle = self.pipeline.is_none()
            || match self.restart_with {
                Some(Restart::Update(_)) => matches!(self.status, Some(Status::Ready(_))),
                Some(Restart::Settings) => matches!(
                    self.status,
                    Some(Status::Ready(_) | Status::Error(_) | Status::NeedsPermission(_))
                ),
                None => false,
            };
        if self.restart_with.is_none() || !idle {
            return;
        }
        let (proxy, n) = (self.proxy.clone(), self.changes);
        // If this fails, the item still restarts on click.
        let _ = thread::Builder::new()
            .name("restart-timer".into())
            .spawn(move || {
                thread::sleep(RESTART_SETTLE);
                let _ = proxy.send_event(AppEvent::Update(UpdateEvent::Settled(n)));
            });
    }

    /// Exits after applying the update (Velopack then starts it) or starting a new instance
    /// (which reads the changed settings).
    fn restart_now(&mut self, restart: Restart, event_loop: &ActiveEventLoop) {
        let asset = match restart {
            Restart::Update(asset) => asset,
            Restart::Settings => {
                match relaunch() {
                    Ok(()) => {
                        tracing::info!("restarting to apply changed settings");
                        self.exit_reason = agt_telemetry::ExitReason::SettingsRestart;
                        event_loop.exit();
                    }
                    Err(e) => tracing::error!("cannot restart to apply settings: {e}"),
                }
                return;
            }
        };
        let asset = &asset;
        let (Some(updater), Some(tray)) = (&self.updater, &self.tray) else {
            return;
        };
        tray.set_update(&format!("Restarting into {}...", asset.Version), false);
        match update::apply_and_restart(&updater.um, asset) {
            Ok(()) => {
                self.exit_reason = agt_telemetry::ExitReason::UpdateRestart;
                event_loop.exit();
            }
            Err(e) => {
                tracing::error!("failed to start updater: {e}");
                tray.set_update("Update failed - see log", true);
            }
        }
    }

    /// Re-reads `config.toml` after [`AppEvent::ConfigFile`].
    fn config_file_changed(&mut self) {
        let file = Config::load(&self.config_path).map_err(|e| e.to_string());
        if file == self.file {
            return;
        }
        if let Err(e) = &file {
            tracing::error!("{e}; keeping the settings AGT runs with until it's fixed");
        }
        self.file = file;
        self.settings_changed();
    }

    fn settings_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::Set(changes) => match agt_core::config::set(&self.config_path, &changes) {
                Ok(c) => {
                    self.file = Ok(c);
                    self.settings_changed();
                }
                Err(e) => {
                    tracing::error!("cannot change config.toml: {e}");
                    // Puts back the checkmark the click moved.
                    self.show_settings();
                }
            },
            Action::Toggle(path, get) => {
                let file = Config::load(&self.config_path).map_err(|e| e.to_string());
                match file {
                    Ok(c) => self
                        .settings_action(Action::Set(vec![(path, (!get(&c)).into())]), event_loop),
                    Err(e) => {
                        tracing::error!("cannot change config.toml: {e}");
                        self.file = Err(e);
                        self.settings_changed();
                    }
                }
            }
            Action::EditFile => open_in_editor(&self.config_path),
            Action::ApplyNow => {
                let restart = self.restart_with.take().unwrap_or(Restart::Settings);
                self.restart_now(restart, event_loop);
            }
        }
    }

    /// The file's settings now differ from the running ones (restart once idle), or match
    /// them again, or can't be read (keep running as is).
    fn settings_changed(&mut self) {
        let differs = self.file.as_ref().is_ok_and(|c| *c != self.running);
        match self.restart_with {
            // An update restart applies the settings too.
            Some(Restart::Update(_)) => {}
            _ => self.restart_with = differs.then_some(Restart::Settings),
        }
        // Debounces: a few changes in a row restart once.
        self.changes += 1;
        self.start_restart_timer();
        self.show_settings();
    }

    /// Rebuilds the Settings submenu and the config error line from [`Self::file`].
    fn show_settings(&mut self) {
        let applying = self.file.as_ref().is_ok_and(|c| *c != self.running);
        let Some(tray) = &mut self.tray else { return };
        let (shown, editable) = match &self.file {
            Ok(c) => (c, true),
            Err(_) => (&self.running, false),
        };
        if let Err(e) = tray.settings.show(shown, editable, applying) {
            tracing::error!("cannot build the Settings menu: {e}");
        }
        tray.set_warning(match (&self.file, self.config_error) {
            (Ok(_), _) => None,
            (Err(_), true) => Some("Config error (auto-updates off) - see log"),
            (Err(_), false) => Some("config.toml has an error (not applied) - see log"),
        });
    }

    fn update_overlay(&mut self, status: &pipeline::Status, failures: u64) {
        use agt_overlay::Mode;
        use pipeline::Status;
        let (Some(overlay), Some(pipeline)) = (&mut self.overlay, &self.pipeline) else {
            return;
        };
        let was_hidden = overlay.mode() == Mode::Hidden;
        if *status == Status::Recording {
            // Every recording starts a new baseline (also when an error flash is still up).
            self.failures_at_start = failures;
        }
        let mode = match status {
            Status::Recording => Mode::Recording,
            Status::Transcribing => Mode::Transcribing,
            // A red flash if this dictation failed. An error from before (still shown in the
            // tray after a cancel, a short tap or "no speech") just hides it.
            Status::Error(_) if failures != self.failures_at_start => Mode::Error,
            Status::Error(_)
            | Status::Ready(_)
            | Status::Loading(_)
            | Status::NeedsPermission(_) => Mode::Hidden,
        };
        overlay.set_mode(mode, pipeline.level.get(), Instant::now());
        if was_hidden
            && mode == Mode::Recording
            && let Some(down) = pipeline.last_key_down()
        {
            tracing::debug!(
                "key down → overlay visible {:.1} ms",
                down.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}

/// Opens `file` in a text editor: the default one on macOS, Notepad on Windows (`.toml`
/// usually has no app associated there).
fn open_in_editor(file: &std::path::Path) {
    let mut command = if cfg!(windows) {
        std::process::Command::new("notepad")
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg("-t");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    if let Err(e) = command.arg(file).spawn() {
        tracing::error!("failed to open {}: {e}", file.display());
    }
}

fn open_folder(dir: &std::path::Path) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(dir).spawn() {
        tracing::error!("failed to open {}: {e}", dir.display());
    }
}
