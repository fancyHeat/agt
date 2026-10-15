//! Platform directories. Nothing here creates directories.
//!
//! | | config (`config_dir`) | data: logs, models, lock (`data_dir`) |
//! |---|---|---|
//! | Windows | `%APPDATA%\AGT` (roaming) | `%LOCALAPPDATA%\AGT` (local; also the Velopack install root) |
//! | macOS | `~/Library/Application Support/AGT` | same |
//! | other | `$XDG_CONFIG_HOME/agt` | `$XDG_DATA_HOME/agt` |
//!
//! `AGT_HOME`, if set, replaces both (handy for tests and dev runs).

use std::path::PathBuf;

/// Holds `config.toml`. Small, user-edited, fine to roam.
pub fn config_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::config_dir)
}

/// Holds logs, the instance lock and (later) models. Machine-local, never roams.
pub fn data_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::data_dir)
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

fn home_override() -> Option<PathBuf> {
    std::env::var_os("AGT_HOME").map(PathBuf::from)
}

fn env_dir(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("${var} is not set")))
}

#[cfg(windows)]
mod platform {
    use super::*;
    pub fn config_dir() -> PathBuf {
        env_dir("APPDATA").join("AGT")
    }
    pub fn data_dir() -> PathBuf {
        env_dir("LOCALAPPDATA").join("AGT")
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    pub fn config_dir() -> PathBuf {
        env_dir("HOME").join("Library/Application Support/AGT")
    }
    pub fn data_dir() -> PathBuf {
        config_dir()
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    fn xdg(var: &str, fallback: &str) -> PathBuf {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| env_dir("HOME").join(fallback))
            .join("agt")
    }
    pub fn config_dir() -> PathBuf {
        xdg("XDG_CONFIG_HOME", ".config")
    }
    pub fn data_dir() -> PathBuf {
        xdg("XDG_DATA_HOME", ".local/share")
    }
}
