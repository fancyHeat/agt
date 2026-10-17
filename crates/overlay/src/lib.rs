//! The overlay: a small pill near the bottom (or top) of the screen that shows a live
//! waveform while recording and a sweeping highlight while transcribing, then hides.
//!
//! - It never takes focus or intercepts input: on Windows a layered, click-through,
//!   no-activate tool window (not in the taskbar or Alt+Tab); on macOS a non-activating
//!   `NSPanel` that ignores the mouse and joins every Space, including full-screen apps.
//! - It's drawn on the CPU by [`canvas`] (a few antialiased rounded rectangles; no GPU, no
//!   webview) and handed to the OS as a premultiplied-alpha bitmap.
//! - The window is created hidden at start-up, so showing it is just one present.
//! - It costs nothing while hidden: [`Overlay::next_frame`] is `None`, so the caller's event
//!   loop has no timer to wake for. While visible it asks for ~60 frames per second.
//!
//! - After a paste that had no text field to land in, a notice takes the pill's place: "You
//!   weren't in a text box", with a Copy button (macOS; the only window here that takes a
//!   click, and it still never activates AGT or moves keyboard focus). A key press while
//!   the speech model is still loading shows "Getting ready..." the same way, without the
//!   button.
//!
//! All methods must be called on the UI (main) thread.

pub mod canvas;
pub mod scene;

#[cfg_attr(windows, path = "windows.rs")]
#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(not(any(windows, target_os = "macos")), path = "unsupported.rs")]
mod sys;

use canvas::Canvas;
use scene::{Meter, Scene};
use std::time::{Duration, Instant};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// What the overlay shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Hidden,
    /// Live waveform from the microphone level.
    Recording,
    /// Waiting for the transcript to be pasted.
    Transcribing,
    /// Something failed: a red flash for [`ERROR_FOR`], then hidden. `set_mode` ignores it
    /// while hidden (nothing on screen failed); [`Overlay::flash_error`] shows it anyway.
    Error,
}

/// Which screen edge the pill sits at (horizontally centered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Bottom,
    Top,
}

/// Frame interval while visible.
pub const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
/// How long the error flash stays up.
pub const ERROR_FOR: Duration = Duration::from_millis(900);
/// How long the "not in a text box" notice stays up if its button isn't clicked.
pub const NOTICE_FOR: Duration = Duration::from_secs(12);
/// How long it says "Copied" after the click.
pub const COPIED_FOR: Duration = Duration::from_millis(1200);
/// How long "Getting ready..." stays up.
pub const GETTING_READY_FOR: Duration = Duration::from_millis(1800);

pub const NOT_IN_TEXT_BOX: &str = "You weren't in a text box";
pub const GETTING_READY: &str = "Getting ready...";

/// The OS window the pill is shown in.
trait Surface {
    /// Picks the screen for the next showing and remembers where on it the pill goes.
    /// Sizes are in logical pixels; returns the scale (pixels per logical pixel).
    fn place(&mut self, width: f32, height: f32, margin: f32, top: bool) -> f32;
    /// Puts the pixels on screen at the placed position, showing the window if hidden.
    fn present(&mut self, canvas: &Canvas);
    fn hide(&mut self);
}

