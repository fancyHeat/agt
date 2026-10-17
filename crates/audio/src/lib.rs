//! Microphone capture to 16 kHz mono f32, plus resampling and WAV reading and writing.
//! [`Recorder`] is the dictation mic (started and stopped per key press); [`Capture`] is a
//! continuous stream of the mic or of what the computer plays, for meeting notes.
//!
//! ## Mic stream lifecycle
//!
//! The input stream is *built* once, at start-up (`Recorder::prepare`): that is the slow part
//! (device enumeration, WASAPI client initialization). What happens between dictations
//! depends on `audio.keep_mic_open`:
//!
//! - `false` (default): the stream is only *started* while the hotkey is held and stopped on
//!   release, so the OS "microphone in use" indicator is off the rest of the time. Starting a
//!   built stream takes a few ms on WASAPI; `start()` logs how long the first audio took.
//! - `true`: the stream runs all the time and the last [`PREROLL_MS`] before the key press
//!   are prepended, so even a word started slightly before the key is caught; the cost is the
//!   mic indicator being permanently on (and a few % of one core for resampling).
//!
//! The device is `audio.device` (by name), or the system default input device if that's
//! empty or not connected. The stream is rebuilt automatically if it fails (device
//! unplugged) or, when on the default device, if the default changed since it was built; on
//! the fallback, each recording checks whether the chosen device is back (an enumeration, a
//! few ms) and switches to it. A chosen device that is connected but fails to open also
//! falls back, and is tried again a minute later or once it has been reconnected.

mod capture;
pub mod resample;
#[cfg(target_os = "macos")]
mod tap;
pub mod wav;

pub use capture::{Capture, Source, current_device_id};

use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use resample::Resampler;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

pub use agt_core::stt::SAMPLE_RATE;

/// Audio kept from before the key press when the mic stream stays open.
pub const PREROLL_MS: usize = 300;
/// Recordings are cut off after this long (memory and model-context safety valve).
pub const MAX_SECONDS: usize = 5 * 60;
/// A chosen microphone that failed to open is tried again after this long.
const CHOSEN_RETRY: Duration = Duration::from_secs(60);

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Current input level (RMS of the latest callback chunk, 0..1), for a level meter.
#[derive(Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// State shared with the audio callback.
struct Shared {
    recording: bool,
    resampler: Resampler,
    /// 16 kHz output of the current recording.
    samples: Vec<f32>,
    /// Last `PREROLL_MS` of 16 kHz audio while not recording (keep-open mode only).
    preroll: VecDeque<f32>,
    keep_preroll: bool,
    /// When recording started and when its first audio arrived (start-up latency).
    started: Option<Instant>,
    first_audio: Option<Duration>,
    truncated: bool,
}

struct Live {
    stream: cpal::Stream,
    device_id: Option<String>,
    /// Built on the chosen `audio.device` (not the default as a fallback).
    chosen: bool,
}

pub struct Recorder {
    keep_open: bool,
    /// `audio.device`: "" = the default input device.
    device: String,
    /// The chosen device's absence was logged (once, not on every recording).
    missing_logged: bool,
    /// When the chosen device last failed to open: it is retried after [`CHOSEN_RETRY`], or
    /// as soon as it was unplugged and is back.
    chosen_failed: Option<Instant>,
    live: Option<Live>,
    capture: Arc<Mutex<Shared>>,
    failed: Arc<AtomicBool>,
    level: Level,
}

impl Recorder {
    /// `device`: an input device name from [`input_device_names`], or "" for the default.
    /// `level` is updated with the input level while the stream runs (see [`Level`]).
    pub fn new(keep_open: bool, device: String, level: Level) -> Self {
        Self {
            keep_open,
            device,
            missing_logged: false,
            chosen_failed: None,
            live: None,
            capture: Arc::new(Mutex::new(Shared {
                recording: false,
                resampler: Resampler::new(SAMPLE_RATE, SAMPLE_RATE),
                samples: Vec::new(),
                preroll: VecDeque::new(),
                keep_preroll: keep_open,
                started: None,
                first_audio: None,
                truncated: false,
            })),
            failed: Arc::new(AtomicBool::new(false)),
            level,
        }
    }

    /// The input level meter (updated from the audio callback while the stream runs).
    pub fn level(&self) -> Level {
        self.level.clone()
    }

