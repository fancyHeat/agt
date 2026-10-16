//! The tray's Settings submenu: every `config.toml` setting, read from the file and written
//! back to it ([`agt_core::config::set`]); the file stays the only place settings live.
//! Choices (on/off, enums, a few presets for numbers) are clickable; free text (API key,
//! prompts, URLs, the models folder) is shown and changed in the file ("Edit config.toml...").

use agt_core::config::{
    self, Config, Device, Engine, OverlayPosition, PolishProvider, PolishStyle, Precision,
    S1Context, S1Device, S1Structure, S1Styling, SettingPath, SettingValue,
};
use std::collections::HashMap;
use tray_icon::menu::{CheckMenuItem, Error, MenuId, MenuItem, PredefinedMenuItem, Submenu};

/// What clicking a Settings item does.
#[derive(Clone)]
pub enum Action {
    /// Write these values to the file.
    Set(Vec<(SettingPath, SettingValue)>),
    /// Flip an on/off setting: the file's value at click time (`get` reads it), not the
    /// menu's, which may be up to a poll behind an edit made elsewhere.
    Toggle(SettingPath, fn(&Config) -> bool),
    /// Open `config.toml` in a text editor.
    EditFile,
    /// Restart now instead of waiting for dictation to be idle.
    ApplyNow,
}

pub struct Settings {
    pub menu: Submenu,
    actions: HashMap<MenuId, Action>,
}

impl Settings {
    pub fn new() -> Self {
        Self {
            menu: Submenu::new("Settings", true),
            actions: HashMap::new(),
        }
    }

    pub fn action(&self, id: &MenuId) -> Option<&Action> {
        self.actions.get(id)
    }

    /// Rebuilds the submenu for `config` (the file's contents). `editable`: false while the
    /// file has an error (it's never rewritten then); the items show the running settings.
    /// `applying`: a change is waiting for a restart.
    pub fn show(&mut self, config: &Config, editable: bool, applying: bool) -> Result<(), Error> {
        while self.menu.remove_at(0).is_some() {}
        self.actions.clear();
        self.menu.set_text(if applying {
            "Settings (applying...)"
        } else {
            "Settings"
        });
        let mut b = Builder {
            actions: &mut self.actions,
            config,
            editable,
        };
        if applying {
            b.item(&self.menu, "Restart now to apply", Action::ApplyNow)?;
            self.menu.append(&PredefinedMenuItem::separator())?;
        }
        if !editable {
            b.info(
                &self.menu,
                "config.toml has an error: fix it to change settings",
            )?;
        }
        add_all(&mut b, &self.menu, config)?;
        self.menu.append(&PredefinedMenuItem::separator())?;
        Builder {
            editable: true,
            ..b
        }
        .item(&self.menu, "Edit config.toml...", Action::EditFile)?;
        Ok(())
    }
}

