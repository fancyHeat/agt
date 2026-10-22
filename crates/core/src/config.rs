//! `config.toml`. Every field has a default, so a partial or empty file is valid.
//! Unknown keys are rejected so typos surface instead of being silently ignored.
//!
//! The file is the only place settings live: the tray's menu and the settings window edit
//! it through [`set`], which keeps the user's comments and layout.

use serde::Deserialize;
use std::{fs, io, path::Path, path::PathBuf};

// A macro rather than a const so `concat!` can splice it into `DEFAULT_CONFIG`.
#[cfg(target_os = "macos")]
macro_rules! default_hotkey {
    () => {
        "Fn"
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! default_hotkey {
    () => {
        "LeftCtrl+LeftWin"
    };
}

/// Default `hotkey.key`: Fn (the Globe key) on macOS, Left Ctrl + Left Win elsewhere.
pub const DEFAULT_HOTKEY: &str = default_hotkey!();

/// Written on first run. Kept as text (not serialized) so the comments survive.
pub const DEFAULT_CONFIG: &str = concat!(
    r#"# AGT configuration. The tray menu's Settings window changes this file too. AGT applies a
# saved change by restarting itself once you're not dictating.
# If this file has an error, a running AGT keeps its settings until it's fixed; starting with
# an error, AGT uses defaults (updates and polish off). Either way it records the error.

# Which events are recorded in agt.db (the events table): "error", "warn", "info", "debug"
# or "trace".
log_level = "info"
# Start AGT when you log in (installed AGT only, not development builds). Applied on AGT's
# first start and whenever you change it here. Turning AGT off or removing it in Task Manager's
# Startup apps (Windows) or System Settings > General > Login Items (macOS) sticks until then.
launch_at_login = true

[hotkey]
# Hold this key to dictate, release to paste. One of: RightCtrl, LeftCtrl, RightShift,
# LeftShift (still work normally in apps), or F13-F24 (e.g. a remapped mouse button; AGT
# takes these keys over). Pressing any other key while holding it (e.g. Esc) cancels.
# Or a chord of up to 3 keys held together, e.g. "LeftCtrl+LeftWin" (the default on
# Windows); LeftWin/RightWin only work in a chord with Ctrl or Shift. A chord starts once
# all its keys are down and ends when any of them is released.
# macOS keys: Fn (the Globe key, the default; on its own, tapping it no longer also opens
# the emoji picker), RightOption, RightCommand, or F13-F20.
# With LeftCtrl, LeftShift, RightShift or Win in the hotkey, text is only pasted once those
# keys are up: if you already hold the hotkey for the next dictation, the previous text
# lands when you release it (after 10 s it's pasted anyway).
key = ""#,
    default_hotkey!(),
    r#""
# Holds shorter than this many milliseconds are ignored, so a tap never pastes anything.
min_hold_ms = 150
# Hands-free: tap the hotkey twice (the second press within this many milliseconds of letting
# go of the first) and AGT keeps recording after you let go, until you press the hotkey
# again; that press pastes. Esc cancels, and so does running for 30 minutes. 0: off.
double_tap_ms = 400

[audio]
# Microphone to record from, by name as the Settings menu lists it. "": the system's default
# input device (and whichever device that later becomes). A named microphone that isn't
# connected falls back to the default.
device = ""
# Keep the microphone stream running all the time. Off: the stream is only started while the
# hotkey is held, so the OS "microphone in use" indicator is off when you're not dictating
# (start-up takes a few milliseconds, measured on the first dictation and logged).
# On: no start-up delay, at the cost of the indicator being on whenever AGT runs.
keep_mic_open = false
# Turn other audio (music, videos, calls) down while the hotkey is held, so it doesn't talk
# over you; it comes back up when you let go. This lowers the system output volume: if you
# change the volume yourself while dictating, AGT leaves it where you put it.
duck = true
# How much of the volume to keep while dictating, from 0 (silence) to 1 (unchanged): 0.1 turns
# the volume slider down to a tenth of where it was.
duck_level = 0.1

[stt]
# Speech-to-text engine: "parakeet" (NVIDIA Parakeet TDT via ONNX Runtime), or "whisper"
# (whisper.cpp; only in builds with the `whisper` feature, not in releases).
engine = "parakeet"
# Parakeet: "parakeet-tdt-0.6b-v2" (English) or "parakeet-tdt-0.6b-v3" (25 European languages).
# Whisper: "whisper-large-v3-turbo" (or -q8_0, -q5_0), "whisper-small.en" (or -q8_0).
model = "parakeet-tdt-0.6b-v2"
# Parakeet encoder weights: "int8" (650 MB) or "fp32" (2.5 GB download and RAM/VRAM; a bit
# faster and more accurate on a GPU).
precision = "int8"
# Where inference runs. "auto": on Windows the GPU (DirectML) if a quick check at start-up
# finds it no slower than the CPU, else the CPU; on macOS the CPU. "cpu"; "directml" (Windows,
# any DirectX 12 GPU; holds ~1.1 GB of VRAM while AGT runs); "coreml" (macOS GPU; always the
# fp32 encoder: 2.4 GB download plus a 2.4 GB compiled copy; the first start compiles the
# model for this Mac, ~45 s, once; faster than the CPU only for dictations over ~20 s);
# "cuda" (NVIDIA; builds with the `cuda` feature);
# "vulkan" / "metal" (whisper). A GPU that can't be used falls back to the CPU.
device = "auto"
# CPU threads for inference; 0 = automatic.
threads = 0
# Where models are downloaded to. "" = the default: %LOCALAPPDATA%\AGT-models on Windows,
# ~/Library/Application Support/AGT/models on macOS.
models_dir = ""

[paste]
# Add a space after the pasted text, so consecutive dictations don't run together.
trailing_space = true
# Put the previous clipboard contents back this many milliseconds after pasting (0 = never).
# Too short and slow apps may paste the old clipboard instead of the transcript.
restore_clipboard_ms = 500

[polish]
# Clean-up of each transcript by an LLM before pasting: punctuation, capitalization, filler
# words ("um", "you know") and false starts. On by default. Adds latency (typically 0.05-0.5 s
# on a GPU, more on a CPU). On any error, timeout or suspicious output, the raw transcript is
# pasted instead.
enabled = true
# "s1-mini": S1-mini by Superwhisper, a small model made for exactly this (English only), run
# by AGT on this machine. On first use AGT downloads it (485 MB) to the models folder and
# llama.cpp's llama-server (12-32 MB) to its own data folder, and starts the server in the
# background.
# "openai": an OpenAI-compatible chat completions server you run yourself: Ollama, LM Studio,
# llama.cpp's llama-server, or a cloud provider. "anthropic": the Claude API (cloud).
provider = "s1-mini"
# openai/anthropic only. "" = provider default: "http://localhost:11434/v1" (Ollama) for
# openai, "https://api.anthropic.com" for anthropic. LM Studio: "http://localhost:1234/v1",
# llama-server: "http://localhost:8080/v1".
base_url = ""
# openai/anthropic only. "" = provider default: "qwen2.5:3b" for openai, "claude-haiku-5-5"
# for anthropic.
model = ""
# Name of an environment variable holding the API key. "" = provider default:
# ANTHROPIC_API_KEY for anthropic, none for openai (local servers need no key).
api_key_env = ""
# Or the API key itself (takes precedence over api_key_env). This file is local to your
# machine; never share or commit it with a key in it.
api_key = ""
# openai/anthropic built-in prompt (s1-mini: see [polish.s1-mini] below).
# "light": punctuation, capitalization, fillers and false starts; keeps your wording.
# "formal": also tidies grammar and phrasing for written text.
style = "light"
# Replaces the built-in instructions (the system prompt; for s1-mini its required prompt);
# "" = built-in.
prompt = ""
# The user message; {text} is replaced by the transcript. "" = built-in: the transcript between
# <transcript> tags, or for s1-mini its control line (from [polish.s1-mini]) and the transcript.
user_message = ""
# Give up and paste the raw transcript after timeout_ms + timeout_per_word_ms per word.
# 0 = provider default: 2000 for s1-mini and openai (local servers), 3000 for anthropic (cloud).
timeout_ms = 0
timeout_per_word_ms = 15
# Transcripts with fewer words than this are pasted as they are.
min_words = 4

[polish.s1-mini]
# How S1-mini writes: "casual" (all lowercase, no apostrophes), "semi-casual" (your phrasing,
# "I" capitalized), "semi-formal" (standard written English, contractions kept, "gonna" →
# "going to"), or "formal" (also expands contractions: "I am", "cannot").
styling = "semi-formal"
# "prose", or "lists": lets it turn an enumeration of 3 or more items into a bulleted list.
structure = "prose"
# "general", or "email": lays the text out as an email (greeting line, body, sign-off).
context = "general"
# "auto": on the GPU (Metal on a Mac, Vulkan on Windows; ~0.85 GB of VRAM) if there is one,
# else the CPU. "cpu": always on the CPU (slower: ~0.5 s instead of 0.08 s for a sentence).
device = "auto"
# Draft from the transcript (llama.cpp's speculative decoding): up to 2-3x faster on long
# dictations, since the cleaned text mostly repeats the transcript (little gain when it
# rewrites a lot). The trade-off: now and then a word or punctuation mark comes out
# differently where two choices were nearly tied. Only the S1-mini that AGT runs uses it.
fast_drafting = false

[overlay]
# Show a small bar with a live waveform while recording and an animation while transcribing.
# It never takes focus or catches clicks.
enabled = true
# "bottom" or "top" of the screen with the focused window (horizontally centered).
position = "bottom"

[history]
# Keep a copy of each dictation, e.g. to look back at what you said or to build a custom
# dictionary. Stored only on this machine, as plain files in the "dictations" folder next to
# agt.db (tray: Settings... > History > Open history folder); never uploaded. Turning saving off
# keeps what's already there (it's still deleted after keep_days).
# Save the transcript (before polish), the polished text and the pasted text (dictations.jsonl).
save_text = false
# Save the recording as a WAV file (16 kHz mono; about 1.9 MB per minute of speech).
save_audio = false
# Delete saved dictations after this many days; 0 = keep them forever. Whole days are
# deleted, so up to one day more may be kept. Each record joins its timings and settings in
# agt.db, which keeps those 180 days: older records no longer find theirs.
keep_days = 30

[update]
# Check GitHub Releases for updates on startup and periodically (tray "Check for updates" works
# either way).
enabled = true
# Minutes between update checks (minimum 30).
check_interval_minutes = 240
"#
);

/// `ALL` variants and `as_str`, the name in the file (must match the serde renames; a test
/// checks), for the tray's Settings menu.
macro_rules! names {
    ($ty:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $ty {
            pub const ALL: &[Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }
        }
    };
}

names!(Engine { Parakeet => "parakeet", Whisper => "whisper" });
names!(Precision { Int8 => "int8", Fp32 => "fp32" });
names!(Device {
    Auto => "auto",
    Cpu => "cpu",
    DirectMl => "directml",
    Cuda => "cuda",
    CoreMl => "coreml",
    Vulkan => "vulkan",
    Metal => "metal",
});
names!(PolishProvider { S1Mini => "s1-mini", OpenAi => "openai", Anthropic => "anthropic" });
names!(PolishStyle { Light => "light", Formal => "formal" });
names!(S1Styling {
    Casual => "casual",
    SemiCasual => "semi-casual",
    SemiFormal => "semi-formal",
    Formal => "formal",
});
names!(S1Structure { Prose => "prose", Lists => "lists" });
names!(S1Context { General => "general", Email => "email" });
names!(S1Device { Auto => "auto", Cpu => "cpu" });
names!(OverlayPosition { Bottom => "bottom", Top => "top" });

/// Lower bound for `update.check_interval_minutes`.
pub const MIN_CHECK_INTERVAL_MINUTES: u64 = 30;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub log_level: String,
    pub launch_at_login: bool,
    pub hotkey: HotkeyConfig,
    pub audio: AudioConfig,
    pub stt: SttConfig,
    pub paste: PasteConfig,
    pub polish: PolishConfig,
    pub overlay: OverlayConfig,
    pub history: HistoryConfig,
    pub update: UpdateConfig,
    /// Why `[polish]` couldn't be used (see [`Config::parse`]); polish is then off.
    #[serde(skip)]
    pub polish_error: Option<String>,
    /// Things about `[polish]` worth a warning in the log (see [`Config::parse`]).
    #[serde(skip)]
    pub polish_notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// Key name or `+`-separated chord; parsed (and rejected if invalid) by `agt-platform`.
    pub key: String,
    pub min_hold_ms: u64,
    /// Max gap between a tap's release and the next press for a double tap; 0 = off.
    pub double_tap_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Input device name; "" = the system default.
    pub device: String,
    pub keep_mic_open: bool,
    pub duck: bool,
    /// As written; the method clamps it.
    pub duck_level: f32,
}

/// Default `audio.duck_level`.
pub const DEFAULT_DUCK_LEVEL: f32 = 0.1;

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: String::new(),
            keep_mic_open: false,
            duck: true,
            duck_level: DEFAULT_DUCK_LEVEL,
        }
    }
}

