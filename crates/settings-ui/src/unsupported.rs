//! Other platforms: no settings window (the tray's Settings menu and config.toml remain).

use crate::{Error, Icon, Model};

pub fn run(_: &str, _: Box<dyn Model>, _: Option<&str>, _: Option<Icon>) -> Result<(), Error> {
    Err("the settings window isn't available on this platform; edit config.toml".into())
}

pub fn activate(_: u32, _: Option<&str>) -> bool {
    false
}