fn add_all(b: &mut Builder, menu: &Submenu, c: &Config) -> Result<(), Error> {
    // Hotkey.
    let hotkey = b.submenu(menu, "Hotkey")?;
    b.choice(&hotkey, "Key", hotkey_choices(&c.hotkey.key))?;
    b.choice(
        &hotkey,
        "Minimum hold",
        numbers(
            &["hotkey", "min_hold_ms"],
            c.hotkey.min_hold_ms,
            &[100, 150, 250, 400],
            ms,
        ),
    )?;

    b.check(
        menu,
        "Keep microphone open",
        &["audio", "keep_mic_open"],
        |c| c.audio.keep_mic_open,
    )?;
    b.check(
        menu,
        "Lower other audio while dictating",
        &["audio", "duck"],
        |c| c.audio.duck,
    )?;
    b.choice(
        menu,
        "Lower other audio to",
        duck_levels(c.audio.duck_level),
    )?;

    // Speech-to-text.
    let stt = b.submenu(menu, "Speech-to-text")?;
    let engines = Engine::ALL
        .iter()
        .copied()
        .filter(|&e| !agt_stt::model_names(e).is_empty() || e == c.stt.engine);
    let engine_choices = engines
        .map(|e| {
            // A model of the new engine comes with it: the old one's name means nothing there.
            // Re-picking the current engine changes nothing (it would reset the model).
            let model = agt_stt::model_names(e).first().copied().unwrap_or_default();
            let changes = if e == c.stt.engine {
                Vec::new()
            } else {
                vec![
                    (&["stt", "engine"][..], e.as_str().into()),
                    (&["stt", "model"][..], model.into()),
                ]
            };
            Choice {
                label: engine_label(e).into(),
                changes,
                checked: e == c.stt.engine,
            }
        })
        .collect();
    b.choice(&stt, "Engine", engine_choices)?;
    let mut models = agt_stt::model_names(c.stt.engine);
    if !models.contains(&c.stt.model.as_str()) {
        models.push(&c.stt.model);
    }
    let model_choices = models
        .into_iter()
        .map(|m| {
            Choice::set(
                &["stt", "model"],
                model_label(m),
                m.into(),
                m == c.stt.model,
            )
        })
        .collect();
    b.choice(&stt, "Model", model_choices)?;
    if c.stt.engine == Engine::Parakeet {
        b.choice(
            &stt,
            "Precision",
            enums(
                &["stt", "precision"],
                Precision::ALL,
                c.stt.precision,
                |p| match p {
                    Precision::Int8 => "int8 (650 MB)",
                    Precision::Fp32 => "fp32 (2.5 GB)",
                },
            ),
        )?;
    }
    let devices: Vec<Device> = Device::ALL
        .iter()
        .copied()
        .filter(|&d| device_available(d, c.stt.engine) || d == c.stt.device)
        .collect();
    b.choice(
        &stt,
        "Device",
        enums(&["stt", "device"], &devices, c.stt.device, device_label),
    )?;
    b.choice(
        &stt,
        "CPU threads",
        numbers(
            &["stt", "threads"],
            c.stt.threads as u64,
            &[0, 2, 4, 8],
            |n| match n {
                0 => "Automatic".into(),
                n => n.to_string(),
            },
        ),
    )?;
    let models_dir = c.stt.models_path();
    b.info(&stt, &format!("Models folder: {}", models_dir.display()))?;

    // Paste.
    let paste = b.submenu(menu, "Paste")?;
    b.check(
        &paste,
        "Add a space after the text",
        &["paste", "trailing_space"],
        |c| c.paste.trailing_space,
    )?;
    b.choice(
        &paste,
        "Restore clipboard after",
        numbers(
            &["paste", "restore_clipboard_ms"],
            c.paste.restore_clipboard_ms,
            &[0, 250, 500, 1000, 2000],
            |n| if n == 0 { "Never".into() } else { ms(n) },
        ),
    )?;

    add_polish(b, menu, c)?;

    // Overlay.
    let overlay = b.submenu(menu, "Overlay")?;
    b.check(&overlay, "Show overlay", &["overlay", "enabled"], |c| {
        c.overlay.enabled
    })?;
    b.choice(
        &overlay,
        "Position",
        enums(
            &["overlay", "position"],
            OverlayPosition::ALL,
            c.overlay.position,
            |p| match p {
                OverlayPosition::Bottom => "Bottom",
                OverlayPosition::Top => "Top",
            },
        ),
    )?;

    // Updates.
    let update = b.submenu(menu, "Updates")?;
    b.check(
        &update,
        "Check automatically",
        &["update", "enabled"],
        |c| c.update.enabled,
    )?;
    b.choice(
        &update,
        "Check every",
        numbers(
            &["update", "check_interval_minutes"],
            c.update.check_interval_minutes,
            &[60, 240, 720, 1440],
            |m| match m {
                m if m % 60 == 0 => format!("{} h", m / 60),
                m => format!("{m} min"),
            },
        ),
    )?;

    let levels = ["error", "warn", "info", "debug", "trace"];
    let mut level_choices: Vec<Choice> = levels
        .iter()
        .map(|&l| Choice::set(&["log_level"], l.into(), l.into(), l == c.log_level))
        .collect();
    if !levels.contains(&c.log_level.as_str()) {
        level_choices.push(Choice::current(&c.log_level));
    }
    b.choice(menu, "Log level", level_choices)
}

