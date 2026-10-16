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
//! Only for `[Context: general]`: S1-mini's email context writes a greeting and a sign-off,
//! which needs the whole text.

/// A piece shorter than this waits for more text (a short piece has little context to go on,
/// and pieces under `polish.min_words` would not be polished at all).
pub const MIN_PIECE_WORDS: usize = 20;

/// Abbreviations that end with a period but don't end a sentence before a capital.
const ABBREVIATIONS: &[&str] = &[
    "mr.", "mrs.", "ms.", "dr.", "prof.", "st.", "jr.", "sr.", "mt.", "vs.", "no.", "etc.",
];

/// Splits a dictation's segment transcripts into pieces to polish ahead of release.
///
/// ```text
/// for each segment transcript:  if let Some(piece) = ahead.push(text) { polish piece now }
/// at release:                   polish ahead.finish(tail's transcript)
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the next segment's transcript. Returns the text that is ready to polish: up to
    /// the last sentence end that isn't at a segment boundary, if that gives a piece of at
    /// least [`MIN_PIECE_WORDS`] words.
    pub fn push(&mut self, segment: &str) -> Option<String> {
        let before = self.words.len();
        self.words
            .extend(segment.split_whitespace().map(str::to_owned));
        if self.words.len() == before {
            return None;
        }
        self.seams.push(self.words.len());
        // Cut after word `i`: a sentence end, followed by a capital or a digit, not where a
        // segment ended.
        let cut = (MIN_PIECE_WORDS..self.words.len()).rev().find(|&n| {
            !self.seams.contains(&n)
                && ends_sentence(&self.words[n - 1])
                && starts_sentence(&self.words[n])
        })?;
        let piece = self.words.drain(..cut).collect::<Vec<_>>().join(" ");
        self.seams.retain_mut(|s| {
            *s -= cut.min(*s);
            *s > 0
        });
        Some(piece)
    }

    /// At release: everything not handed out yet followed by the tail's transcript, as one
    /// last piece to polish (one request, however much is left).
    pub fn finish(mut self, tail: &str) -> String {
        self.words
            .extend(tail.split_whitespace().map(str::to_owned));
        self.words.join(" ")
    }
}

/// `end.`, `why?"`, `no!)`, but not `Mr.` or an initial (`J.`).
fn ends_sentence(word: &str) -> bool {
    let w = word.trim_end_matches(['"', '\'', ')', '\u{201d}', '\u{2019}']);
    if !w.ends_with(['.', '?', '!']) {
        return false;
    }
    let lower = w.to_lowercase();
    let initial = w.len() == 2 && w.starts_with(|c: char| c.is_alphabetic());
    !initial && !ABBREVIATIONS.contains(&lower.as_str())
}

fn starts_sentence(word: &str) -> bool {
    word.trim_start_matches(['"', '\'', '(', '\u{201c}', '\u{2018}'])
        .starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize, w: &str) -> String {
        vec![w; n].join(" ")
    }

    #[test]
    fn cuts_at_the_last_sentence_end_inside_a_segment() {
        let mut a = Ahead::new();
        let seg = format!("{}. Then we left. And then", words(20, "a"));
        assert_eq!(
            a.push(&seg).as_deref(),
            Some(format!("{}. Then we left.", words(20, "a")).as_str())
        );
        assert_eq!(a.finish("we went home."), "And then we went home.");
    }

    #[test]
    fn never_cuts_at_a_segment_boundary() {
        let mut a = Ahead::new();
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
        assert_eq!(a.finish(""), "Here");
    }

    #[test]
    fn a_later_segment_releases_text_held_back_earlier() {
        let mut a = Ahead::new();
        assert_eq!(a.push(&format!("{} end.", words(5, "a"))), None); // too short to cut
        let piece = a
            .push(&format!("More {}. Next one", words(20, "bee")))
            .unwrap();
        assert!(piece.starts_with("a a a a a end. More bee"), "{piece}");
        assert!(piece.ends_with("bee."), "{piece}");
        assert_eq!(a.finish(""), "Next one");
    }

    #[test]
    fn abbreviations_initials_and_lowercase_are_not_sentence_ends() {
        let mut a = Ahead::new();
        let seg = format!(
            "{} met Mr. Smith and J. K. Rowling. e.g. this. then",
            words(20, "a")
        );
        assert_eq!(a.push(&seg), None);
        assert!(ends_sentence("left.\"") && ends_sentence("why?") && !ends_sentence("met,"));
        assert!(!ends_sentence("Dr.") && !ends_sentence("J."));
        assert!(starts_sentence("\"Hello") && starts_sentence("2021") && !starts_sentence("and"));
    }

    #[test]
    fn empty_segments_change_nothing() {
        let mut a = Ahead::new();
        assert_eq!(a.push("   "), None);
        assert_eq!(a.finish(""), "");
    }
}
