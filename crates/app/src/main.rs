// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logging;
mod tray;
mod update;

use agt_core::{Config, paths};
use std::fs::{File, TryLockError};
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
/// Later phases add hotkey, audio level and transcript events here.
#[derive(Debug)]
enum AppEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
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

    let mut app = App {
        tray: None,
        updater,
        config_error,
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
}

impl ApplicationHandler<AppEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // The tray must be created once the event loop is running (required on macOS).
        if cause == StartCause::Init {
            match tray::Tray::new(VERSION, self.config_error) {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    tracing::error!("failed to create tray icon: {e}");
                    event_loop.exit();
                }
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        let Some(tray) = &self.tray else { return };
        match event {
            AppEvent::Menu(e) if e.id == tray.open_config => open_folder(&paths::config_dir()),
            AppEvent::Menu(e) if e.id == tray.quit => {
                if let Some(um) = &self.updater {
                    update::apply_pending_on_exit(um);
                }
                event_loop.exit();
            }
            AppEvent::Tray(e) => tracing::debug!("tray: {e:?}"),
            AppEvent::Menu(_) => {}
        }
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
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
