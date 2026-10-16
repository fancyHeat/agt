//! Long-audio chunking for any [`SttEngine`].
//!
//! Full-attention encoders (Parakeet; Whisper has a hard 30 s window) get superlinearly slower
//! and hungrier with length: one 6-minute pass took 71 s and 8.6 GB. [`Chunked`] splits long
//! recordings at the quietest point near each chunk limit (between words, normally), transcribes
//! the pieces in order and joins the text, so cost grows linearly. Audio that fits in one chunk
//! goes straight to the engine: short dictations take exactly the same path as before.
//!
//! [`Cutter`] makes the same cuts while audio is still arriving (and optionally earlier ones,
//! at pauses), so finished segments can be transcribed while the user is still talking.

use agt_core::stt::{Result, SAMPLE_RATE, SttEngine, TranscribeOpts, Transcript};
use std::{collections::VecDeque, ops::Range};

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

/// A block's level is the median of the last 9 blocks' (in dB): background noise varies by
/// 10-20 dB from one 10 ms block to the next; the median evens that out, ignores clicks, and
/// follows speech within ~50 ms.
const SMOOTH: usize = 9;
/// A block is silence if its level is less than this far above the noise floor.
const MARGIN_DB: f32 = 15.0;
/// The noise floor is the lowest level of the last 20 s (long, so it can't drift up to the
/// level of the quieter syllables in a long stretch of speech) ...
const FLOOR_BLOCKS: usize = 2000;
/// ... but never below the first (digital silence at stream start, a muted mic) or above the
/// second (no silence seen yet: speech from the first block on).
const FLOOR_DB: (f32, f32) = (-70.0, -40.0);
/// Loud blocks in a row that end a silence.
const ONSET: usize = 2;

/// Settings for a [`Cutter`]; lengths in samples.
#[derive(Debug, Clone)]
pub struct CutConfig {
    /// Longest segment. Audio that reaches it is cut at the quietest [`PAUSE`]-long window in
    /// the last `search_len` samples before it, exactly where [`split`] cuts.
    pub max_len: usize,
    pub search_len: usize,
    /// Also cut at pauses; `None`: only at `max_len` (the same segments as [`split`]).
    pub pause: Option<PauseCut>,
}

impl CutConfig {
    /// The settings measured best for transcribing while recording (`docs/benchmarks.md`,
    /// "Long dictations"): segments of at most `max_secs` (the chunk length, or an engine's
    /// window if shorter), cut at 1 s pauses once they have 8 s of audio, pauses trimmed to
    /// 250 ms either side.
    pub fn live(max_secs: f32) -> Self {
        let secs = |s: f32| (s * SAMPLE_RATE as f32) as usize;
        Self {
            max_len: secs(max_secs),
            search_len: secs(crate::CHUNK_SEARCH_SECS.min(max_secs / 2.0)),
            pause: Some(PauseCut {
                len: secs(1.0),
                min_segment: secs(8.0),
                pad: secs(0.25),
                trim: true,
            }),
        }
    }
}

/// Pause-triggered cuts: see [`Cutter`].
#[derive(Debug, Clone)]
pub struct PauseCut {
    /// Silence at least this long is a pause.
    pub len: usize,
    /// A pause ends the segment only if at least this much audio precedes it.
    pub min_segment: usize,
    /// Silence kept after the last speech before a cut (and before speech after a trimmed
    /// pause). At most `len`.
    pub pad: usize,
    /// Drop each pause's silence beyond `pad` on either side instead of transcribing it;
    /// a tail without any speech is dropped too. Only once speech has been heard: before
    /// that, everything is kept.
    pub trim: bool,
}

/// Why a [`Segment`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// At a pause, once it was `PauseCut::len` long.
    Pause,
    /// At the quietest point before `max_len`.
    Limit,
    /// At [`Cutter::finish`] (the hotkey was released).
    End,
}

/// A finished piece of a recording, ready to transcribe.
#[derive(Debug, Clone)]
pub struct Segment {
    pub audio: Vec<f32>,
    pub cut: Cut,
    /// Samples received when it was cut: a pause cut comes `PauseCut::len` after the speech.
    pub at: usize,
}

