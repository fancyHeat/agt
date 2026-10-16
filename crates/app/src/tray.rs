//! Tray icon (on macOS, the menu-bar item): a status line, a polish on/off line, a config
//! error line (while there is one), "Copy last transcript", "Update now", "Settings" (see
//! [`crate::settings`]), (macOS) "Privacy & Security settings", "Open config folder", "Open
//! data folder" (where it differs) and "Quit".

use crate::settings::Settings;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
};

/// Label of the "Update now" item when it's idle.
pub const UPDATE_NOW: &str = "Update now";

/// Where the config error line goes: after the title, status, polish line and separator.
const WARNING_AT: usize = 4;

pub struct Tray {
    icon: TrayIcon,
    menu: Menu,
    title: String,
    status: MenuItem,
    polish: MenuItem,
    warning: MenuItem,
    warning_shown: bool,
    pub settings: Settings,
    copy_last_item: MenuItem,
    update_item: MenuItem,
    pub copy_last: MenuId,
    pub update: MenuId,
    /// macOS only: opens the System Settings page of the first missing permission.
    pub privacy: Option<MenuId>,
    pub open_config: MenuId,
    /// "Open data folder" (the usage history, agt.db), when it differs from the config folder.
    pub open_data: Option<MenuId>,
    pub quit: MenuId,
}

impl Tray {
    /// `polish`: a disabled info line, e.g. "Polish: off".
    /// `installed`: whether this is a Velopack install (else "Update now" is disabled).
    /// `data_folder`: whether to offer "Open data folder".
    /// The Settings submenu starts empty: fill it with [`Settings::show`].
    pub fn new(
        version: &str,
        polish: &str,
        installed: bool,
        data_folder: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let title = format!("AGT {version}");
        let title_item = MenuItem::new(&title, false, None);
        let status = MenuItem::new("Starting...", false, None);
        let warning = MenuItem::new("", false, None);
        let polish = MenuItem::new(polish, false, None);
        // Recovery path when a paste failed; enabled once there is a transcript.
        let copy_last = MenuItem::new("Copy last transcript", false, None);
        let update = if installed {
            MenuItem::new(UPDATE_NOW, true, None)
        } else {
            MenuItem::new("Update now (not installed)", false, None)
        };
        let settings = Settings::new();
        let privacy = cfg!(target_os = "macos")
            .then(|| MenuItem::new("Privacy & Security settings...", true, None));
        let open_config = MenuItem::new("Open config folder", true, None);
        let open_data = data_folder.then(|| MenuItem::new("Open data folder", true, None));
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::with_items(&[
            &title_item,
            &status,
            &polish,
            &PredefinedMenuItem::separator(),
        ])?;
        menu.append_items(&[&copy_last, &update, &settings.menu])?;
        if let Some(p) = &privacy {
            menu.append(p)?;
        }
        menu.append(&open_config)?;
        if let Some(item) = &open_data {
            menu.append(item)?;
        }
        menu.append(&quit)?;

        let builder = TrayIconBuilder::new()
            .with_tooltip(&title)
            .with_menu(Box::new(menu.clone()));
        // macOS: monochrome template (the system tints it for light/dark menu bars).
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon(Style::Template)?);
        // Elsewhere: a colored disk that reads on both light and dark taskbars.
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon(Style::Color)?);
        Ok(Self {
            icon: builder.build()?,
            menu,
            title,
            status,
            polish,
            warning,
            warning_shown: false,
            settings,
            copy_last: copy_last.id().clone(),
            copy_last_item: copy_last,
            update: update.id().clone(),
            update_item: update,
            privacy: privacy.map(|p| p.id().clone()),
            open_config: open_config.id().clone(),
            open_data: open_data.map(|i| i.id().clone()),
            quit: quit.id().clone(),
        })
    }

    /// Shows a disabled line about a config problem so it's noticeable, or removes it.
    pub fn set_warning(&mut self, warning: Option<&str>) {
        let result = match (warning, self.warning_shown) {
            (Some(text), shown) => {
                self.warning.set_text(text);
                if shown {
                    Ok(())
                } else {
                    self.menu.insert(&self.warning, WARNING_AT)
                }
            }
            (None, true) => self.menu.remove(&self.warning),
            (None, false) => Ok(()),
        };
        match result {
            Ok(()) => self.warning_shown = warning.is_some(),
            Err(e) => tracing::debug!("cannot update the config error line: {e}"),
        }
    }

    /// Replaces the polish info line (the managed S1-mini reports its progress there).
    pub fn set_polish(&self, polish: &str) {
        self.polish.set_text(polish);
    }

    pub fn set_copy_enabled(&self, enabled: bool) {
        if self.copy_last_item.is_enabled() != enabled {
            self.copy_last_item.set_enabled(enabled);
        }
    }

    /// Shows update progress or a result on the "Update now" item; clickable only when idle.
    pub fn set_update(&self, text: &str, enabled: bool) {
        self.update_item.set_text(text);
        self.update_item.set_enabled(enabled);
    }

    /// Shows `status` in the menu and the tooltip.
    pub fn set_status(&self, status: &str) {
        self.status.set_text(status);
        // Tooltips are limited to 127 characters on Windows.
        let tip: String = format!("{} - {status}", self.title)
            .chars()
            .take(127)
            .collect();
        if let Err(e) = self.icon.set_tooltip(Some(tip)) {
            tracing::debug!("cannot set tooltip: {e}");
        }
    }
}

#[allow(dead_code)] // each platform uses one variant
enum Style {
    /// Black ring and dot on transparent; only alpha matters.
    Template,
    /// White dot on a blue disk.
    Color,
}

/// A 32x32 glyph drawn in code, so there are no image assets to ship.
fn icon(style: Style) -> Result<Icon, tray_icon::BadIcon> {
    const N: usize = 32;
    const BLUE: [f32; 3] = [37.0, 99.0, 235.0];
    let c = (N as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity(N * N * 4);
    for y in 0..N {
        for x in 0..N {
            let d = (x as f32 - c).hypot(y as f32 - c);
            // Antialiased coverage of the band inner..outer.
            let band = |inner: f32, outer: f32| {
                (d - inner + 0.5).clamp(0.0, 1.0) * (outer - d + 0.5).clamp(0.0, 1.0)
            };
            let dot = band(-1.0, 6.0);
            let px = match style {
                Style::Template => [0.0, 0.0, 0.0, dot.max(band(11.0, 15.0))],
                Style::Color => {
                    let mix = |b: f32| b + (255.0 - b) * dot;
                    [mix(BLUE[0]), mix(BLUE[1]), mix(BLUE[2]), band(-1.0, 15.0)]
                }
            };
            rgba.extend_from_slice(&[px[0] as u8, px[1] as u8, px[2] as u8, (px[3] * 255.0) as u8]);
        }
    }
    Icon::from_rgba(rgba, N as u32, N as u32)
}
