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
/// earlier (AGT's). While the handle lives, no key event reaches apps or AGT's listener, so
/// a press neither starts a dictation nor acts in the window behind.
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

/// The press being recorded, from the key events the tap or hook sees: auto-repeat (a press
/// of a key already down) and the release of a key that went down before recording started
/// are ignored. Platform-independent so it can be unit-tested.
#[derive(Debug, Default)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) struct KeyCapture {
    /// Down now, in press order.
    held: Vec<u16>,
    /// The most keys down at once so far, in press order.
    peak: Vec<u16>,
    /// `Done` or `Cancelled` was reported.
    finished: bool,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl KeyCapture {
    /// The key with platform code `code` went down (`down`) or up: what to report, if
    /// anything.
    pub(crate) fn on_key(&mut self, code: u16, down: bool) -> Option<Capture> {
        if self.finished {
            return None;
        }
        if down {
            if code == ESC_CODE {
                self.finished = true;
                return Some(Capture::Cancelled);
            }
            if self.held.contains(&code) {
                return None;
            }
            self.held.push(code);
            if self.held.len() > self.peak.len() {
                self.peak.clone_from(&self.held);
            }
        } else {
            let i = self.held.iter().position(|&c| c == code)?;
            self.held.remove(i);
            if self.held.is_empty() {
                self.finished = true;
                return Some(Capture::Done(std::mem::take(&mut self.peak)));
            }
        }
        Some(Capture::Held(self.held.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(codes: &[u16]) -> Option<Capture> {
        Some(Capture::Held(codes.to_vec()))
    }

    fn done(codes: &[u16]) -> Option<Capture> {
        Some(Capture::Done(codes.to_vec()))
    }

    #[test]
    fn records_peak_set_on_release() {
        let mut c = KeyCapture::default();
        assert_eq!(c.on_key(1, true), held(&[1]));
        assert_eq!(c.on_key(2, true), held(&[1, 2]));
        assert_eq!(c.on_key(2, false), held(&[1]));
        assert_eq!(c.on_key(1, false), done(&[1, 2]));
    }

    #[test]
    fn peak_is_the_first_largest_set() {
        let mut c = KeyCapture::default();
        c.on_key(1, true);
        c.on_key(2, true);
        assert_eq!(c.on_key(1, false), held(&[2]));
        assert_eq!(c.on_key(3, true), held(&[2, 3]));
        c.on_key(2, false);
        assert_eq!(c.on_key(3, false), done(&[1, 2]));
    }

    #[test]
    fn repeat_is_ignored() {
        let mut c = KeyCapture::default();
        assert_eq!(c.on_key(1, true), held(&[1]));
        assert_eq!(c.on_key(1, true), None);
        assert_eq!(c.on_key(1, false), done(&[1]));
    }

    #[test]
    fn esc_cancels() {
        let mut c = KeyCapture::default();
        assert_eq!(c.on_key(1, true), held(&[1]));
        assert_eq!(c.on_key(ESC_CODE, true), Some(Capture::Cancelled));
        assert_eq!(c.on_key(ESC_CODE, false), None);
        assert_eq!(c.on_key(1, false), None);
        assert_eq!(c.on_key(2, true), None);
        assert_eq!(c.on_key(2, false), None);
    }

    #[test]
    fn single_tap() {
        let mut c = KeyCapture::default();
        assert_eq!(c.on_key(7, true), held(&[7]));
        assert_eq!(c.on_key(7, false), done(&[7]));
    }

    #[test]
    fn nothing_after_done() {
        let mut c = KeyCapture::default();
        c.on_key(7, true);
        assert_eq!(c.on_key(7, false), done(&[7]));
        assert_eq!(c.on_key(8, true), None);
        assert_eq!(c.on_key(ESC_CODE, true), None);
    }

    #[test]
    fn release_of_a_key_down_before_recording_is_ignored() {
        let mut c = KeyCapture::default();
        assert_eq!(c.on_key(3, false), None);
        assert_eq!(c.on_key(1, true), held(&[1]));
        assert_eq!(c.on_key(3, false), None);
        assert_eq!(c.on_key(1, false), done(&[1]));
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