    /// Builds the input stream (and starts it, in keep-open mode). Call once at start-up so
    /// the first dictation doesn't pay for it; `start` calls it again when needed.
    pub fn prepare(&mut self) -> Result<(), Error> {
        let healthy = self.live.is_some() && !self.failed.load(Ordering::Relaxed);
        // The chosen device, running: nothing to check (no enumeration on every key press).
        // Except a paused stream on Windows: cpal reports a removed device only once the
        // stream plays, so the press would record nothing; it's looked up (~3 ms, like the
        // default device's check).
        let unplug_reported = self.keep_open || !cfg!(windows);
        if unplug_reported && healthy && self.live.as_ref().is_some_and(|l| l.chosen) {
            return Ok(());
        }
        let host = cpal::default_host();
        let (device, chosen) = self.pick_device(&host)?;
        let device_id = device.id().ok().map(|id| id.to_string());
        if healthy
            && let Some(live) = &mut self.live
            && live.device_id == device_id
        {
            live.chosen = chosen;
            return Ok(());
        }
        self.live = None; // drops (closes) any previous stream first
        match self.open(&device, device_id, chosen) {
            Err(e) if chosen => {
                // Connected but unusable (held exclusively, broken driver): the default until
                // a retry, rather than failing every press.
                tracing::warn!(
                    "cannot open microphone \"{}\": {e}; using the default input device",
                    self.device
                );
                self.chosen_failed = Some(Instant::now());
                let device = host
                    .default_input_device()
                    .ok_or("no microphone (default input device) found")?;
                let device_id = device.id().ok().map(|id| id.to_string());
                self.open(&device, device_id, false)
            }
            r => r,
        }
    }

    /// Builds (and in keep-open mode starts) the stream on `device`.
    fn open(
        &mut self,
        device: &cpal::Device,
        device_id: Option<String>,
        chosen: bool,
    ) -> Result<(), Error> {
        let t = Instant::now();
        let config = device.default_input_config()?;
        let name = device_name(device).unwrap_or_else(|| "?".into());
        let stream_config = config.config();
        {
            let mut c = self.capture.lock().unwrap();
            c.resampler = Resampler::new(stream_config.sample_rate, SAMPLE_RATE);
            c.preroll.clear();
        }
        self.failed.store(false, Ordering::Relaxed);
        let stream = build_mono_stream(
            device,
            &config,
            "microphone",
            self.failed.clone(),
            self.on_audio(),
        )?;
        if self.keep_open {
            stream.play()?;
        }
        tracing::info!(
            "microphone: {name}, {} Hz, {} ch, {}; stream built in {:.1} ms{}",
            stream_config.sample_rate,
            stream_config.channels,
            config.sample_format(),
            t.elapsed().as_secs_f64() * 1e3,
            if self.keep_open { " (kept open)" } else { "" }
        );
        self.live = Some(Live {
            stream,
            device_id,
            chosen,
        });
        Ok(())
    }

    /// The chosen device if it's connected (`true`), else the default input device.
    /// (If it failed to open, the default until [`CHOSEN_RETRY`] passed or it was unplugged.)
    fn pick_device(&mut self, host: &cpal::Host) -> Result<(cpal::Device, bool), Error> {
        if !self.device.is_empty() {
            let broken = self
                .chosen_failed
                .is_some_and(|t| t.elapsed() < CHOSEN_RETRY);
            match named_input(host, &self.device) {
                Ok(Some(d)) if !broken => {
                    self.missing_logged = false;
                    return Ok((d, true));
                }
                Ok(Some(_)) => {}
                Ok(None) => self.chosen_failed = None,
                Err(e) => tracing::warn!("cannot list microphones: {e}"),
            }
            if !self.missing_logged && !broken {
                tracing::warn!(
                    "microphone \"{}\" not found; using the default input device",
                    self.device
                );
                self.missing_logged = true;
            }
        }
        let device = host
            .default_input_device()
            .ok_or("no microphone (default input device) found")?;
        Ok((device, false))
    }

