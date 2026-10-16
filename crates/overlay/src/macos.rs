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
//!
//! The "not in a text box" [`Notice`] is the same kind of panel, but it takes clicks: a
//! non-activating panel that can't become key gets them without AGT activating or the focused
//! app losing focus. Its pill is drawn by its view's layer (color, border, corner radius) and
//! the text is a real `NSTextField` and `NSButton`, so VoiceOver can read and press them.

use crate::{
    Error, NoticeSurface, Surface,
    canvas::{Canvas, Rgba},
    scene,
};
use objc2::{
    DefinedClass, Encode, Encoding, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::AnyObject,
    sel,
};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSBezelStyle, NSButton, NSColor, NSControl, NSControlSize, NSFont, NSFontWeightMedium, NSPanel,
    NSResponder, NSScreen, NSStatusWindowLevel, NSTextField, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CFRetained, CGFloat};
use objc2_core_graphics::{
    CGBitmapInfo, CGColor, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use std::panic::AssertUnwindSafe;

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

/// A borderless, transparent, non-activating panel above other windows on every Space
/// (see the module docs).
fn panel(mtm: MainThreadMarker) -> Retained<NSPanel> {
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
    panel
}

/// The screen the overlay goes on (see the module docs): its visible frame (minus menu bar and
/// Dock), in points, and its scale.
fn main_screen(mtm: MainThreadMarker) -> Option<(NSRect, f64)> {
    let screen = NSScreen::mainScreen(mtm).or_else(|| NSScreen::screens(mtm).firstObject())?;
    Some((screen.visibleFrame(), screen.backingScaleFactor()))
}

impl Window {
    pub fn new() -> Result<Self, Error> {
        let mtm =
            MainThreadMarker::new().ok_or("the overlay must be created on the main thread")?;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let panel = panel(mtm);

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
        if let Some((screen, scale)) = main_screen(self.mtm) {
            self.screen = screen;
            self.scale = scale;
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

// ---------------------------------------------------------------------------------------
// The "not in a text box" notice

const NOTICE_TEXT: &str = "You weren't in a text box";
const COPY_TEXT: &str = "Copy";
const COPIED_TEXT: &str = "Copied";
/// Notice height and paddings, in points. A little taller than the pill (36), centered on
/// the same line.
const NOTICE_HEIGHT: f64 = 40.0;
const PAD_LEFT: f64 = 16.0;
const PAD_RIGHT: f64 = 6.0;
const GAP: f64 = 10.0;

pub struct ButtonIvars {
    on_click: Box<dyn Fn()>,
}

define_class!(
    // SAFETY: NSButton has no subclassing requirements; `CopyButton` doesn't implement Drop.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AGTNoticeCopyButton"]
    #[ivars = ButtonIvars]
    pub struct CopyButton;

    impl CopyButton {
        /// The notice's panel is never key, so without this the first click would only try to
        /// make it key, and the button wouldn't fire.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&AnyObject>) -> bool {
            true
        }

        #[unsafe(method(agtCopyClicked:))]
        fn clicked(&self, _sender: Option<&AnyObject>) {
            // A panic mustn't unwind into AppKit's event dispatch.
            if std::panic::catch_unwind(AssertUnwindSafe(&self.ivars().on_click)).is_err() {
                tracing::error!("the notice's Copy handler panicked");
            }
        }
    }
);

impl CopyButton {
    fn new(mtm: MainThreadMarker, on_click: Box<dyn Fn()>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ButtonIvars { on_click });
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect] };
        // The target is a weak reference: no cycle.
        unsafe {
            this.setTarget(Some(&this));
            this.setAction(Some(sel!(agtCopyClicked:)));
        }
        this
    }
}

pub struct Notice {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    button: Retained<CopyButton>,
    /// Where the notice is centered, in screen points (kept when it shrinks to "Copied").
    center: NSPoint,
}

/// A `CGColorRef` argument. Not passed as an object: debug builds check message argument types
/// against the method's (`^{CGColor=}`), and an object (`@`) fails that.
#[repr(transparent)]
struct ColorRef(*const CGColor);

impl ColorRef {
    fn of(c: &CFRetained<CGColor>) -> Self {
        Self(CFRetained::as_ptr(c).as_ptr())
    }
}

// SAFETY: a pointer to an opaque `CGColor` struct, as CALayer's color setters take.
unsafe impl Encode for ColorRef {
    const ENCODING: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
}