impl AudioConfig {
    /// The configured duck level, clamped to 0..=1 (NaN: the default).
    pub fn duck_level(&self) -> f32 {
        if self.duck_level.is_nan() {
            DEFAULT_DUCK_LEVEL
        } else {
            self.duck_level.clamp(0.0, 1.0)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttConfig {
    pub engine: Engine,
    pub model: String,
    pub precision: Precision,
    pub device: Device,
    pub threads: usize,
    /// As written in the config ("" = default); use [`SttConfig::models_path`] to resolve it.
    pub models_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Parakeet,
    Whisper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Precision {
    Int8,
    Fp32,
}

/// Where inference runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Device {
    /// The engine's best device on this platform, falling back to the CPU.
    Auto,
    Cpu,
    DirectMl,
    Cuda,
    /// Apple's Core ML: the GPU (macOS only; parakeet).
    CoreMl,
    Vulkan,
    Metal,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PasteConfig {
    pub trailing_space: bool,
    pub restore_clipboard_ms: u64,
}

/// `[polish]`: LLM clean-up of transcripts, on by default (see `agt-polish`).
#[derive(Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolishConfig {
    pub enabled: bool,
    pub provider: PolishProvider,
    /// As written ("" = provider default); use [`PolishConfig::base_url`].
    pub base_url: String,
    /// As written ("" = provider default); use [`PolishConfig::model`].
    pub model: String,
    pub api_key_env: String,
    /// Never log this; use [`PolishConfig::api_key`].
    pub api_key: String,
    pub style: PolishStyle,
    pub prompt: String,
    pub user_message: String,
    pub timeout_ms: u64,
    pub timeout_per_word_ms: u64,
    pub min_words: usize,
    /// `[polish.s1-mini]`: settings of the managed S1-mini.
    #[serde(rename = "s1-mini")]
    pub s1_mini: S1MiniConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolishProvider {
    /// S1-mini by Superwhisper, downloaded and run (llama.cpp's `llama-server`) by AGT.
    #[serde(rename = "s1-mini")]
    S1Mini,
    /// OpenAI-compatible `/chat/completions` (Ollama, LM Studio, llama-server, cloud).
    OpenAi,
    /// Anthropic Messages API.
    Anthropic,
}

/// `[polish.s1-mini]`. The first three are S1-mini's control-line axes, with the values it
/// was trained on (anything else makes it produce garbage, so they're enums).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S1MiniConfig {
    pub styling: S1Styling,
    pub structure: S1Structure,
    pub context: S1Context,
    pub device: S1Device,
    /// Speculative decoding from the prompt (llama-server `--spec-type ngram-simple`).
    pub fast_drafting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum S1Styling {
    Casual,
    SemiCasual,
    SemiFormal,
    Formal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum S1Structure {
    Prose,
    Lists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum S1Context {
    General,
    Email,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum S1Device {
    /// All layers on the GPU if llama.cpp finds one (Metal, Vulkan), else the CPU.
    Auto,
    Cpu,
}

impl Default for S1MiniConfig {
    fn default() -> Self {
        Self {
            styling: S1Styling::SemiFormal,
            structure: S1Structure::Prose,
            context: S1Context::General,
            device: S1Device::Auto,
            fast_drafting: false,
        }
    }
}

impl S1MiniConfig {
    /// The first line of every S1-mini input, e.g.
    /// `[Styling: semi-formal] [Structure: prose] [Context: general]`.
    pub fn control_line(&self) -> String {
        format!(
            "[Styling: {}] [Structure: {}] [Context: {}]",
            self.styling.as_str(),
            self.structure.as_str(),
            self.context.as_str()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolishStyle {
    Light,
    Formal,
}

impl Default for PolishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: PolishProvider::S1Mini,
            base_url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            api_key: String::new(),
            style: PolishStyle::Light,
            prompt: String::new(),
            user_message: String::new(),
            timeout_ms: 0,
            timeout_per_word_ms: 15,
            min_words: 4,
            s1_mini: S1MiniConfig::default(),
        }
    }
}

impl PolishConfig {
    /// The server's base URL. For `s1-mini` this is decided at run time (a free local port),
    /// and the setting is ignored.
    pub fn base_url(&self) -> &str {
        match (self.base_url.as_str(), self.provider) {
            (_, PolishProvider::S1Mini) => "",
            ("", PolishProvider::OpenAi) => "http://localhost:11434/v1",
            ("", PolishProvider::Anthropic) => "https://api.anthropic.com",
            (url, _) => url,
        }
    }

    pub fn model(&self) -> &str {
        match (self.model.as_str(), self.provider) {
            (_, PolishProvider::S1Mini) => "s1-mini",
            ("", PolishProvider::OpenAi) => "qwen2.5:3b",
            ("", PolishProvider::Anthropic) => "claude-haiku-5-5",
            (model, _) => model,
        }
    }

    /// The environment variable the key is read from, if any. `s1-mini` gets a random key
    /// per server start instead.
    pub fn api_key_var(&self) -> Option<&str> {
        match (self.api_key_env.as_str(), self.provider) {
            (_, PolishProvider::S1Mini) => None,
            ("", PolishProvider::OpenAi) => None,
            ("", PolishProvider::Anthropic) => Some("ANTHROPIC_API_KEY"),
            (var, _) => Some(var),
        }
    }

    /// `api_key` if set, else the (non-empty) variable named by [`Self::api_key_var`].
    pub fn api_key(&self) -> Option<String> {
        if self.provider == PolishProvider::S1Mini {
            return None;
        }
        let key = match self.api_key.trim() {
            "" => std::env::var(self.api_key_var()?).ok()?,
            key => key.to_owned(),
        };
        let key = key.trim();
        (!key.is_empty()).then(|| key.to_owned())
    }

    /// The time budget for polishing a transcript of `words` words. `timeout_ms = 0` means
    /// the provider default: cloud round trips have a slower tail than a local server.
    pub fn timeout(&self, words: usize) -> std::time::Duration {
        let base = match (self.timeout_ms, self.provider) {
            (0, PolishProvider::S1Mini | PolishProvider::OpenAi) => 2000,
            (0, PolishProvider::Anthropic) => 3000,
            (ms, _) => ms,
        };
        std::time::Duration::from_millis(
            base.saturating_add(self.timeout_per_word_ms.saturating_mul(words as u64)),
        )
    }
}

/// Hand-written so the API key can't end up in a log through `{:?}`.
impl std::fmt::Debug for PolishConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = if self.api_key.is_empty() {
            ""
        } else {
            "<redacted>"
        };
        f.debug_struct("PolishConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &key)
            .field("style", &self.style)
            .field("prompt", &self.prompt)
            .field("user_message", &self.user_message)
            .field("timeout_ms", &self.timeout_ms)
            .field("timeout_per_word_ms", &self.timeout_per_word_ms)
            .field("min_words", &self.min_words)
            .field("s1_mini", &self.s1_mini)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverlayConfig {
    pub enabled: bool,
    pub position: OverlayPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayPosition {
    Bottom,
    Top,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: OverlayPosition::Bottom,
        }
    }
}

/// `[history]`: optional copies of dictations on this machine (see `agt-history`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    pub save_text: bool,
    pub save_audio: bool,
    /// 0 = forever.
    pub keep_days: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            save_text: false,
            save_audio: false,
            keep_days: 30,
        }
    }
}

impl HistoryConfig {
    /// Whether dictations are saved at all.
    pub fn enabled(&self) -> bool {
        self.save_text || self.save_audio
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub enabled: bool,
    /// As written; the method floors it.
    pub check_interval_minutes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            launch_at_login: true,
            hotkey: HotkeyConfig::default(),
            audio: AudioConfig::default(),
            stt: SttConfig::default(),
            paste: PasteConfig::default(),
            polish: PolishConfig::default(),
            overlay: OverlayConfig::default(),
            history: HistoryConfig::default(),
            update: UpdateConfig::default(),
            polish_error: None,
            polish_notices: Vec::new(),
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: DEFAULT_HOTKEY.into(),
            min_hold_ms: 150,
            double_tap_ms: 400,
        }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            engine: Engine::Parakeet,
            model: "parakeet-tdt-0.6b-v2".into(),
            precision: Precision::Int8,
            device: Device::Auto,
            threads: 0,
            models_dir: String::new(),
        }
    }
}

impl SttConfig {
    /// `models_dir` if set, else [`crate::paths::models_dir`].
    pub fn models_path(&self) -> PathBuf {
        if self.models_dir.is_empty() {
            crate::paths::models_dir()
        } else {
            PathBuf::from(&self.models_dir)
        }
    }
}

impl Default for PasteConfig {
    fn default() -> Self {
        Self {
            trailing_space: true,
            restore_clipboard_ms: 500,
        }
    }
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_minutes: 240,
        }
    }
}

