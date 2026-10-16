//! What the pill looks like in each state. Pure: draws into a [`Canvas`], so it is unit-tested
//! and can be rendered to PNGs off-screen (see the `demo` example).

use crate::{
    Mode,
    canvas::{Canvas, Rgba},
};
use std::time::{Duration, Instant};

/// Pill size and its distance from the screen edge, in logical pixels (points on macOS).
pub const WIDTH: f32 = 180.0;
pub const HEIGHT: f32 = 36.0;
pub const MARGIN: f32 = 48.0;

/// Waveform bars: one per [`BAR_STEP`], so they show the last ~1 s.
pub const BARS: usize = 24;
const BAR_W: f32 = 3.0;
const BAR_GAP: f32 = 2.0;
const BAR_MIN: f32 = 3.0;
const BAR_MAX: f32 = 22.0;
const BAR_STEP: Duration = Duration::from_millis(42);

pub(crate) const BG: Rgba = Rgba(0.07, 0.07, 0.09, 0.84);
const BG_ERROR: Rgba = Rgba(0.50, 0.08, 0.08, 0.90);
pub(crate) const BORDER: Rgba = Rgba(1.0, 1.0, 1.0, 0.14);
const BAR: Rgba = Rgba(1.0, 1.0, 1.0, 0.95);
const DOT: Rgba = Rgba(1.0, 0.27, 0.27, 1.0);

/// Canvas size in pixels for a given scale factor (pixels per logical pixel).
pub fn pixel_size(scale: f32) -> (usize, usize) {
    (
        (WIDTH * scale).round().max(1.0) as usize,
        (HEIGHT * scale).round().max(1.0) as usize,
    )
}

/// One frame's inputs.
pub struct Scene<'a> {
    pub mode: Mode,
    /// Seconds since `mode` started (drives the animations).
    pub t: f32,
    /// Bar heights, `0..=1`, oldest first; the last one is live.
    pub levels: &'a [f32; BARS],
    pub scale: f32,
}

/// Draws `scene` onto `canvas` (which must be [`pixel_size`]`(scene.scale)` and clear).
pub fn render(scene: &Scene, canvas: &mut Canvas) {
    render_background(scene.mode, scene.scale, canvas);
    render_content(scene, canvas);
}

/// The pill itself, which only changes with the scale and between error and not, so the
/// overlay draws it once per showing and copies it under every frame.
pub fn render_background(mode: Mode, scale: f32, canvas: &mut Canvas) {
    let (w, h) = (canvas.width() as f32, canvas.height() as f32);
    let bg = if mode == Mode::Error { BG_ERROR } else { BG };
    canvas.fill_round_rect(0.0, 0.0, w, h, f32::MAX, bg);
    canvas.stroke_round_rect(0.0, 0.0, w, h, f32::MAX, scale.max(1.0), BORDER);
}

/// Everything on top of the background.
pub fn render_content(scene: &Scene, canvas: &mut Canvas) {
    let s = scene.scale;
    let cy = canvas.height() as f32 / 2.0;
    let bars_w = BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP;
    let x0 = (WIDTH - bars_w) / 2.0;
    let bar = |canvas: &mut Canvas, i: usize, height: f32, color: Rgba| {
        let x = (x0 + i as f32 * (BAR_W + BAR_GAP)) * s;
        let bh = height * s;
        canvas.fill_round_rect(x, cy - bh / 2.0, BAR_W * s, bh, f32::MAX, color);
    };

    match scene.mode {
        Mode::Recording => {
            // Pulsing "recording" dot.
            let pulse = 0.7 + 0.3 * (scene.t * std::f32::consts::TAU * 0.8).cos();
            let r = 4.0 * s;
            canvas.fill_round_rect(
                16.0 * s - r,
                cy - r,
                2.0 * r,
                2.0 * r,
                f32::MAX,
                DOT.with_alpha(pulse),
            );
            for (i, &level) in scene.levels.iter().enumerate() {
                // Older bars fade a little, so the waveform reads as moving left.
                let age = i as f32 / (BARS - 1) as f32;
                let height = BAR_MIN + (BAR_MAX - BAR_MIN) * level.clamp(0.0, 1.0);
                bar(canvas, i, height, BAR.with_alpha(0.45 + 0.5 * age));
            }
        }
        Mode::Transcribing => {
            // A soft highlight sweeping left to right over low bars.
            let sweep = (scene.t / 1.1).fract() * 1.6 - 0.3;
            for i in 0..BARS {
                let x = i as f32 / (BARS - 1) as f32;
                let k = (-((x - sweep) / 0.13).powi(2)).exp();
                bar(
                    canvas,
                    i,
                    BAR_MIN + 7.0 * k,
                    BAR.with_alpha(0.28 + 0.67 * k),
                );
            }
        }
        Mode::Error => {
            for i in 0..BARS {
                bar(canvas, i, BAR_MIN, BAR.with_alpha(0.5));
            }
        }
        Mode::Hidden => {}
    }
}

