//! Polishing a long dictation while it is still being recorded (not wired into the app yet;
//! see docs/benchmarks.md, "Polish speed").
//!
//! The pipeline transcribes finished segments of a long dictation while the user is still
//! talking. [`Ahead`] takes those transcripts in order and hands out pieces that can be
//! polished right away, so that at release only the rest is left to polish.
//!
//! Pieces are cut only at a sentence end *inside* a segment, never at a segment boundary: a
//! segment is cut at a pause, which may be mid-sentence, and the speech recognizer often
//! closes a segment with a period anyway ("My great-grandparents. | Died all of them").
//! Polish repairs such a join only if it sees both sides, so each boundary stays inside one
//! piece. The text after the last usable sentence end waits for the next segment.
//!
//! When in doubt it doesn't cut: a word with a period inside (`U.S.`, `a.m.`, `e.g.`),
//! a known abbreviation (`Mr.`, `approx.`, `Inc.`) or an initial never ends a sentence, and
//! the next word must start with a capital letter (`Fig. 3`, `approx. 5` don't cut). Missing
//! a cut only makes a piece longer. So that held text can't grow without bound (a long run-on
//! with no usable sentence end), more than [`MAX_HELD_WORDS`] are cut at a segment boundary
//! after all, and no piece is ever over [`MAX_PIECE_WORDS`].

use agt_core::config::{PolishConfig, PolishProvider, S1Context, S1Structure};

/// A piece shorter than this waits for more text (a short piece has little context to go on,
/// and pieces under `polish.min_words` would not be polished at all).
pub const MIN_PIECE_WORDS: usize = 20;
/// Held text longer than this is cut at its last segment boundary when no sentence end is
/// usable (rare: one segment is at most ~25 s, ~100 words).
pub const MAX_HELD_WORDS: usize = 800;
/// No piece is longer than this, well under what S1-mini's context holds
/// ([`crate::guard::S1_MINI_MAX_WORDS`]).
pub const MAX_PIECE_WORDS: usize = 1000;

/// Abbreviations that end with a period but don't end a sentence before a capital. Words
/// with a period inside (`U.S.`, `a.m.`, `Ph.D.`) and initials are recognized without a list.
const ABBREVIATIONS: &[&str] = &[
    "mr.", "mrs.", "ms.", "dr.", "prof.", "st.", "jr.", "sr.", "mt.", "vs.", "no.", "etc.",
    "approx.", "fig.", "figs.", "inc.", "ltd.", "co.", "corp.", "dept.", "est.", "vol.",
];

/// Splits a dictation's segment transcripts into pieces to polish ahead of release.
///
/// ```text
/// let Some(mut ahead) = Ahead::new(&cfg) else { polish the whole text at release, as today };
/// for each segment transcript:  if let Some(piece) = ahead.push(text) { polish piece now }
/// at release:                   polish each of ahead.finish(tail's transcript)
/// ```
/// The polished pieces, joined with spaces in order, are the polished dictation.
#[derive(Debug, Default)]
pub struct Ahead {
    /// Words not handed out yet.
    words: Vec<String>,
    /// Number of words in `words` at the end of each pushed segment.
    seams: Vec<usize>,
}

impl Ahead {
    /// `None` when the output needs the whole text: S1-mini's email context (a greeting and
    /// a sign-off around everything) and its list structure (bullets need all the items).
    /// Whether pieces are worth a request each (a cloud provider bills them) is the caller's
    /// call.
    pub fn new(cfg: &PolishConfig) -> Option<Self> {
        let s1 = &cfg.s1_mini;
        let whole_only = cfg.provider == PolishProvider::S1Mini
            && (s1.context == S1Context::Email || s1.structure == S1Structure::Lists);
        (!whole_only).then(Self::default)
    }