impl UpdateConfig {
    /// The configured interval, floored at [`MIN_CHECK_INTERVAL_MINUTES`].
    pub fn check_interval_minutes(&self) -> u64 {
        self.check_interval_minutes.max(MIN_CHECK_INTERVAL_MINUTES)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    /// Only toml's message and where (1-based line and column): its full error quotes the
    /// offending line, which may hold an API key, and this error is recorded in agt.db.
    Parse {
        message: String,
        at: Option<(usize, usize)>,
    },
}

/// Removes the values serde/toml quote in their messages (`invalid type: string "..."`,
/// ``unknown variant `...` ``): a key typed into the wrong field must not end up in
/// agt.db. Keys and the expected values stay.
pub(crate) fn redact_values(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    loop {
        let next = [("string \"", '"'), ("variant `", '`')]
            .into_iter()
            .filter_map(|(open, close)| rest.find(open).map(|i| (i, open, close)))
            .min_by_key(|&(i, ..)| i);
        let Some((i, open, close)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i]);
        out.push_str(&open[..open.len() - 1]);
        out.push_str("(redacted)");
        // The value ends at the closing quote; `"` strings are Debug-escaped (`\"`).
        let value = &rest[i + open.len()..];
        let mut escaped = false;
        let end = value.char_indices().find(|&(_, c)| {
            let done = c == close && !escaped;
            escaped = c == '\\' && !escaped;
            done
        });
        rest = match end {
            Some((j, c)) => &value[j + c.len_utf8()..],
            None => "",
        };
    }
}

