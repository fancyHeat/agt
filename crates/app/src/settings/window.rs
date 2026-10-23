//! The settings window: `agt settings`, a process of its own, so it costs dictation nothing
//! and is gone once closed. It changes `config.toml` the way the tray does; the running AGT
//! notices the change (`watch_config`) and applies it once dictation is idle.

use super::{Action, metrics, schema};
use agt_core::{
    Config, Dictionary,
    config::{self, SettingPath, SettingValue},
    paths,
};
use agt_settings_ui::{Control, Event, Page, Row, Section};
use std::{
    fs::{self, File, TryLockError},
    path::PathBuf,
    time::SystemTime,
};

/// Opens the window, on the Metrics page if `metrics`: starts `agt settings [metrics]`,
/// which brings an open one to the front instead.
pub fn open(metrics: bool) {
    // Lets the new process take the foreground (Windows only hands it to the process the
    // user last clicked in, AGT, which spawns it).
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
            windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
        );
    }
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.arg("settings");
        if metrics {
            command.arg("metrics");
        }
        command.spawn()
    });
    match spawned {
        // Reaped when it exits (on macOS it would otherwise stay a zombie while AGT runs).
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("settings-window".into())
                .spawn(move || child.wait());
        }
        Err(e) => tracing::error!("cannot open the settings window: {e}"),
    }
}

