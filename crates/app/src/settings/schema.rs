//! Every `config.toml` setting, described once for both places that show them: the tray's
//! Settings submenu ([`super::Settings`]) and the settings window ([`super::window`]).
//! Choices (on/off, enums, a few presets for numbers) are lists of what a click writes;
//! free text (API key, prompts, URLs, the models folder) is edited in the window and shown
//! as a line in the tray.

use super::Action;
use agt_core::config::{
    self, Config, Device, Engine, OverlayPosition, PolishProvider, PolishStyle, Precision,
    S1Context, S1Device, S1Structure, S1Styling, SettingPath, SettingValue,
};

/// A page of the window.
pub struct Page {
    pub title: &'static str,
    /// SF Symbol for the macOS toolbar.
    pub symbol: &'static str,
    pub tray: Tray,
    pub sections: Vec<Section>,
}

/// Where a page's settings go in the tray.
pub enum Tray {
    /// All in one submenu of this title, a separator between sections.
    Submenu(&'static str),
    /// Each section in a submenu of its title, or in the Settings submenu itself if it has
    /// none.
    Sections,
}

pub struct Section {
    /// "" for none.
    pub title: &'static str,
    pub items: Vec<Item>,
}

pub struct Item {
    pub label: String,
    pub kind: Kind,
    /// Help shown under the item in the window ("" for none).
    pub note: String,
}

pub enum Kind {
    /// On/off; `get` reads it from a config (the tray flips the file's value at click time).
    Check {
        path: SettingPath,
        get: fn(&Config) -> bool,
        on: bool,
    },
    /// One of these, the current one checked.
    Choice(Vec<Choice>),
    Text(Text),
    /// A line that shows a value.
    Info(String),
    /// Not a setting: works while the file has an error too.
    Button(Action),
}

pub struct Text {
    pub path: SettingPath,
    pub value: String,
    /// What "" means (e.g. the provider's default).
    pub placeholder: String,
    /// Shown as dots (and never in the tray).
    pub secret: bool,
    /// A few lines (prompts).
    pub multiline: bool,
    /// The tray's line about it.
    pub summary: String,
}

/// One option of a [`Kind::Choice`].
pub struct Choice {
    pub label: String,
    /// Empty: shown (checked) but not clickable, for a value from the file the list doesn't
    /// offer.
    pub changes: Vec<(SettingPath, SettingValue)>,
    pub checked: bool,
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

impl Item {
    fn new(label: impl Into<String>, kind: Kind) -> Self {
        Self {
            label: label.into(),
            kind,
            note: String::new(),
        }
    }

    fn check(label: &str, path: SettingPath, get: fn(&Config) -> bool, c: &Config) -> Self {
        Self::new(
            label,
            Kind::Check {
                path,
                get,
                on: get(c),
            },
        )
    }

    fn choice(label: &str, choices: Vec<Choice>) -> Self {
        Self::new(label, Kind::Choice(choices))
    }

