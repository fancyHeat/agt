//! Greedy decoding for Token-and-Duration Transducers (TDT), independent of ONNX so it can be
//! unit-tested. Matches NeMo's / onnx-asr's greedy TDT decoding.

use agt_core::stt::Result;

/// Frame advances the duration head can predict (Parakeet TDT: 0..=4).
pub const DURATIONS: [usize; 5] = [0, 1, 2, 3, 4];
/// Safety valve against a model that keeps emitting tokens without advancing.
const MAX_SYMBOLS_PER_FRAME: usize = 10;

/// One prediction-network + joint evaluation.
pub trait Joint {
    /// Scores encoder frame `t` given the last emitted token (blank at the start).
    /// Returns `(token, duration index)`, both argmaxes.
    fn step(&mut self, t: usize, last_token: usize) -> Result<(usize, usize)>;
    /// Keep the prediction-network state produced by the last `step`
    /// (called only when that step emitted a non-blank token).
    fn accept(&mut self);
}

/// Decodes `frames` encoder frames; returns the emitted token ids.
pub fn greedy_decode(frames: usize, blank: usize, joint: &mut impl Joint) -> Result<Vec<usize>> {
    let mut tokens = Vec::new();
    let (mut t, mut emitted_here) = (0, 0);
    while t < frames {
        let (token, dur) = joint.step(t, tokens.last().copied().unwrap_or(blank))?;
        if token != blank {
            joint.accept();
            tokens.push(token);
            emitted_here += 1;
        }
        let skip = DURATIONS.get(dur).copied().unwrap_or(1);
        if skip > 0 {
            t += skip;
            emitted_here = 0;
        } else if token == blank || emitted_here >= MAX_SYMBOLS_PER_FRAME {
            t += 1;
            emitted_here = 0;
        }
    }
    Ok(tokens)
}

/// Index of the largest value (first one on ties); 0 for an empty slice.
pub fn argmax(v: &[f32]) -> usize {
    let mut best = 0;
    for (i, &x) in v.iter().enumerate() {
        if x > v[best] {
            best = i;
        }
    }
    best
}

/// SentencePiece pieces to text: `▁` marks a word start. Special tokens (`<unk>`, `<|...|>`)
/// are dropped.
pub fn detokenize<'a>(pieces: impl IntoIterator<Item = &'a str>) -> String {
    let mut s = String::new();
    for p in pieces {
        if !(p.starts_with('<') && p.ends_with('>')) {
            s.push_str(p);
        }
    }
    s.replace('\u{2581}', " ").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays a script of `(expected t, token, duration index)` steps.
    struct Scripted {
        script: Vec<(usize, usize, usize)>,
        accepted: usize,
    }

    impl Joint for Scripted {
        fn step(&mut self, t: usize, _last: usize) -> Result<(usize, usize)> {
            let (want_t, tok, dur) = self.script.remove(0);
            assert_eq!(t, want_t, "decoder visited the wrong frame");
            Ok((tok, dur))
        }
        fn accept(&mut self) {
            self.accepted += 1;
        }
    }

    const BLANK: usize = 9;

    #[test]
    fn durations_skip_frames_and_zero_duration_stays() {
        let mut j = Scripted {
            script: vec![
                (0, 1, 0),     // token, stay on frame 0
                (0, 2, 2),     // token, skip to 2
                (2, BLANK, 0), // blank with duration 0 still advances by one
                (3, 3, 4),     // token, skip past the end
            ],
            accepted: 0,
        };
        assert_eq!(greedy_decode(5, BLANK, &mut j).unwrap(), vec![1, 2, 3]);
        assert_eq!(j.accepted, 3);
        assert!(j.script.is_empty());
    }

    #[test]
    fn symbols_per_frame_are_capped() {
        let mut script = vec![(0, 1, 0); MAX_SYMBOLS_PER_FRAME];
        script.push((1, BLANK, 1));
        let mut j = Scripted {
            script,
            accepted: 0,
        };
        let out = greedy_decode(2, BLANK, &mut j).unwrap();
        assert_eq!(out.len(), MAX_SYMBOLS_PER_FRAME);
    }

    #[test]
    fn argmax_picks_first_max() {
        assert_eq!(argmax(&[0.1, 0.5, 0.5, -1.0]), 1);
        assert_eq!(argmax(&[]), 0);
    }

    #[test]
    fn detokenize_joins_pieces() {
        assert_eq!(
            detokenize(["\u{2581}Hel", "lo", ",", "\u{2581}world", "<unk>", "."]),
            "Hello, world."
        );
    }
}
