//! macOS: a borderless, non-activating `NSPanel` showing the frame in an `NSImageView`.
//!
//! - `NonactivatingPanel` style + `orderFrontRegardless`: showing it never activates AGT or
//!   takes key focus from the frontmost app.
//! - `ignoresMouseEvents`: clicks go to whatever is below.
//! - Status-bar window level, and `CanJoinAllSpaces | FullScreenAuxiliary | Stationary |
//!   IgnoresCycle`: above normal windows, on every Space including full-screen apps, not
//!   moved by Mission Control, skipped by Cmd+` window cycling.
//! - `hidesOnDeactivate = false`: panels hide when their app deactivates by default, and AGT
//!   (an accessory app) is never active.
//!
//! The pill goes on `NSScreen.mainScreen`, the screen with the keyboard focus.

use crate::{Error, Surface, canvas::Canvas};
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSBackingStoreType, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSImage, NSImageScaling,
    NSImageView, NSPanel, NSScreen, NSStatusWindowLevel, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

pub struct Window {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    view: Retained<NSImageView>,
    /// Visible frame (screen minus menu bar and Dock) of the chosen screen, in points.
    screen: NSRect,
    scale: f64,
    margin: f64,
    top: bool,
    frame: Option<NSRect>,
    visible: bool,
}

impl Window {
    pub fn new() -> Result<Self, Error> {
        let mtm =
            MainThreadMarker::new().ok_or("the overlay must be created on the main thread")?;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // We own the panel; closing must not free it behind our back.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setIgnoresMouseEvents(true);
        panel.setLevel(NSStatusWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setHidesOnDeactivate(false);
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);

        let view = NSImageView::new(mtm);
        view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
        let content: &NSView = &view;
        panel.setContentView(Some(content));
        Ok(Self {
            mtm,
            panel,
            view,
            screen: rect,
            scale: 2.0,
            margin: 0.0,
            top: false,
            frame: None,
            visible: false,
        })
    }
}

impl Surface for Window {
    fn place(&mut self, _width: f32, _height: f32, margin: f32, top: bool) -> f32 {
        let screen =
            NSScreen::mainScreen(self.mtm).or_else(|| NSScreen::screens(self.mtm).firstObject());
        if let Some(screen) = screen {
            self.screen = screen.visibleFrame();
            self.scale = screen.backingScaleFactor();
        }
        self.margin = margin as f64;
        self.top = top;
        self.frame = None;
        self.scale as f32
    }

    fn present(&mut self, canvas: &Canvas) {
        let (w, h) = (canvas.width(), canvas.height());
        let size = NSSize::new(w as f64 / self.scale, h as f64 / self.scale);
        let s = self.screen;
        let origin = NSPoint::new(
            s.origin.x + ((s.size.width - size.width) / 2.0).round(),
            if self.top {
                s.origin.y + s.size.height - self.margin - size.height
            } else {
                s.origin.y + self.margin
            },
        );
        let frame = NSRect::new(origin, size);
        if self.frame != Some(frame) {
            self.panel.setFrame_display(frame, false);
            self.frame = Some(frame);
        }

        // An RGBA (premultiplied, alpha last) bitmap that owns its pixel buffer.
        let Some(rep) = (unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                w as isize,
                h as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                (w * 4) as isize,
                32,
            )
        }) else {
            tracing::warn!("overlay: cannot create bitmap");
            return;
        };
        let data = rep.bitmapData();
        if data.is_null() {
            return;
        }
        let out = unsafe { std::slice::from_raw_parts_mut(data, w * h * 4) };
        for (px, o) in canvas.pixels().iter().zip(out.as_chunks_mut::<4>().0) {
            // 0xAARRGGBB (premultiplied) -> R, G, B, A
            o.copy_from_slice(&[
                (px >> 16) as u8,
                (px >> 8) as u8,
                *px as u8,
                (px >> 24) as u8,
            ]);
        }
        let image = NSImage::initWithSize(NSImage::alloc(), size);
        image.addRepresentation(&rep);
        self.view.setImage(Some(&image));

        if !self.visible {
            self.panel.orderFrontRegardless();
            self.visible = true;
        }
    }

    fn hide(&mut self) {
        if self.visible {
            self.panel.orderOut(None);
            self.visible = false;
        }
    }
}