    fn text(label: &str, text: Text) -> Self {
        Self::new(label, Kind::Text(text))
    }

    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
}

fn section(title: &'static str, items: Vec<Item>) -> Section {
    Section { title, items }
}

/// All settings, with `c`'s values.
pub fn pages(c: &Config) -> Vec<Page> {
    vec![general(c), speech(c), polish(c), history(c), advanced(c)]
}

fn general(c: &Config) -> Page {
    let hotkey = section(
        "Hotkey",
        vec![
            Item::choice("Key", hotkey_choices(&c.hotkey.key)).note(
                "Hold it to dictate, let go to paste. A chord written in config.toml is \
                 kept and shown as is.",
            ),
            Item::choice(
                "Minimum hold",
                numbers(
                    &["hotkey", "min_hold_ms"],
                    c.hotkey.min_hold_ms,
                    &[100, 150, 250, 400],
                    ms,
                ),
            ),
        ],
    );
    let audio = section(
        "",
        vec![
            Item::choice(
                "Microphone",
                microphone_choices(&c.audio.device, agt_audio::input_device_names()),
            ),
            Item::check(
                "Lower other audio while dictating",
                &["audio", "duck"],
                |c| c.audio.duck,
                c,
            ),
            Item::choice("Lower other audio to", duck_levels(c.audio.duck_level())),
        ],
    );
    let paste = section(
        "Paste",
        vec![
            Item::check(
                "Add a space after the text",
                &["paste", "trailing_space"],
                |c| c.paste.trailing_space,
                c,
            ),
            Item::choice(
                "Restore clipboard after",
                numbers(
                    &["paste", "restore_clipboard_ms"],
                    c.paste.restore_clipboard_ms,
                    &[0, 250, 500, 1000, 2000],
                    |n| if n == 0 { "Never".into() } else { ms(n) },
                ),
            )
            .note("Too short and slow apps may paste the old clipboard instead."),
        ],
    );
    let overlay = section(
        "Overlay",
        vec![
            Item::check(
                "Show overlay",
                &["overlay", "enabled"],
                |c| c.overlay.enabled,
                c,
            ),
            Item::choice(
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
            ),
        ],
    );
    Page {
        title: "General",
        symbol: "gearshape",
        tray: Tray::Sections,
        sections: vec![hotkey, audio, paste, overlay],
    }
}

fn speech(c: &Config) -> Page {
    let mut items = Vec::new();
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
    items.push(Item::choice("Engine", engine_choices));
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
    items.push(Item::choice("Model", model_choices));
    if c.stt.engine == Engine::Parakeet {
        items.push(Item::choice(
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
        ));
    }
    let devices: Vec<Device> = Device::ALL
        .iter()
        .copied()
        .filter(|&d| device_available(d, c.stt.engine) || d == c.stt.device)
        .collect();
    items.push(Item::choice(
        "Device",
        enums(&["stt", "device"], &devices, c.stt.device, device_label),
    ));
    items.push(Item::choice(
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
    ));
    let default_dir = config::SttConfig {
        models_dir: String::new(),
        ..c.stt.clone()
    }
    .models_path();
    items.push(
        Item::text(
            "Models folder",
            Text {
                path: &["stt", "models_dir"],
                value: c.stt.models_dir.clone(),
                placeholder: default_dir.display().to_string(),
                secret: false,
                multiline: false,
                summary: format!("Models folder: {}", c.stt.models_path().display()),
            },
        )
        .note("Where models are downloaded to. Empty: the default."),
    );
    Page {
        title: "Speech",
        symbol: "waveform",
        tray: Tray::Submenu("Speech-to-text"),
        sections: vec![section("", items)],
    }
}

fn polish(c: &Config) -> Page {
    let p = &c.polish;
    let mut main = vec![
        Item::check(
            "Polish transcripts",
            &["polish", "enabled"],
            |c| c.polish.enabled,
            c,
        )
        .note("Punctuation, capitalization, filler words and false starts, cleaned up by an LLM before pasting."),
        Item::choice(
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
        ),
    ];
    if p.provider == PolishProvider::S1Mini {
        let s1 = &p.s1_mini;
        main.push(Item::choice(
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
        ));
        main.push(Item::choice(
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
        ));
        main.push(Item::choice(
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
        ));
        main.push(Item::choice(
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
        ));
        main.push(
            Item::check(
                "Fast drafting",
                &["polish", "s1-mini", "fast_drafting"],
                |c| c.polish.s1_mini.fast_drafting,
                c,
            )
            .note("Up to 2-3x faster on long text with a GPU; now and then a word or comma comes out differently."),
        );
    } else {
        main.push(Item::choice(
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
        ));
    }
    main.push(Item::choice(
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
    ));
    main.push(Item::choice(
        "Extra time per word",
        numbers(
            &["polish", "timeout_per_word_ms"],
            p.timeout_per_word_ms,
            &[0, 15, 30],
            ms,
        ),
    ));
    main.push(
        Item::choice(
            "Skip transcripts under",
            numbers(
                &["polish", "min_words"],
                p.min_words as u64,
                &[1, 4, 8],
                |n| format!("{n} words"),
            ),
        )
        .note("On an error, a timeout or odd output, the transcript is pasted as it is."),
    );
    if let Some(e) = &c.polish_error {
        main.insert(0, Item::new("", Kind::Info(format!("Polish is off: {e}"))));
    }
    let mut sections = vec![section("", main)];

    if p.provider != PolishProvider::S1Mini {
        // What "" means for this provider.
        let defaults = config::PolishConfig {
            base_url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            ..p.clone()
        };
        let key = match (p.api_key.trim().is_empty(), p.api_key_var()) {
            (false, _) => "API key: in config.toml".to_owned(),
            (true, Some(var)) if p.api_key().is_some() => format!("API key: from {var}"),
            (true, Some(var)) => format!("API key: none ({var} is not set)"),
            (true, None) => "API key: none".to_owned(),
        };
        sections.push(section(
            "Server",
            vec![
                Item::text(
                    "Server URL",
                    Text {
                        path: &["polish", "base_url"],
                        value: p.base_url.clone(),
                        placeholder: defaults.base_url().into(),
                        secret: false,
                        multiline: false,
                        summary: format!("Server: {}", without_userinfo(p.base_url())),
                    },
                ),
                Item::text(
                    "Model",
                    Text {
                        path: &["polish", "model"],
                        value: p.model.clone(),
                        placeholder: defaults.model().into(),
                        secret: false,
                        multiline: false,
                        summary: format!("Model: {}", p.model()),
                    },
                ),
                Item::text(
                    "API key",
                    Text {
                        path: &["polish", "api_key"],
                        value: p.api_key.clone(),
                        placeholder: match defaults.api_key_var() {
                            Some(var) => format!("From {var}"),
                            None => "None".into(),
                        },
                        secret: true,
                        multiline: false,
                        summary: key,
                    },
                )
                .note("Kept in config.toml, on this machine only."),
                Item::text(
                    "Key variable",
                    Text {
                        path: &["polish", "api_key_env"],
                        value: p.api_key_env.clone(),
                        placeholder: defaults.api_key_var().unwrap_or("None").into(),
                        secret: false,
                        multiline: false,
                        summary: String::new(),
                    },
                )
                .note("An environment variable with the key, used when the field above is empty."),
            ],
        ));
    }

    let custom = |s: &str| if s.is_empty() { "built-in" } else { "custom" };
    sections.push(section(
        "Prompts",
        vec![
            Item::text(
                "Instructions",
                Text {
                    path: &["polish", "prompt"],
                    value: p.prompt.clone(),
                    placeholder: String::new(),
                    secret: false,
                    multiline: true,
                    summary: format!("Prompt: {}", custom(&p.prompt)),
                },
            )
            .note("Replaces the built-in system prompt. Empty: built-in."),
            Item::text(
                "User message",
                Text {
                    path: &["polish", "user_message"],
                    value: p.user_message.clone(),
                    placeholder: String::new(),
                    secret: false,
                    multiline: true,
                    summary: format!("User message: {}", custom(&p.user_message)),
                },
            )
            .note("{text} is replaced by the transcript. Empty: built-in."),
        ],
    ));
    Page {
        title: "Polish",
        symbol: "wand.and.stars",
        tray: Tray::Submenu("Polish"),
        sections,
    }
}

fn history(c: &Config) -> Page {
    let items = vec![
        Item::check(
            "Save transcripts",
            &["history", "save_text"],
            |c| c.history.save_text,
            c,
        )
        .note("The transcript, the polished text and the pasted text."),
        Item::check(
            "Save audio",
            &["history", "save_audio"],
            |c| c.history.save_audio,
            c,
        )
        .note("A WAV file per dictation, about 1.9 MB per minute of speech."),
        Item::choice("Keep for", keep_days(c.history.keep_days))
            .note("Stored only on this machine; never uploaded."),
        Item::new("Open history folder", Kind::Button(Action::OpenHistory)),
    ];
    Page {
        title: "History",
        symbol: "clock.arrow.circlepath",
        tray: Tray::Submenu("History"),
        sections: vec![section("", items)],
    }
}

fn advanced(c: &Config) -> Page {
    let updates = section(
        "Updates",
        vec![
            Item::check(
                "Check automatically",
                &["update", "enabled"],
                |c| c.update.enabled,
                c,
            ),
            Item::choice(
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
            ),
        ],
    );
    let levels = ["error", "warn", "info", "debug", "trace"];
    let mut level_choices: Vec<Choice> = levels
        .iter()
        .map(|&l| Choice::set(&["log_level"], l.into(), l.into(), l == c.log_level))
        .collect();
    if !levels.contains(&c.log_level.as_str()) {
        level_choices.push(Choice::current(&c.log_level));
    }
    let other = section(
        "",
        vec![
            Item::choice("Log level", level_choices).note("Which events are recorded in agt.db."),
            edit_file(),
        ],
    );
    Page {
        title: "Advanced",
        symbol: "slider.horizontal.3",
        tray: Tray::Sections,
        sections: vec![updates, other],
    }
}

/// "Edit config.toml...": every setting, with a comment on each.
pub fn edit_file() -> Item {
    Item::new("Edit config.toml...", Kind::Button(Action::EditFile))
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

/// `audio.duck_level` presets (percent of the volume kept), plus the level in use if it's
/// another one. `current`: the clamped level ([`agt_core::config::AudioConfig::duck_level`]).
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
        choices.push(Choice::current(&format!("{}%", pct(current))));
    }
    choices
}

fn ms(n: u64) -> String {
    format!("{n} ms")
}

fn keep_days(current: u64) -> Vec<Choice> {
    numbers(
        &["history", "keep_days"],
        current,
        &[0, 7, 30, 90],
        |n| match n {
            0 => "Forever".into(),
            1 => "1 day".into(),
            n => format!("{n} days"),
        },
    )
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

/// The system default, the connected input devices, and the file's device if it isn't
/// connected (still checked: AGT goes back to it when it is).
fn microphone_choices(current: &str, names: Vec<String>) -> Vec<Choice> {
    let path = &["audio", "device"];
    let mut choices = vec![Choice::set(
        path,
        "System default".into(),
        "".into(),
        current.is_empty(),
    )];
    if !current.is_empty() && !names.iter().any(|n| n == current) {
        choices.push(Choice::set(
            path,
            format!("{current} (not connected)"),
            current.into(),
            true,
        ));
    }
    choices.extend(names.into_iter().map(|n| {
        let checked = n == current;
        Choice::set(path, n.clone(), n.into(), checked)
    }));
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
    fn every_page_value_is_valid() {
        // Every choice and check of every page, for each polish provider, writes a valid file.
        for provider in PolishProvider::ALL {
            let text = config::with_settings(
                config::DEFAULT_CONFIG,
                &[(&["polish", "provider"], provider.as_str().into())],
            )
            .unwrap();
            let c = Config::parse(&text).unwrap();
            for page in pages(&c) {
                for item in page.sections.iter().flat_map(|s| &s.items) {
                    let changes: Vec<_> = match &item.kind {
                        Kind::Check { path, on, .. } => vec![vec![(*path, (!on).into())]],
                        Kind::Choice(choices) => {
                            choices.iter().map(|c| c.changes.clone()).collect()
                        }
                        Kind::Text(t) => vec![vec![(t.path, "x".into())]],
                        Kind::Info(_) | Kind::Button(_) => Vec::new(),
                    };
                    for change in changes {
                        config::with_settings(&text, &change)
                            .unwrap_or_else(|e| panic!("{}: {e}", item.label));
                    }
                }
            }
        }
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
        all.extend(microphone_choices(
            "",
            vec![r#"Mic "A" \ (USB)"#.into(), "Headset".into()],
        ));
        all.extend(keep_days(c.history.keep_days));
        all.extend(duck_levels(c.audio.duck_level()));
        for choice in all.iter().filter(|c| !c.changes.is_empty()) {
            config::with_settings(config::DEFAULT_CONFIG, &choice.changes)
                .unwrap_or_else(|e| panic!("{}: {e}", choice.label));
        }
        // The fast drafting check flips the file's key (off by default).
        let path: SettingPath = &["polish", "s1-mini", "fast_drafting"];
        let text = config::with_settings(config::DEFAULT_CONFIG, &[(path, true.into())]).unwrap();
        let on = Config::parse(&text).unwrap();
        assert!(on.polish.s1_mini.fast_drafting && !c.polish.s1_mini.fast_drafting);
        // The default hotkey is offered and checked; a custom chord is shown as is.
        let keys = hotkey_choices(config::DEFAULT_HOTKEY);
        assert_eq!(keys.iter().filter(|c| c.checked).count(), 1);
        let keys = hotkey_choices("F13+F14+X");
        assert!(keys.last().unwrap().checked && keys.last().unwrap().changes.is_empty());
        // The default mic is checked by default; a chosen mic that's unplugged stays listed.
        let mics = microphone_choices("", vec!["Headset".into()]);
        assert!(mics[0].checked && mics.iter().filter(|c| c.checked).count() == 1);
        let mics = microphone_choices("Gone", vec!["Headset".into()]);
        let labels: Vec<_> = mics.iter().map(|c| (c.label.as_str(), c.checked)).collect();
        assert_eq!(
            labels,
            [
                ("System default", false),
                ("Gone (not connected)", true),
                ("Headset", false)
            ]
        );
        // A non-preset number is listed, in order, and checked.
        let n = numbers(&["x"], 7, &[0, 500], ms);
        let labels: Vec<_> = n.iter().map(|c| (c.label.as_str(), c.checked)).collect();
        assert_eq!(labels, [("0 ms", false), ("7 ms", true), ("500 ms", false)]);
        // A custom history retention is kept; "Forever" is 0.
        let days: Vec<_> = keep_days(14).into_iter().map(|c| c.label).collect();
        assert_eq!(days, ["Forever", "7 days", "14 days", "30 days", "90 days"]);
        // The default duck level is a preset; another one is shown as is.
        let levels = duck_levels(c.audio.duck_level());
        assert_eq!(levels.iter().filter(|c| c.checked).count(), 1);
        assert!(levels.iter().any(|c| c.checked && c.label == "10%"));
        let levels = duck_levels(0.3);
        assert!(levels.last().unwrap().checked && levels.last().unwrap().changes.is_empty());
        // What's shown is the level in use: out of range clamped, NaN the default.
        let shown = |v: &str| {
            let c = Config::parse(&format!("[audio]\nduck_level = {v}\n")).unwrap();
            let levels = duck_levels(c.audio.duck_level());
            levels.into_iter().find(|c| c.checked).unwrap().label
        };
        assert_eq!(shown("3"), "100% (from config.toml)");
        assert_eq!(shown("nan"), "10%");
        assert_eq!(shown("-1"), "Silent");
    }
}
