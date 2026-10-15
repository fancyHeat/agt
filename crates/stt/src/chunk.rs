//! Long-audio chunking for any [`SttEngine`].
//!
//! Full-attention encoders (Parakeet; Whisper has a hard 30 s window) get superlinearly slower
//! and hungrier with length: one 6-minute pass took 71 s and 8.6 GB. [`Chunked`] splits long
//! recordings at the quietest point near each chunk limit (between words, normally), transcribes
//! the pieces in order and joins the text, so cost grows linearly. Audio that fits in one chunk
//! goes straight to the engine: short dictations take exactly the same path as before.

use agt_core::stt::{Result, SAMPLE_RATE, SttEngine, TranscribeOpts, Transcript};
use std::ops::Range;

/// Energy is measured over 20 ms frames when looking for a split point.
const FRAME: usize = SAMPLE_RATE as usize / 50;

/// Splits `audio` into consecutive ranges of at most `max_len` samples. Each cut is placed at
/// the centre of the lowest-energy frame within the last `search_len` samples before the limit.
pub fn split(audio: &[f32], max_len: usize, search_len: usize) -> Vec<Range<usize>> {
    assert!(max_len > 0 && search_len <= max_len);
    let mut ranges = Vec::new();
    let mut start = 0;
    while audio.len() - start > max_len {
        let limit = start + max_len;
        let from = limit - search_len;
        let cut = (from..=limit.saturating_sub(FRAME))
            .step_by(FRAME / 2)
            .min_by(|&a, &b| energy(&audio[a..a + FRAME]).total_cmp(&energy(&audio[b..b + FRAME])))
            .map_or(limit, |f| f + FRAME / 2);
        ranges.push(start..cut);
        start = cut;
    }
    ranges.push(start..audio.len());
    ranges
}

fn energy(frame: &[f32]) -> f32 {
    frame.iter().map(|x| x * x).sum()
}

/// Wraps an engine so long audio is transcribed in chunks (see the module docs).
pub struct Chunked {
    inner: Box<dyn SttEngine>,
    max_len: usize,
    search_len: usize,
}

impl Chunked {
    /// Chunks are at most `max_secs` long, cut within the last `search_secs` of each.
    pub fn new(inner: Box<dyn SttEngine>, max_secs: f32, search_secs: f32) -> Self {
        let max_len = (max_secs * SAMPLE_RATE as f32) as usize;
        let search_len = ((search_secs * SAMPLE_RATE as f32) as usize).clamp(FRAME, max_len);
        Self {
            inner,
            max_len,
            search_len,
        }
    }
}

impl SttEngine for Chunked {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn warmup(&mut self) -> Result<()> {
        self.inner.warmup()
    }

    fn transcribe(&mut self, audio: &[f32], opts: &TranscribeOpts) -> Result<Transcript> {
        if audio.len() <= self.max_len {
            return self.inner.transcribe(audio, opts);
        }
        let ranges = split(audio, self.max_len, self.search_len);
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

    /// Loud "speech" everywhere except quiet gaps at the given second offsets (0.2 s each).
    fn speech_with_gaps(secs: usize, gaps: &[f32]) -> Vec<f32> {
        let mut a: Vec<f32> = (0..secs * S)
            .map(|i| ((i as f32) * 0.05).sin() * 0.5)
            .collect();
        for &g in gaps {
            let at = (g * S as f32) as usize;
            a[at..at + S / 5].iter_mut().for_each(|x| *x *= 0.001);
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
            (g..g + S / 5).contains(&cut)
        };
        assert!(in_gap(ranges[0].end, 22.0), "cut at {}", ranges[0].end);
        assert!(in_gap(ranges[1].end, 44.0), "cut at {}", ranges[1].end);
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
}
