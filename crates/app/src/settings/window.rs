//! The settings window: `agt settings`, a process of its own, so it costs dictation nothing
//! and is gone once closed. It changes `config.toml` the way the tray does, and the
//! Replacements page `dictionary.toml`; the running AGT notices a change (`watch_config`) and
//! applies the config once dictation is idle, the dictionary right away.

use super::{Action, metrics, schema};
use agt_core::{
    Config, Dictionary,
    config::{self, SettingPath, SettingValue},
    dictionary, paths,
};
use agt_platform::{Hotkey, Hotkeys};
use agt_settings_ui::{Control, Event, Page, Row, Section};
use std::{
    fs::{self, File, TryLockError},
    io,
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
    // The window's events (the key recorder's: which tap it has, ...) go to the app's database
    // too, with no session (see `agt_telemetry::Sink::start_events`).
    let log_level =
        Config::load(&paths::config_file()).map_or_else(|_| "info".into(), |c| c.log_level);
    crate::logging::init(&log_level);
    agt_telemetry::global().start_events(paths::db_file());

    let model = Model {
        path: paths::config_file(),
        dictionary_path: paths::dictionary_file(),
        actions: Vec::new(),
        stamp: None,
        dictionary_stamp: None,
        metrics: metrics::Metrics::new(paths::db_file()),
        advanced: false,
        adding_hotkey: false,
        hotkeys: Vec::new(),
        draft: schema::Draft::default(),
    };
    let icon = crate::tray::color_icon;
    if let Err(e) = agt_settings_ui::run("AGT Settings", Box::new(model), Some(page), Some(icon)) {
        tracing::error!("settings window: {e}");
    }
    // On macOS, closing the window ends the process before this; events are written as they
    // come, so only those of its last moment can be lost there.
    agt_telemetry::global().flush(crate::TELEMETRY_FLUSH);
}

/// What a row of the window does.
enum Act {
    Check(SettingPath),
    /// Per option, what it writes (nothing: the file's value, which isn't one of them).
    Choice(Vec<Vec<(SettingPath, SettingValue)>>),
    /// `multiline`: kept as typed; one line is trimmed.
    Text(SettingPath, bool),
    Button(Action),
    /// Hotkey `index` of the list (`None`: the one being added): recorded keys replace it (or
    /// join the list), its minus button removes it.
    Hotkey(Option<usize>),
    Replace(schema::ReplaceField),
    Metrics(metrics::Act),
    /// "Show advanced settings" / "Hide advanced settings".
    Advanced,
    None,
}

struct Model {
    path: PathBuf,
    /// `dictionary.toml` (the Replacements page writes it; "Edit dictionary..." opens it).
    dictionary_path: PathBuf,
    /// By row id, for the pages last shown.
    actions: Vec<Act>,
    /// The file's modification time and size when last read or written, and
    /// `dictionary.toml`'s when last read (its error is shown under "Edit dictionary...").
    stamp: Option<(Option<SystemTime>, u64)>,
    dictionary_stamp: Option<(Option<SystemTime>, u64)>,
    metrics: metrics::Metrics,
    /// Showing every setting, not just the basic ones (until the window closes).
    advanced: bool,
    /// Showing a row that listens for a new hotkey, until it reports its keys or is
    /// cancelled.
    adding_hotkey: bool,
    /// `[hotkey] key` as the pages last shown list it (what [`Act::Hotkey`]'s index means).
    hotkeys: Vec<String>,
    /// The replacement being typed on the Replacements page, until "Add replacement".
    draft: schema::Draft,
}

/// `[hotkey] key`.
const HOTKEY_KEY: SettingPath = &["hotkey", "key"];

