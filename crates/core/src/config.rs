//! `config.toml`. Every field has a default, so a partial or empty file is valid.

use serde::Deserialize;
use std::{fs, io, path::Path};

/// Written on first run. Kept as text (not serialized) so the comments survive.
pub const DEFAULT_CONFIG: &str = r#"# AGT configuration. Restart AGT after editing.

# Log verbosity: "error", "warn", "info", "debug" or "trace".
log_level = "info"

[update]
# Check GitHub Releases for updates on startup and periodically.
enabled = true
# Minutes between update checks.
check_interval_minutes = 240
# GitHub token with read access to the (private) release repo.
# Can also be set via the AGT_GITHUB_TOKEN environment variable, which takes precedence.
# Without a token, update checks are skipped.
github_token = ""
"#;

/// Name of the environment variable that overrides `update.github_token`.
pub const TOKEN_ENV: &str = "AGT_GITHUB_TOKEN";

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
    pub check_interval_minutes: u64,
    pub github_token: String,
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
            github_token: String::new(),
        }
    }
}

impl UpdateConfig {
    /// Token from `AGT_GITHUB_TOKEN`, else from the config file. `None` if neither is set.
    pub fn token(&self) -> Option<String> {
        std::env::var(TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| Some(self.github_token.clone()).filter(|t| !t.trim().is_empty()))
            .map(|t| t.trim().to_owned())
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
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(ConfigError::Io)?;
            }
            fs::write(path, DEFAULT_CONFIG).map_err(ConfigError::Io)?;
        }
        Self::parse(&fs::read_to_string(path).map_err(ConfigError::Io)?)
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
        let c = Config::parse("[update]\ngithub_token = \"abc\"\n").unwrap();
        assert_eq!(c.update.github_token, "abc");
        assert_eq!(c.update.check_interval_minutes, 240);
        assert_eq!(c.log_level, "info");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("typo = 1").is_err());
    }

    #[test]
    fn load_or_create_writes_default() {
        let dir = std::env::temp_dir().join(format!("agt-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = fs::remove_dir_all(&dir);
        let c = Config::load_or_create(&path).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        fs::remove_dir_all(&dir).unwrap();
    }
}