/// The OS window the notice ("not in a text box", "Getting ready...") is shown in.
trait NoticeSurface {
    /// Shows `text`, and the Copy button if `copy`, centered where the pill goes (`margin`
    /// from the bottom or top edge of the screen with the keyboard focus). `false` if it
    /// couldn't be shown (no screen).
    fn show(&mut self, text: &'static str, copy: bool, margin: f32, top: bool) -> bool;
    /// Swaps the message and button for "Copied", in place.
    fn copied(&mut self);
    fn hide(&mut self);
}

pub struct Overlay {
    window: Box<dyn Surface>,
    /// `None` where there is no notice (Windows for now).
    notice: Option<Box<dyn NoticeSurface>>,
    /// While the notice is up: when it hides.
    notice_until: Option<Instant>,
    /// Whether the notice up has the Copy button.
    notice_copy: bool,
    position: Position,
    mode: Mode,
    since: Instant,
    scale: f32,
    canvas: Canvas,
    /// The pill without its content, and whether it is the error one.
    background: Canvas,
    background_error: Option<bool>,
    meter: Meter,
    next: Option<Instant>,
}

impl Overlay {
    /// Creates the (hidden) windows. `on_copy` is called (on the UI thread) when the notice's
    /// Copy button is clicked.
    pub fn new(position: Position, on_copy: impl Fn() + 'static) -> Result<Self, Error> {
        let window = Box::new(sys::Window::new()?);
        let notice = match sys::Notice::new(Box::new(on_copy)) {
            Ok(n) => Some(Box::new(n) as Box<dyn NoticeSurface>),
            Err(e) => {
                tracing::info!("no \"not in a text box\" notice: {e}");
                None
            }
        };
        Ok(Self::with_surfaces(window, notice, position))
    }

