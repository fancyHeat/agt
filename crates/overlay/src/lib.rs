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

/// The OS window the pill is shown in.
trait Surface {
    /// Picks the screen for the next showing and remembers where on it the pill goes.
    /// Sizes are in logical pixels; returns the scale (pixels per logical pixel).
    fn place(&mut self, width: f32, height: f32, margin: f32, top: bool) -> f32;
    /// Puts the pixels on screen at the placed position, showing the window if hidden.
    fn present(&mut self, canvas: &Canvas);
    fn hide(&mut self);
}

pub struct Overlay {
    window: Box<dyn Surface>,
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
    /// Creates the (hidden) window.
    pub fn new(position: Position) -> Result<Self, Error> {
        Ok(Self::with_surface(Box::new(sys::Window::new()?), position))
    }

    fn with_surface(window: Box<dyn Surface>, position: Position) -> Self {
        let now = Instant::now();
        Self {
            window,
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
    /// recording (no microphone, model not ready), which would otherwise show nothing.
    pub fn flash_error(&mut self, level: f32, now: Instant) {
        if self.mode != Mode::Error {
            self.enter(Mode::Error, level, now);
        }
    }

    fn enter(&mut self, mode: Mode, level: f32, now: Instant) {
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

    /// When [`Overlay::tick`] should next be called; `None` while hidden (no timer needed).
    pub fn next_frame(&self) -> Option<Instant> {
        self.next
    }

    /// Draws the next frame if it's due (and hides after an error flash).
    pub fn tick(&mut self, level: f32, now: Instant) {
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

    fn overlay() -> (Overlay, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        (
            Overlay::with_surface(Box::new(Fake(log.clone())), Position::Bottom),
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
}
