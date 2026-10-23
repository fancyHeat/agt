//! The real captures behind a meeting's tracks: [`agt_audio::Capture`] on the chosen
//! microphone, and on what the computer plays (the global process tap on macOS, WASAPI
//! loopback of the default output device on Windows). Kept thin: the session does the
//! restarting, and this needs real devices to test.

use crate::session::{CaptureFactory, CaptureHandle, Sink, Track};
use agt_audio::{Capture, Level, Source, current_device_id};

/// [`CaptureFactory`] on this machine's devices.
#[derive(Clone, Default)]
pub struct DeviceCaptures {
    /// `audio.device`: the microphone's name, or "" for the default input device.
    pub mic_device: String,
    /// The tracks' input levels, for the meters (zero while a track isn't capturing).
    pub mic_level: Level,
    pub system_level: Level,
}

impl CaptureFactory for DeviceCaptures {
    fn start(&self, track: Track, sink: Sink) -> Result<Box<dyn CaptureHandle>, String> {
        let (source, level) = match track {
            Track::Mic => (Source::Microphone(self.mic_device.clone()), &self.mic_level),
            Track::System => (Source::SystemOutput, &self.system_level),
        };
        let capture = Capture::start(&source, level.clone(), sink).map_err(|e| e.to_string())?;
        Ok(Box::new(Running { capture, source }))
    }
}

struct Running {
    capture: Capture,
    source: Source,
}

impl CaptureHandle for Running {
    fn failed(&self) -> bool {
        self.capture.failed()
    }

    /// The device `source` would open now isn't this one: the default changed, or a chosen
    /// microphone that was missing is back. An enumeration on Windows and for a named mic (a
    /// few ms), a constant for the macOS tap.
    fn stale(&self) -> bool {
        current_device_id(&self.source).as_deref() != self.capture.device_id()
    }
}
