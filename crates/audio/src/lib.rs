//! Microphone capture to 16 kHz mono f32, plus resampling and WAV reading.
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
//! The stream is rebuilt automatically if it fails (device unplugged) or if the default input
//! device changed since it was built.

pub mod resample;
pub mod wav;

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
struct Capture {
    recording: bool,
    resampler: Resampler,
    /// 16 kHz output of the current recording.
    samples: Vec<f32>,
    /// Last `PREROLL_MS` of 16 kHz audio while not recording (keep-open mode only).
    preroll: VecDeque<f32>,
    keep_preroll: bool,
    /// Scratch buffer for the mono downmix.
    mono: Vec<f32>,
    /// When recording started and when its first audio arrived (start-up latency).
    started: Option<Instant>,
    first_audio: Option<Duration>,
    truncated: bool,
}

struct Live {
    stream: cpal::Stream,
    device_id: Option<String>,
}

pub struct Recorder {
    keep_open: bool,
    live: Option<Live>,
    capture: Arc<Mutex<Capture>>,
    failed: Arc<AtomicBool>,
    level: Level,
}

impl Recorder {
    pub fn new(keep_open: bool) -> Self {
        Self {
            keep_open,
            live: None,
            capture: Arc::new(Mutex::new(Capture {
                recording: false,
                resampler: Resampler::new(SAMPLE_RATE, SAMPLE_RATE),
                samples: Vec::new(),
                preroll: VecDeque::new(),
                keep_preroll: keep_open,
                mono: Vec::new(),
                started: None,
                first_audio: None,
                truncated: false,
            })),
            failed: Arc::new(AtomicBool::new(false)),
            level: Level::default(),
        }
    }

    /// The input level meter (updated from the audio callback while the stream runs).
    pub fn level(&self) -> Level {
        self.level.clone()
    }

    /// Builds the input stream (and starts it, in keep-open mode). Call once at start-up so
    /// the first dictation doesn't pay for it; `start` calls it again when needed.
    pub fn prepare(&mut self) -> Result<(), Error> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("no microphone (default input device) found")?;
        let device_id = device.id().ok().map(|id| id.to_string());
        if self.live.as_ref().is_some_and(|l| l.device_id == device_id)
            && !self.failed.load(Ordering::Relaxed)
        {
            return Ok(());
        }
        self.live = None; // drops (closes) any previous stream first

        let t = Instant::now();
        let config = device.default_input_config()?;
        let name = device
            .description()
            .map_or_else(|_| "?".into(), |d| d.name().to_owned());
        let stream_config = config.config();
        {
            let mut c = self.capture.lock().unwrap();
            c.resampler = Resampler::new(stream_config.sample_rate, SAMPLE_RATE);
            c.preroll.clear();
        }
        self.failed.store(false, Ordering::Relaxed);
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => self.build::<f32>(&device, &stream_config),
            cpal::SampleFormat::I16 => self.build::<i16>(&device, &stream_config),
            cpal::SampleFormat::I32 => self.build::<i32>(&device, &stream_config),
            cpal::SampleFormat::U16 => self.build::<u16>(&device, &stream_config),
            other => return Err(format!("unsupported microphone sample format {other}").into()),
        }?;
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
        self.live = Some(Live { stream, device_id });
        Ok(())
    }

    fn build<T>(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
    ) -> Result<cpal::Stream, Error>
    where
        T: SizedSample + Send + 'static,
        f32: FromSample<T>,
    {
        let capture = self.capture.clone();
        let level = self.level.clone();
        let failed = self.failed.clone();
        let channels = config.channels as usize;
        let preroll_len = PREROLL_MS * SAMPLE_RATE as usize / 1000;
        let max_len = MAX_SECONDS * SAMPLE_RATE as usize;
        let stream = device.build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _| {
                let Ok(mut guard) = capture.lock() else {
                    return;
                };
                let c = &mut *guard;
                if !c.recording && !c.keep_preroll {
                    return;
                }
                c.mono.clear();
                c.mono.extend(data.chunks(channels).map(|f| {
                    f.iter()
                        .map(|&s| <f32 as FromSample<T>>::from_sample_(s))
                        .sum::<f32>()
                        / channels as f32
                }));
                let sq: f32 = c.mono.iter().map(|x| x * x).sum();
                level.set((sq / c.mono.len().max(1) as f32).sqrt());
                if c.recording {
                    if c.first_audio.is_none() {
                        c.first_audio = c.started.map(|s| s.elapsed());
                    }
                    if c.samples.len() < max_len {
                        c.resampler.process(&c.mono, &mut c.samples);
                    } else {
                        c.truncated = true;
                    }
                } else {
                    let before = c.samples.len();
                    c.resampler.process(&c.mono, &mut c.samples);
                    c.preroll.extend(c.samples.drain(before..));
                    let excess = c.preroll.len().saturating_sub(preroll_len);
                    c.preroll.drain(..excess);
                }
            },
            move |e: cpal::Error| {
                // WASAPI flags a data discontinuity every time a paused stream is resumed;
                // glitches are harmless, anything else means the stream must be rebuilt.
                if e.kind() == cpal::ErrorKind::Xrun {
                    tracing::debug!("microphone stream: {e}");
                } else {
                    tracing::warn!("microphone stream error: {e}");
                    failed.store(true, Ordering::Relaxed);
                }
            },
            None,
        )?;
        Ok(stream)
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
            let mut r = Recorder::new(keep_open);
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

    #[test]
    fn downmix_averages_frames() {
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(downmix(&[0.25, 0.5], 1), vec![0.25, 0.5]);
    }
}
