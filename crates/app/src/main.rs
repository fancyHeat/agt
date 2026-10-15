// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logging;
mod tray;
mod update;

use agt_core::{Config, paths};
use tray_icon::menu::MenuEvent;
use velopack::{UpdateManager, VelopackApp};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::WindowId,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let config_path = paths::config_file();
    let (config, config_err) = match Config::load_or_create(&config_path) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    // Logging is set up before Velopack so its install/update hooks are logged too.
    logging::init(&paths::log_dir(), &config.log_level);

    // Handles Velopack install/update hooks (may exit) and applies downloaded updates.
    VelopackApp::build().run();

    tracing::info!(
        "AGT {VERSION} starting; config at {}",
        config_path.display()
    );
    if let Some(e) = config_err {
        tracing::error!("{e}; using defaults");
    }

    let updater = update::start(&config.update);

    #[allow(unused_mut)]
    let mut builder = EventLoop::<MenuEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // Menu-bar-only app: no Dock icon.
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(e);
    }));

    let mut app = App {
        tray: None,
        updater,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!("event loop error: {e}");
    }
    tracing::info!("AGT exiting");
}

struct App {
    tray: Option<tray::Tray>,
    updater: Option<UpdateManager>,
}

impl ApplicationHandler<MenuEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // The tray must be created once the event loop is running (required on macOS).
        if cause == StartCause::Init {
            match tray::Tray::new(VERSION) {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    tracing::error!("failed to create tray icon: {e}");
                    event_loop.exit();
                }
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: MenuEvent) {
        let Some(tray) = &self.tray else { return };
        if event.id == tray.open_config {
            open_folder(&paths::app_dir());
        } else if event.id == tray.quit {
            if let Some(um) = &self.updater {
                update::apply_pending_on_exit(um);
            }
            event_loop.exit();
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
