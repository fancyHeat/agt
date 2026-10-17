//! Continuous capture (meeting notes): the microphone, or what the computer plays, as a
//! stream of 16 kHz mono f32 for as long as the [`Capture`] lives.
//!
//! Unlike [`Recorder`](crate::Recorder) there is no pre-built idle stream, pre-roll or
//! length cap: a capture runs from `start` until dropped, and every callback's audio goes to
//! the caller. It doesn't watch its device either: the owner polls [`Capture::failed`]
//! (unplugged) and [`current_device_id`] (the default device changed) and starts a new one.
//!
//! System output on macOS is a global Core Audio process tap (see `tap.rs`): everything any
//! app plays, whichever device it plays on, so it never needs restarting for an output change.
//! It needs the "System Audio Recording" permission, and delivers silence until that is
//! granted. On Windows it is cpal's WASAPI loopback of the default output device, which
//! delivers no packets at all while nothing plays, so there the sample count of a
//! system-output capture is not a clock: time it by the wall clock.

use crate::{
    Error, Level, SAMPLE_RATE, build_mono_stream, device_name, named_input, resample::Resampler,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[cfg(target_os = "macos")]
use crate::tap::GlobalTap;

/// What a [`Capture`] records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The input device with this name (`audio.device`; see
    /// [`input_device_names`](crate::input_device_names)), or the default input device if
    /// it's "" or not connected.
    Microphone(String),
    /// What the computer plays: a global process tap on macOS, the default output device
    /// through WASAPI loopback on Windows.
    SystemOutput,
}

impl Source {
    /// For logs and errors.
    fn label(&self) -> &'static str {
        match self {
            Source::Microphone(_) => "microphone",
            Source::SystemOutput => "system audio",
        }
    }

    /// The device this source opens now; `true` if it's the named microphone, not the
    /// default as a fallback. (Not used for system output on macOS: that's the tap.)
    fn device(&self, host: &cpal::Host) -> Result<(cpal::Device, bool), Error> {
        match self {
            Source::Microphone(name) => {
                // A listing error counts as "not connected": the default is what's left.
                if !name.is_empty()
                    && let Ok(Some(device)) = named_input(host, name)
                {
                    return Ok((device, true));
                }
                let device = host
                    .default_input_device()
                    .ok_or("no microphone (default input device) found")?;
                Ok((device, false))
            }
            Source::SystemOutput => {
                let device = host
                    .default_output_device()
                    .ok_or("no output device (speakers, headphones) found")?;
                Ok((device, false))
            }
        }
    }
}

/// A running capture; dropping it stops it (and zeroes its level).
pub struct Capture {
    /// Only `None` while dropping: the stream goes first, so no callback can set the level
    /// after it was zeroed, and nothing is still reading the tap when it is destroyed.
    stream: Option<cpal::Stream>,
    #[cfg(target_os = "macos")]
    tap: Option<GlobalTap>,
    name: String,
    device_id: Option<String>,
    failed: Arc<AtomicBool>,
    level: Level,
}

/// What [`Capture::start`] opens a stream on.
struct Opened {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    name: String,
    device_id: Option<String>,
    /// System output on macOS: the tap behind `device`.
    #[cfg(target_os = "macos")]
    tap: Option<GlobalTap>,
}

impl Opened {
    fn open(source: &Source, host: &cpal::Host) -> Result<Opened, Error> {
        #[cfg(target_os = "macos")]
        if *source == Source::SystemOutput {
            let tap = GlobalTap::new()?;
            let device = tap.device(host)?;
            return Ok(Opened {
                config: device.default_input_config()?,
                device,
                name: "System audio".into(),
                device_id: Some(crate::tap::DEVICE_ID.into()),
                tap: Some(tap),
            });
        }
        let (device, chosen) = source.device(host)?;
        let name = device_name(&device).unwrap_or_else(|| "?".into());
        let config = match source {
            Source::Microphone(wanted) => {
                if !wanted.is_empty() && !chosen {
                    tracing::warn!(
                        "microphone \"{wanted}\" not found; using the default input device"
                    );
                }
                device.default_input_config()?
            }
            // WASAPI loopback: shared mode, in the device's own mix format.
            Source::SystemOutput => device.default_output_config()?,
        };
        Ok(Opened {
            device_id: device.id().ok().map(|id| id.to_string()),
            device,
            config,
            name,
            #[cfg(target_os = "macos")]
            tap: None,
        })
    }
}