    /// The stream callback: mono audio of the device's rate into the recording (or the
    /// pre-roll).
    fn on_audio(&self) -> impl FnMut(&[f32]) + Send + 'static {
        let capture = self.capture.clone();
        let level = self.level.clone();
        let preroll_len = PREROLL_MS * SAMPLE_RATE as usize / 1000;
        let max_len = MAX_SECONDS * SAMPLE_RATE as usize;
        move |mono: &[f32]| {
            let Ok(mut guard) = capture.lock() else {
                return;
            };
            let c = &mut *guard;
            if !c.recording && !c.keep_preroll {
                return;
            }
            level.set(rms(mono));
            if c.recording {
                if c.first_audio.is_none() {
                    c.first_audio = c.started.map(|s| s.elapsed());
                }
                if c.samples.len() < max_len {
                    c.resampler.process(mono, &mut c.samples);
                } else {
                    c.truncated = true;
                }
            } else {
                let before = c.samples.len();
                c.resampler.process(mono, &mut c.samples);
                c.preroll.extend(c.samples.drain(before..));
                let excess = c.preroll.len().saturating_sub(preroll_len);
                c.preroll.drain(..excess);
            }
        }
    }

    /// Starts a recording.
    pub fn start(&mut self) -> Result<(), Error> {
        // Rebuild if the stream died or the default device changed.
        self.prepare()?;
        {
            let mut c = self.capture.lock().unwrap();
            c.samples.clear();
            if c.keep_preroll {
                let preroll: Vec<f32> = c.preroll.drain(..).collect();
                c.samples.extend(preroll);
            } else {
                c.resampler.reset();
            }
            c.recording = true;
            c.started = Some(Instant::now());
            c.first_audio = None;
            c.truncated = false;
        }
        if !self.keep_open {
            let live = self.live.as_ref().ok_or("microphone stream not built")?;
            if let Err(e) = live.stream.play() {
                self.capture.lock().unwrap().recording = false;
                self.failed.store(true, Ordering::Relaxed);
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Ends the recording and returns its 16 kHz mono samples.
    pub fn stop(&mut self) -> Recording {
        if !self.keep_open
            && let Some(live) = &self.live
            && let Err(e) = live.stream.pause()
        {
            tracing::warn!("cannot pause microphone stream: {e}");
            self.failed.store(true, Ordering::Relaxed);
        }
        let mut c = self.capture.lock().unwrap();
        c.recording = false;
        let mut samples = std::mem::take(&mut c.samples);
        if !c.keep_preroll {
            c.resampler.finish(&mut samples);
        }
        if c.truncated {
            tracing::warn!("recording cut off at {MAX_SECONDS} s");
        }
        self.level.set(0.0);
        Recording {
            samples,
            first_audio: c.first_audio,
        }
    }
}

/// The names of the connected input devices, as `audio.device` takes them (duplicates once,
/// and not the system audio tap of a running meeting, see [`is_own_tap_device`]).
pub fn input_device_names() -> Vec<String> {
    let devices = match cpal::default_host().input_devices() {
        Ok(devices) => devices,
        Err(e) => {
            tracing::warn!("cannot list microphones: {e}");
            return Vec::new();
        }
    };
    let mut names: Vec<String> = Vec::new();
    for name in devices
        .filter_map(|d| device_name(&d))
        .filter(|n| !is_own_tap_device(n))
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Names of the private aggregate devices behind macOS system audio capture (`tap.rs`): one
/// exists while a meeting records, and this process sees it as an input device. Choosing it as
/// the microphone would record the call as the user's own voice.
const OWN_TAP_PREFIX: &str = "AGT system audio ";

/// `name` is this process's system audio tap, not a microphone: hidden from
/// [`input_device_names`] and never picked by name.
fn is_own_tap_device(name: &str) -> bool {
    name.starts_with(OWN_TAP_PREFIX)
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

/// The connected input device called `name`, if any (never the meeting's system audio tap).
fn named_input(host: &cpal::Host, name: &str) -> Result<Option<cpal::Device>, cpal::Error> {
    if is_own_tap_device(name) {
        return Ok(None);
    }
    Ok(host
        .input_devices()?
        .find(|d| device_name(d).as_deref() == Some(name)))
}

/// Builds (does not start) an input stream on `device` in `config`, whatever its sample
/// format, that hands each callback's audio to `on_mono` downmixed to mono f32 (at the
/// device's rate). Stream errors other than glitches set `failed`: the stream is dead (device
/// unplugged) and must be rebuilt. `what` names the source in logs and errors.
fn build_mono_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    what: &'static str,
    failed: Arc<AtomicBool>,
    on_mono: impl FnMut(&[f32]) + Send + 'static,
) -> Result<cpal::Stream, Error> {
    let stream_config = config.config();
    match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(device, &stream_config, what, failed, on_mono),
        cpal::SampleFormat::I16 => build::<i16>(device, &stream_config, what, failed, on_mono),
        cpal::SampleFormat::I32 => build::<i32>(device, &stream_config, what, failed, on_mono),
        cpal::SampleFormat::U16 => build::<u16>(device, &stream_config, what, failed, on_mono),
        other => Err(format!("unsupported {what} sample format {other}").into()),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    what: &'static str,
    failed: Arc<AtomicBool>,
    mut on_mono: impl FnMut(&[f32]) + Send + 'static,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    // Reused: no allocation on the audio thread once it has grown to the callback size.
    let mut mono = Vec::new();
    let stream = device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _| {
            mono.clear();
            mono.extend(data.chunks(channels).map(|f| {
                f.iter()
                    .map(|&s| <f32 as FromSample<T>>::from_sample_(s))
                    .sum::<f32>()
                    / channels as f32
            }));
            on_mono(&mono);
        },
        move |e: cpal::Error| {
            // WASAPI flags a data discontinuity every time a paused stream is resumed;
            // glitches are harmless, anything else means the stream must be rebuilt.
            if e.kind() == cpal::ErrorKind::Xrun {
                tracing::debug!("{what} stream: {e}");
            } else {
                tracing::warn!("{what} stream error: {e}");
                failed.store(true, Ordering::Relaxed);
            }
        },
        None,
    )?;
    Ok(stream)
}

/// RMS of `x` (0 if empty), for a [`Level`].
fn rms(x: &[f32]) -> f32 {
    let sq: f32 = x.iter().map(|v| v * v).sum();
    (sq / x.len().max(1) as f32).sqrt()
}

pub struct Recording {
    /// 16 kHz mono.
    pub samples: Vec<f32>,
    /// Time from `start()` until the first audio arrived (`None` if none did).
    pub first_audio: Option<Duration>,
}

/// Averages interleaved frames of `channels` samples into mono.
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a real microphone: `cargo test -p agt-audio -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn records_from_default_microphone() {
        for keep_open in [false, true] {
            let mut r = Recorder::new(keep_open, String::new(), Level::default());
            let t = Instant::now();
            r.prepare().expect("prepare");
            let built = t.elapsed();
            std::thread::sleep(Duration::from_millis(400));
            let t = Instant::now();
            r.start().expect("start");
            let started = t.elapsed();
            std::thread::sleep(Duration::from_millis(1000));
            let level = r.level().get();
            let rec = r.stop();
            let n = rec.samples.len();
            let rms = (rec.samples.iter().map(|x| x * x).sum::<f32>() / n.max(1) as f32).sqrt();
            println!(
                "keep_open={keep_open}: built {built:?}, start() {started:?}, first audio after {:?}, {n} samples ({:.2} s), rms {rms:.5}, level {level:.5}",
                rec.first_audio,
                n as f64 / SAMPLE_RATE as f64
            );
            // ~1 s (plus pre-roll when kept open).
            assert!(n > SAMPLE_RATE as usize * 9 / 10, "too few samples: {n}");
            assert!(rec.samples.iter().any(|&x| x != 0.0), "all-zero audio");
        }
    }

    /// Needs a real microphone: `cargo test -p agt-audio -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn records_from_named_microphone() {
        let names = input_device_names();
        println!("input devices: {names:?}");
        let name = names.first().expect("no input devices");
        let mut r = Recorder::new(false, name.clone(), Level::default());
        r.prepare().expect("prepare");
        assert!(r.live.as_ref().unwrap().chosen, "not built on {name}");
        r.start().expect("start");
        std::thread::sleep(Duration::from_millis(500));
        assert!(r.stop().samples.len() > SAMPLE_RATE as usize * 4 / 10);
        // Unknown: falls back to the default device.
        let mut r = Recorder::new(false, "no such mic".into(), Level::default());
        r.prepare().expect("prepare");
        assert!(!r.live.as_ref().unwrap().chosen);
    }

    #[test]
    fn the_system_audio_tap_is_not_a_microphone() {
        assert!(is_own_tap_device("AGT system audio 812.0"));
        for mic in ["MacBook Air Microphone", "AGT", "Microsoft Teams Audio", ""] {
            assert!(!is_own_tap_device(mic), "{mic}");
        }
        // Picking it by name finds nothing (so `Recorder` and `Capture` use the default).
        let found = named_input(&cpal::default_host(), "AGT system audio 812.0");
        assert!(matches!(found, Ok(None)));
    }

    #[test]
    fn downmix_averages_frames() {
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(downmix(&[0.25, 0.5], 1), vec![0.25, 0.5]);
    }
}
