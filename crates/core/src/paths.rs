//! Platform directories. Nothing here creates directories.
//!
//! | | config (`config_dir`) | data: instance lock, ducked-volume and login-item markers, the llama.cpp builds in `llama.cpp/` (`data_dir`) | usage history: `agt.db`, `llama-server.log`, saved dictations in `dictations/` (`history_dir`) | models (`models_dir`) |
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
//! and dev runs. `stt.models_dir` in the config overrides the models location; that folder
//! may be shared, so the programs AGT downloads (llama.cpp) don't go there.

use std::path::PathBuf;

/// Holds `config.toml`. Small, user-edited, fine to roam.
pub fn config_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::config_dir)
}

/// Holds the instance lock, the ducked-volume and login-item markers and the llama.cpp builds.
/// Machine-local, never roams.
pub fn data_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::data_dir)
}

/// Holds the usage history, `agt.db`, and `llama-server.log` (see the module docs).
/// Machine-local; the tray's "Open data folder".
pub fn history_dir() -> PathBuf {
    home_override().unwrap_or_else(platform::history_dir)
}

/// Default location of downloaded models (see the module docs).
pub fn models_dir() -> PathBuf {
    home_override().map_or_else(platform::models_dir, |h| h.join("models"))
}

/// The llama.cpp builds the managed S1-mini runs (see `agt-polish`): per-user, never the
/// models folder (it may be shared). On Windows this is inside the Velopack install root,
/// so uninstalling removes it with the app, not the history hook; a re-run of Setup.exe
/// means one more 32 MB download.
pub fn llama_dir() -> PathBuf {
    data_dir().join("llama.cpp")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// The telemetry database (sessions, dictation metrics, events; see `agt-telemetry`).
pub fn db_file() -> PathBuf {
    history_dir().join("agt.db")
}

/// Saved dictations, when `[history]` is on (see `agt-history`): one folder per local day.
pub fn dictations_dir() -> PathBuf {
    history_dir().join("dictations")
}

/// While other audio is ducked, the volume to restore if AGT dies before it does.
pub fn ducked_volume_file() -> PathBuf {
    data_dir().join("ducked-volume")
}

/// `launch_at_login` as AGT last applied it to the OS ("on" or "off"). On Windows it's in
/// the install folder, which uninstalling (whose hook also deletes it) or re-running
/// Setup.exe removes, so a reinstall registers again; on macOS it stays with the settings,
/// as there's no uninstaller.
pub fn login_item_file() -> PathBuf {
    data_dir().join("login-item")
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
