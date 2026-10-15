// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logging;
mod pipeline;
mod tray;
mod update;

use agt_core::{Config, config::OverlayPosition, paths};
use std::{
    fs::{File, TryLockError},
    time::Instant,
};
use tray_icon::{TrayIconEvent, menu::MenuEvent};
use velopack::{UpdateManager, VelopackApp};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::WindowId,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything other threads post to the UI thread, via `EventLoopProxy<AppEvent>`.
#[derive(Debug)]
enum AppEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Status(pipeline::Status),
}

fn main() {
    let config_path = paths::config_file();
    let config = Config::load_or_create(&config_path);
    let log_level = config.as_ref().map_or("info", |c| &c.log_level).to_owned();
    // Logging is set up before Velopack so its install/update hooks are logged too.
    logging::init(&paths::log_dir(), &log_level);

    // One instance only (Phase 1 adds global hotkeys; two instances would both paste).
    // Held for the life of the process; the OS releases it on exit.
    let Some(_instance_lock) = acquire_instance_lock() else {
        tracing::info!("AGT is already running; exiting");
        return;
    };

    // Handles Velopack install/update hooks (may exit) and applies downloaded updates.
    VelopackApp::build().run();

    tracing::info!(
        "AGT {VERSION} starting; config at {}",
        config_path.display()
    );
    let (config, config_error) = match config {
        Ok(c) => (c, false),
        Err(e) => {
            tracing::error!("{e}");
            tracing::error!("running with defaults and updates OFF; the file was not modified");
            (Config::fallback(), true)
        }
    };

    let updater = update::start(&config.update);

    #[allow(unused_mut)]
    let mut builder = EventLoop::<AppEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // Menu-bar-only app: no Dock icon.
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
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

    let proxy = event_loop.create_proxy();
    let pipeline = pipeline::start(&config, move |s| {
        let _ = proxy.send_event(AppEvent::Status(s));
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
        updater,
        config_error,
        polish_info,
        status: status.map(|s| s.to_string()),
        pipeline: pipeline.ok(),
        overlay: None,
        failures_when_shown: 0,
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
    }
    tracing::info!("AGT exiting");
}

/// `None` if another instance holds the lock. `Some(None)` if locking failed for another
/// reason; we run anyway rather than refuse to start.
fn acquire_instance_lock() -> Option<Option<File>> {
    let dir = paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = match File::create(dir.join("agt.lock")) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("cannot create instance lock ({e}); continuing without it");
            return Some(None);
        }
    };
    match file.try_lock() {
        Ok(()) => Some(Some(file)),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(e)) => {
            tracing::warn!("cannot take instance lock ({e}); continuing without it");
            Some(None)
        }
    }
}

struct App {
    tray: Option<tray::Tray>,
    updater: Option<UpdateManager>,
    config_error: bool,
    /// Tray line about LLM polish.
    polish_info: String,
    /// Latest pipeline status, kept so it can be shown once the tray exists.
    status: Option<String>,
    pipeline: Option<pipeline::Pipeline>,
    /// Created at start-up (hidden) when enabled and dictation works.
    overlay: Option<agt_overlay::Overlay>,
    /// `None` if the overlay is turned off.
    overlay_position: Option<agt_overlay::Position>,
    /// [`pipeline::Pipeline::failures`] when the overlay last appeared.
    failures_when_shown: u64,
}

impl ApplicationHandler<AppEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // The tray must be created once the event loop is running (required on macOS).
        if cause == StartCause::Init {
            match tray::Tray::new(VERSION, self.config_error, &self.polish_info) {
                Ok(t) => {
                    if let Some(s) = &self.status {
                        t.set_status(s);
                    }
                    self.tray = Some(t);
                }
                Err(e) => {
                    tracing::error!("failed to create tray icon: {e}");
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
        if let AppEvent::Status(s) = &event {
            // Overlay first: it's what the user is looking at on key down.
            self.update_overlay(s);
            let s = s.to_string();
            if let Some(tray) = &self.tray {
                tray.set_status(&s);
                tray.set_copy_enabled(self.pipeline.as_ref().is_some_and(|p| p.has_transcript()));
            }
            self.status = Some(s);
            return;
        }
        let Some(tray) = &self.tray else { return };
        match event {
            AppEvent::Menu(e) if e.id == tray.copy_last => {
                if let Some(p) = &self.pipeline {
                    p.copy_last_transcript();
                }
            }
            AppEvent::Menu(e) if e.id == tray.open_config => open_folder(&paths::config_dir()),
            AppEvent::Menu(e) if e.id == tray.quit => {
                if let Some(um) = &self.updater {
                    update::apply_pending_on_exit(um);
                }
                event_loop.exit();
            }
            AppEvent::Tray(e) => tracing::debug!("tray: {e:?}"),
            AppEvent::Menu(_) | AppEvent::Status(_) => {}
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
    fn update_overlay(&mut self, status: &pipeline::Status) {
        use agt_overlay::Mode;
        use pipeline::Status;
        let (Some(overlay), Some(pipeline)) = (&mut self.overlay, &self.pipeline) else {
            return;
        };
        let was_hidden = overlay.mode() == Mode::Hidden;
        if was_hidden {
            self.failures_when_shown = pipeline.failures();
        }
        let mode = match status {
            Status::Recording => Mode::Recording,
            Status::Transcribing => Mode::Transcribing,
            // A red flash if this dictation failed. An error from before (still shown in the
            // tray after a cancel, a short tap or "no speech") just hides it.
            Status::Error(_) if pipeline.failures() != self.failures_when_shown => Mode::Error,
            Status::Error(_) | Status::Ready(_) | Status::Loading(_) => Mode::Hidden,
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