fn cg_color(c: Rgba) -> CFRetained<CGColor> {
    CGColor::new_srgb(
        c.0 as CGFloat,
        c.1 as CGFloat,
        c.2 as CGFloat,
        c.3 as CGFloat,
    )
}

impl Notice {
    pub fn new(on_copy: Box<dyn Fn()>) -> Result<Self, Error> {
        let mtm = MainThreadMarker::new().ok_or("the notice must be created on the main thread")?;
        let panel = panel(mtm);
        panel.setIgnoresMouseEvents(false);
        // Light text and a dark button on the dark pill, whatever the system appearance.
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            panel.setAppearance(Some(&dark));
        }

        let view = NSView::new(mtm);
        view.setWantsLayer(true);
        let layer: *mut AnyObject = unsafe { msg_send![&*view, layer] };
        if layer.is_null() {
            return Err("the notice view has no layer".into());
        }
        // The same pill as the overlay's: background, hairline border, round ends.
        let (bg, border) = (cg_color(scene::BG), cg_color(scene::BORDER));
        unsafe {
            let _: () = msg_send![layer, setBackgroundColor: ColorRef::of(&bg)];
            let _: () = msg_send![layer, setBorderColor: ColorRef::of(&border)];
            let _: () = msg_send![layer, setBorderWidth: 1.0 as CGFloat];
            let _: () = msg_send![layer, setCornerRadius: (NOTICE_HEIGHT / 2.0) as CGFloat];
        }

        let label = NSTextField::labelWithString(&NSString::from_str(NOTICE_TEXT), mtm);
        label.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe {
            NSFontWeightMedium
        })));
        label.setTextColor(Some(&NSColor::whiteColor()));
        let button = CopyButton::new(mtm, on_copy);
        button.setTitle(&NSString::from_str(COPY_TEXT));
        button.setBezelStyle(NSBezelStyle::Push);
        button.setControlSize(NSControlSize::Regular);
        view.addSubview(&label);
        view.addSubview(&button);
        panel.setContentView(Some(&view));
        Ok(Self {
            mtm,
            panel,
            label,
            button,
            center: NSPoint::new(0.0, 0.0),
        })
    }

    /// Sizes the label (and the button, if shown) to fit, and the panel around them, centered
    /// on [`Notice::center`].
    fn layout(&self) {
        self.label.sizeToFit();
        let label = self.label.frame().size;
        let mut width = PAD_LEFT + label.width + PAD_LEFT;
        if !self.button.isHidden() {
            self.button.sizeToFit();
            let button = self.button.frame().size;
            let x = PAD_LEFT + label.width + GAP;
            self.button.setFrameOrigin(NSPoint::new(
                x,
                ((NOTICE_HEIGHT - button.height) / 2.0).round(),
            ));
            width = x + button.width + PAD_RIGHT;
        }
        self.label.setFrameOrigin(NSPoint::new(
            PAD_LEFT,
            ((NOTICE_HEIGHT - label.height) / 2.0).round(),
        ));
        let width = width.ceil();
        let origin = NSPoint::new(
            (self.center.x - width / 2.0).round(),
            (self.center.y - NOTICE_HEIGHT / 2.0).round(),
        );
        self.panel
            .setFrame_display(NSRect::new(origin, NSSize::new(width, NOTICE_HEIGHT)), true);
    }
}

impl NoticeSurface for Notice {
    fn show(&mut self, margin: f32, top: bool) -> bool {
        let Some((s, _)) = main_screen(self.mtm) else {
            return false;
        };
        // On the pill's center line.
        let half_pill = scene::HEIGHT as f64 / 2.0;
        let margin = margin as f64;
        self.center = NSPoint::new(
            s.origin.x + s.size.width / 2.0,
            if top {
                s.origin.y + s.size.height - margin - half_pill
            } else {
                s.origin.y + margin + half_pill
            },
        );
        self.label.setStringValue(&NSString::from_str(NOTICE_TEXT));
        self.button.setHidden(false);
        self.layout();
        self.panel.orderFrontRegardless();
        true
    }

    fn copied(&mut self) {
        self.label.setStringValue(&NSString::from_str(COPIED_TEXT));
        self.button.setHidden(true);
        self.layout();
    }

    fn hide(&mut self) {
        self.panel.orderOut(None);
    }
}

impl Drop for Notice {
    fn drop(&mut self) {
        self.panel.orderOut(None);
        self.panel.close();
    }
}