fn add_polish(b: &mut Builder, menu: &Submenu, c: &Config) -> Result<(), Error> {
    let p = &c.polish;
    let polish = b.submenu(menu, "Polish")?;
    b.check(&polish, "Polish transcripts", &["polish", "enabled"], |c| {
        c.polish.enabled
    })?;
    b.choice(
        &polish,
        "Provider",
        enums(
            &["polish", "provider"],
            PolishProvider::ALL,
            p.provider,
            |p| match p {
                PolishProvider::S1Mini => "S1-mini (on this machine)",
                PolishProvider::OpenAi => "OpenAI-compatible server",
                PolishProvider::Anthropic => "Claude API",
            },
        ),
    )?;
    if p.provider == PolishProvider::S1Mini {
        let s1 = &p.s1_mini;
        b.choice(
            &polish,
            "Styling",
            enums(
                &["polish", "s1-mini", "styling"],
                S1Styling::ALL,
                s1.styling,
                |s| match s {
                    S1Styling::Casual => "Casual",
                    S1Styling::SemiCasual => "Semi-casual",
                    S1Styling::SemiFormal => "Semi-formal",
                    S1Styling::Formal => "Formal",
                },
            ),
        )?;
        b.choice(
            &polish,
            "Structure",
            enums(
                &["polish", "s1-mini", "structure"],
                S1Structure::ALL,
                s1.structure,
                |s| match s {
                    S1Structure::Prose => "Prose",
                    S1Structure::Lists => "Lists",
                },
            ),
        )?;
        b.choice(
            &polish,
            "Context",
            enums(
                &["polish", "s1-mini", "context"],
                S1Context::ALL,
                s1.context,
                |s| match s {
                    S1Context::General => "General",
                    S1Context::Email => "Email",
                },
            ),
        )?;
        b.choice(
            &polish,
            "Device",
            enums(
                &["polish", "s1-mini", "device"],
                S1Device::ALL,
                s1.device,
                |d| match d {
                    S1Device::Auto => "Automatic (GPU if there is one)",
                    S1Device::Cpu => "CPU",
                },
            ),
        )?;
    } else {
        b.choice(
            &polish,
            "Style",
            enums(
                &["polish", "style"],
                PolishStyle::ALL,
                p.style,
                |s| match s {
                    PolishStyle::Light => "Light",
                    PolishStyle::Formal => "Formal",
                },
            ),
        )?;
    }
    b.choice(
        &polish,
        "Timeout",
        numbers(
            &["polish", "timeout_ms"],
            p.timeout_ms,
            &[0, 1000, 2000, 3000, 5000],
            |n| {
                if n == 0 {
                    "Provider default".into()
                } else {
                    ms(n)
                }
            },
        ),
    )?;
    b.choice(
        &polish,
        "Extra time per word",
        numbers(
            &["polish", "timeout_per_word_ms"],
            p.timeout_per_word_ms,
            &[0, 15, 30],
            ms,
        ),
    )?;
    b.choice(
        &polish,
        "Skip transcripts under",
        numbers(
            &["polish", "min_words"],
            p.min_words as u64,
            &[1, 4, 8],
            |n| format!("{n} words"),
        ),
    )?;
    // Free text: shown here, changed in the file. Never the key itself.
    polish.append(&PredefinedMenuItem::separator())?;
    if p.provider != PolishProvider::S1Mini {
        b.info(
            &polish,
            &format!("Server: {}", without_userinfo(p.base_url())),
        )?;
        b.info(&polish, &format!("Model: {}", p.model()))?;
        let key = match (p.api_key.trim().is_empty(), p.api_key_var()) {
            (false, _) => "API key: in config.toml".to_owned(),
            (true, Some(var)) if p.api_key().is_some() => format!("API key: from {var}"),
            (true, Some(var)) => format!("API key: none ({var} is not set)"),
            (true, None) => "API key: none".to_owned(),
        };
        b.info(&polish, &key)?;
    }
    let custom = |s: &str| if s.is_empty() { "built-in" } else { "custom" };
    b.info(&polish, &format!("Prompt: {}", custom(&p.prompt)))?;
    b.info(
        &polish,
        &format!("User message: {}", custom(&p.user_message)),
    )?;
    b.info(&polish, "Edit these in config.toml")
}

struct Builder<'a> {
    actions: &'a mut HashMap<MenuId, Action>,
    config: &'a Config,
    editable: bool,
}

/// One option of a [`Builder::choice`].
struct Choice {
    label: String,
    /// Empty: shown (checked) but not clickable, for a value from the file the menu doesn't
    /// offer.
    changes: Vec<(SettingPath, SettingValue)>,
    checked: bool,
}

impl Choice {
    fn set(path: SettingPath, label: String, value: SettingValue, checked: bool) -> Self {
        Self {
            label,
            changes: vec![(path, value)],
            checked,
        }
    }

    /// The file's value when it isn't one of the options.
    fn current(value: &str) -> Self {
        Self {
            label: format!("{value} (from config.toml)"),
            changes: Vec::new(),
            checked: true,
        }
    }
}

