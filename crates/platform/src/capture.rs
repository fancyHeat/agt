//! Recording a key press for the settings window: the keys pressed together, from the first
//! one down to the last one up, kept from every other app (AGT's own hotkey listener
//! included) while recording.

use crate::{Error, imp, keys::ESC_CODE};

/// What the key recorder ([`capture_keys`]) reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capture {
    /// The keys down now (platform key codes, in the order they went down), after each key
    /// that goes down or comes up, until the last one is released.
    Held(Vec<u16>),
    /// Every key was released: the most keys that were down at once (the first time that many
    /// were), in the order they went down. [`crate::Hotkey::from_codes`] says whether they
    /// make a hotkey.
    Done(Vec<u16>),
    /// Esc was pressed.
    Cancelled,
}

/// Keeps the key recorder running; dropping it removes its keyboard tap or hook and waits for
/// its thread to end.
#[allow(dead_code)] // held for its `Drop`
pub struct CaptureHandle(imp::CaptureHandle);

/// Starts recording a key press on a dedicated thread, ahead of every other app's view of the
/// keyboard: macOS an active event tap at the HID level, which comes before AGT's own (at the
/// session level); Windows a low-level keyboard hook, which runs before those installed
/// earlier (AGT's). While the handle lives, a key pressed reaches neither apps nor AGT's
/// listener, so it neither starts a dictation nor acts in the window behind. A key already
/// down when recording started (the Enter that clicked "Add hotkey") isn't recorded, and its
/// auto-repeat and release reach apps as usual.
///
/// `on` runs on that thread for each change ([`Capture`]) and must return quickly (every
/// keystroke on the system waits for it); it must not drop the handle. Nothing is reported
/// after [`Capture::Done`] or [`Capture::Cancelled`], but keys stay kept from apps until the
/// handle is dropped (so that, say, Esc's auto-repeat doesn't reach the window): drop it right
/// after either.
///
/// macOS: needs Accessibility (an active tap takes it); without it this fails at once.
pub fn capture_keys(on: impl Fn(Capture) + Send + 'static) -> Result<CaptureHandle, Error> {
    imp::capture_keys(Box::new(on)).map(CaptureHandle)
}