/// Cuts a recording into segments while it is still arriving: [`push`](Self::push) the
/// audio as it comes in and transcribe each segment it returns (in order), then
/// [`finish`](Self::finish) on release for the rest. Joining the segments' texts with spaces
/// gives the transcript.
///
/// Every segment fits in `max_len`. With [`PauseCut`], a silence of `len` ends the segment as
/// soon as it is seen (cut `pad` after the last speech), so on release only the audio since
/// the last pause is left to transcribe. Silence is measured against a running noise floor
/// (the quietest moment of the last 20 s), so it doesn't depend on the mic gain; speech has
/// to stand out from the noise (tested down to ~25 dB). Nothing is trimmed until speech has
/// been heard: a recording that starts mid-word at a low level could otherwise teach the floor
/// the speech's own level and lose it as "silence"; the worst case is now transcribing noise.
///
/// No threads, no I/O; memory is the audio not handed out yet (at most `max_len` plus a
/// block). Cancelling is dropping it.
pub struct Cutter {
    cfg: CutConfig,
    /// Audio not handed out yet (minus trimmed silence).
    buf: Vec<f32>,
    /// `buf[..scanned]` has been looked at, in 10 ms blocks.
    scanned: usize,
    /// The last [`SMOOTH`] blocks' dB, and the levels of the last [`FLOOR_BLOCKS`].
    raw: VecDeque<f32>,
    levels: VecDeque<f32>,
    silence: Option<Silence>,
    /// `buf[..speech_end]` contains speech; 0 if `buf` has none.
    speech_end: usize,
    /// Speech has been heard at all. Until then the floor may be the speech itself (the user
    /// was already talking, quietly, when recording started), so nothing is trimmed.
    heard: bool,
    /// Samples pushed so far.
    received: usize,
}

/// The silence `buf` currently ends in.
struct Silence {
    /// Where it starts in `buf`.
    start: usize,
    /// How long it has lasted, in blocks (trimmed ones included).
    blocks: usize,
    /// Loud blocks in a row inside it.
    loud: usize,
    /// It has reached the pause length (and been cut or trimmed).
    pause: bool,
    /// Silence from here on in `buf`, except the last `pad`, is dropped as it arrives.
    trim_from: Option<usize>,
}

impl Cutter {
    pub fn new(mut cfg: CutConfig) -> Self {
        assert!(cfg.max_len > 0 && cfg.search_len <= cfg.max_len);
        if let Some(p) = &mut cfg.pause {
            p.len = p.len.max(HOP);
            p.pad = p.pad.min(p.len);
        }
        Self {
            cfg,
            buf: Vec::new(),
            scanned: 0,
            raw: VecDeque::with_capacity(SMOOTH),
            levels: VecDeque::with_capacity(FLOOR_BLOCKS),
            silence: None,
            speech_end: 0,
            heard: false,
            received: 0,
        }
    }

    /// Adds recorded audio; returns the segments that are finished now (usually none).
    pub fn push(&mut self, samples: &[f32]) -> Vec<Segment> {
        self.received += samples.len();
        self.buf.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.buf.len() - self.scanned >= HOP {
            self.scanned += HOP;
            self.scan_block(&mut out);
            while self.scanned > self.cfg.max_len {
                out.push(self.cut_at_limit());
            }
        }
        out
    }

    /// The hotkey was released: the rest of the audio, as one segment (or more if it is over
    /// `max_len`). None if there is nothing left, or (trimming) nothing but silence since
    /// the last segment.
    pub fn finish(mut self) -> Vec<Segment> {
        self.scanned = self.buf.len();
        let mut out = Vec::new();
        while self.buf.len() > self.cfg.max_len {
            out.push(self.cut_at_limit());
        }
        let trim = self.cfg.pause.as_ref().filter(|p| p.trim);
        if let (Some(p), Some(s), true) = (trim, &self.silence, self.heard) {
            // Trailing silence: keep `pad` of it.
            let end = (s.start + p.pad).min(self.buf.len());
            self.buf.truncate(end);
            self.scanned = end;
        }
        // Nothing heard: hand it all out rather than risk dropping quiet speech.
        if !self.buf.is_empty() && (trim.is_none() || self.speech_end > 0 || !self.heard) {
            let len = self.buf.len();
            out.push(self.take(len, Cut::End));
        }
        out
    }

    fn cut_at_limit(&mut self) -> Segment {
        let (max_len, search_len) = (self.cfg.max_len, self.cfg.search_len);
        let region = max_len - search_len;
        let cut = quietest_point(&self.buf[region..max_len]).map_or(max_len, |c| region + c);
        self.take(cut, Cut::Limit)
    }

