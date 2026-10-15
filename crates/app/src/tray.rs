//! Tray icon with "Open config folder" and "Quit".

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
};

pub struct Tray {
    _icon: TrayIcon,
    pub open_config: MenuId,
    pub quit: MenuId,
}

impl Tray {
    pub fn new(version: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let title = MenuItem::new(format!("AGT {version}"), false, None);
        let open_config = MenuItem::new("Open config folder", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::with_items(&[
            &title,
            &PredefinedMenuItem::separator(),
            &open_config,
            &quit,
        ])?;
        let builder = TrayIconBuilder::new()
            .with_tooltip(format!("AGT {version}"))
            .with_menu(Box::new(menu));
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon()?);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon()?);
        let icon = builder.build()?;
        Ok(Self {
            _icon: icon,
            open_config: open_config.id().clone(),
            quit: quit.id().clone(),
        })
    }
}

/// A 32x32 white ring-and-dot glyph drawn in code, so there are no image assets to ship.
/// White suits the Windows taskbar; on macOS it is used as a template image.
fn icon() -> Result<Icon, tray_icon::BadIcon> {
    const N: usize = 32;
    let c = (N as f32 - 1.0) / 2.0;
    let mut rgba = Vec::with_capacity(N * N * 4);
    for y in 0..N {
        for x in 0..N {
            let d = (x as f32 - c).hypot(y as f32 - c);
            // Antialiased coverage of the band inner..outer.
            let band = |inner: f32, outer: f32| {
                (d - inner + 0.5).clamp(0.0, 1.0) * (outer - d + 0.5).clamp(0.0, 1.0)
            };
            let a = band(-1.0, 6.0).max(band(11.0, 15.0));
            rgba.extend_from_slice(&[255, 255, 255, (a * 255.0) as u8]);
        }
    }
    Icon::from_rgba(rgba, N as u32, N as u32)
}