impl ConfigError {
    fn parse(e: toml::de::Error, text: &str) -> Self {
        Self::at(e.message(), e.span(), text)
    }

    fn at(message: &str, span: Option<std::ops::Range<usize>>, text: &str) -> Self {
        Self::Parse {
            message: redact_values(message.trim()),
            at: line_column(span, text),
        }
    }
}

/// 1-based line and column where `span` starts in `text`.
pub(crate) fn line_column(
    span: Option<std::ops::Range<usize>>,
    text: &str,
) -> Option<(usize, usize)> {
    span.map(|span| {
        let before = &text[..span.start.min(text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        (line, column)
    })
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "config I/O error: {e}"),
            Self::Parse { message, at } => {
                f.write_str(&located("invalid config.toml", message, *at))
            }
        }
    }
}

/// `<what> at line L, column C: <message>`, or without the position if there is none.
pub(crate) fn located(what: &str, message: &str, at: Option<(usize, usize)>) -> String {
    match at {
        Some((line, column)) => format!("{what} at line {line}, column {column}: {message}"),
        None => format!("{what}: {message}"),
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Parses the file. `[polish]` is parsed on its own: a problem there (an unknown key, or
    /// a value this version doesn't know, such as a provider added in a later release) only
    /// turns polish off ([`Config::polish_error`] says why) and keeps the rest of the
    /// user's settings. Anything wrong elsewhere fails the whole file.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        // Deserialized from the parsed document rather than a `toml::Value`, which has no
        // positions: errors keep their line and column.
        let mut table = toml::de::DeTable::parse(text).map_err(|e| ConfigError::parse(e, text))?;
        let polish = table.get_mut().remove("polish");
        let mut config = Self::deserialize(toml::de::Deserializer::from(table))
            .map_err(|e| ConfigError::parse(e, text))?;
        if let Some(mut polish) = polish {
            // Written before `s1-mini` became the default: `base_url` without a provider
            // meant the OpenAI-compatible one (a server the user runs).
            if let toml::de::DeValue::Table(t) = polish.get_mut()
                && !t.contains_key("provider")
                && t.get("base_url")
                    .and_then(|v| v.get_ref().as_str())
                    .is_some_and(|u| !u.trim().is_empty())
            {
                t.insert(
                    toml::Spanned::new(0..0, "provider".into()),
                    toml::Spanned::new(0..0, toml::de::DeValue::String("openai".into())),
                );
                config.polish_notices.push(
                    "[polish] has base_url but no provider: using provider = \"openai\" (write it into config.toml to silence this)".into(),
                );
            }
            match PolishConfig::deserialize(toml::de::ValueDeserializer::from(polish)) {
                Ok(p) => {
                    let ignored = p.provider == PolishProvider::S1Mini
                        && (!p.base_url.is_empty()
                            || !p.model.is_empty()
                            || !p.api_key.is_empty()
                            || !p.api_key_env.is_empty());
                    if ignored {
                        config.polish_notices.push(
                            "[polish] provider = \"s1-mini\" ignores base_url, model and api_key*; for your own server use provider = \"openai\"".into(),
                        );
                    }
                    config.polish = p;
                }
                Err(e) => {
                    config.polish = PolishConfig::default();
                    config.polish.enabled = false;
                    config.polish_error = Some(located(
                        "[polish]",
                        &redact_values(e.message().trim()),
                        line_column(e.span(), text),
                    ));
                }
            }
        }
        Ok(config)
    }

    /// Load `path`, writing [`DEFAULT_CONFIG`] there first if it does not exist.
    /// An existing file is never modified.
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(ConfigError::Io)?;
            }
            fs::write(path, DEFAULT_CONFIG).map_err(ConfigError::Io)?;
        }
        Self::parse(&fs::read_to_string(path).map_err(ConfigError::Io)?)
    }

    /// Load `path` as it is (see [`Config::load_or_create`] for the first run).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&fs::read_to_string(path).map_err(ConfigError::Io)?)
    }

    /// What to run with when the config file can't be loaded: defaults, but with updates
    /// and polish off, since the user may have disabled them in the file we couldn't read
    /// (polish would otherwise download S1-mini and change the pasted text).
    pub fn fallback() -> Self {
        let mut c = Self::default();
        c.update.enabled = false;
        c.polish.enabled = false;
        c
    }
}