    /// Adds the next segment's transcript. Returns the text that is ready to polish: up to
    /// the last sentence end that isn't at a segment boundary, if that gives a piece of at
    /// least [`MIN_PIECE_WORDS`] words (or a forced cut, see the module docs).
    pub fn push(&mut self, segment: &str) -> Option<String> {
        let before = self.words.len();
        self.words
            .extend(segment.split_whitespace().map(str::to_owned));
        if self.words.len() == before {
            return None;
        }
        self.seams.push(self.words.len());
        let held = self.words.len();
        let cut = self.sentence_cut(held - 1).or_else(|| {
            // The last boundary before this segment, so only this segment stays held.
            (held > MAX_HELD_WORDS).then(|| self.forced_cut(held - 1))
        })?;
        Some(self.take(cut))
    }

    /// At release: everything not handed out yet followed by the tail's transcript, as the
    /// last pieces to polish; one piece unless that would be over [`MAX_PIECE_WORDS`].
    pub fn finish(mut self, tail: &str) -> Vec<String> {
        self.words
            .extend(tail.split_whitespace().map(str::to_owned));
        let mut pieces = Vec::new();
        while self.words.len() > MAX_PIECE_WORDS {
            let cut = self
                .sentence_cut(MAX_PIECE_WORDS)
                .unwrap_or_else(|| self.forced_cut(MAX_PIECE_WORDS));
            pieces.push(self.take(cut));
        }
        if !self.words.is_empty() || pieces.is_empty() {
            pieces.push(self.words.join(" "));
        }
        pieces
    }

    /// The longest piece of at most `max` words (and [`MIN_PIECE_WORDS`] or more) that ends
    /// at a sentence end, not at a segment boundary, with more words after it.
    fn sentence_cut(&self, max: usize) -> Option<usize> {
        (MIN_PIECE_WORDS..=max.min(self.words.len() - 1))
            .rev()
            .find(|&n| {
                !self.seams.contains(&n)
                    && ends_sentence(&self.words[n - 1])
                    && starts_sentence(&self.words[n])
            })
    }

    /// The last segment boundary within `max` words, or `max` itself if there is none.
    fn forced_cut(&self, max: usize) -> usize {
        self.seams
            .iter()
            .copied()
            .filter(|&s| (MIN_PIECE_WORDS..=max).contains(&s))
            .max()
            .unwrap_or(max)
    }

    /// Hands out the first `cut` words.
    fn take(&mut self, cut: usize) -> String {
        let piece = self.words.drain(..cut).collect::<Vec<_>>().join(" ");
        self.seams.retain_mut(|s| {
            *s -= cut.min(*s);
            *s > 0
        });
        piece
    }
}

/// `end.`, `why?"`, `no!)`, `3.5.`, but not `Mr.`, `U.S.`, `a.m.` or an initial (`J.`).
fn ends_sentence(word: &str) -> bool {
    let w = word.trim_end_matches(['"', '\'', ')', '\u{201d}', '\u{2019}']);
    let Some(body) = w.strip_suffix(['.', '?', '!']) else {
        return false;
    };
    if w.ends_with('.') {
        let number = body
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',');
        let dotted = body.contains('.') && !number;
        let initial = body.chars().count() == 1 && body.starts_with(char::is_alphabetic);
        if dotted || initial || ABBREVIATIONS.contains(&w.to_lowercase().as_str()) {
            return false;
        }
    }
    true
}