/// `agt settings [metrics]`. One window at a time: if one is open, this brings it to the
/// front (on the Metrics page if asked for).
pub fn run() {
    // Any title but the Metrics page's shows the first page: "Settings..." while the window
    // is open on Metrics goes back to the settings.
    let page = if std::env::args().nth(2).as_deref() == Some("metrics") {
        metrics::TITLE
    } else {
        "Settings"
    };
    let dir = paths::data_dir();
    let _ = fs::create_dir_all(&dir);
    let pid_file = dir.join("settings.pid");
    // Held while the window is open; the OS releases it on exit.
    let _lock = match File::create(dir.join("settings.lock")) {
        Ok(file) => match file.try_lock() {
            Ok(()) => Some(file),
            Err(TryLockError::WouldBlock) => {
                // The open one may be just starting (no pid file or window yet): retry a
                // little. Either way, never a second window.
                for _ in 0..20 {
                    let pid = fs::read_to_string(&pid_file).ok();
                    if pid
                        .and_then(|p| p.trim().parse().ok())
                        .is_some_and(|pid| agt_settings_ui::activate(pid, Some(page)))
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                return;
            }
            Err(TryLockError::Error(_)) => None,
        },
        Err(_) => None,
    };
    // Written whole (renamed into place): a reader never sees half a pid.
    let tmp = pid_file.with_extension("pid.tmp");
    if fs::write(&tmp, std::process::id().to_string()).is_ok() {
        let _ = fs::rename(&tmp, &pid_file);
    }

    let model = Model {
        path: paths::config_file(),
        actions: Vec::new(),
        stamp: None,
        dictionary_stamp: None,
        metrics: metrics::Metrics::new(paths::db_file()),
        advanced: false,
    };
    let icon = crate::tray::color_icon;
    if let Err(e) = agt_settings_ui::run("AGT Settings", Box::new(model), Some(page), Some(icon)) {
        eprintln!("agt settings: {e}");
    }
}

/// What a row of the window does.
enum Act {
    Check(SettingPath),
    /// Per option, what it writes (nothing: the file's value, which isn't one of them).
    Choice(Vec<Vec<(SettingPath, SettingValue)>>),
    /// `multiline`: kept as typed; one line is trimmed.
    Text(SettingPath, bool),
    Button(Action),
    Metrics(metrics::Act),
    /// "Show advanced settings" / "Hide advanced settings".
    Advanced,
    None,
}

struct Model {
    path: PathBuf,
    /// By row id, for the pages last shown.
    actions: Vec<Act>,
    /// The file's modification time and size when last read or written, and
    /// `dictionary.toml`'s when last read (its error is shown under "Edit dictionary...").
    stamp: Option<(Option<SystemTime>, u64)>,
    dictionary_stamp: Option<(Option<SystemTime>, u64)>,
    metrics: metrics::Metrics,
    /// Showing every setting, not just the basic ones (until the window closes).
    advanced: bool,
}

fn stamp(path: &std::path::Path) -> Option<(Option<SystemTime>, u64)> {
    fs::metadata(path)
        .ok()
        .map(|m| (m.modified().ok(), m.len()))
}

impl Model {
    fn stamp(&self) -> Option<(Option<SystemTime>, u64)> {
        stamp(&self.path)
    }

    fn row(&mut self, mut item: schema::Item, enabled: bool) -> Row {
        if let schema::Kind::Button(Action::EditDictionary) = item.kind
            && let Err(e) = Dictionary::load(&paths::dictionary_file())
        {
            item.note = format!(
                "{e}. Until it's fixed, AGT uses the last version that worked since it started (none if the error was there at start-up)."
            );
        }
        let id = self.actions.len();
        let (label, control, act) = match item.kind {
            schema::Kind::Check { path, on, .. } => (
                String::new(),
                Control::Toggle {
                    text: item.label,
                    on,
                },
                Act::Check(path),
            ),
            schema::Kind::Choice(choices) => (
                item.label,
                Control::Choice {
                    selected: choices.iter().position(|c| c.checked).unwrap_or(0),
                    options: choices.iter().map(|c| c.label.clone()).collect(),
                },
                Act::Choice(choices.into_iter().map(|c| c.changes).collect()),
            ),
            schema::Kind::Text(t) => {
                let control = if t.multiline {
                    Control::LongText { value: t.value }
                } else {
                    Control::Text {
                        value: t.value,
                        placeholder: t.placeholder,
                        secret: t.secret,
                    }
                };
                (item.label, control, Act::Text(t.path, t.multiline))
            }
            schema::Kind::Info(text) => (item.label, Control::Value(text), Act::None),
            schema::Kind::Button(action) => (
                String::new(),
                Control::Button(item.label),
                Act::Button(action),
            ),
        };
        self.actions.push(act);
        Row {
            id,
            label,
            control,
            note: item.note,
            // Buttons aren't settings: they work while the file has an error.
            enabled: enabled || matches!(self.actions[id], Act::Button(_)),
        }
    }

    /// The Metrics page, after the settings.
    fn metrics_page(&mut self) -> Page {
        let actions = &mut self.actions;
        let sections = self.metrics.sections(&mut |act| {
            actions.push(act.map_or(Act::None, Act::Metrics));
            actions.len() - 1
        });
        Page {
            title: metrics::TITLE.into(),
            symbol: metrics::SYMBOL,
            sections,
        }
    }

    /// A section with the button that shows or hides the advanced settings (the last of
    /// every settings page).
    fn advanced_section(&mut self) -> Section {
        let id = self.actions.len();
        self.actions.push(Act::Advanced);
        let label = if self.advanced {
            "Hide advanced settings"
        } else {
            "Show advanced settings"
        };
        Section {
            title: String::new(),
            rows: vec![Row {
                id,
                label: String::new(),
                control: Control::Button(label.into()),
                note: String::new(),
                enabled: true,
            }],
        }
    }

    fn set(&mut self, changes: &[(SettingPath, SettingValue)]) -> Result<(), String> {
        let result = config::set(&self.path, changes);
        self.stamp = self.stamp();
        result.map(drop).map_err(|e| e.to_string())
    }
}

impl agt_settings_ui::Model for Model {
    fn pages(&mut self) -> Vec<Page> {
        self.stamp = self.stamp();
        self.dictionary_stamp = stamp(&paths::dictionary_file());
        self.actions.clear();
        // Not `load_or_create`: a file deleted while the window is open isn't brought back
        // with defaults (which AGT would then apply).
        let config = match Config::load(&self.path) {
            Ok(c) => c,
            Err(e) => {
                let rows = vec![
                    self.row(
                        schema::Item {
                            label: String::new(),
                            kind: schema::Kind::Info(format!(
                                "config.toml has an error; fix it to change settings here.\n\n{e}"
                            )),
                            note: String::new(),
                            advanced: false,
                        },
                        false,
                    ),
                    self.row(schema::edit_file(), true),
                ];
                return vec![
                    Page {
                        title: "Settings".into(),
                        symbol: "exclamationmark.triangle",
                        sections: vec![Section {
                            title: String::new(),
                            rows,
                        }],
                    },
                    self.metrics_page(),
                ];
            }
        };
        let pages = schema::pages(&config, self.advanced);
        // The buttons' ids come first, so they're the same either way: the focus (by id)
        // stays on the button that was clicked.
        let buttons: Vec<Section> = pages.iter().map(|_| self.advanced_section()).collect();
        let mut pages: Vec<Page> = pages
            .into_iter()
            .zip(buttons)
            .map(|(page, button)| {
                let mut sections: Vec<Section> = page
                    .sections
                    .into_iter()
                    .map(|s| Section {
                        title: s.title.into(),
                        rows: s.items.into_iter().map(|i| self.row(i, true)).collect(),
                    })
                    .collect();
                sections.push(button);
                Page {
                    title: page.title.into(),
                    symbol: page.symbol,
                    sections,
                }
            })
            .collect();
        pages.push(self.metrics_page());
        pages
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        let id = match &event {
            Event::Toggle(id, _)
            | Event::Choose(id, _)
            | Event::Text(id, _)
            | Event::Click(id)
            | Event::Hotkey(id, _) => *id,
        };
        let changes = match (self.actions.get(id), event) {
            (Some(Act::Check(path)), Event::Toggle(_, on)) => vec![(*path, on.into())],
            (Some(Act::Choice(options)), Event::Choose(_, i)) => {
                options.get(i).cloned().unwrap_or_default()
            }
            (Some(Act::Text(path, multiline)), Event::Text(_, text)) => {
                let text = if *multiline { &text } else { text.trim() };
                vec![(*path, text.into())]
            }
            (Some(Act::Metrics(metrics::Act::Period)), Event::Choose(_, i)) => {
                self.metrics.set_period(i);
                return Ok(());
            }
            (Some(Act::Metrics(metrics::Act::OpenFolder)), Event::Click(_)) => {
                crate::open_folder(&self.metrics.folder());
                return Ok(());
            }
            (Some(Act::Button(action)), Event::Click(_)) => {
                match action {
                    Action::EditFile => crate::open_in_editor(&self.path),
                    Action::EditDictionary => {
                        let path = paths::dictionary_file();
                        Dictionary::create_if_missing(&path)
                            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
                        crate::open_in_editor(&path);
                    }
                    Action::OpenHistory => crate::open_history(),
                }
                return Ok(());
            }
            // The window asks for the pages again after this.
            (Some(Act::Advanced), Event::Click(_)) => {
                self.advanced = !self.advanced;
                return Ok(());
            }
            _ => return Ok(()),
        };
        if changes.is_empty() {
            return Ok(());
        }
        self.set(&changes)
    }

    fn changed_elsewhere(&mut self) -> bool {
        let file =
            self.stamp() != self.stamp || stamp(&paths::dictionary_file()) != self.dictionary_stamp;
        // Always asked: it keeps the figures current.
        let figures = self.metrics.refresh();
        file || figures
    }
}
