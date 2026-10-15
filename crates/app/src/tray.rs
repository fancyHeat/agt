//! Tray icon: a status line, a polish on/off line, "Open config folder" and "Quit".

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
};

pub struct Tray {
    icon: TrayIcon,
    title: String,
    status: MenuItem,
    pub open_config: MenuId,
    pub quit: MenuId,
}

impl Tray {
    /// `config_error`: shown as a disabled menu line so a broken config is noticeable.
    /// `polish`: a disabled info line, e.g. "Polish: off".
    pub fn new(
        version: &str,
        config_error: bool,
        polish: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let title = format!("AGT {version}");
        let title_item = MenuItem::new(&title, false, None);
        let status = MenuItem::new("Starting...", false, None);
        let warning = MenuItem::new("Config error (updates off) - see log", false, None);
        let polish = MenuItem::new(polish, false, None);
        let open_config = MenuItem::new("Open config folder", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::with_items(&[
            &title_item,
            &status,
            &polish,
            &PredefinedMenuItem::separator(),
        ])?;
        if config_error {
            menu.append(&warning)?;
        }
        menu.append_items(&[&open_config, &quit])?;

        let builder = TrayIconBuilder::new()
            .with_tooltip(&title)
            .with_menu(Box::new(menu));
        // macOS: monochrome template (the system tints it for light/dark menu bars).
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon(Style::Template)?);
        // Elsewhere: a colored disk that reads on both light and dark taskbars.
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon(Style::Color)?);
        Ok(Self {
            icon: builder.build()?,
            title,
            status,
            open_config: open_config.id().clone(),
            quit: quit.id().clone(),
        })
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