    /// Hands out `buf[..cut]`.
    fn take(&mut self, cut: usize, kind: Cut) -> Segment {
        let at = self.received - (self.buf.len() - self.scanned);
        let audio: Vec<f32> = self.buf.drain(..cut).collect();
        self.scanned = self.scanned.saturating_sub(cut);
        self.speech_end = self.speech_end.saturating_sub(cut);
        if let Some(s) = &mut self.silence {
            s.start = s.start.saturating_sub(cut);
            s.trim_from = s.trim_from.map(|t| t.saturating_sub(cut));
        }
        Segment {
            audio,
            cut: kind,
            at,
        }
    }

    /// Classifies the block that ends at `scanned` and acts on a pause.
    fn scan_block(&mut self, out: &mut Vec<Segment>) {
        let Some(p) = self.cfg.pause.clone() else {
            return;
        };
        let block = &self.buf[self.scanned - HOP..self.scanned];
        let energy = block.iter().map(|&x| x * x).sum::<f32>() / HOP as f32;
        if self.raw.len() == SMOOTH {
            self.raw.pop_front();
        }
        self.raw.push_back(10.0 * (energy + 1e-12).log10());
        let mut recent = [0f32; SMOOTH];
        let recent = &mut recent[..self.raw.len()];
        for (r, &x) in recent.iter_mut().zip(&self.raw) {
            *r = x;
        }
        recent.sort_unstable_by(f32::total_cmp);
        let level = recent[recent.len() / 2];
        if self.levels.len() == FLOOR_BLOCKS {
            self.levels.pop_front();
        }
        self.levels.push_back(level);
        let floor = self.levels.iter().copied().fold(f32::MAX, f32::min);
        let quiet = level < floor.clamp(FLOOR_DB.0, FLOOR_DB.1) + MARGIN_DB;

        let s = match (&mut self.silence, quiet) {
            (None, false) => {
                self.speech_end = self.scanned;
                self.heard = true;
                return;
            }
            (None, true) => self.silence.insert(Silence {
                start: self.scanned - HOP,
                blocks: 0,
                loud: 0,
                pause: false,
                trim_from: None,
            }),
            (Some(s), _) => s,
        };
        if quiet {
            s.loud = 0;
        } else {
            s.loud += 1;
            if s.loud >= ONSET {
                self.silence = None;
                self.speech_end = self.scanned;
                self.heard = true;
                return;
            }
        }
        s.blocks += 1;
        if !s.pause && s.blocks * HOP >= p.len {
            s.pause = true;
            let start = s.start;
            if self.speech_end > 0 && start >= p.min_segment {
                out.push(self.take(start + p.pad, Cut::Pause));
                if p.trim {
                    self.silence.as_mut().unwrap().trim_from = Some(0);
                }
            } else if p.trim && self.heard {
                let keep = if self.speech_end > 0 {
                    start + p.pad
                } else {
                    0
                };
                self.silence.as_mut().unwrap().trim_from = Some(keep);
            }
        }
        let s = self.silence.as_mut().unwrap();
        if let Some(from) = s.trim_from
            && quiet
            && self.scanned > from + p.pad
        {
            // Keep the last `pad` (it goes before the next speech) and anything not scanned.
            let to = self.scanned - p.pad;
            self.buf.drain(from..to);
            self.scanned -= to - from;
        }
    }
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

    /// Pushes `audio` in uneven pieces (like audio callbacks); all segments, with the tail.
    fn cut_all(cfg: CutConfig, audio: &[f32]) -> Vec<Segment> {
        let mut c = Cutter::new(cfg);
        let mut out = Vec::new();
        for piece in audio.chunks(317) {
            out.extend(c.push(piece));
        }
        out.extend(c.finish());
        out
    }

    fn limit_only() -> CutConfig {
        CutConfig {
            max_len: 25 * S,
            search_len: 5 * S,
            pause: None,
        }
    }

    fn pauses(len_ms: usize, min_segment_s: usize, trim: bool) -> CutConfig {
        CutConfig {
            pause: Some(PauseCut {
                len: len_ms * S / 1000,
                min_segment: min_segment_s * S,
                pad: S / 4,
                trim,
            }),
            ..limit_only()
        }
    }

    /// `samples` is `secs` long, or up to 60 ms (the level's lag) longer.
    fn near(samples: usize, secs: f32) -> bool {
        let s = samples as f32 / S as f32;
        (secs..secs + 0.06).contains(&s)
    }

