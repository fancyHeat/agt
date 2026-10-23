//! Echo: on speakers, the microphone also hears the other side, so their words would show up
//! twice, once as "Them" and again as "Me". There is no acoustic echo canceller; a Me line is
//! dropped when its words are (mostly) the Them words said around the same time.
//!
//! The comparison is by words, not letters: lowercase alphanumerics, apostrophes kept inside
//! words (`don't`, curly or straight), everything else a separator. Two passes of the engine
//! over the same sound (one through the speakers and the room) rarely agree on punctuation or
//! every little word, so a majority of the words is enough. Each Them word vouches for one Me word
//! only: a Me line repeating a word the other side said once is not an echo.

use std::collections::HashMap;

/// At least this share (percent) of a Me line's words must be in the Them words.
const ECHO_PERCENT: usize = 60;

/// Whether `me` (a Me line) is an echo of `them_nearby` (the Them lines around it, ±3 s).
/// Never when the Them track was quiet over the line (`them_playing` false): the user was the
/// only one talking. Otherwise an echo when at least 60% of its words are in the Them words,
/// each Them word counted once (so a one-word line when that word is).
pub fn is_echo(me: &str, them_nearby: &[&str], them_playing: bool) -> bool {
    if !them_playing {
        return false;
    }
    let me = words(me);
    if me.is_empty() {
        return false;
    }
    let mut them: HashMap<String, usize> = HashMap::new();
    for word in them_nearby.iter().flat_map(|t| words(t)) {
        *them.entry(word).or_default() += 1;
    }
    let found = me
        .iter()
        .filter(|w| match them.get_mut(*w) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        })
        .count();
    found * 100 >= me.len() * ECHO_PERCENT
}

/// `So, we don’t!` → `["so", "we", "don't"]`.
fn words(text: &str) -> Vec<String> {
    const APOSTROPHES: [char; 2] = ['\'', '\u{2019}'];
    text.split(|c: char| !c.is_alphanumeric() && !APOSTROPHES.contains(&c))
        .map(|w| w.trim_matches(APOSTROPHES))
        .filter(|w| !w.is_empty())
        .map(|w| w.replace('\u{2019}', "'").to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_exact_copy() {
        assert!(is_echo(
            "so the plan is to ship friday",
            &["okay so the plan is to ship on friday guys"],
            true
        ));
    }

    #[test]
    fn not_echo_when_them_silent() {
        assert!(!is_echo(
            "so the plan is to ship friday",
            &["okay so the plan is to ship on friday guys"],
            false
        ));
    }

    #[test]
    fn not_echo_different_words() {
        assert!(!is_echo(
            "can we move it to monday instead",
            &["okay so the plan is to ship on friday guys"],
            true
        ));
        // Some shared words aren't enough: 2 of 5 here.
        assert!(!is_echo(
            "the plan sounds risky though",
            &["okay so the plan is to ship on friday"],
            true
        ));
    }

    #[test]
    fn one_word_echo() {
        assert!(is_echo("Right.", &["right, and then"], true));
        assert!(!is_echo("Right.", &["left, and then"], true));
    }

    #[test]
    fn case_and_punctuation_ignored() {
        assert!(is_echo(
            "So, the PLAN is: ship it Friday!",
            &["okay", "so the plan is", "to ship it on friday."],
            true
        ));
        // Apostrophes stay inside words, curly or straight.
        assert!(is_echo("We don’t know.", &["we don't know yet"], true));
        assert!(!is_echo("dont know", &["don't know"], true));
    }

    #[test]
    fn them_words_count_once() {
        // "yes" said three times by Me, once by Them: 1 of 3.
        assert!(!is_echo("yes yes yes", &["yes"], true));
        assert!(is_echo("yes yes yes", &["yes yes", "and yes"], true));
    }

    #[test]
    fn empty_me_is_not_echo() {
        assert!(!is_echo("", &["anything at all"], true));
        assert!(!is_echo(" ... ", &["anything at all"], true));
        assert!(!is_echo("hello there", &[], true));
    }
}