    fn with_surfaces(
        window: Box<dyn Surface>,
        notice: Option<Box<dyn NoticeSurface>>,
        position: Position,
    ) -> Self {
        let now = Instant::now();
        Self {
            window,
            notice,
            notice_until: None,
            notice_copy: false,
            position,
            mode: Mode::Hidden,
            since: now,
            scale: 1.0,
            canvas: Canvas::new(0, 0),
            background: Canvas::new(0, 0),
            background_error: None,
            meter: Meter::new(now),
            next: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switches to `mode`, drawing (and showing) it right away. `level`: the microphone RMS.
    pub fn set_mode(&mut self, mode: Mode, level: f32, now: Instant) {
        if mode == self.mode {
            return;
        }
        match mode {
            Mode::Hidden => {
                self.window.hide();
                self.mode = Mode::Hidden;
                self.next = None;
                return;
            }
            // Nothing on screen to flag.
            Mode::Error if self.mode == Mode::Hidden => return,
            _ => {}
        }
        self.enter(mode, level, now);
    }

    /// Flashes the error state even if hidden: for a key press that couldn't start a
    /// recording (no microphone, the model failed to load), which would otherwise show
    /// nothing.
    pub fn flash_error(&mut self, level: f32, now: Instant) {
        if self.mode != Mode::Error {
            self.enter(Mode::Error, level, now);
        }
    }

    /// Shows "You weren't in a text box" with a Copy button where the pill was: for a
    /// dictation pasted where nothing takes text. Not over the pill (a new recording has
    /// already started); it hides after [`NOTICE_FOR`] or when the pill comes back.
    pub fn show_notice(&mut self, now: Instant) {
        if self.mode == Mode::Hidden {
            self.show_text(NOT_IN_TEXT_BOX, true, NOTICE_FOR, now);
        }
    }

    /// Says "Getting ready..." for [`GETTING_READY_FOR`]: for a key press while the speech
    /// model is still loading. Where there is no notice (Windows for now), flashes the error
    /// instead, so the press still shows something.
    pub fn show_getting_ready(&mut self, level: f32, now: Instant) {
        self.set_mode(Mode::Hidden, level, now);
        if !self.show_text(GETTING_READY, false, GETTING_READY_FOR, now) {
            self.flash_error(level, now);
        }
    }

    /// Shows the notice for `time`; `false` if it couldn't be.
    fn show_text(&mut self, text: &'static str, copy: bool, time: Duration, now: Instant) -> bool {
        let Some(notice) = &mut self.notice else {
            return false;
        };
        if !notice.show(text, copy, scene::MARGIN, self.position == Position::Top) {
            return false;
        }
        self.notice_until = Some(now + time);
        self.notice_copy = copy;
        true
    }

    /// The notice's text was copied: it says so, then hides after [`COPIED_FOR`].
    pub fn notice_copied(&mut self, now: Instant) {
        if let (Some(notice), Some(_), true) =
            (&mut self.notice, self.notice_until, self.notice_copy)
        {
            notice.copied();
            self.notice_until = Some(now + COPIED_FOR);
        }
    }

    /// Whether the notice is up.
    pub fn notice_visible(&self) -> bool {
        self.notice_until.is_some()
    }

    fn hide_notice(&mut self) {
        if self.notice_until.take().is_some()
            && let Some(notice) = &mut self.notice
        {
            notice.hide();
        }
    }

    fn enter(&mut self, mode: Mode, level: f32, now: Instant) {
        // The pill takes its place back.
        self.hide_notice();
        let showing = self.mode == Mode::Hidden;
        if showing {
            // Pick the screen (and so the scale) each time it appears.
            self.scale = self.window.place(
                scene::WIDTH,
                scene::HEIGHT,
                scene::MARGIN,
                self.position == Position::Top,
            );
            let (w, h) = scene::pixel_size(self.scale);
            self.canvas.reset(w, h);
            self.background_error = None;
        }
        if mode == Mode::Recording {
            self.meter = Meter::new(now);
        }
        self.mode = mode;
        self.since = now;
        self.draw(level, now);
        if showing {
            tracing::debug!(
                "overlay shown in {:.2} ms ({}x{} px)",
                now.elapsed().as_secs_f64() * 1e3,
                self.canvas.width(),
                self.canvas.height()
            );
        }
        self.next = Some(
            now + if mode == Mode::Error {
                ERROR_FOR
            } else {
                FRAME
            },
        );
    }

    /// When [`Overlay::tick`] should next be called; `None` while the pill and the notice are
    /// hidden (no timer needed). While only the notice is up, just when it hides.
    pub fn next_frame(&self) -> Option<Instant> {
        [self.next, self.notice_until].into_iter().flatten().min()
    }

    /// Draws the next frame if it's due (and hides after an error flash or the notice's time).
    pub fn tick(&mut self, level: f32, now: Instant) {
        if self.notice_until.is_some_and(|until| now >= until) {
            self.hide_notice();
        }
        let Some(due) = self.next else { return };
        if now < due {
            return;
        }
        if self.mode == Mode::Error {
            self.set_mode(Mode::Hidden, level, now);
            return;
        }
        self.draw(level, now);
        // Keep a steady cadence, but don't try to catch up after a stall.
        let next = due + FRAME;
        self.next = Some(if next > now { next } else { now + FRAME });
    }

    fn draw(&mut self, level: f32, now: Instant) {
        if self.mode == Mode::Recording {
            self.meter.update(level, now);
        }
        let error = self.mode == Mode::Error;
        if self.background_error != Some(error) {
            self.background
                .reset(self.canvas.width(), self.canvas.height());
            scene::render_background(self.mode, self.scale, &mut self.background);
            self.background_error = Some(error);
        }
        self.canvas.copy_from(&self.background);
        scene::render_content(
            &Scene {
                mode: self.mode,
                t: now.saturating_duration_since(self.since).as_secs_f32(),
                levels: self.meter.levels(),
                scale: self.scale,
            },
            &mut self.canvas,
        );
        self.window.present(&self.canvas);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    /// Records what the overlay asked of the window.
    #[derive(Default)]
    struct Log {
        places: usize,
        presents: usize,
        visible: bool,
        notice: Option<&'static str>,
        button: bool,
        /// The notice's `show` fails.
        no_screen: bool,
    }

    struct Fake(Rc<RefCell<Log>>);

    impl Surface for Fake {
        fn place(&mut self, _: f32, _: f32, _: f32, _: bool) -> f32 {
            self.0.borrow_mut().places += 1;
            1.5
        }
        fn present(&mut self, c: &Canvas) {
            assert_eq!((c.width(), c.height()), scene::pixel_size(1.5));
            let mut l = self.0.borrow_mut();
            l.presents += 1;
            l.visible = true;
        }
        fn hide(&mut self) {
            self.0.borrow_mut().visible = false;
        }
    }

    impl NoticeSurface for Fake {
        fn show(&mut self, text: &'static str, copy: bool, _: f32, _: bool) -> bool {
            let mut log = self.0.borrow_mut();
            if log.no_screen {
                return false;
            }
            log.notice = Some(text);
            log.button = copy;
            true
        }
        fn copied(&mut self) {
            let mut log = self.0.borrow_mut();
            log.notice = Some("copied");
            log.button = false;
        }
        fn hide(&mut self) {
            self.0.borrow_mut().notice = None;
        }
    }

    fn overlay() -> (Overlay, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        (
            Overlay::with_surfaces(
                Box::new(Fake(log.clone())),
                Some(Box::new(Fake(log.clone()))),
                Position::Bottom,
            ),
            log,
        )
    }

    #[test]
    fn shows_immediately_animates_while_visible_and_idles_when_hidden() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        assert_eq!(o.next_frame(), None);
        o.set_mode(Mode::Recording, 0.1, t);
        assert!(log.borrow().visible);
        assert_eq!(
            log.borrow().presents,
            1,
            "drawn at once, not on the next tick"
        );
        assert_eq!(o.next_frame(), Some(t + FRAME));

        o.tick(0.1, t + FRAME / 2);
        assert_eq!(log.borrow().presents, 1, "not due yet");
        o.tick(0.1, t + FRAME);
        assert_eq!(log.borrow().presents, 2);
        assert_eq!(o.next_frame(), Some(t + FRAME * 2));
        // After a stall, the cadence restarts from now instead of catching up.
        o.tick(0.1, t + FRAME * 10);
        assert_eq!(o.next_frame(), Some(t + FRAME * 11));

        o.set_mode(Mode::Transcribing, 0.0, t + FRAME * 12);
        assert_eq!(log.borrow().places, 1, "placed only when appearing");
        o.set_mode(Mode::Hidden, 0.0, t + FRAME * 13);
        assert!(!log.borrow().visible);
        assert_eq!(o.next_frame(), None, "no wakeups while hidden");
        o.tick(0.0, t + FRAME * 100);
        assert_eq!(log.borrow().presents, 4);
    }

    #[test]
    fn error_flashes_then_hides_and_is_ignored_while_hidden() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.set_mode(Mode::Error, 0.0, t);
        assert_eq!(o.mode(), Mode::Hidden);
        assert_eq!(log.borrow().presents, 0);

        o.set_mode(Mode::Transcribing, 0.0, t);
        o.set_mode(Mode::Error, 0.0, t);
        assert_eq!(o.mode(), Mode::Error);
        assert_eq!(
            o.next_frame(),
            Some(t + ERROR_FOR),
            "no animation, just the hide"
        );
        o.tick(0.0, t + ERROR_FOR);
        assert_eq!(o.mode(), Mode::Hidden);
        assert!(!log.borrow().visible);
        assert_eq!(o.next_frame(), None);
    }

    #[test]
    fn flash_error_shows_even_from_hidden_then_hides() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.flash_error(0.0, t);
        assert_eq!(o.mode(), Mode::Error);
        assert!(log.borrow().visible);
        assert_eq!(log.borrow().places, 1);
        o.tick(0.0, t + ERROR_FOR);
        assert_eq!(o.mode(), Mode::Hidden);
        assert!(!log.borrow().visible);
    }

