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
# Hold this key to dictate, release to paste. One of: RightCtrl, LeftCtrl, RightAlt,
# RightShift, LeftShift, RightWin, CapsLock, ScrollLock, Pause, Insert, F1-F24.
# The key is not swallowed. Pressing any other key while holding it (e.g. Esc) cancels.
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
# Speech-to-text engine. Currently only "parakeet" (NVIDIA Parakeet TDT via ONNX Runtime).
engine = "parakeet"
# "parakeet-tdt-0.6b-v2" (English) or "parakeet-tdt-0.6b-v3" (25 European languages).
model = "parakeet-tdt-0.6b-v2"
# "int8" (650 MB, fast on CPU) or "fp32" (2.5 GB; use with a GPU).
precision = "int8"
# Where inference runs: "cpu", or "directml" (any DirectX 12 GPU; Windows only).
device = "cpu"
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

[update]
# Check GitHub Releases for updates on startup and periodically.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Precision {
    Int8,
    Fp32,
}

/// ONNX Runtime execution provider. Phase 2 adds `cuda` and `coreml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Device {
    Cpu,
    DirectMl,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PasteConfig {
    pub trailing_space: bool,
    pub restore_clipboard_ms: u64,
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
            device: Device::Cpu,
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
        assert!(Config::parse("[stt]\nengine = \"whisper\"").is_err());
    }

    #[test]
    fn models_dir_override() {
        let c = Config::parse("[stt]\nmodels_dir = 'D:\\models'").unwrap();
        assert_eq!(c.stt.models_path(), PathBuf::from("D:\\models"));
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