/// The press being recorded, from the key events the tap or hook sees, and which of them to keep
/// from apps (and from AGT's own listener). Platform-independent so it can be unit-tested.
///
/// - A key pressed during the capture is kept, its auto-repeat and release too. It's recorded
///   until [`Capture::Done`] or [`Capture::Cancelled`]; after either, keys are still kept but
///   nothing is reported.
/// - A key that was already down when the capture started (the Enter or Space that clicked
///   "Add hotkey", a held Ctrl) is never recorded, and its auto-repeat and release pass: apps
///   saw it go down, so they must see it come up.
#[derive(Debug, Default)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) struct KeyCapture {
    /// The keys pressed during the capture and not released since, in press order: recorded
    /// (until it's finished) and kept from apps.
    down: Vec<u16>,
    /// The most keys down at once so far, in press order.
    peak: Vec<u16>,
    /// `Done` or `Cancelled` was reported.
    finished: bool,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl KeyCapture {
    /// The key with platform code `code` went down (`down`) or up; `repeat`: the OS says this
    /// press is an auto-repeat (or already had the key down). Returns what to report, if
    /// anything, and whether to keep the event from apps.
    pub(crate) fn on_key(
        &mut self,
        code: u16,
        down: bool,
        repeat: bool,
    ) -> (Option<Capture>, bool) {
        match self.down.iter().position(|&c| c == code) {
            // An auto-repeat of a key pressed during the capture.
            Some(_) if down => (None, true),
            Some(i) => {
                self.down.remove(i);
                if self.finished {
                    (None, true)
                } else if self.down.is_empty() {
                    self.finished = true;
                    (Some(Capture::Done(std::mem::take(&mut self.peak))), true)
                } else {
                    (Some(Capture::Held(self.down.clone())), true)
                }
            }
            // Down before the capture started: its auto-repeat or release.
            None if !down || repeat => (None, false),
            None => {
                self.down.push(code);
                if self.finished {
                    (None, true)
                } else if code == ESC_CODE {
                    self.finished = true;
                    (Some(Capture::Cancelled), true)
                } else {
                    if self.down.len() > self.peak.len() {
                        self.peak.clone_from(&self.down);
                    }
                    (Some(Capture::Held(self.down.clone())), true)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kept from apps, or passed on.
    const KEEP: bool = true;
    const PASS: bool = false;

    fn held(codes: &[u16]) -> Option<Capture> {
        Some(Capture::Held(codes.to_vec()))
    }

    fn done(codes: &[u16]) -> Option<Capture> {
        Some(Capture::Done(codes.to_vec()))
    }

    /// A press as the hook sees it (on Windows, an auto-repeat of a key the capture saw go
    /// down looks the same).
    fn press(c: &mut KeyCapture, code: u16) -> (Option<Capture>, bool) {
        c.on_key(code, true, false)
    }

    /// A press the OS marks as an auto-repeat.
    fn repeat(c: &mut KeyCapture, code: u16) -> (Option<Capture>, bool) {
        c.on_key(code, true, true)
    }

    fn release(c: &mut KeyCapture, code: u16) -> (Option<Capture>, bool) {
        c.on_key(code, false, false)
    }

    #[test]
    fn records_peak_set_on_release() {
        let mut c = KeyCapture::default();
        assert_eq!(press(&mut c, 1), (held(&[1]), KEEP));
        assert_eq!(press(&mut c, 2), (held(&[1, 2]), KEEP));
        assert_eq!(release(&mut c, 2), (held(&[1]), KEEP));
        assert_eq!(release(&mut c, 1), (done(&[1, 2]), KEEP));
    }

    #[test]
    fn peak_is_the_first_largest_set() {
        let mut c = KeyCapture::default();
        press(&mut c, 1);
        press(&mut c, 2);
        assert_eq!(release(&mut c, 1), (held(&[2]), KEEP));
        assert_eq!(press(&mut c, 3), (held(&[2, 3]), KEEP));
        release(&mut c, 2);
        assert_eq!(release(&mut c, 3), (done(&[1, 2]), KEEP));
    }

    #[test]
    fn repeat_is_ignored() {
        let mut c = KeyCapture::default();
        assert_eq!(press(&mut c, 1), (held(&[1]), KEEP));
        assert_eq!(press(&mut c, 1), (None, KEEP));
        assert_eq!(repeat(&mut c, 1), (None, KEEP));
        assert_eq!(release(&mut c, 1), (done(&[1]), KEEP));
    }

    #[test]
    fn esc_cancels() {
        let mut c = KeyCapture::default();
        assert_eq!(press(&mut c, 1), (held(&[1]), KEEP));
        assert_eq!(press(&mut c, ESC_CODE), (Some(Capture::Cancelled), KEEP));
        // Still kept, but nothing more is reported.
        assert_eq!(repeat(&mut c, ESC_CODE), (None, KEEP));
        assert_eq!(release(&mut c, ESC_CODE), (None, KEEP));
        assert_eq!(release(&mut c, 1), (None, KEEP));
        assert_eq!(press(&mut c, 2), (None, KEEP));
        assert_eq!(release(&mut c, 2), (None, KEEP));
    }

    #[test]
    fn single_tap() {
        let mut c = KeyCapture::default();
        assert_eq!(press(&mut c, 7), (held(&[7]), KEEP));
        assert_eq!(release(&mut c, 7), (done(&[7]), KEEP));
    }

    #[test]
    fn nothing_after_done() {
        let mut c = KeyCapture::default();
        press(&mut c, 7);
        assert_eq!(release(&mut c, 7), (done(&[7]), KEEP));
        assert_eq!(press(&mut c, 8), (None, KEEP));
        assert_eq!(repeat(&mut c, 8), (None, KEEP));
        assert_eq!(release(&mut c, 8), (None, KEEP));
        assert_eq!(press(&mut c, ESC_CODE), (None, KEEP));
    }

    #[test]
    fn key_down_before_capture_passes_and_is_not_recorded() {
        let mut c = KeyCapture::default();
        // Enter clicked "Add hotkey" and is still held: its repeats and release pass.
        assert_eq!(repeat(&mut c, 3), (None, PASS));
        assert_eq!(press(&mut c, 1), (held(&[1]), KEEP));
        assert_eq!(repeat(&mut c, 3), (None, PASS));
        assert_eq!(release(&mut c, 3), (None, PASS));
        assert_eq!(release(&mut c, 1), (done(&[1]), KEEP));
        // A held Esc's repeat doesn't cancel either.
        let mut c = KeyCapture::default();
        assert_eq!(repeat(&mut c, ESC_CODE), (None, PASS));
        assert_eq!(press(&mut c, 1), (held(&[1]), KEEP));
    }

    #[test]
    fn key_down_before_capture_passes_after_it_finished() {
        let mut c = KeyCapture::default();
        press(&mut c, 1);
        assert_eq!(release(&mut c, 1), (done(&[1]), KEEP));
        assert_eq!(repeat(&mut c, 3), (None, PASS));
        assert_eq!(release(&mut c, 3), (None, PASS));
        let mut c = KeyCapture::default();
        press(&mut c, 1);
        assert_eq!(press(&mut c, ESC_CODE), (Some(Capture::Cancelled), KEEP));
        assert_eq!(repeat(&mut c, 3), (None, PASS));
        assert_eq!(release(&mut c, 3), (None, PASS));
        assert_eq!(release(&mut c, 1), (None, KEEP));
    }

    /// Takes over the keyboard for a moment (macOS: needs Accessibility for the process
    /// running it): run by hand with
    /// `cargo test -p agt-platform dropping_capture -- --ignored`.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    #[ignore]
    fn dropping_capture_stops_it() {
        use std::{
            sync::{Arc, mpsc},
            thread,
            time::Duration,
        };
        // `on` holds a clone: gone once the handle is.
        let alive = Arc::new(());
        let on_alive = alive.clone();
        let handle = capture_keys(move |_| {
            let _keep = &on_alive;
        })
        .expect("start the key recorder");
        // Dropping the handle joins the capture thread.
        let (dropped, ended) = mpsc::channel();
        thread::spawn(move || {
            drop(handle);
            let _ = dropped.send(());
        });
        ended
            .recv_timeout(Duration::from_secs(1))
            .expect("dropping the handle ends the capture thread within 1 s");
        assert_eq!(
            Arc::strong_count(&alive),
            1,
            "the capture thread let go of `on`"
        );
    }
}