    #[test]
    fn notice_shows_while_hidden_then_times_out() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.show_notice(t);
        assert_eq!(log.borrow().notice, Some(NOT_IN_TEXT_BOX));
        assert!(log.borrow().button);
        assert_eq!(
            o.next_frame(),
            Some(t + NOTICE_FOR),
            "one wakeup, to hide it"
        );
        o.tick(0.0, t + NOTICE_FOR / 2);
        assert!(o.notice_visible());
        o.tick(0.0, t + NOTICE_FOR);
        assert_eq!(log.borrow().notice, None);
        assert_eq!(o.next_frame(), None);
    }

    #[test]
    fn notice_says_copied_then_hides_soon() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.notice_copied(t);
        assert_eq!(log.borrow().notice, None, "nothing to confirm while hidden");
        o.show_notice(t);
        o.notice_copied(t + Duration::from_secs(2));
        assert_eq!(log.borrow().notice, Some("copied"));
        let until = t + Duration::from_secs(2) + COPIED_FOR;
        assert_eq!(o.next_frame(), Some(until));
        o.tick(0.0, until);
        assert!(!o.notice_visible());
    }

    #[test]
    fn notice_gives_way_to_the_pill() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.set_mode(Mode::Recording, 0.1, t);
        o.show_notice(t);
        assert_eq!(log.borrow().notice, None, "not over a recording");
        o.set_mode(Mode::Hidden, 0.0, t);
        o.show_notice(t);
        o.set_mode(Mode::Recording, 0.1, t + FRAME);
        assert_eq!(log.borrow().notice, None, "the next recording hides it");
        assert!(!o.notice_visible());
        assert_eq!(o.next_frame(), Some(t + FRAME * 2));
    }

    #[test]
    fn notice_shown_again_starts_over() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.show_notice(t);
        o.notice_copied(t + FRAME);
        let later = t + Duration::from_secs(5);
        o.show_notice(later);
        assert_eq!(
            log.borrow().notice,
            Some(NOT_IN_TEXT_BOX),
            "the button is back"
        );
        assert_eq!(o.next_frame(), Some(later + NOTICE_FOR));
    }

    #[test]
    fn notice_survives_status_churn_but_not_an_error() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.show_notice(t);
        // Already hidden (e.g. polish finishing after the paste): the notice stays.
        o.set_mode(Mode::Hidden, 0.0, t + FRAME);
        assert!(o.notice_visible());
        o.flash_error(0.0, t + FRAME * 2);
        assert_eq!(log.borrow().notice, None);
        assert!(!o.notice_visible());
    }

    #[test]
    fn notice_that_cannot_show_is_not_up() {
        let (mut o, log) = overlay();
        log.borrow_mut().no_screen = true;
        o.show_notice(Instant::now());
        assert!(
            !o.notice_visible(),
            "nothing on screen: don't hold off a restart"
        );
        assert_eq!(o.next_frame(), None);

        let log = Rc::new(RefCell::new(Log::default()));
        let mut o = Overlay::with_surfaces(Box::new(Fake(log)), None, Position::Bottom);
        o.show_notice(Instant::now());
        assert!(!o.notice_visible(), "no notice on this platform");
    }

    #[test]
    fn getting_ready_shows_without_a_button_then_times_out() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.show_getting_ready(0.0, t);
        assert_eq!(log.borrow().notice, Some(GETTING_READY));
        assert!(!log.borrow().button);
        assert_eq!(o.mode(), Mode::Hidden, "not the red flash");
        assert!(!log.borrow().visible);
        o.notice_copied(t + FRAME);
        assert_eq!(
            log.borrow().notice,
            Some(GETTING_READY),
            "nothing was copied from it"
        );
        assert_eq!(o.next_frame(), Some(t + GETTING_READY_FOR));
        o.tick(0.0, t + GETTING_READY_FOR);
        assert_eq!(log.borrow().notice, None);
        assert_eq!(o.next_frame(), None);
    }

    #[test]
    fn getting_ready_replaces_an_error_flash() {
        let (mut o, log) = overlay();
        let t = Instant::now();
        o.flash_error(0.0, t);
        o.show_getting_ready(0.0, t + FRAME);
        assert_eq!(o.mode(), Mode::Hidden);
        assert!(!log.borrow().visible);
        assert_eq!(log.borrow().notice, Some(GETTING_READY));
    }

    #[test]
    fn getting_ready_without_a_notice_flashes_the_error() {
        let log = Rc::new(RefCell::new(Log::default()));
        let mut o = Overlay::with_surfaces(Box::new(Fake(log.clone())), None, Position::Bottom);
        o.show_getting_ready(0.0, Instant::now());
        assert_eq!(o.mode(), Mode::Error);
        assert!(log.borrow().visible);
    }
}
