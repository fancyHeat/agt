//! Platform directories. Nothing here creates directories.
//!
//! | | config (`config_dir`) | data: lock, llama-server log (`data_dir`) | usage history: `agt.db` (`history_dir`) | models (`models_dir`) |
//! |---|---|---|---|---|
//! | Windows | `%APPDATA%\AGT` (roaming) | `%LOCALAPPDATA%\AGT` (also the Velopack install root) | `%LOCALAPPDATA%\AGT-data` | `%LOCALAPPDATA%\AGT-models` |
//! | macOS | `~/Library/Application Support/AGT` | same | same | `<data>/models` |
//! | other | `$XDG_CONFIG_HOME/agt` | `$XDG_DATA_HOME/agt` | same as data | `<data>/models` |
//!
//! On Windows, models and the usage history live outside `%LOCALAPPDATA%\AGT` because
//! Velopack wipes that folder when Setup.exe is re-run: multi-GB models and the history must
//! survive reinstalls. Uninstalling removes `AGT-data` (the app's uninstall hook) but not
//! the models.
//!
//! `AGT_HOME`, if set, replaces them all (models go to `$AGT_HOME/models`); handy for tests
//! and dev runs. `stt.models_dir` in the config overrides the models location.

use std::path::PathBuf;

/// Holds `config.toml`. Small, user-edited, fine to roam.
pub fn config_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::config_dir)
}

/// Holds the instance lock and llama-server's log. Machine-local, never roams.
pub fn data_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::data_dir)
}

/// Holds the usage history, `agt.db` (see the module docs). Machine-local.
pub fn history_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::history_dir)
}

/// Default location of downloaded models (see the module docs).
pub fn models_dir() -> PathBuf {
    home_override().map_or_else(platform::models_dir, |h| h.join("models"))
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// The telemetry database (sessions, dictation metrics, events; see `agt-telemetry`).
pub fn db_file() -> PathBuf {
    history_dir().join("agt.db")
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
    pub fn models_dir() -> PathBuf {
        env_dir("LOCALAPPDATA").join("AGT-models")
    }
    pub fn history_dir() -> PathBuf {
        env_dir("LOCALAPPDATA").join("AGT-data")
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
    pub fn models_dir() -> PathBuf {
        data_dir().join("models")
    }
    pub fn history_dir() -> PathBuf {
        data_dir()
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
    pub fn models_dir() -> PathBuf {
        data_dir().join("models")
    }
    pub fn history_dir() -> PathBuf {
        data_dir()
    }
}