impl Builder<'_> {
    fn item(&mut self, menu: &Submenu, label: &str, action: Action) -> Result<(), Error> {
        let item = MenuItem::new(label, self.editable, None);
        self.actions.insert(item.id().clone(), action);
        menu.append(&item)
    }

    /// A line that shows a value.
    fn info(&mut self, menu: &Submenu, text: &str) -> Result<(), Error> {
        menu.append(&MenuItem::new(text, false, None))
    }

    fn submenu(&mut self, menu: &Submenu, title: &str) -> Result<Submenu, Error> {
        let sub = Submenu::new(title, true);
        menu.append(&sub)?;
        Ok(sub)
    }

    fn check(
        &mut self,
        menu: &Submenu,
        label: &str,
        path: SettingPath,
        get: fn(&Config) -> bool,
    ) -> Result<(), Error> {
        let item = CheckMenuItem::new(label, self.editable, get(self.config), None);
        self.actions
            .insert(item.id().clone(), Action::Toggle(path, get));
        menu.append(&item)
    }

    /// A submenu titled "title: current" with one checked option. A click writes the file,
    /// and the menu is rebuilt from it, so the checkmarks always show the file.
    fn choice(&mut self, menu: &Submenu, title: &str, choices: Vec<Choice>) -> Result<(), Error> {
        let current = choices.iter().find(|c| c.checked).map(|c| c.label.as_str());
        let sub = Submenu::new(format!("{title}: {}", current.unwrap_or("?")), true);
        for c in choices {
            let clickable = self.editable && !c.changes.is_empty();
            let item = CheckMenuItem::new(&c.label, clickable, c.checked, None);
            if clickable {
                self.actions
                    .insert(item.id().clone(), Action::Set(c.changes));
            }
            sub.append(&item)?;
        }
        menu.append(&sub)
    }
}

fn enums<T: Copy + PartialEq + Named>(
    path: SettingPath,
    all: &[T],
    current: T,
    label: impl Fn(T) -> &'static str,
) -> Vec<Choice> {
    all.iter()
        .map(|&v| Choice::set(path, label(v).into(), v.name().into(), v == current))
        .collect()
}

/// `presets`, plus the file's value if it's another one.
fn numbers(
    path: SettingPath,
    current: u64,
    presets: &[u64],
    label: impl Fn(u64) -> String,
) -> Vec<Choice> {
    let mut values = presets.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    values
        .into_iter()
        .map(|n| {
            // Larger than any setting needs; saturating keeps a silly value from wrapping.
            let value = SettingValue::from(i64::try_from(n).unwrap_or(i64::MAX));
            Choice::set(path, label(n), value, n == current)
        })
        .collect()
}

/// `audio.duck_level` presets (percent of the volume kept), plus the file's value if it's
/// another one.
fn duck_levels(current: f32) -> Vec<Choice> {
    let pct = |v: f32| (f64::from(v) * 100.0).round();
    let mut choices: Vec<Choice> = [0, 10, 25, 50]
        .into_iter()
        .map(|p| {
            let label = if p == 0 {
                "Silent".into()
            } else {
                format!("{p}%")
            };
            let value = SettingValue::from(f64::from(p) / 100.0);
            Choice::set(
                &["audio", "duck_level"],
                label,
                value,
                pct(current) == f64::from(p),
            )
        })
        .collect();
    if !choices.iter().any(|c| c.checked) {
        choices.push(Choice::current(&current.to_string()));
    }
    choices
}

fn ms(n: u64) -> String {
    format!("{n} ms")
}

/// The keys `agt-platform` accepts on their own, the default chord, and the file's value
/// if it's another one (a chord the user wrote).
fn hotkey_choices(current: &str) -> Vec<Choice> {
    let mut names: Vec<&str> = agt_platform::KEYS
        .iter()
        .filter(|(_, _, kind)| *kind != agt_platform::KeyKind::ChordOnly)
        .map(|(name, _, _)| *name)
        .collect();
    if !names.contains(&config::DEFAULT_HOTKEY) {
        names.insert(0, config::DEFAULT_HOTKEY);
    }
    let is_current = |n: &str| n.eq_ignore_ascii_case(current.trim());
    let mut choices: Vec<Choice> = names
        .iter()
        .map(|&n| {
            let label = if n == config::DEFAULT_HOTKEY {
                format!("{n} (default)")
            } else {
                n.to_owned()
            };
            Choice::set(&["hotkey", "key"], label, n.into(), is_current(n))
        })
        .collect();
    if !names.iter().any(|n| is_current(n)) {
        choices.push(Choice::current(current));
    }
    choices
}

fn engine_label(e: Engine) -> &'static str {
    match e {
        Engine::Parakeet => "Parakeet",
        Engine::Whisper => "Whisper",
    }
}

