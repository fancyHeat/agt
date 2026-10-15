//! Long-audio chunking for any [`SttEngine`].
//!
//! Full-attention encoders (Parakeet; Whisper has a hard 30 s window) get superlinearly slower
//! and hungrier with length: one 6-minute pass took 71 s and 8.6 GB. [`Chunked`] splits long
//! recordings at the quietest point near each chunk limit (between words, normally), transcribes
//! the pieces in order and joins the text, so cost grows linearly. Audio that fits in one chunk
//! goes straight to the engine: short dictations take exactly the same path as before.

use agt_core::stt::{Result, SAMPLE_RATE, SttEngine, TranscribeOpts, Transcript};
use std::ops::Range;

/// Energy is summed over 10 ms blocks; candidate cut windows slide in 10 ms steps.
const HOP: usize = SAMPLE_RATE as usize / 100;
/// A cut goes in the middle of the quietest 400 ms stretch. Single 20-40 ms dips also occur
/// between words *inside* a phrase; cutting there makes the model finish the clipped clause on
/// its own ("licenses, and so on. And the timeline"). 400 ms only fits a real pause.
const PAUSE: usize = 40 * HOP;

/// Splits `audio` into consecutive ranges of at most `max_len` samples. Each cut is placed in
/// the middle of the lowest-energy [`PAUSE`]-long window within the last `search_len` samples
/// before the limit.
pub fn split(audio: &[f32], max_len: usize, search_len: usize) -> Vec<Range<usize>> {
    assert!(max_len > 0 && search_len <= max_len);
    let mut ranges = Vec::new();
    let mut start = 0;
    while audio.len() - start > max_len {
        let limit = start + max_len;
        let cut = quietest_point(&audio[limit - search_len..limit])
            .map_or(limit, |c| limit - search_len + c);
        ranges.push(start..cut);
        start = cut;
    }
    ranges.push(start..audio.len());
    ranges
}

/// Centre of the lowest-energy window of `PAUSE` samples (or of the whole region, if shorter),
/// as an offset into `region`. `None` if the region is shorter than one block.
fn quietest_point(region: &[f32]) -> Option<usize> {
    let blocks: Vec<f64> = region
        .as_chunks::<HOP>()
        .0
        .iter()
        .map(|b| b.iter().map(|&x| f64::from(x) * f64::from(x)).sum())
        .collect();
    if blocks.is_empty() {
        return None;
    }
    let w = (PAUSE / HOP).min(blocks.len());
    let mut sum: f64 = blocks[..w].iter().sum();
    let (mut best, mut best_at) = (sum, 0);
    for i in w..blocks.len() {
        sum += blocks[i] - blocks[i - w];
        if sum < best {
            (best, best_at) = (sum, i + 1 - w);
        }
    }
    Some(best_at * HOP + w * HOP / 2)
}

/// Wraps an engine so long audio is transcribed in chunks (see the module docs).
pub struct Chunked {
    inner: Box<dyn SttEngine>,
    max_secs: f32,
    search_secs: f32,
}

impl Chunked {
    /// Chunks are at most `max_secs` long, or the engine's [`SttEngine::max_input_secs`] if
    /// that is shorter, and cut within the last `search_secs` (at most half a chunk) of each.
    pub fn new(inner: Box<dyn SttEngine>, max_secs: f32, search_secs: f32) -> Self {
        Self {
            inner,
            max_secs,
            search_secs,
        }
    }

    /// `(max_len, search_len)` in samples. Asked on every call: the engine's limit can go
    /// away after load (a GPU-to-CPU fallback).
    fn limits(&self) -> (usize, usize) {
        let secs = self
            .inner
            .max_input_secs()
            .map_or(self.max_secs, |m| m.min(self.max_secs));
        let max_len = ((secs * SAMPLE_RATE as f32) as usize).max(HOP);
        let search_len =
            ((self.search_secs.min(secs / 2.0) * SAMPLE_RATE as f32) as usize).clamp(HOP, max_len);
        (max_len, search_len)
    }
}

impl SttEngine for Chunked {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn warmup(&mut self) -> Result<()> {
        self.inner.warmup()
    }

    fn notice(&self) -> Option<String> {
        self.inner.notice()
    }

