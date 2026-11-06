//! Filler-word count for a transcript: how many words were spoken and how many of them were
//! fillers ("um", "uh", "like", "you know", ...), for the after-dictation word count.
//!
//! Counted on the raw transcript, before polish (which removes fillers). Hesitation sounds
//! ("um", "uh", "hmm") always count. Words that are often real words count only where they
//! read as filler:
//! - "like" next to a comma ("it was, like, huge", "like, why"), never "I like it".
//! - "you know" and "I mean" before a comma or at the end of a sentence ("it's, you know,
//!   fine", "I mean, sure"), never "do you know where" or "I mean it".
//! - "basically" and "literally" always.

/// Words that are fillers wherever they appear.
const ALWAYS: &[&str] = &[
    "um",
    "umm",
    "ummm",
    "uhm",
    "uh",
    "uhh",
    "uhhh",
    "er",
    "erm",
    "ah",
    "ahh",
    "hmm",
    "hm",
    "mm",
    "mmm",
    "basically",
    "literally",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FillerStats {
    /// Words in the transcript, fillers included.
    pub words: usize,
    /// Filler words (a two-word filler like "you know" counts once).
    pub fillers: usize,
}

/// One whitespace-separated word, lowercased, without surrounding punctuation.
struct Token {
    word: String,
    /// Followed by a comma (or a dash).
    comma_after: bool,
    /// Ends a sentence (., ?, !) or the transcript.
    ends: bool,
}

fn tokens(text: &str) -> Vec<Token> {
    let raw: Vec<&str> = text.split_whitespace().collect();
    let n = raw.len();
    raw.iter()
        .enumerate()
        .filter_map(|(i, w)| {
            let word: String = w
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                .trim_matches('\'')
                .to_lowercase();
            if word.is_empty() {
                return None;
            }
            let tail: String = w
                .chars()
                .rev()
                .take_while(|c| !c.is_alphanumeric())
                .collect();
            Some(Token {
                word,
                comma_after: tail.contains([',', ';', '-', '\u{2014}', '\u{2013}']),
                ends: i + 1 == n || tail.contains(['.', '?', '!']),
            })
        })
        .collect()
}

pub fn count(text: &str) -> FillerStats {
    let t = tokens(text);
    let mut fillers = 0;
    let mut i = 0;
    while i < t.len() {
        let w = t[i].word.as_str();
        let comma_before = i > 0 && t[i - 1].comma_after;
        // How many words the filler at `i` spans (0: not a filler).
        let len = if ALWAYS.contains(&w) || (w == "like" && (t[i].comma_after || comma_before)) {
            1
        } else if let Some(next) = t.get(i + 1)
            && (next.comma_after || next.ends)
            && ((w == "you" && next.word == "know") || (w == "i" && next.word == "mean"))
        {
            2
        } else {
            0
        };
        if len > 0 {
            fillers += 1;
        }
        i += len.max(1);
    }
    FillerStats {
        words: t.len(),
        fillers,
    }
}

impl FillerStats {
    /// The line shown after a dictation: "3 filler words".
    pub fn summary(&self) -> String {
        plural(self.fillers, "filler word")
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_sounds() {
        let s = count("Um, so I was, uh, thinking. Umm, uhh, hmm.");
        assert_eq!(s.words, 9);
        assert_eq!(s.fillers, 5);
    }

    #[test]
    fn like_only_next_to_a_comma() {
        assert_eq!(count("I like it a lot.").fillers, 0);
        assert_eq!(count("It was, like, huge.").fillers, 1);
        assert_eq!(count("Like, why would you do that?").fillers, 1);
        assert_eq!(count("It looks like rain, like, soon").fillers, 1);
    }

    #[test]
    fn you_know_and_i_mean_only_as_filler() {
        assert_eq!(count("Do you know where it is?").fillers, 0);
        assert_eq!(count("I mean it this time.").fillers, 0);
        let s = count("It's, you know, fine. I mean, sure. It's good, you know.");
        assert_eq!(s.fillers, 3);
        assert_eq!(s.words, 11);
    }

    #[test]
    fn always_fillers_and_punctuation() {
        let s = count("Basically it's \"literally\" done — right?");
        assert_eq!(s.fillers, 2);
        assert_eq!(s.words, 5, "a lone dash isn't a word");
        // Not a match inside other words.
        assert_eq!(count("Umbrella, uhaul, hmmmm").fillers, 0);
    }

    #[test]
    fn summary_lines() {
        assert_eq!(count("").summary(), "0 filler words");
        assert_eq!(count("Um hello").summary(), "1 filler word");
        assert_eq!(
            count("Um, so, uh, it was, like, um, fine.").summary(),
            "4 filler words"
        );
    }
}