/// Whether `new` can be hotkey `replacing` of `hotkeys` (`None`: added to them), by
/// [`Hotkeys::check_new`]'s rules and words. Each is checked on its own, so a list AGT can't
/// use (a name that isn't a hotkey, two that clash) can still be fixed here.
fn check_hotkey(hotkeys: &[String], new: &Hotkey, replacing: Option<usize>) -> Result<(), String> {
    for (i, spec) in hotkeys.iter().enumerate() {
        if Some(i) != replacing
            && let Ok(old) = Hotkeys::parse(std::slice::from_ref(spec))
        {
            old.check_new(new, None)?;
        }
    }
    Ok(())
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
            && let Err(e) = Dictionary::load(&self.dictionary_path)
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
            schema::Kind::Hotkey {
                index,
                label,
                removable,
                listening,
            } => (
                item.label,
                Control::Hotkey {
                    label,
                    listening,
                    removable,
                },
                Act::Hotkey(index),
            ),
            schema::Kind::Replacement {
                field,
                value,
                placeholder,
            } => (
                item.label,
                Control::Text {
                    value,
                    placeholder,
                    secret: false,
                },
                Act::Replace(field),
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

    /// The Replacements page's content, from `dictionary.toml` now.
    fn replacing(&self) -> schema::Replacing {
        let draft = self.draft.clone();
        match Dictionary::load(&self.dictionary_path) {
            Ok(d) => schema::Replacing {
                list: d
                    .replacements()
                    .iter()
                    .map(|r| {
                        let profiles = d.profiles_of(r).into_iter().map(Into::into).collect();
                        (r.from.clone(), r.to.clone(), profiles)
                    })
                    .collect(),
                broken: false,
                draft,
            },
            Err(_) => schema::Replacing {
                broken: true,
                draft,
                ..Default::default()
            },
        }
    }

    /// Writes `dictionary.toml` as `change` makes it from the file now (from the template if
    /// there's none yet). The running AGT reloads it.
    fn change_dictionary(
        &mut self,
        change: impl FnOnce(&str) -> Result<String, String>,
    ) -> Result<(), String> {
        let path = &self.dictionary_path;
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => dictionary::TEMPLATE.to_owned(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let out = change(&text)?;
        let saved = dictionary::save(path, &out)
            .map_err(|e| format!("cannot write {}: {e}", path.display()));
        self.dictionary_stamp = stamp(path);
        saved
    }

    fn add_replacement(&mut self) -> Result<(), String> {
        let schema::Draft { from, to } = self.draft.clone();
        if let Some(problem) = replacement_problem(&from, &to) {
            return Err(problem.into());
        }
        self.change_dictionary(|text| {
            dictionary::with_replacement(text, &from, &to).map_err(|e| e.to_string())
        })?;
        self.draft = schema::Draft::default();
        Ok(())
    }

    /// The listed replacement of `from` now inserts `to`; empty: it's removed.
    fn set_replacement(&mut self, from: &str, to: &str) -> Result<(), String> {
        self.change_dictionary(|text| {
            let d = Dictionary::parse(text).map_err(|e| e.to_string())?;
            // Changed in the file since the page was shown: not this entry.
            if !d.replacements().iter().any(|r| r.from == from) {
                return Err("dictionary.toml changed; try again.".into());
            }
            match to {
                "" => dictionary::without_replacement(text, from),
                to => dictionary::with_replacement(text, from, to),
            }
            .map_err(|e| e.to_string())
        })
    }
}

/// Why the speech and inserted text typed on the Replacements page can't be added, if they
/// can't.
fn replacement_problem(from: &str, to: &str) -> Option<&'static str> {
    if !from.chars().any(char::is_alphanumeric) {
        Some("Type the speech first: the words you say.")
    } else if to.is_empty() {
        Some("Type the inserted text: what AGT types when it hears the speech.")
    } else {
        None
    }
}

impl agt_settings_ui::Model for Model {
    fn pages(&mut self) -> Vec<Page> {
        self.stamp = self.stamp();
        self.dictionary_stamp = stamp(&self.dictionary_path);
        self.actions.clear();
        // Not `load_or_create`: a file deleted while the window is open isn't brought back
        // with defaults (which AGT would then apply).
        let config = match Config::load(&self.path) {
            Ok(c) => c,
            Err(e) => {
                // The row being added is gone with the page.
                self.adding_hotkey = false;
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
        self.hotkeys = config.hotkey.keys.clone();
        let replacing = self.replacing();
        let pages = schema::pages(&config, self.advanced, self.adding_hotkey, &replacing);
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
            (Some(Act::Replace(field)), Event::Text(_, text)) => {
                let text = text.trim().to_owned();
                match field.clone() {
                    schema::ReplaceField::NewFrom => self.draft.from = text,
                    schema::ReplaceField::NewTo => self.draft.to = text,
                    schema::ReplaceField::To(from) => return self.set_replacement(&from, &text),
                }
                return Ok(());
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
                        let path = &self.dictionary_path;
                        Dictionary::create_if_missing(path)
                            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
                        crate::open_in_editor(path);
                    }
                    Action::OpenHistory => crate::open_history(),
                    // The window asks for the pages again after this.
                    Action::AddHotkey => self.adding_hotkey = true,
                    Action::AddReplacement => return self.add_replacement(),
                }
                return Ok(());
            }
            (Some(Act::Hotkey(index)), Event::Hotkey(_, keys)) => {
                let index = *index;
                if index.is_none() {
                    // The row being added goes, whatever came of it (also when it couldn't
                    // listen: shown again, it would try again).
                    self.adding_hotkey = false;
                }
                let Some(codes) = keys else {
                    return Ok(());
                };
                let new = Hotkey::from_codes(&codes)?;
                check_hotkey(&self.hotkeys, &new, index)?;
                let mut hotkeys = self.hotkeys.clone();
                // The rows' indexes come from the same list.
                match index {
                    Some(i) => hotkeys[i] = new.to_string(),
                    None => hotkeys.push(new.to_string()),
                }
                vec![(HOTKEY_KEY, config::hotkey_value(&hotkeys))]
            }
            // Its minus button.
            (Some(Act::Hotkey(Some(i))), Event::Click(_)) => {
                let hotkeys: Vec<String> = self
                    .hotkeys
                    .iter()
                    .enumerate()
                    .filter(|&(j, _)| j != *i)
                    .map(|(_, spec)| spec.clone())
                    .collect();
                // Never none (the window shows no minus for the only one).
                if hotkeys.is_empty() {
                    return Err("At least one hotkey is needed.".into());
                }
                vec![(HOTKEY_KEY, config::hotkey_value(&hotkeys))]
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
            self.stamp() != self.stamp || stamp(&self.dictionary_path) != self.dictionary_stamp;
        // Always asked: it keeps the figures current.
        let figures = self.metrics.refresh();
        file || figures
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agt_settings_ui::Model as _;

    /// A hotkey row as the window shows it: its id, keys, whether it listens and whether it
    /// has a minus button.
    type HotkeyRow = (usize, String, bool, bool);

    /// The hotkey rows of `model`'s pages, and the "+ Add hotkey" button's id.
    fn shown(model: &mut Model) -> (Vec<HotkeyRow>, usize) {
        let pages = model.pages();
        let rows: Vec<&Row> = pages
            .iter()
            .flat_map(|p| &p.sections)
            .flat_map(|s| &s.rows)
            .collect();
        let hotkeys = rows
            .iter()
            .filter_map(|r| match &r.control {
                Control::Hotkey {
                    label,
                    listening,
                    removable,
                } => Some((r.id, label.clone(), *listening, *removable)),
                _ => None,
            })
            .collect();
        let add = rows
            .iter()
            .find(|r| r.control == Control::Button("+ Add hotkey".into()))
            .expect("an \"+ Add hotkey\" button")
            .id;
        (hotkeys, add)
    }

    /// A window on `dir`'s config.toml, dictionary.toml and agt.db.
    fn test_model(dir: &std::path::Path) -> Model {
        Model {
            path: dir.join("config.toml"),
            dictionary_path: dir.join("dictionary.toml"),
            actions: Vec::new(),
            stamp: None,
            dictionary_stamp: None,
            metrics: metrics::Metrics::new(dir.join("agt.db")),
            advanced: false,
            adding_hotkey: false,
            hotkeys: Vec::new(),
            draft: schema::Draft::default(),
        }
    }

    /// Run by the settings schema's only test that builds pages (see there).
    pub(crate) fn hotkeys_are_set_by_pressing_them() {
        let dir = std::env::temp_dir().join(format!("agt-settings-hotkeys-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut model = test_model(&dir);
        let code = |name| agt_platform::Key::parse(name).unwrap().code();
        let file = || fs::read_to_string(&path).unwrap();
        let start = |text: &str| fs::write(&path, format!("[hotkey]\n{text}\n")).unwrap();
        let keys = |line: &str| format!("\n{line}\n");

        // "+ Add hotkey" adds a row that listens; the keys pressed there join the list.
        start("key = \"F13\"");
        let (rows, add) = shown(&mut model);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].1.as_str(), rows[0].2, rows[0].3),
            ("F13", false, false)
        );
        assert_eq!(model.handle(Event::Click(add)), Ok(()));
        let (rows, _) = shown(&mut model);
        let added = rows[1].clone();
        assert_eq!((added.1.as_str(), added.2, added.3), ("", true, false));
        let f14 = Some(vec![code("F14")]);
        assert_eq!(model.handle(Event::Hotkey(added.0, f14.clone())), Ok(()));
        assert!(
            file().contains(&keys("key = [\"F13\", \"F14\"]")),
            "{}",
            file()
        );
        let (rows, _) = shown(&mut model);
        let labels: Vec<_> = rows.iter().map(|r| (r.1.as_str(), r.2, r.3)).collect();
        assert_eq!(labels, [("F13", false, true), ("F14", false, true)]);

        // Cancelled (or the recorder couldn't start): nothing written, the row goes.
        start("key = \"F13\"");
        let (_, add) = shown(&mut model);
        model.handle(Event::Click(add)).unwrap();
        let (rows, _) = shown(&mut model);
        assert_eq!(model.handle(Event::Hotkey(rows[1].0, None)), Ok(()));
        assert_eq!(file(), "[hotkey]\nkey = \"F13\"\n");
        assert_eq!(shown(&mut model).0.len(), 1);

        // Recording a row again replaces it; one hotkey is written as a string.
        let (rows, _) = shown(&mut model);
        let f15 = Some(vec![code("F15")]);
        assert_eq!(model.handle(Event::Hotkey(rows[0].0, f15)), Ok(()));
        assert!(file().contains(&keys("key = \"F15\"")), "{}", file());

        // A refusal says why and writes nothing; the row being added goes too.
        start("key = \"F13\"");
        let (_, add) = shown(&mut model);
        model.handle(Event::Click(add)).unwrap();
        let (rows, _) = shown(&mut model);
        let f13 = Some(vec![code("F13")]);
        assert_eq!(
            model.handle(Event::Hotkey(rows[1].0, f13)),
            Err("That's already a hotkey.".into())
        );
        assert_eq!(file(), "[hotkey]\nkey = \"F13\"\n");
        assert_eq!(shown(&mut model).0.len(), 1);
        // The one already there is named first, whichever is shorter.
        start("key = \"LeftShift+A\"");
        let (_, add) = shown(&mut model);
        model.handle(Event::Click(add)).unwrap();
        let (rows, _) = shown(&mut model);
        let shift = Some(vec![code("LeftShift")]);
        assert_eq!(
            model.handle(Event::Hotkey(rows[1].0, shift)),
            Err("Left Shift + A is already a hotkey, and it starts with Left Shift.".into())
        );
        // So are the recorder's own refusals.
        let (_, add) = shown(&mut model);
        model.handle(Event::Click(add)).unwrap();
        let (rows, _) = shown(&mut model);
        let two = Some(vec![code("F13"), code("F14")]);
        assert_eq!(
            model.handle(Event::Hotkey(rows[1].0, two)),
            Err("A hotkey can have only one key that isn't a modifier (F13, F14).".into())
        );
        assert_eq!(file(), "[hotkey]\nkey = \"LeftShift+A\"\n");

        // A name the file gets wrong can be recorded over (it isn't checked against).
        start("key = [\"Hyper\", \"F13\"]");
        let (rows, _) = shown(&mut model);
        assert_eq!(rows[0].1, "Hyper");
        assert_eq!(model.handle(Event::Hotkey(rows[0].0, f14)), Ok(()));
        assert!(
            file().contains(&keys("key = [\"F14\", \"F13\"]")),
            "{}",
            file()
        );

        // Recording a row with its own keys again changes nothing (it isn't checked against
        // itself).
        start("key = [\"F13\", \"F14\"]");
        let (rows, _) = shown(&mut model);
        let same = Some(vec![code("F13")]);
        assert_eq!(model.handle(Event::Hotkey(rows[0].0, same)), Ok(()));
        assert!(
            file().contains(&keys("key = [\"F13\", \"F14\"]")),
            "{}",
            file()
        );
        let (rows, _) = shown(&mut model);
        let labels: Vec<_> = rows.iter().map(|r| (r.1.as_str(), r.2, r.3)).collect();
        assert_eq!(labels, [("F13", false, true), ("F14", false, true)]);

        // The modifiers are saved first, whatever order they went down in.
        let space_then_shift = Some(vec![code("Space"), code("LeftShift")]);
        assert_eq!(
            model.handle(Event::Hotkey(rows[1].0, space_then_shift)),
            Ok(())
        );
        assert!(
            file().contains(&keys("key = [\"F13\", \"LeftShift+Space\"]")),
            "{}",
            file()
        );
        assert_eq!(shown(&mut model).0[1].1, "Left Shift + Space");

        // The minus removes a hotkey; the last one stays.
        start("key = [\"F13\", \"F14\"]");
        let (rows, _) = shown(&mut model);
        assert_eq!(model.handle(Event::Click(rows[0].0)), Ok(()));
        assert!(file().contains(&keys("key = \"F14\"")), "{}", file());
        let (rows, _) = shown(&mut model);
        assert_eq!(
            model.handle(Event::Click(rows[0].0)),
            Err("At least one hotkey is needed.".into())
        );
        assert!(file().contains(&keys("key = \"F14\"")), "{}", file());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The Replacements page's rows: (id, label, value) of each text field, and the "Add
    /// replacement" button's id.
    fn replacement_rows(model: &mut Model) -> (Vec<(usize, String, String)>, usize) {
        let pages = model.pages();
        let page = pages
            .iter()
            .find(|p| p.title == "Replacements")
            .expect("a Replacements page");
        let rows: Vec<&Row> = page.sections.iter().flat_map(|s| &s.rows).collect();
        let fields = rows
            .iter()
            .filter_map(|r| match &r.control {
                Control::Text { value, .. } => Some((r.id, r.label.clone(), value.clone())),
                _ => None,
            })
            .collect();
        let add = rows
            .iter()
            .find(|r| r.control == Control::Button("Add replacement".into()))
            .expect("an \"Add replacement\" button")
            .id;
        (fields, add)
    }

    /// Run by the settings schema's only test that builds pages (see there).
    pub(crate) fn replacements_are_set_on_their_page() {
        let dir = std::env::temp_dir().join(format!("agt-settings-replace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), "").unwrap();
        let mut model = test_model(&dir);
        let dictionary = dir.join("dictionary.toml");
        let file = || fs::read_to_string(&dictionary).unwrap();
        let labels = |fields: &[(usize, String, String)]| {
            fields
                .iter()
                .map(|(_, label, value)| (label.clone(), value.clone()))
                .collect::<Vec<_>>()
        };
        let s = String::from;

        // No file yet: the two fields of a new one, nothing listed.
        let (fields, add) = replacement_rows(&mut model);
        assert_eq!(
            labels(&fields),
            [(s("Speech"), s("")), (s("Inserted text"), s(""))]
        );
        // Both are needed; nothing is written without them.
        assert_eq!(
            model.handle(Event::Click(add)),
            Err("Type the speech first: the words you say.".into())
        );
        model
            .handle(Event::Text(fields[0].0, " Reddit True Tide ".into()))
            .unwrap();
        assert_eq!(
            model.handle(Event::Click(add)),
            Err("Type the inserted text: what AGT types when it hears the speech.".into())
        );
        assert!(!dictionary.exists());

        // Added: the file is made from the template, the fields empty again, and it's listed.
        model
            .handle(Event::Text(fields[1].0, "retatrutide".into()))
            .unwrap();
        assert_eq!(model.handle(Event::Click(add)), Ok(()));
        assert!(
            file().starts_with(dictionary::TEMPLATE.trim_end()),
            "{}",
            file()
        );
        let d = Dictionary::parse(&file()).unwrap();
        assert_eq!(d.replace("a reddit true tide b").0, "a retatrutide b");
        let (fields, add) = replacement_rows(&mut model);
        assert_eq!(
            labels(&fields),
            [
                (s("Speech"), s("")),
                (s("Inserted text"), s("")),
                (s("Reddit True Tide"), s("retatrutide")),
            ]
        );

        // The same speech again (case and spacing don't count) changes that one.
        model
            .handle(Event::Text(fields[0].0, "reddit  true tide".into()))
            .unwrap();
        model
            .handle(Event::Text(fields[1].0, "Retatrutide".into()))
            .unwrap();
        assert_eq!(model.handle(Event::Click(add)), Ok(()));
        let (fields, _) = replacement_rows(&mut model);
        assert_eq!(
            labels(&fields)[2..],
            [(s("Reddit True Tide"), s("Retatrutide"))]
        );

        // Changed in its field; emptied, it's removed.
        assert_eq!(
            model.handle(Event::Text(fields[2].0, "retatrutide ".into())),
            Ok(())
        );
        assert!(file().contains("to = \"retatrutide\"\n"), "{}", file());
        assert_eq!(model.handle(Event::Text(fields[2].0, "".into())), Ok(()));
        assert_eq!(Dictionary::parse(&file()).unwrap().replacements(), []);
        assert_eq!(replacement_rows(&mut model).0.len(), 2);

        // A profile's replacement says so; one removed from the file since the page was
        // shown isn't brought back.
        fs::write(
            &dictionary,
            "[profiles.code]\napps = [\"Code.exe\"]\n\n\
             [[replace]]\nfrom = \"dot env\"\nto = \".env\"\nprofiles = [\"code\"]\n",
        )
        .unwrap();
        let (fields, _) = replacement_rows(&mut model);
        assert_eq!(labels(&fields)[2..], [(s("dot env (in code)"), s(".env"))]);
        fs::write(&dictionary, "").unwrap();
        assert_eq!(
            model.handle(Event::Text(fields[2].0, ".envrc".into())),
            Err("dictionary.toml changed; try again.".into())
        );
        assert_eq!(file(), "");

        // A file with an error: listed as such, and left as it is.
        fs::write(&dictionary, "terms = [").unwrap();
        let (fields, add) = replacement_rows(&mut model);
        assert_eq!(fields.len(), 2);
        model.handle(Event::Text(fields[0].0, "a".into())).unwrap();
        model.handle(Event::Text(fields[1].0, "b".into())).unwrap();
        assert!(model.handle(Event::Click(add)).is_err());
        assert_eq!(file(), "terms = [");

        let _ = fs::remove_dir_all(&dir);
    }
}
