//! The settings window: `agt settings`, a process of its own, so it costs dictation nothing
//! and is gone once closed. It changes `config.toml` the way the tray does; the running AGT
//! notices the change (`watch_config`) and applies it once dictation is idle.

use super::{Action, schema};
use agt_core::{
    Config,
    config::{self, SettingPath, SettingValue},
    paths,
};
use agt_settings_ui::{Control, Event, Page, Row, Section};
use std::{
    fs::{self, File, TryLockError},
    path::PathBuf,
    time::SystemTime,
};

/// Opens the window: starts `agt settings`, which brings an open one to the front instead.
pub fn open() {
    // Lets the new process take the foreground (Windows only hands it to the process the
    // user last clicked in, AGT, which spawns it).
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
            windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
        );
    }
    let spawned = std::env::current_exe()
        .and_then(|exe| std::process::Command::new(exe).arg("settings").spawn());
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

/// `agt settings`. One window at a time: if one is open, this brings it to the front.
pub fn run() {
    let dir = paths::data_dir();
    let _ = fs::create_dir_all(&dir);
    let pid_file = dir.join("settings.pid");
    // Held while the window is open; the OS releases it on exit.
    let _lock = match File::create(dir.join("settings.lock")) {
        Ok(file) => match file.try_lock() {
            Ok(()) => Some(file),
            Err(TryLockError::WouldBlock) => {
                let pid = fs::read_to_string(&pid_file).ok();
                if pid
                    .and_then(|p| p.trim().parse().ok())
                    .is_some_and(agt_settings_ui::activate)
                {
                    return;
                }
                // Not found (it's just exiting): open another.
                None
            }
            Err(TryLockError::Error(_)) => None,
        },
        Err(_) => None,
    };
    let _ = fs::write(&pid_file, std::process::id().to_string());

    let model = Model {
        path: paths::config_file(),
        actions: Vec::new(),
        stamp: None,
    };
    if let Err(e) = agt_settings_ui::run("AGT Settings", Box::new(model)) {
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
    None,
}

struct Model {
    path: PathBuf,
    /// By row id, for the pages last shown.
    actions: Vec<Act>,
    /// The file's modification time and size when last read or written.
    stamp: Option<(Option<SystemTime>, u64)>,
}

impl Model {
    fn stamp(&self) -> Option<(Option<SystemTime>, u64)> {
        fs::metadata(&self.path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()))
    }

    fn row(&mut self, item: schema::Item, enabled: bool) -> Row {
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

    fn set(&mut self, changes: &[(SettingPath, SettingValue)]) -> Result<(), String> {
        let result = config::set(&self.path, changes);
        self.stamp = self.stamp();
        result.map(drop).map_err(|e| e.to_string())
    }
}

impl agt_settings_ui::Model for Model {
    fn pages(&mut self) -> Vec<Page> {
        self.stamp = self.stamp();
        self.actions.clear();
        let config = match Config::load_or_create(&self.path) {
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
                        },
                        false,
                    ),
                    self.row(schema::edit_file(), true),
                ];
                return vec![Page {
                    title: "Settings".into(),
                    symbol: "exclamationmark.triangle",
                    sections: vec![Section {
                        title: String::new(),
                        rows,
                    }],
                }];
            }
        };
        schema::pages(&config)
            .into_iter()
            .map(|page| Page {
                title: page.title.into(),
                symbol: page.symbol,
                sections: page
                    .sections
                    .into_iter()
                    .map(|s| Section {
                        title: s.title.into(),
                        rows: s.items.into_iter().map(|i| self.row(i, true)).collect(),
                    })
                    .collect(),
            })
            .collect()
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        let id = match &event {
            Event::Toggle(id, _) | Event::Choose(id, _) | Event::Text(id, _) | Event::Click(id) => {
                *id
            }
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
            (Some(Act::Button(action)), Event::Click(_)) => {
                match action {
                    Action::EditFile => crate::open_in_editor(&self.path),
                    Action::OpenHistory => crate::open_history(),
                    _ => {}
                }
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
        self.stamp() != self.stamp
    }
}
