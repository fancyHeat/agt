//! A small streaming windowed-sinc resampler (any rate → any rate), good enough for speech
//! recognition and cheap enough to run inside the audio callback: 32 taps per output sample,
//! kernel precomputed for 256 fractional phases (nearest phase, no interpolation).
//! Positions are tracked as exact fractions, so chunking never changes the output.

use std::f64::consts::PI;

/// Taps on each side of the output position.
const HALF: usize = 16;
const PHASES: usize = 256;

pub struct Resampler {
    /// Input samples per output sample, as `step_int + step_num / den`.
    step_int: usize,
    step_num: u64,
    den: u64,
    /// Pending input; `buf[0]` is the oldest sample still needed.
    buf: Vec<f32>,
    /// Position of the next output sample in `buf`: `base + frac / den`.
    base: usize,
    frac: u64,
    /// `PHASES` rows of `2 * HALF` taps; `None` when the rates are equal (passthrough).
    kernel: Option<Vec<f32>>,
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        assert!(in_rate > 0 && out_rate > 0);
        let g = gcd(in_rate as u64, out_rate as u64);
        let (num, den) = (in_rate as u64 / g, out_rate as u64 / g);
        let kernel = (in_rate != out_rate).then(|| {
            // Low-pass at 90% of the lower Nyquist frequency (relative to the input rate).
            let cutoff = (out_rate as f64 / in_rate as f64).min(1.0) * 0.9;
            let mut k = vec![0f32; PHASES * 2 * HALF];
            for phase in 0..PHASES {
                let frac = phase as f64 / PHASES as f64;
                let row = &mut k[phase * 2 * HALF..(phase + 1) * 2 * HALF];
                for (j, tap) in row.iter_mut().enumerate() {
                    // Tap j multiplies input sample base - HALF + 1 + j.
                    let x = j as f64 - (HALF as f64 - 1.0) - frac;
                    let sinc = if x == 0.0 {
                        1.0
                    } else {
                        (PI * cutoff * x).sin() / (PI * cutoff * x)
                    };
                    // Blackman window over [-HALF, HALF].
                    let w = (x / HALF as f64 + 1.0) / 2.0;
                    let window = 0.42 - 0.5 * (2.0 * PI * w).cos() + 0.08 * (4.0 * PI * w).cos();
                    *tap = (sinc * window) as f32;
                }
                let sum: f32 = row.iter().sum();
                row.iter_mut().for_each(|t| *t /= sum);
            }
            k
        });
        let mut r = Self {
            step_int: (num / den) as usize,
            step_num: num % den,
            den,
            buf: Vec::new(),
            base: 0,
            frac: 0,
            kernel,
        };
        r.reset();
        r
    }

    /// Feeds `input`, appending every output sample that can be computed so far to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(kernel) = &self.kernel else {
            out.extend_from_slice(input);
            return;
        };
        self.buf.extend_from_slice(input);
        while self.base + HALF < self.buf.len() {
            let phase = (self.frac * PHASES as u64 / self.den) as usize;
            let taps = &kernel[phase * 2 * HALF..(phase + 1) * 2 * HALF];
            let window = &self.buf[self.base + 1 - HALF..=self.base + HALF];
            out.push(taps.iter().zip(window).map(|(t, x)| t * x).sum());
            self.base += self.step_int;
            self.frac += self.step_num;
            if self.frac >= self.den {
                self.frac -= self.den;
                self.base += 1;
            }
        }
        // Drop input that no future output needs.
        let keep_from = (self.base + 1).saturating_sub(HALF);
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.base -= keep_from;
        }
    }

    /// Flushes the tail (pads with silence) and resets for the next recording.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.kernel.is_some() {
            self.process(&[0.0; HALF], out);
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        // Start with HALF - 1 virtual zeros so the first output sits on the first input.
        self.buf.clear();
        self.buf.resize(HALF - 1, 0.0);
        self.base = HALF - 1;
        self.frac = 0;
    }
}

/// One-shot convenience: resample a whole buffer.
pub fn resample(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    let mut r = Resampler::new(in_rate, out_rate);
    let mut out =
        Vec::with_capacity((input.len() as u64 * out_rate as u64 / in_rate as u64) as usize + HALF);
    r.process(input, &mut out);
    r.finish(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, secs: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize)
            .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn output_length_matches_ratio() {
        for (inr, outr) in [
            (48_000, 16_000),
            (44_100, 16_000),
            (8_000, 16_000),
            (16_000, 16_000),
        ] {
            let out = resample(&vec![0.1; inr as usize], inr, outr);
            let expected = outr as usize;
            assert!(
                out.len().abs_diff(expected) <= 2,
                "{inr}->{outr}: got {}, expected ~{expected}",
                out.len()
            );
        }
    }

    #[test]
    fn passband_sine_survives_and_dc_is_preserved() {
        let out = resample(&sine(1000.0, 48_000, 1.0), 48_000, 16_000);
        let mid = &out[1000..15_000];
        assert!(
            (rms(mid) - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02,
            "rms {}",
            rms(mid)
        );
        // Phase alignment: compare against an ideal 1 kHz sine at 16 kHz.
        let ideal = sine(1000.0, 16_000, 1.0);
        let err: Vec<f32> = mid
            .iter()
            .zip(&ideal[1000..15_000])
            .map(|(a, b)| a - b)
            .collect();
        assert!(rms(&err) < 0.02, "error rms {}", rms(&err));

        let dc = resample(&vec![0.5; 44_100], 44_100, 16_000);
        assert!(dc[100..15_000].iter().all(|v| (v - 0.5).abs() < 1e-3));
    }

    #[test]
    fn stopband_sine_is_attenuated() {
        // 12 kHz is above the 8 kHz output Nyquist: must not alias into the output.
        let out = resample(&sine(12_000.0, 48_000, 1.0), 48_000, 16_000);
        assert!(
            rms(&out[1000..15_000]) < 0.01,
            "rms {}",
            rms(&out[1000..15_000])
        );
    }

    #[test]
    fn streaming_in_chunks_equals_one_shot() {
        let input = sine(440.0, 44_100, 0.5);
        let one_shot = resample(&input, 44_100, 16_000);
        let mut r = Resampler::new(44_100, 16_000);
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        r.finish(&mut out);
        assert_eq!(out, one_shot);
        // And the resampler is reusable after finish().
        let mut again = Vec::new();
        r.process(&input, &mut again);
        r.finish(&mut again);
        assert_eq!(again, one_shot);
    }
}
