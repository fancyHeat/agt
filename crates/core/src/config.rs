//! `config.toml`. Every field has a default, so a partial or empty file is valid.
//! Unknown keys are rejected so typos surface instead of being silently ignored.

use serde::Deserialize;
use std::{fs, io, path::Path};

/// Written on first run. Kept as text (not serialized) so the comments survive.
pub const DEFAULT_CONFIG: &str = r#"# AGT configuration. Restart AGT after editing.
# If this file has an error, AGT starts with defaults (updates off) and logs the error.

# Log verbosity: "error", "warn", "info", "debug" or "trace".
log_level = "info"

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
    pub update: UpdateConfig,
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
            update: UpdateConfig::default(),
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