impl Capture {
    /// Opens and plays `source`. `on_audio` gets its audio as it arrives, 16 kHz mono f32,
    /// on the audio callback thread: keep it cheap (a channel send), a slow callback loses
    /// audio. `level` follows the RMS of each callback.
    ///
    /// A named microphone that isn't connected falls back to the default input device; one
    /// that is connected but fails to open is an error (falling back would make
    /// [`current_device_id`] disagree with [`device_id`](Self::device_id) and restart it
    /// forever).
    pub fn start(
        source: &Source,
        level: Level,
        mut on_audio: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Capture, Error> {
        let t = Instant::now();
        let what = source.label();
        let o = Opened::open(source, &cpal::default_host())?;
        let stream_config = o.config.config();
        let mut resampler = Resampler::new(stream_config.sample_rate, SAMPLE_RATE);
        let mut out = Vec::new();
        let failed = Arc::new(AtomicBool::new(false));
        let meter = level.clone();
        let stream = build_mono_stream(&o.device, &o.config, what, failed.clone(), move |mono| {
            meter.set(crate::rms(mono));
            out.clear();
            resampler.process(mono, &mut out);
            if !out.is_empty() {
                on_audio(&out);
            }
        })?;
        stream.play()?;
        tracing::info!(
            "capturing {what}: {}, {} Hz, {} ch, {}; started in {:.1} ms",
            o.name,
            stream_config.sample_rate,
            stream_config.channels,
            o.config.sample_format(),
            t.elapsed().as_secs_f64() * 1e3,
        );
        Ok(Capture {
            stream: Some(stream),
            #[cfg(target_os = "macos")]
            tap: o.tap,
            name: o.name,
            device_id: o.device_id,
            failed,
            level,
        })
    }

    /// The device's name (what the user picked, or the default device's); "System audio"
    /// for the macOS tap.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A stream error was reported (device unplugged, audio service restarted): it delivers
    /// nothing more; drop it and start again.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// The opened device's id; differs from [`current_device_id`] once the default device
    /// changed (or a missing named microphone came back).
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.stream.take());
        #[cfg(target_os = "macos")]
        drop(self.tap.take());
        self.level.set(0.0);
    }
}

/// The id of the device `source` would open now, to notice that the default device changed
/// (compare it with [`Capture::device_id`] and restart). Enumerates devices for a named
/// microphone (a few ms), so poll it at most every second or so. System output on macOS is
/// a constant: the tap isn't bound to a device.
pub fn current_device_id(source: &Source) -> Option<String> {
    #[cfg(target_os = "macos")]
    if *source == Source::SystemOutput {
        return Some(crate::tap::DEVICE_ID.into());
    }
    let (device, _) = source.device(&cpal::default_host()).ok()?;
    device.id().ok().map(|id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex, time::Duration};

    #[test]
    fn a_capture_can_move_to_another_thread() {
        fn send<T: Send>() {}
        send::<Capture>();
    }

    /// Runs `source` for `ms`; returns the capture's name and the samples it delivered.
    fn run(source: &Source, ms: u64) -> (String, Vec<f32>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let level = Level::default();
        let c = Capture::start(source, level.clone(), move |a| {
            sink.lock().unwrap().extend_from_slice(a)
        })
        .expect("start");
        std::thread::sleep(Duration::from_millis(ms));
        println!("input devices meanwhile: {:?}", crate::input_device_names());
        assert!(!c.failed());
        assert_eq!(c.device_id().map(str::to_owned), current_device_id(source));
        let name = c.name().to_owned();
        drop(c);
        assert_eq!(level.get(), 0.0);
        let samples = std::mem::take(&mut *got.lock().unwrap());
        (name, samples)
    }

    /// Needs the "System Audio Recording" permission for real audio (silence without it), so
    /// it prompts on first run: `cargo test -p agt-audio -- --ignored --nocapture`. Play
    /// something meanwhile to see a non-zero rms.
    #[test]
    #[ignore]
    fn system_output_global_tap_starts() {
        let (name, samples) = run(&Source::SystemOutput, 1000);
        println!(
            "{name}: {} samples in 1 s, rms {:.5}",
            samples.len(),
            crate::rms(&samples)
        );
    }

    /// Needs a real microphone: `cargo test -p agt-audio -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn microphone_capture_delivers_16k() {
        let (name, samples) = run(&Source::Microphone(String::new()), 1000);
        println!("{name}: {} samples in 1 s", samples.len());
        assert!(
            samples.len() > SAMPLE_RATE as usize * 8 / 10,
            "{}",
            samples.len()
        );
        assert!(samples.iter().any(|&x| x != 0.0), "all-zero audio");
    }

    /// Needs a microphone.
    #[test]
    #[ignore]
    fn current_device_id_microphone_default() {
        let default = current_device_id(&Source::Microphone(String::new()));
        println!("default input: {default:?}");
        assert!(default.is_some());
        // Unknown: the default, like `Capture::start`.
        assert_eq!(
            current_device_id(&Source::Microphone("no such mic".into())),
            default
        );
    }
}