fn model_label(m: &str) -> String {
    match m {
        "parakeet-tdt-0.6b-v2" => format!("{m} (English)"),
        "parakeet-tdt-0.6b-v3" => format!("{m} (25 European languages)"),
        m => m.to_owned(),
    }
}

/// Whether this build can use `d` with `engine` (see `[stt] device` in the default config).
fn device_available(d: Device, engine: Engine) -> bool {
    match (d, engine) {
        (Device::Auto | Device::Cpu, _) => true,
        (Device::DirectMl, Engine::Parakeet) => cfg!(windows),
        (Device::CoreMl, Engine::Parakeet) => cfg!(target_os = "macos"),
        (Device::Cuda, Engine::Parakeet) => cfg!(feature = "cuda"),
        (Device::Vulkan, Engine::Whisper) => cfg!(feature = "whisper-vulkan"),
        (Device::Metal, Engine::Whisper) => cfg!(feature = "whisper-metal"),
        _ => false,
    }
}

fn device_label(d: Device) -> &'static str {
    match d {
        Device::Auto => "Automatic",
        Device::Cpu => "CPU",
        Device::DirectMl => "DirectML (GPU)",
        Device::Cuda => "CUDA (NVIDIA GPU)",
        Device::CoreMl => "Core ML (GPU)",
        Device::Vulkan => "Vulkan (GPU)",
        Device::Metal => "Metal (GPU)",
    }
}

/// `scheme://user:pass@host/...` → `scheme://host/...`: the menu may be on a shared screen.
fn without_userinfo(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host_end = rest.find('/').unwrap_or(rest.len());
    let rest = match rest[..host_end].rfind('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    match scheme {
        "" => rest.to_owned(),
        s => format!("{s}://{rest}"),
    }
}

/// The config enums' names in the file, for [`enums`].
trait Named {
    fn name(self) -> &'static str;
}

macro_rules! named {
    ($($ty:ty),+) => {
        $(impl Named for $ty {
            fn name(self) -> &'static str {
                self.as_str()
            }
        })+
    };
}

named!(
    Device,
    Precision,
    PolishProvider,
    PolishStyle,
    S1Styling,
    S1Structure,
    S1Context,
    S1Device,
    OverlayPosition
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_credentials() {
        assert_eq!(
            without_userinfo("https://user:pw@example.com/v1"),
            "https://example.com/v1"
        );
        assert_eq!(
            without_userinfo("http://localhost:11434/v1"),
            "http://localhost:11434/v1"
        );
        assert_eq!(without_userinfo("a@b/c@d"), "b/c@d");
    }

    #[test]
    fn every_menu_value_is_valid() {
        // Each preset written into the default file gives a config that parses.
        let c = Config::default();
        let mut all = Vec::new();
        all.extend(hotkey_choices(&c.hotkey.key));
        all.extend(enums(
            &["stt", "device"],
            Device::ALL,
            c.stt.device,
            device_label,
        ));
        all.extend(numbers(
            &["paste", "restore_clipboard_ms"],
            7,
            &[0, 500],
            ms,
        ));
        all.extend(duck_levels(c.audio.duck_level));
        for choice in all.iter().filter(|c| !c.changes.is_empty()) {
            config::with_settings(config::DEFAULT_CONFIG, &choice.changes)
                .unwrap_or_else(|e| panic!("{}: {e}", choice.label));
        }
        // The default hotkey is offered and checked; a custom chord is shown as is.
        let keys = hotkey_choices(config::DEFAULT_HOTKEY);
        assert_eq!(keys.iter().filter(|c| c.checked).count(), 1);
        let keys = hotkey_choices("F13+F14+X");
        assert!(keys.last().unwrap().checked && keys.last().unwrap().changes.is_empty());
        // A non-preset number is listed, in order, and checked.
        let n = numbers(&["x"], 7, &[0, 500], ms);
        let labels: Vec<_> = n.iter().map(|c| (c.label.as_str(), c.checked)).collect();
        assert_eq!(labels, [("0 ms", false), ("7 ms", true), ("500 ms", false)]);
        // The default duck level is a preset; another one is shown as is.
        let levels = duck_levels(c.audio.duck_level);
        assert_eq!(levels.iter().filter(|c| c.checked).count(), 1);
        assert!(levels.iter().any(|c| c.checked && c.label == "10%"));
        let levels = duck_levels(0.3);
        assert!(levels.last().unwrap().checked && levels.last().unwrap().changes.is_empty());
    }
}
