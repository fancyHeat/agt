//! Tray icon (on macOS, the menu-bar item): a status line, a polish on/off line, a config
//! error line (while there is one), "Restart now to apply settings" (while changed settings
//! wait for a restart), "Copy last transcript", "Check for updates", "Settings..." (the
//! settings window, see [`crate::settings`]), "Metrics..." (the same window on its Metrics
//! page), (macOS) "Privacy & Security settings", "Open config folder", "Open data folder"
//! (where it differs) and "Quit".

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
};

/// Label of the update item when it's idle.
const CHECK_FOR_UPDATES: &str = "Check for updates";

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
    apply_item: MenuItem,
    apply_shown: bool,
    copy_last_item: MenuItem,
    update_item: MenuItem,
    pub copy_last: MenuId,
    pub update: MenuId,
    /// "Restart now to apply settings": restarts without waiting for dictation to be idle.
    pub apply_now: MenuId,
    /// "Settings...": opens the settings window.
    pub open_settings: MenuId,
    /// "Metrics...": opens the settings window on its Metrics page.
    pub open_metrics: MenuId,
    /// macOS only: opens the System Settings page of the first missing permission.
    pub privacy: Option<MenuId>,
    pub open_config: MenuId,
    /// "Open data folder" (the usage history, agt.db), when it differs from the config folder.
    pub open_data: Option<MenuId>,
    pub quit: MenuId,
}

impl Tray {
    /// `polish`: a disabled info line, e.g. "Polish: off".
    /// `installed`: whether this is a Velopack install (else "Check for updates" is disabled).
    /// `data_folder`: whether to offer "Open data folder".
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
        let apply = MenuItem::new("Restart now to apply settings", true, None);
        // Recovery path when a paste failed; enabled once there is a transcript.
        let copy_last = MenuItem::new("Copy last transcript", false, None);
        let update = if installed {
            MenuItem::new(CHECK_FOR_UPDATES, true, None)
        } else {
            MenuItem::new(format!("{CHECK_FOR_UPDATES} (not installed)"), false, None)
        };
        let open_settings = MenuItem::new("Settings...", true, None);
        let open_metrics = MenuItem::new("Metrics...", true, None);
        // "&&": a lone "&" marks a mnemonic and is dropped from the text.
        let privacy = cfg!(target_os = "macos")
            .then(|| MenuItem::new("Privacy && Security settings...", true, None));
        let open_config = MenuItem::new("Open config folder", true, None);
        let open_data = data_folder.then(|| MenuItem::new("Open data folder", true, None));
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::with_items(&[
            &title_item,
            &status,
            &polish,
            &PredefinedMenuItem::separator(),
        ])?;
        menu.append_items(&[&copy_last, &update, &open_settings, &open_metrics])?;
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
            apply_now: apply.id().clone(),
            apply_item: apply,
            apply_shown: false,
            copy_last: copy_last.id().clone(),
            copy_last_item: copy_last,
            update: update.id().clone(),
            update_item: update,
            open_settings: open_settings.id().clone(),
            open_metrics: open_metrics.id().clone(),
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

    /// Shows "Restart now to apply settings" (after the config error line, if any) while
    /// changed settings wait for a restart, or removes it.
    pub fn set_applying(&mut self, applying: bool) {
        if applying == self.apply_shown {
            return;
        }
        let result = if applying {
            let at = WARNING_AT + usize::from(self.warning_shown);
            self.menu.insert(&self.apply_item, at)
        } else {
            self.menu.remove(&self.apply_item)
        };
        match result {
            Ok(()) => self.apply_shown = applying,
            Err(e) => tracing::debug!("cannot update the restart item: {e}"),
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

    /// Shows update progress or a result on the "Check for updates" item; clickable when idle
    /// or while an install waits ("Install update X now").
    pub fn set_update(&self, text: &str, enabled: bool) {
        self.update_item.set_text(text);
        self.update_item.set_enabled(enabled);
    }

    /// Opens the menu again (clicking an item closes it), so what the click started, such as
    /// an update check, can be watched. macOS only. Deferred to the run loop: the open menu
    /// runs a loop of its own, and inside an event handler it would hold back every other
    /// event (the check's progress among them) until it closed.
    pub fn reopen_menu(&self) {
        #[cfg(target_os = "macos")]
        {
            use objc2_core_foundation::{CFRunLoop, kCFRunLoopCommonModes};
            let Some(main) = CFRunLoop::main() else {
                return;
            };
            let icon = std::cell::Cell::new(Some(self.icon.clone()));
            let block = block2::RcBlock::new(move || {
                if let Some(icon) = icon.take() {
                    icon.show_menu();
                }
            });
            // SAFETY: the main run loop, a run loop mode, a block that takes nothing.
            unsafe {
                main.perform_block(kCFRunLoopCommonModes.map(|m| &**m), Some(&block));
            }
            main.wake_up();
        }
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

/// The tray's icon, 32x32.
fn icon(style: Style) -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_rgba(rgba(style, 32), 32, 32)
}

/// The app's icon, `n` pixels square, RGBA: the tray's (but on macOS) and the settings
/// window's.
pub fn color_icon(n: u32) -> Vec<u8> {
    rgba(Style::Color, n as usize)
}

/// A glyph drawn in code, so there are no image assets to ship: `n` pixels square (drawn
/// for 32, scaled), RGBA.
fn rgba(style: Style, n: usize) -> Vec<u8> {
    const BLUE: [f32; 3] = [37.0, 99.0, 235.0];
    let s = n as f32 / 32.0;
    let c = (n as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity(n * n * 4);
    for y in 0..n {
        for x in 0..n {
            let d = (x as f32 - c).hypot(y as f32 - c);
            // Antialiased coverage of the band inner..outer.
            let band = |inner: f32, outer: f32| {
                (d - inner + 0.5).clamp(0.0, 1.0) * (outer - d + 0.5).clamp(0.0, 1.0)
            };
            let dot = band(-1.0, 6.0 * s);
            let px = match style {
                Style::Template => [0.0, 0.0, 0.0, dot.max(band(11.0 * s, 15.0 * s))],
                Style::Color => {
                    let mix = |b: f32| b + (255.0 - b) * dot;
                    [
                        mix(BLUE[0]),
                        mix(BLUE[1]),
                        mix(BLUE[2]),
                        band(-1.0, 15.0 * s),
                    ]
                }
            };
            rgba.extend_from_slice(&[px[0] as u8, px[1] as u8, px[2] as u8, (px[3] * 255.0) as u8]);
        }
    }
    rgba
}
