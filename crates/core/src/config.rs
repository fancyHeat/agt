//! `config.toml`. Every field has a default, so a partial or empty file is valid.
//! Unknown keys are rejected so typos surface instead of being silently ignored.

use serde::Deserialize;
use std::{fs, io, path::Path, path::PathBuf};

/// Written on first run. Kept as text (not serialized) so the comments survive.
pub const DEFAULT_CONFIG: &str = r#"# AGT configuration. Restart AGT after editing.
# If this file has an error, AGT starts with defaults (updates off) and logs the error.

# Log verbosity: "error", "warn", "info", "debug" or "trace".
log_level = "info"

[hotkey]
# Hold this key to dictate, release to paste. One of: RightCtrl, LeftCtrl, RightShift,
# LeftShift (still work normally in apps), or F13-F24 (e.g. a remapped mouse button; AGT
# takes these keys over). Pressing any other key while holding it (e.g. Esc) cancels.
key = "RightCtrl"
# Holds shorter than this many milliseconds are ignored, so a tap never pastes anything.
min_hold_ms = 150

[audio]
# Keep the microphone stream running all the time. Off: the stream is only started while the
# hotkey is held, so the OS "microphone in use" indicator is off when you're not dictating
# (start-up takes a few milliseconds, measured on the first dictation and logged).
# On: no start-up delay, at the cost of the indicator being on whenever AGT runs.
keep_mic_open = false

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
# any DirectX 12 GPU; holds ~1.1 GB of VRAM while AGT runs); "cuda" (NVIDIA; builds with the
# `cuda` feature); "vulkan" / "metal" (whisper). A GPU that can't be used falls back to the CPU.
device = "auto"
# CPU threads for inference; 0 = automatic.
threads = 0
# Where models are downloaded to; "" = the default location (see README).
models_dir = ""

[paste]
# Add a space after the pasted text, so consecutive dictations don't run together.
trailing_space = true
# Put the previous clipboard contents back this many milliseconds after pasting (0 = never).
# Too short and slow apps may paste the old clipboard instead of the transcript.
restore_clipboard_ms = 500

[polish]
# Optional clean-up of each transcript by an LLM before pasting: punctuation, capitalization,
# filler words ("um", "you know") and false starts. Off by default. Adds latency (typically
# 0.2-1 s), and the transcript is sent to the configured server (local or cloud). On any
# error, timeout or suspicious output, the raw transcript is pasted instead.
enabled = false
# "openai": any OpenAI-compatible chat completions server: Ollama, LM Studio, llama.cpp's
# llama-server (all local), or a cloud provider. "anthropic": the Claude API.
provider = "openai"
# "" = provider default: "http://localhost:11434/v1" (Ollama) for openai,
# "https://api.anthropic.com" for anthropic. LM Studio: "http://localhost:1234/v1",
# llama-server: "http://localhost:8080/v1".
base_url = ""
# "" = provider default: "qwen2.5:3b" for openai, "claude-haiku-5-5" for anthropic.
model = ""
# Name of an environment variable holding the API key. "" = provider default:
# ANTHROPIC_API_KEY for anthropic, none for openai (local servers need no key).
api_key_env = ""
# Or the API key itself (takes precedence over api_key_env). This file is local to your
# machine; never share or commit it with a key in it.
api_key = ""
# "light": punctuation, capitalization, fillers and false starts; keeps your wording.
# "formal": also tidies grammar and phrasing for written text.
style = "light"
# Replaces the built-in instructions (the system prompt) and examples; "" = use the style.
prompt = ""
# The user message; {text} is replaced by the transcript. "" = the transcript between
# <transcript> tags. For models that expect a specific input format (see README: S1-mini).
user_message = ""
# Give up and paste the raw transcript after timeout_ms + timeout_per_word_ms per word.
# 0 = provider default: 2000 for openai (local servers), 3000 for anthropic (cloud).
timeout_ms = 0
timeout_per_word_ms = 15
# Transcripts with fewer words than this are pasted as they are.
min_words = 4

[overlay]
# Show a small bar with a live waveform while recording and an animation while transcribing.
# It never takes focus or catches clicks.
enabled = true
# "bottom" or "top" of the screen with the focused window (horizontally centered).
position = "bottom"