/// Turns the microphone RMS into a scrolling bar history.
pub(crate) struct Meter {
    levels: [f32; BARS],
    last_push: Instant,
}

impl Meter {
    pub fn new(now: Instant) -> Self {
        Self {
            levels: [0.0; BARS],
            last_push: now,
        }
    }

    pub fn levels(&self) -> &[f32; BARS] {
        &self.levels
    }

    /// Feeds the current RMS (`0..=1`). The newest bar follows it live (fast attack, slower
    /// release) and is frozen into the history every [`BAR_STEP`].
    pub fn update(&mut self, rms: f32, now: Instant) {
        let behind = now.saturating_duration_since(self.last_push);
        if behind >= BAR_STEP * BARS as u32 {
            // Long gap (e.g. the loop stalled): start the history over.
            *self = Self::new(now);
        } else {
            let steps = (behind.as_nanos() / BAR_STEP.as_nanos()) as usize;
            if steps > 0 {
                let last = self.levels[BARS - 1];
                self.levels.copy_within(steps.., 0);
                self.levels[BARS - steps..].fill(last);
                self.last_push += BAR_STEP * steps as u32;
            }
        }
        let v = loudness(rms);
        let live = &mut self.levels[BARS - 1];
        *live = if v > *live {
            v
        } else {
            *live + (v - *live) * 0.3
        };
    }
}

/// RMS to `0..=1` on a dB scale: -54 dBFS (a quiet room) and below is 0, -12 dBFS is 1.
fn loudness(rms: f32) -> f32 {
    let db = 20.0 * rms.max(1e-6).log10();
    ((db + 54.0) / 42.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudness_scale() {
        assert_eq!(loudness(0.0), 0.0);
        assert_eq!(loudness(0.001), 0.0); // -60 dB
        assert!((loudness(0.25) - 1.0).abs() < 0.01); // -12 dB
        assert!(loudness(0.02) > 0.3 && loudness(0.02) < 0.7); // normal speech
        assert_eq!(loudness(1.0), 1.0);
    }

    #[test]
    fn meter_scrolls_one_bar_per_step() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0);
        m.update(0.5, t0);
        assert_eq!(m.levels()[BARS - 1], 1.0);
        m.update(0.0, t0 + BAR_STEP);
        // The loud bar moved into the history; the live one is decaying.
        assert_eq!(m.levels()[BARS - 2], 1.0);
        assert!(m.levels()[BARS - 1] < 1.0 && m.levels()[BARS - 1] > 0.0);
        assert_eq!(m.levels()[BARS - 3], 0.0);
        // A long stall resets the history.
        m.update(0.0, t0 + BAR_STEP * 100);
        assert!(m.levels().iter().all(|&l| l == 0.0));
    }

    #[test]
    fn every_mode_renders_inside_the_pill() {
        let levels = [0.5; BARS];
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for mode in [Mode::Recording, Mode::Transcribing, Mode::Error] {
                let (w, h) = pixel_size(scale);
                let mut c = Canvas::new(w, h);
                render(
                    &Scene {
                        mode,
                        t: 0.3,
                        levels: &levels,
                        scale,
                    },
                    &mut c,
                );
                let px = |x: usize, y: usize| c.pixels()[y * w + x];
                assert_eq!(px(0, 0) >> 24, 0, "{mode:?}: corner transparent");
                assert!(px(w / 2, h / 2) >> 24 > 150, "{mode:?}: center drawn");
            }
        }
    }
}