    /// Room noise (about -65 dBFS) with "speech" in the given ranges (s): a tone whose level
    /// swings 20 dB four times a second, like syllables.
    fn talk(secs: f32, speech: &[(f32, f32)]) -> Vec<f32> {
        let mut seed = 1u32;
        let mut a: Vec<f32> = (0..(secs * S as f32) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.002 - 0.001
            })
            .collect();
        for &(from, to) in speech {
            let r = (from * S as f32) as usize..(to * S as f32) as usize;
            for i in r {
                let syllables =
                    0.55 + 0.45 * (i as f32 * std::f32::consts::TAU * 4.0 / S as f32).sin();
                a[i] += (i as f32 * 0.05).sin() * 0.3 * syllables;
            }
        }
        a
    }

    #[test]
    fn without_pauses_it_cuts_like_split() {
        for audio in [
            speech_with_gaps(80, &[22.0, 44.0, 61.0]),
            speech_with_gaps(80, &[]),
            speech_with_gaps(25, &[]),
            speech_with_gaps(7, &[]),
        ] {
            let lens: Vec<usize> = cut_all(limit_only(), &audio)
                .iter()
                .map(|s| s.audio.len())
                .collect();
            let want: Vec<usize> = split(&audio, 25 * S, 5 * S)
                .iter()
                .map(|r| r.len())
                .collect();
            assert_eq!(lens, want);
        }
    }

    #[test]
    fn a_pause_cuts_as_soon_as_it_is_long_enough() {
        // Room noise, 5.5 s of speech, a 10 s thinking pause, 4 s of speech.
        let audio = talk(20.0, &[(0.5, 6.0), (16.0, 20.0)]);
        let mut c = Cutter::new(pauses(1000, 0, true));
        let mut first = None;
        for (i, piece) in audio.chunks(S / 50).enumerate() {
            let segs = c.push(piece);
            if !segs.is_empty() {
                assert!(first.is_none());
                first = Some((i, segs));
            }
        }
        let (i, segs) = first.expect("no cut at the pause");
        // Seen 1 s into the pause (plus the level's lag), cut 250 ms after the speech.
        let seen = (i + 1) as f32 / 50.0;
        assert!((7.0..7.08).contains(&seen), "{seen} s");
        assert_eq!(segs[0].cut, Cut::Pause);
        assert!(near(segs[0].audio.len(), 6.25), "{}", segs[0].audio.len());
        assert!(near(segs[0].at, 7.0), "{}", segs[0].at);
        // The rest of the pause is dropped but for 250 ms before the speech.
        let tail = c.finish();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].cut, Cut::End);
        let len = tail[0].audio.len() as f32 / S as f32;
        assert!((4.2..4.3).contains(&len), "{len} s");
    }

    #[test]
    fn short_pauses_and_clicks_do_not_cut() {
        // 0.6 s gaps between words, and a 20 ms click inside a 2 s pause.
        let mut audio = talk(
            14.0,
            &[
                (0.0, 2.0),
                (2.6, 5.0),
                (5.6, 7.0),
                (9.0, 11.0),
                (11.5, 14.0),
            ],
        );
        audio[8 * S..8 * S + S / 50]
            .iter_mut()
            .for_each(|x| *x = 0.5);
        let segs = cut_all(pauses(1500, 0, false), &audio);
        assert_eq!(
            segs.len(),
            2,
            "{:?}",
            segs.iter().map(|s| s.cut).collect::<Vec<_>>()
        );
        assert!(near(segs[0].audio.len(), 7.25));
        assert_eq!(segs.iter().map(|s| s.audio.len()).sum::<usize>(), 14 * S);
    }

    #[test]
    fn a_pause_after_too_little_speech_is_only_trimmed() {
        // 2 s of speech, 8 s pause, 3 s speech, 8 s pause, 2 s speech; segments >= 4 s.
        let audio = talk(23.0, &[(0.0, 2.0), (10.0, 13.0), (21.0, 23.0)]);
        let segs = cut_all(pauses(1000, 4, true), &audio);
        let lens: Vec<f32> = segs
            .iter()
            .map(|s| s.audio.len() as f32 / S as f32)
            .collect();
        // [2 + 0.25 + 0.25 + 3 + 0.25] = 5.75 s, then [0.25 + 2] = 2.25 s.
        assert_eq!(lens.len(), 2, "{lens:?}");
        assert!(
            (lens[0] - 5.75).abs() < 0.08 && (lens[1] - 2.25).abs() < 0.08,
            "{lens:?}"
        );
    }

    #[test]
    fn nothing_is_trimmed_before_speech_is_heard() {
        // No speech at all: handed out whole (it might be speech too quiet to tell apart).
        let audio = talk(30.0, &[]);
        let segs = cut_all(pauses(1000, 0, true), &audio);
        assert_eq!(segs.iter().map(|s| s.audio.len()).sum::<usize>(), 30 * S);
        // Leading silence is kept; trailing silence is trimmed to the padding.
        let audio = talk(12.0, &[(5.0, 7.0)]);
        let segs = cut_all(pauses(1000, 0, true), &audio);
        let lens: Vec<usize> = segs.iter().map(|s| s.audio.len()).collect();
        assert_eq!(lens.len(), 1);
        assert!(near(lens[0], 7.25), "{lens:?}");
        // Without trimming nothing is lost.
        let segs = cut_all(pauses(1000, 0, false), &audio);
        assert_eq!(segs.iter().map(|s| s.audio.len()).sum::<usize>(), 12 * S);
    }

    #[test]
    fn live_settings() {
        // 5 s of speech, a pause (too early to cut), 5 s more, a pause (cut), 3 s.
        let audio = talk(24.0, &[(0.5, 5.5), (8.0, 13.0), (16.0, 19.0), (21.0, 24.0)]);
        let segs = cut_all(CutConfig::live(25.0), &audio);
        let cuts: Vec<Cut> = segs.iter().map(|s| s.cut).collect();
        assert_eq!(cuts, [Cut::Pause, Cut::End]);
        // 0.5 + 5 + 0.25 + (trimmed) 0.25 + 5 + 0.25
        assert!(near(segs[0].audio.len(), 11.25), "{}", segs[0].audio.len());
    }

    #[test]
    fn released_after_trailing_silence() {
        // Speech, then 0.5 s (not yet a pause) or 3 s (a pause, trimmed) of room noise.
        for secs in [7.0, 9.5] {
            let audio = talk(secs, &[(0.5, 6.5)]);
            let segs = cut_all(CutConfig::live(25.0), &audio);
            assert_eq!(segs.len(), 1);
            assert_eq!(segs[0].cut, Cut::End);
            assert_eq!(segs[0].at, audio.len());
            assert!(near(segs[0].audio.len(), 6.75), "{}", segs[0].audio.len());
        }
    }

    #[test]
    fn quiet_speech_from_the_first_sample_is_kept() {
        // Already talking when recording starts, peaks ~24 dB over the room noise: the floor
        // first learned is the speech's own; none of it may be dropped as silence. (At this
        // level the end of the last syllable, ~0.1 s, may be taken for the start of the pause.)
        let audio: Vec<f32> = talk(6.5, &[(0.0, 6.0)]).iter().map(|x| x * 0.03).collect();
        let segs = cut_all(CutConfig::live(25.0), &audio);
        let kept: usize = segs.iter().map(|s| s.audio.len()).sum();
        assert!(
            kept >= 5 * S + 3 * S / 4,
            "{} s kept",
            kept as f32 / S as f32
        );
    }

    #[test]
    fn a_monologue_falls_back_to_limit_cuts() {
        let audio = talk(60.0, &[(0.0, 60.0)]);
        let segs = cut_all(pauses(1000, 0, true), &audio);
        assert_eq!(segs.len(), 3);
        assert!(segs.iter().all(|s| s.audio.len() <= 25 * S));
        assert_eq!(segs[0].cut, Cut::Limit);
        assert_eq!(segs.iter().map(|s| s.audio.len()).sum::<usize>(), 60 * S);
    }

    #[test]
    fn works_at_any_level() {
        // The same recording 30 dB quieter. (Starting with a little room noise: a
        // tone has no quiet moments to learn the floor from, speech does.)
        let audio = talk(21.0, &[(0.5, 6.0), (16.0, 21.0)]);
        let quiet: Vec<f32> = audio.iter().map(|x| x * 0.03).collect();
        let lens = |a: &[f32]| -> Vec<usize> {
            cut_all(pauses(1000, 0, true), a)
                .iter()
                .map(|s| s.audio.len())
                .collect()
        };
        // The same cuts, give or take the end of a syllable.
        let (a, b) = (lens(&audio), lens(&quiet));
        assert_eq!(a.len(), 2);
        assert_eq!(a.len(), b.len());
        assert!(
            a.iter().zip(&b).all(|(x, y)| x.abs_diff(*y) < S / 8),
            "{a:?} {b:?}"
        );
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