[update]
# Check GitHub Releases for updates on startup and periodically (tray "Update now" works
# either way).
enabled = true
# Minutes between update checks (minimum 30).
check_interval_minutes = 240
"#;

/// Lower bound for `update.check_interval_minutes`.
pub const MIN_CHECK_INTERVAL_MINUTES: u64 = 30;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub log_level: String,
    pub hotkey: HotkeyConfig,
    pub audio: AudioConfig,
    pub stt: SttConfig,
    pub paste: PasteConfig,
    pub polish: PolishConfig,
    pub overlay: OverlayConfig,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// Key name; parsed (and rejected if unknown) by `agt-platform`.
    pub key: String,
    pub min_hold_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub keep_mic_open: bool,
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
    /// Apple's Core ML. Parsed, but no engine supports it yet (the macOS work, issue #9).
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

/// `[polish]`: optional LLM clean-up of transcripts (see `agt-polish`).
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolishProvider {
    /// OpenAI-compatible `/chat/completions` (Ollama, LM Studio, llama-server, cloud).
    OpenAi,
    /// Anthropic Messages API.
    Anthropic,
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
            enabled: false,
            provider: PolishProvider::OpenAi,
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
        }
    }
}

impl PolishConfig {
    pub fn base_url(&self) -> &str {
        match (self.base_url.as_str(), self.provider) {
            ("", PolishProvider::OpenAi) => "http://localhost:11434/v1",
            ("", PolishProvider::Anthropic) => "https://api.anthropic.com",
            (url, _) => url,
        }
    }

    pub fn model(&self) -> &str {
        match (self.model.as_str(), self.provider) {
            ("", PolishProvider::OpenAi) => "qwen2.5:3b",
            ("", PolishProvider::Anthropic) => "claude-haiku-5-5",
            (model, _) => model,
        }
    }

    /// The environment variable the key is read from, if any.
    pub fn api_key_var(&self) -> Option<&str> {
        match (self.api_key_env.as_str(), self.provider) {
            ("", PolishProvider::OpenAi) => None,
            ("", PolishProvider::Anthropic) => Some("ANTHROPIC_API_KEY"),
            (var, _) => Some(var),
        }
    }

    /// `api_key` if set, else the (non-empty) variable named by [`Self::api_key_var`].
    pub fn api_key(&self) -> Option<String> {
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
            (0, PolishProvider::OpenAi) => 2000,
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub enabled: bool,
    check_interval_minutes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            hotkey: HotkeyConfig::default(),
            audio: AudioConfig::default(),
            stt: SttConfig::default(),
            paste: PasteConfig::default(),
            polish: PolishConfig::default(),
            overlay: OverlayConfig::default(),
            update: UpdateConfig::default(),
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: "RightCtrl".into(),
            min_hold_ms: 150,
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
    Parse(toml::de::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "config I/O error: {e}"),
            Self::Parse(e) => write!(f, "invalid config.toml: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(ConfigError::Parse)
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

    /// What to run with when the config file can't be loaded: defaults, but with
    /// updates off, since the user may have disabled them in the file we couldn't read.
    pub fn fallback() -> Self {
        let mut c = Self::default();
        c.update.enabled = false;
        c
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
        assert_eq!(c.stt, SttConfig::default());
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
    fn models_dir_override() {
        let c = Config::parse("[stt]\nmodels_dir = 'D:\\models'").unwrap();
        assert_eq!(c.stt.models_path(), PathBuf::from("D:\\models"));
    }

    #[test]
    fn polish_defaults_depend_on_provider() {
        let c = Config::default().polish;
        assert!(!c.enabled);
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
        assert!(Config::parse("[polish]\nprovider = \"gemini\"").is_err());
        assert!(Config::parse("[polish]\nstyle = \"pirate\"").is_err());
    }

    #[test]
    fn polish_api_key_is_never_debug_printed() {
        let c = Config::parse("[polish]\napi_key = \" sk-secret \"").unwrap();
        assert_eq!(c.polish.api_key().as_deref(), Some("sk-secret"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-secret") && dbg.contains("<redacted>"));
    }

    #[test]
    fn fallback_disables_updates() {
        assert!(!Config::fallback().update.enabled);
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