/// A value to write with [`set`] (a string, integer or bool).
pub use toml_edit::Value as SettingValue;

/// A setting's place in the file, e.g. `&["polish", "s1-mini", "styling"]`.
pub type SettingPath = &'static [&'static str];

/// Returns `text` with each setting changed, keeping comments and layout (a value keeps its
/// trailing comment). Missing tables and keys are added. Fails, quoting nothing, if `text`
/// can't be parsed or the result isn't a valid config.
pub fn with_settings(
    text: &str,
    changes: &[(SettingPath, SettingValue)],
) -> Result<String, ConfigError> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| ConfigError::at(e.message(), e.span(), text))?;
    for (path, value) in changes {
        let (key, tables) = path.split_last().expect("a setting path is not empty");
        let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        for name in tables {
            if table.get(name).is_none() {
                // Implicit: `[polish.s1-mini]` alone doesn't get an empty `[polish]` above it.
                let mut new = toml_edit::Table::new();
                new.set_implicit(true);
                table.insert(name, toml_edit::Item::Table(new));
            }
            table = table
                .get_mut(name)
                .and_then(|t| t.as_table_like_mut())
                .ok_or_else(|| ConfigError::Parse {
                    message: format!("`{name}` is not a table"),
                    at: None,
                })?;
        }
        match table.get_mut(key).and_then(|v| v.as_value_mut()) {
            Some(old) => {
                let decor = old.decor().clone();
                *old = value.clone();
                *old.decor_mut() = decor;
            }
            None => {
                table.insert(key, toml_edit::Item::Value(value.clone()));
            }
        }
    }
    let out = doc.to_string();
    Config::parse(&out)?;
    Ok(out)
}

/// Changes settings in the file at `path` (see [`with_settings`]; a missing file starts from
/// [`DEFAULT_CONFIG`]) and returns the new config. The file is replaced in one step, so a
/// crash can't leave it half-written; a symlink is followed, not replaced.
pub fn set(path: &Path, changes: &[(SettingPath, SettingValue)]) -> Result<Config, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => DEFAULT_CONFIG.to_owned(),
        Err(e) => return Err(ConfigError::Io(e)),
    };
    let out = with_settings(&text, changes)?;
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    // Per process: the tray and the settings window may both be writing.
    let tmp = target.with_extension(format!("toml.{}.tmp", std::process::id()));
    let written = fs::write(&tmp, &out).and_then(|()| {
        // Keep the file's permissions (e.g. 0600 for a file holding an API key).
        if let Ok(m) = fs::metadata(&target) {
            fs::set_permissions(&tmp, m.permissions())?;
        }
        replace(&tmp, &target)
    });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(ConfigError::Io)?;
    Config::parse(&out)
}

