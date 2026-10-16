//! macOS: a borderless, non-activating `NSPanel` whose layer-backed content view shows each
//! frame as the layer's contents (a `CGImage` over a copy of the canvas pixels).
//!
//! - `NonactivatingPanel` style + `orderFrontRegardless`: showing it never activates AGT or
//!   takes key focus from the frontmost app.
//! - `ignoresMouseEvents`: clicks go to whatever is below.
//! - Status-bar window level, and `CanJoinAllSpaces | FullScreenAuxiliary | Stationary |
//!   IgnoresCycle`: above normal windows, on every Space including full-screen apps, not
//!   moved by Mission Control, skipped by Cmd+` window cycling.
//! - `hidesOnDeactivate = false`: panels hide when their app deactivates by default, and AGT
//!   (an accessory app) is never active.
//! - Frames: the canvas is premultiplied `0xAARRGGBB`, which is exactly Core Graphics'
//!   `PremultipliedFirst | ByteOrder32Little`, so a frame is one ~100 KB copy into a `CFData`
//!   and a new `CGImage` set as `layer.contents` (Core Animation uploads it as is). A new image
//!   per frame, never a mutated one: AppKit and Core Animation cache image contents.
//!
//! The pill goes on `NSScreen.mainScreen`: the screen with the keyboard focus when "Displays
//! have separate Spaces" is on (the default), otherwise the primary display.

use crate::{Error, Surface, canvas::Canvas};
use objc2::{
    MainThreadMarker, MainThreadOnly, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::AnyObject,
};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSScreen, NSStatusWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CFRetained};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

pub struct Window {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    /// The content view's backing `CALayer` (not retained separately: the view owns it).
    layer: *mut AnyObject,
    color_space: CFRetained<CGColorSpace>,
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
        // Not `setFloatingPanel(true)`: that would reset the level to NSFloatingWindowLevel.
        panel.setLevel(NSStatusWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(true);

        let view = NSView::new(mtm);
        view.setWantsLayer(true);
        let layer: *mut AnyObject = unsafe { msg_send![&*view, layer] };
        if layer.is_null() {
            return Err("the overlay view has no layer".into());
        }
        panel.setContentView(Some(&view));
        let color_space = CGColorSpace::new_device_rgb().ok_or("cannot create a color space")?;
        Ok(Self {
            mtm,
            panel,
            layer,
            color_space,
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
        // AppKit autoreleases objects here; drain them every frame, not when the run loop
        // gets to it.
        autoreleasepool(|_| self.present_inner(canvas));
    }

    fn hide(&mut self) {
        if self.visible {
            self.panel.orderOut(None);
            self.visible = false;
        }
    }
}

impl Window {
    fn present_inner(&mut self, canvas: &Canvas) {
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

        let Some(image) = self.image(canvas) else {
            tracing::warn!("overlay: cannot create the frame image");
            return;
        };
        let image = CFRetained::as_ptr(&image).as_ptr() as *const AnyObject;
        // The view's layer disables implicit animations, so this shows at once.
        let _: () = unsafe { msg_send![self.layer, setContents: image] };

        if !self.visible {
            self.panel.orderFrontRegardless();
            self.visible = true;
        }
    }

    /// A `CGImage` over a copy of the canvas pixels.
    fn image(&self, canvas: &Canvas) -> Option<CFRetained<CGImage>> {
        let (w, h) = (canvas.width(), canvas.height());
        let pixels = canvas.pixels();
        // SAFETY: `u32`s viewed as their bytes (little-endian: B, G, R, A).
        let bytes =
            unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), pixels.len() * 4) };
        let data = CFData::from_bytes(bytes);
        let provider = CGDataProvider::with_cf_data(Some(&data))?;
        let info = CGBitmapInfo::from_bits_retain(
            CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0,
        );
        unsafe {
            CGImage::new(
                w,
                h,
                8,
                32,
                w * 4,
                Some(&self.color_space),
                info,
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.panel.orderOut(None);
        self.panel.close();
    }
}
