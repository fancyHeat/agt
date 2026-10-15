//! Platform directories.
//!
//! - Windows: `%APPDATA%\AGT`
//! - macOS:   `~/Library/Application Support/AGT`
//! - other:   `$XDG_CONFIG_HOME/agt` or `~/.config/agt`

use std::path::PathBuf;

/// Directory holding `config.toml` and `logs/`. Not created by this function.
pub fn app_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AGT_HOME") {
        return PathBuf::from(dir);
    }
    base_dir()
}

#[cfg(windows)]
fn base_dir() -> PathBuf {
    let appdata = std::env::var_os("APPDATA").expect("%APPDATA% is not set");
    PathBuf::from(appdata).join("AGT")
}

#[cfg(target_os = "macos")]
fn base_dir() -> PathBuf {
    home().join("Library/Application Support/AGT")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn base_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) => PathBuf::from(x).join("agt"),
        None => home().join(".config/agt"),
    }
}

#[cfg(unix)]
fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("$HOME is not set"))
}

pub fn config_file() -> PathBuf {
    app_dir().join("config.toml")
}

pub fn log_dir() -> PathBuf {
    app_dir().join("logs")
}