/// A capital letter (not a digit: `Fig. 3`, `approx. 5`, `No. 5`).
fn starts_sentence(word: &str) -> bool {
    word.trim_start_matches(['"', '\'', '(', '\u{201c}', '\u{2018}'])
        .starts_with(char::is_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize, w: &str) -> String {
        vec![w; n].join(" ")
    }

    fn ahead() -> Ahead {
        Ahead::new(&PolishConfig::default()).unwrap()
    }

    #[test]
    fn cuts_at_the_last_sentence_end_inside_a_segment() {
        let mut a = ahead();
        let seg = format!("{}. Then we left. And then", words(20, "a"));
        assert_eq!(
            a.push(&seg).as_deref(),
            Some(format!("{}. Then we left.", words(20, "a")).as_str())
        );
        assert_eq!(a.finish("we went home."), ["And then we went home."]);
    }

    #[test]
    fn never_cuts_at_a_segment_boundary() {
        let mut a = ahead();
        // The recognizer's period at the end of a segment may be a pause mid-sentence.
        assert_eq!(
            a.push(&format!("{} My great-grandparents.", words(25, "a"))),
            None
        );
        // The boundary goes to the model inside one piece, with both sides.
        let piece = a.push("Died all of them. Here").unwrap();
        assert!(
            piece.ends_with("My great-grandparents. Died all of them."),
            "{piece}"
        );
        assert_eq!(a.finish(""), ["Here"]);
    }

    #[test]
    fn a_later_segment_releases_text_held_back_earlier() {
        let mut a = ahead();
        assert_eq!(a.push(&format!("{} end.", words(5, "a"))), None); // too short to cut
        let piece = a
            .push(&format!("More {}. Next one", words(20, "bee")))
            .unwrap();
        assert!(piece.starts_with("a a a a a end. More bee"), "{piece}");
        assert!(piece.ends_with("bee."), "{piece}");
        assert_eq!(a.finish(""), ["Next one"]);
    }

    #[test]
    fn abbreviations_initials_and_numbers_are_not_cut() {
        for phrase in [
            "the U.S. Army came",
            "at 9 a.m. Monday we",
            "like e.g. Paris and",
            "the Ph.D. Program is",
            "approx. 5 people came",
            "see Fig. 3 below",
            "Acme Inc. Today we",
            "met Mr. Smith and J. K. Rowling",
            "it costs 3.5 Euros",
            "and this. then that",
        ] {
            let mut a = ahead();
            assert_eq!(
                a.push(&format!("{} {phrase} and more", words(20, "a"))),
                None,
                "{phrase}"
            );
        }
        assert!(ends_sentence("left.\"") && ends_sentence("why?") && ends_sentence("3.5."));
        // An ellipsis may trail off mid-sentence: not a cut.
        assert!(!ends_sentence("wait...") && !ends_sentence("met,") && !ends_sentence("É."));
        assert!(starts_sentence("\"Hello") && !starts_sentence("2021") && !starts_sentence("and"));
    }

    #[test]
    fn held_text_and_the_last_pieces_stay_bounded() {
        // A run-on without a usable sentence end: cut at a boundary once too much is held.
        let mut a = ahead();
        let mut handed = Vec::new();
        for _ in 0..20 {
            handed.extend(a.push(&words(90, "run")));
        }
        assert!(!handed.is_empty());
        for p in &handed {
            let n = p.split_whitespace().count();
            assert!(n > MAX_HELD_WORDS - 90 && n <= MAX_HELD_WORDS + 90, "{n}");
            assert_eq!(n % 90, 0, "cut at a segment boundary");
        }
        // A huge tail is split too.
        let last = a.finish(&words(2500, "tail"));
        assert!(last.len() >= 3);
        assert!(
            last.iter()
                .all(|p| p.split_whitespace().count() <= MAX_PIECE_WORDS)
        );
        let total: usize = handed
            .iter()
            .chain(&last)
            .map(|p| p.split_whitespace().count())
            .sum();
        assert_eq!(total, 20 * 90 + 2500);
    }

    #[test]
    fn whole_text_settings_get_no_ahead() {
        let mut cfg = PolishConfig::default();
        cfg.s1_mini.context = S1Context::Email;
        assert!(Ahead::new(&cfg).is_none());
        cfg.s1_mini.context = S1Context::General;
        cfg.s1_mini.structure = S1Structure::Lists;
        assert!(Ahead::new(&cfg).is_none());
    }

    #[test]
    fn empty_segments_change_nothing() {
        let mut a = ahead();
        assert_eq!(a.push("   "), None);
        assert_eq!(a.finish(""), [""]);
    }
}