    fn transcribe(&mut self, audio: &[f32], opts: &TranscribeOpts) -> Result<Transcript> {
        let (max_len, search_len) = self.limits();
        if audio.len() <= max_len {
            return self.inner.transcribe(audio, opts);
        }
        let ranges = split(audio, max_len, search_len);
        tracing::debug!(
            "transcribing {:.1} s in {} chunks",
            audio.len() as f64 / SAMPLE_RATE as f64,
            ranges.len()
        );
        let mut text = String::new();
        for r in ranges {
            let piece = self.inner.transcribe(&audio[r], opts)?.text;
            let piece = piece.trim();
            if !piece.is_empty() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(piece);
            }
        }
        Ok(Transcript { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: usize = SAMPLE_RATE as usize;

    /// Loud "speech" everywhere except quiet pauses at the given second offsets (0.5 s each).
    fn speech_with_gaps(secs: usize, gaps: &[f32]) -> Vec<f32> {
        let mut a: Vec<f32> = (0..secs * S)
            .map(|i| ((i as f32) * 0.05).sin() * 0.5)
            .collect();
        for &g in gaps {
            let at = (g * S as f32) as usize;
            a[at..at + S / 2].iter_mut().for_each(|x| *x *= 0.001);
        }
        a
    }

    #[test]
    fn short_audio_is_one_range() {
        assert_eq!(split(&vec![0.0; 10 * S], 25 * S, 5 * S), vec![0..10 * S]);
        assert_eq!(split(&vec![0.0; 25 * S], 25 * S, 5 * S), vec![0..25 * S]);
    }

    #[test]
    fn cuts_land_in_the_quiet_gaps() {
        let audio = speech_with_gaps(60, &[22.0, 44.0]);
        let ranges = split(&audio, 25 * S, 5 * S);
        assert_eq!(ranges.len(), 3);
        // Contiguous, complete, each within the limit.
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges.last().unwrap().end, audio.len());
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        assert!(ranges.iter().all(|r| r.len() <= 25 * S));
        let in_gap = |cut: usize, gap: f32| {
            let g = (gap * S as f32) as usize;
            (g..g + S / 2).contains(&cut)
        };
        assert!(in_gap(ranges[0].end, 22.0), "cut at {}", ranges[0].end);
        assert!(in_gap(ranges[1].end, 44.0), "cut at {}", ranges[1].end);
    }

    #[test]
    fn a_real_pause_beats_a_deeper_but_brief_dip() {
        // Speech with a 20 ms dead-silent dip at 21 s (between words inside a phrase) and a
        // 450 ms pause at 23 s that is quiet but not silent (room noise).
        let mut audio = speech_with_gaps(40, &[]);
        audio[21 * S..21 * S + S / 50]
            .iter_mut()
            .for_each(|x| *x = 0.0);
        let pause = 23 * S..23 * S + S * 45 / 100;
        audio[pause.clone()].iter_mut().for_each(|x| *x *= 0.02);
        let ranges = split(&audio, 25 * S, 5 * S);
        assert!(
            pause.contains(&ranges[0].end),
            "cut at {} s",
            ranges[0].end as f32 / S as f32
        );
    }

    #[test]
    fn no_quiet_point_still_respects_the_limit() {
        let audio = speech_with_gaps(80, &[]);
        let ranges = split(&audio, 25 * S, 5 * S);
        assert!(ranges.iter().all(|r| r.len() <= 25 * S && !r.is_empty()));
        assert_eq!(ranges.iter().map(|r| r.len()).sum::<usize>(), audio.len());
    }

    /// Records the lengths it was called with; returns "w<n>" per call.
    struct Fake(Vec<usize>);
    impl SttEngine for Fake {
        fn name(&self) -> String {
            "fake".into()
        }
        fn warmup(&mut self) -> Result<()> {
            Ok(())
        }
        fn transcribe(&mut self, audio: &[f32], _: &TranscribeOpts) -> Result<Transcript> {
            self.0.push(audio.len());
            Ok(Transcript {
                text: format!(" w{} ", self.0.len()),
            })
        }
    }

    #[test]
    fn wrapper_passes_short_audio_through_and_joins_chunks() {
        let mut c = Chunked::new(Box::new(Fake(Vec::new())), 25.0, 5.0);
        let short = c
            .transcribe(&vec![0.1; 3 * S], &TranscribeOpts::default())
            .unwrap();
        assert_eq!(short.text, " w1 "); // untouched: the exact single-call path
        let long = c
            .transcribe(
                &speech_with_gaps(60, &[22.0, 44.0]),
                &TranscribeOpts::default(),
            )
            .unwrap();
        assert_eq!(long.text, "w2 w3 w4");
    }

    /// An engine with a fixed input window, like a static-shape GPU encoder.
    struct Windowed(Vec<usize>, Option<f32>);
    impl SttEngine for Windowed {
        fn name(&self) -> String {
            "windowed".into()
        }
        fn warmup(&mut self) -> Result<()> {
            Ok(())
        }
        fn transcribe(&mut self, audio: &[f32], _: &TranscribeOpts) -> Result<Transcript> {
            if let Some(m) = self.1 {
                assert!(
                    audio.len() <= (m * S as f32) as usize,
                    "{} samples",
                    audio.len()
                );
            }
            self.0.push(audio.len());
            Ok(Transcript::default())
        }
        fn max_input_secs(&self) -> Option<f32> {
            self.1
        }
    }

    #[test]
    fn chunks_fit_the_engines_window() {
        // A 10 s window (Core ML) under the 25 s default: 22 s of audio must be split.
        let mut c = Chunked::new(Box::new(Windowed(Vec::new(), Some(10.0))), 25.0, 5.0);
        c.transcribe(
            &speech_with_gaps(22, &[8.0, 17.0]),
            &TranscribeOpts::default(),
        )
        .unwrap();
        // Exactly the window: one call.
        let mut c = Chunked::new(Box::new(Windowed(Vec::new(), Some(10.0))), 25.0, 5.0);
        c.transcribe(&vec![0.1; 10 * S], &TranscribeOpts::default())
            .unwrap();
        // A window larger than the chunk size changes nothing.
        let mut c = Chunked::new(Box::new(Windowed(Vec::new(), Some(30.0))), 25.0, 5.0);
        let (max_len, _) = c.limits();
        assert_eq!(max_len, 25 * S);
        c.transcribe(&vec![0.1; 25 * S], &TranscribeOpts::default())
            .unwrap();
    }
}