/// `fs::rename`, retried briefly: on Windows it fails while another process (a virus scanner,
/// a sync client) has the target open.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match fs::rename(from, to) {
            Err(_) if cfg!(windows) && tries < 5 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_matches_default_struct() {
        assert_eq!(Config::parse(DEFAULT_CONFIG).unwrap(), Config::default());
    }

    #[test]
    fn partial_file_uses_defaults() {
        let c = Config::parse("[update]\nenabled = false\n").unwrap();
        assert!(!c.update.enabled);
        assert_eq!(c.update.check_interval_minutes(), 240);
        assert_eq!(c.log_level, "info");
        assert!(c.launch_at_login);
        assert_eq!(c.stt, SttConfig::default());
        assert!(
            !Config::parse("launch_at_login = false")
                .unwrap()
                .launch_at_login
        );
    }

    #[test]
    fn duck_level_is_clamped() {
        let level = |v: &str| {
            Config::parse(&format!("[audio]\nduck_level = {v}\n"))
                .unwrap()
                .audio
                .duck_level()
        };
        assert_eq!(level("0.25"), 0.25);
        assert_eq!(level("0"), 0.0); // an integer is fine too
        assert_eq!(level("1"), 1.0);
        assert_eq!(level("-0.5"), 0.0);
        assert_eq!(level("3.0"), 1.0);
        assert_eq!(level("nan"), DEFAULT_DUCK_LEVEL);
        let c = Config::parse("[audio]\nduck = false\n").unwrap();
        assert!(!c.audio.duck);
        assert_eq!(c.audio.duck_level(), DEFAULT_DUCK_LEVEL);
    }

    #[test]
    fn interval_is_floored() {
        let c = Config::parse("[update]\ncheck_interval_minutes = 1\n").unwrap();
        assert_eq!(
            c.update.check_interval_minutes(),
            MIN_CHECK_INTERVAL_MINUTES
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("typo = 1").is_err());
        assert!(Config::parse("[stt]\ntypo = 1").is_err());
        // ...except in [polish], where they only turn polish off (see below).
        assert!(polish_error("[polish]\ntypo = 1"));
    }

    #[test]
    fn parse_errors_never_quote_the_file() {
        // A key with a missing closing quote, and an error on the line after one.
        for text in [
            "[polish]\napi_key = \"sk-ant-SECRET123\n",
            "[polish]\napi_key = \"sk-ant-SECRET123\"\n= 1\n",
            "log_level = \"info\"\n[polish]\napi_key = \"sk-ant-SECRET123\" x\n",
        ] {
            let e = Config::parse(text).unwrap_err().to_string();
            assert!(!e.contains("SECRET"), "{e}");
            assert!(e.contains("line "), "{e}");
        }
        let e = Config::parse("a = 1\n\n  b = \"x\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("invalid config.toml at line 3, column "),
            "{e}"
        );
    }

    #[test]
    fn errors_say_where() {
        // Valid TOML, but not a valid config: in a section, and in [polish] (which is
        // deserialized on its own).
        let e = Config::parse("[paste]\ntrailing_space = true\n\n[stt]\ntypo = 1\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("invalid config.toml at line 5, column 1:"),
            "{e}"
        );
        let c = Config::parse("[stt]\n\n[polish]\nenabled = true\ntimeout_ms = \"x\"\n").unwrap();
        let e = c.polish_error.unwrap();
        assert!(e.starts_with("[polish] at line 5, column 14:"), "{e}");
        let c = Config::parse("[polish]\nbase_url = \"http://h/v1\"\ntypo = 1\n").unwrap();
        let e = c.polish_error.unwrap();
        assert!(e.starts_with("[polish] at line 3, column 1:"), "{e}");
    }

    #[test]
    fn errors_never_echo_values() {
        // A key in a numeric field, an enum field, and outside [polish] (fails the file).
        for (text, field) in [
            ("[polish]\ntimeout_ms = \"sk-SECRET\\\"x\"\n", "polish"),
            ("[polish]\nprovider = \"sk-SECRET\"\n", "polish"),
            ("[polish.s1-mini]\nstyling = \"sk-SECRET\"\n", "polish"),
            ("[stt]\nengine = \"sk-SECRET\"\n", "file"),
            ("[hotkey]\nmin_hold_ms = \"sk-SECRET\"\n", "file"),
        ] {
            let e = match Config::parse(text) {
                Ok(c) => c.polish_error.expect(text),
                Err(e) => e.to_string(),
            };
            assert!(!e.contains("SECRET"), "{field}: {e}");
            assert!(e.contains("(redacted)"), "{field}: {e}");
        }
        assert_eq!(
            redact_values("unknown variant `x`, expected one of `openai`, `anthropic`"),
            "unknown variant (redacted), expected one of `openai`, `anthropic`"
        );
        assert_eq!(
            redact_values(r#"invalid type: string "a\"b", expected u64"#),
            "invalid type: string (redacted), expected u64"
        );
    }

    /// Parses, and says whether `[polish]` was rejected (with polish then off).
    fn polish_error(text: &str) -> bool {
        let c = Config::parse(text).unwrap();
        assert!(c.polish_error.is_none() || !c.polish.enabled);
        c.polish_error.is_some()
    }

    /// A [polish] value this version doesn't know (e.g. a provider from a newer release)
    /// turns polish off and keeps everything else, including updates.
    #[test]
    fn bad_polish_section_only_disables_polish() {
        let text = "[hotkey]\nkey = \"F13\"\nmin_hold_ms = 300\n\n[stt]\ndevice = \"cpu\"\n\n[polish]\nenabled = true\nprovider = \"something-unknown\"\n\n[overlay]\nposition = \"top\"\n\n[update]\nenabled = false\n";
        let c = Config::parse(text).unwrap();
        assert!(!c.polish.enabled);
        let err = c.polish_error.as_deref().unwrap();
        assert!(
            err.contains("[polish]") && !err.contains("something-unknown"),
            "{err}"
        );
        assert_eq!((c.hotkey.key.as_str(), c.hotkey.min_hold_ms), ("F13", 300));
        assert_eq!(c.stt.device, Device::Cpu);
        assert_eq!(c.overlay.position, OverlayPosition::Top);
        assert!(!c.update.enabled);
        // A good [polish] has no error; a broken file elsewhere still fails as a whole.
        assert_eq!(
            Config::parse("[polish]\nenabled = true")
                .unwrap()
                .polish_error,
            None
        );
        assert!(Config::parse("[polish]\nprovider = \"x\"\n[stt]\ntypo = 1").is_err());
        assert!(Config::parse("polish = 3").unwrap().polish_error.is_some());
    }

    #[test]
    fn base_url_without_provider_means_openai() {
        let c = Config::parse("[polish]\nenabled = true\nbase_url = \"http://localhost:8080/v1\"")
            .unwrap();
        assert_eq!(c.polish.provider, PolishProvider::OpenAi);
        assert_eq!(c.polish.base_url(), "http://localhost:8080/v1");
        assert_eq!(c.polish_notices.len(), 1);
        let c = Config::parse("[polish]\nprovider = \"s1-mini\"\nmodel = \"x\"").unwrap();
        assert_eq!(c.polish_notices.len(), 1);
        assert!(c.polish_notices[0].contains("ignores"));
        assert!(
            Config::parse("[polish]\nenabled = true")
                .unwrap()
                .polish_notices
                .is_empty()
        );
        assert!(
            Config::parse(DEFAULT_CONFIG)
                .unwrap()
                .polish_notices
                .is_empty()
        );
    }

    #[test]
    fn stt_enums_parse_and_reject_unknown_values() {
        let c = Config::parse("[stt]\ndevice = \"directml\"\nprecision = \"fp32\"\n").unwrap();
        assert_eq!(c.stt.device, Device::DirectMl);
        assert_eq!(c.stt.precision, Precision::Fp32);
        assert!(Config::parse("[stt]\ndevice = \"gpu\"").is_err());
        assert!(Config::parse("[stt]\nengine = \"vosk\"").is_err());
        let c = Config::parse("[stt]\nengine = \"whisper\"\ndevice = \"vulkan\"").unwrap();
        assert_eq!(
            (c.stt.engine, c.stt.device),
            (Engine::Whisper, Device::Vulkan)
        );
        for d in [
            "auto", "cpu", "directml", "cuda", "coreml", "vulkan", "metal",
        ] {
            assert!(
                Config::parse(&format!("[stt]\ndevice = \"{d}\"")).is_ok(),
                "{d}"
            );
        }
    }

    #[test]
    fn overlay_settings() {
        let c = Config::parse("[overlay]\nenabled = false\nposition = \"top\"\n").unwrap();
        assert!(!c.overlay.enabled);
        assert_eq!(c.overlay.position, OverlayPosition::Top);
        assert!(Config::parse("[overlay]\nposition = \"left\"").is_err());
    }

    #[test]
    fn history_settings() {
        let c = Config::default().history;
        assert!(!c.save_text && !c.save_audio && !c.enabled());
        assert_eq!(c.keep_days, 30);
        let c = Config::parse("[history]\nsave_audio = true\nkeep_days = 0\n")
            .unwrap()
            .history;
        assert!(c.enabled() && !c.save_text);
        assert_eq!(c.keep_days, 0);
        assert!(Config::parse("[history]\ntypo = 1").is_err());
        assert!(Config::parse("[history]\nkeep_days = -1").is_err());
        let out =
            with_settings(DEFAULT_CONFIG, &[(&["history", "keep_days"], 7i64.into())]).unwrap();
        assert_eq!(Config::parse(&out).unwrap().history.keep_days, 7);
    }

    #[test]
    fn models_dir_override() {
        let c = Config::parse("[stt]\nmodels_dir = 'D:\\models'").unwrap();
        assert_eq!(c.stt.models_path(), PathBuf::from("D:\\models"));
    }

    #[test]
    fn polish_defaults_depend_on_provider() {
        let c = Config::default().polish;
        assert!(c.enabled);
        assert_eq!(c.provider, PolishProvider::S1Mini);
        assert_eq!(c.model(), "s1-mini");
        assert_eq!(c.timeout(0), std::time::Duration::from_millis(2000));
        let c = Config::parse("[polish]\nprovider = \"openai\"\n")
            .unwrap()
            .polish;
        assert_eq!(c.base_url(), "http://localhost:11434/v1");
        assert_eq!(c.model(), "qwen2.5:3b");
        assert_eq!(c.api_key_var(), None);
        assert_eq!(c.api_key(), None);
        let c = Config::parse("[polish]\nprovider = \"anthropic\"\nmodel = \"m\"\n")
            .unwrap()
            .polish;
        assert_eq!(c.base_url(), "https://api.anthropic.com");
        assert_eq!(c.model(), "m");
        assert_eq!(c.api_key_var(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(c.timeout(100), std::time::Duration::from_millis(4500));
        assert_eq!(
            Config::default().polish.timeout(100),
            std::time::Duration::from_millis(3500)
        );
        let c = Config::parse("[polish]\nprovider = \"anthropic\"\ntimeout_ms = 1000\n").unwrap();
        assert_eq!(c.polish.timeout(0), std::time::Duration::from_millis(1000));
        assert!(polish_error("[polish]\nprovider = \"gemini\""));
        assert!(polish_error("[polish]\nstyle = \"pirate\""));
    }

    #[test]
    fn s1_mini_is_one_line_to_enable_and_has_simple_keys() {
        let c = Config::parse("[polish]\nenabled = true\n").unwrap().polish;
        assert!(c.enabled);
        assert_eq!(c.provider, PolishProvider::S1Mini);
        assert_eq!(
            c.s1_mini.control_line(),
            "[Styling: semi-formal] [Structure: prose] [Context: general]"
        );
        let c = Config::parse(
            "[polish]\nenabled = true\n[polish.s1-mini]\nstyling = \"semi-casual\"\nstructure = \"lists\"\ncontext = \"email\"\ndevice = \"cpu\"\nfast_drafting = true\n",
        )
        .unwrap()
        .polish;
        assert_eq!(
            c.s1_mini.control_line(),
            "[Styling: semi-casual] [Structure: lists] [Context: email]"
        );
        assert_eq!(c.s1_mini.device, S1Device::Cpu);
        assert!(c.s1_mini.fast_drafting);
        assert!(!S1MiniConfig::default().fast_drafting);
        // Values S1-mini wasn't trained on are config errors, not garbage output.
        assert!(polish_error("[polish.s1-mini]\nstyling = \"pirate\""));
        assert!(polish_error("[polish.s1-mini]\ncontext = \"chat\""));
        assert!(polish_error("[polish.s1-mini]\ntypo = 1"));
        // Settings for the other providers don't leak into the managed server.
        let c = Config::parse(
            "[polish]\nprovider = \"s1-mini\"\nbase_url = \"http://x\"\napi_key = \"k\"\nmodel = \"m\"",
        )
        .unwrap()
        .polish;
        assert_eq!(
            (c.base_url(), c.model(), c.api_key()),
            ("", "s1-mini", None)
        );
    }

    #[test]
    fn polish_api_key_is_never_debug_printed() {
        let c =
            Config::parse("[polish]\nprovider = \"openai\"\napi_key = \" sk-secret \"").unwrap();
        assert_eq!(c.polish.api_key().as_deref(), Some("sk-secret"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-secret") && dbg.contains("<redacted>"));
    }

    #[test]
    fn names_match_the_file_format() {
        fn check<T: serde::de::DeserializeOwned + PartialEq + std::fmt::Debug + Copy>(
            all: &[T],
            name: fn(T) -> &'static str,
        ) {
            for &v in all {
                let parsed: T = toml::Value::String(name(v).into()).try_into().unwrap();
                assert_eq!(parsed, v);
            }
        }
        check(Engine::ALL, Engine::as_str);
        check(Precision::ALL, Precision::as_str);
        check(Device::ALL, Device::as_str);
        check(PolishProvider::ALL, PolishProvider::as_str);
        check(PolishStyle::ALL, PolishStyle::as_str);
        check(S1Styling::ALL, S1Styling::as_str);
        check(S1Structure::ALL, S1Structure::as_str);
        check(S1Context::ALL, S1Context::as_str);
        check(S1Device::ALL, S1Device::as_str);
        check(OverlayPosition::ALL, OverlayPosition::as_str);
    }

    #[test]
    fn settings_edits_keep_comments() {
        let out = with_settings(
            DEFAULT_CONFIG,
            &[
                (&["stt", "device"], "cpu".into()),
                (&["polish", "enabled"], false.into()),
                (&["polish", "s1-mini", "styling"], "casual".into()),
                (&["paste", "restore_clipboard_ms"], 1000i64.into()),
                (&["launch_at_login"], false.into()),
            ],
        )
        .unwrap();
        // Only the five values changed: every comment line is still there.
        let comments = |t: &str| t.lines().filter(|l| l.starts_with('#')).count();
        assert_eq!(comments(&out), comments(DEFAULT_CONFIG));
        assert!(out.contains("\ndevice = \"cpu\"\n# CPU threads"), "{out}");
        let c = Config::parse(&out).unwrap();
        assert_eq!(c.stt.device, Device::Cpu);
        assert!(!c.polish.enabled);
        assert_eq!(c.polish.s1_mini.styling, S1Styling::Casual);
        assert_eq!(c.paste.restore_clipboard_ms, 1000);
        // Everything else is as before.
        let mut expected = Config::default();
        expected.stt.device = Device::Cpu;
        expected.polish.enabled = false;
        expected.polish.s1_mini.styling = S1Styling::Casual;
        expected.paste.restore_clipboard_ms = 1000;
        expected.launch_at_login = false;
        assert_eq!(c, expected);
    }

    #[test]
    fn settings_edits_add_missing_keys_and_tables() {
        let text = "# mine\n[stt]\nthreads = 2 # pinned\n";
        let out = with_settings(
            text,
            &[
                (&["stt", "threads"], 4i64.into()),
                (&["stt", "device"], "cpu".into()),
                (&["polish", "s1-mini", "context"], "email".into()),
                (&["log_level"], "debug".into()),
            ],
        )
        .unwrap();
        // A new top-level key goes above the first table (and the comment that opens it).
        assert!(
            out.starts_with("log_level = \"debug\"\n# mine\n[stt]\n"),
            "{out}"
        );
        assert!(
            out.contains("threads = 4 # pinned\ndevice = \"cpu\"\n"),
            "{out}"
        );
        assert!(
            out.ends_with("\n[polish.s1-mini]\ncontext = \"email\"\n"),
            "{out}"
        );
        assert!(!out.contains("[polish]"), "{out}");
        let c = Config::parse(&out).unwrap();
        assert_eq!((c.stt.threads, c.stt.device), (4, Device::Cpu));
        assert_eq!(c.polish.s1_mini.context, S1Context::Email);
        assert_eq!(c.log_level, "debug");
        // Inline and dotted tables are edited in place.
        let out = with_settings(
            "stt = { threads = 2 }\npolish.enabled = false\n",
            &[
                (&["stt", "threads"], 3i64.into()),
                (&["polish", "enabled"], true.into()),
            ],
        )
        .unwrap();
        assert_eq!(out, "stt = { threads = 3 }\npolish.enabled = true\n");
    }

    #[test]
    fn settings_edits_refuse_invalid_results() {
        // A broken file is never rewritten, and the error doesn't quote it.
        let e = with_settings(
            "api_key = \"sk-SECRET\n",
            &[(&["log_level"], "info".into())],
        )
        .unwrap_err()
        .to_string();
        assert!(!e.contains("SECRET") && e.contains("line 1"), "{e}");
        assert!(with_settings("", &[(&["stt", "device"], "gpu".into())]).is_err());
        assert!(with_settings("stt = 1", &[(&["stt", "device"], "cpu".into())]).is_err());
    }

    #[test]
    fn set_writes_the_file() {
        let dir = std::env::temp_dir().join(format!("agt-set-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // A missing file starts from the default one.
        let c = set(&path, &[(&["overlay", "position"], "top".into())]).unwrap();
        assert_eq!(c.overlay.position, OverlayPosition::Top);
        assert_eq!(Config::load(&path).unwrap(), c);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("# AGT configuration")
        );
        // The file's permissions are kept, and no temporary file is left behind.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            set(&path, &[(&["log_level"], "debug".into())]).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::write(&path, "typo = 1").unwrap();
        assert!(set(&path, &[(&["log_level"], "info".into())]).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "typo = 1");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fallback_disables_updates_and_polish() {
        assert!(!Config::fallback().update.enabled);
        assert!(!Config::fallback().polish.enabled);
    }

    #[test]
    fn load_or_create_writes_default_and_keeps_existing() {
        let dir = std::env::temp_dir().join(format!("agt-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(Config::load_or_create(&path).unwrap(), Config::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);

        fs::write(&path, "typo = 1").unwrap();
        assert!(Config::load_or_create(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "typo = 1");
        fs::remove_dir_all(&dir).unwrap();
    }
}
