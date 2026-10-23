//! How well the dictionary's words come out: for each term (and each replacement's `to`),
//! its occurrences in the references against those in the transcripts, matched exactly
//! (case-sensitive, whole words). WER can't show this: it lowercases everything, so "Maine"
//! for "main" is no error there. This is what biasing the engine toward the terms will be
//! tuned against.

use agt_core::{Dictionary, dictionary::is_word_char};

/// One term's counts over a set of clips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermCount {
    pub term: String,
    /// In the reference and the transcript (as often as both have it).
    pub hits: usize,
    /// In the reference more often than in the transcript.
    pub misses: usize,
    /// In the transcript more often than in the reference.
    pub false_insertions: usize,
}

impl TermCount {
    /// Occurrences in the references.
    pub fn expected(&self) -> usize {
        self.hits + self.misses
    }
}

/// The terms to count for `dictionary`: its terms (global, then the profiles'), then what
/// each replacement types, in file order, without duplicates.
pub fn of(dictionary: &Dictionary) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let all = dictionary.all_terms();
    let inserted = dictionary.replacements().iter().map(|r| r.inserted());
    for t in all.iter().map(String::as_str).chain(inserted) {
        if !terms.iter().any(|x| x == t) {
            terms.push(t.to_owned());
        }
    }
    terms
}

/// Counts per term, clip by clip.
#[derive(Debug, Clone, Default)]
pub struct Terms(Vec<TermCount>);

impl Terms {
    pub fn new(terms: Vec<String>) -> Self {
        Self(
            terms
                .into_iter()
                .map(|term| TermCount {
                    term,
                    ..Default::default()
                })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Adds one clip.
    pub fn add(&mut self, reference: &str, hypothesis: &str) {
        let (reference, hypothesis) = (spaced(reference), spaced(hypothesis));
        for c in &mut self.0 {
            let term = spaced(&c.term);
            let r = occurrences(&term, &reference);
            let h = occurrences(&term, &hypothesis);
            let hits = r.min(h);
            c.hits += hits;
            c.misses += r - hits;
            c.false_insertions += h - hits;
        }
    }

    pub fn counts(&self) -> &[TermCount] {
        &self.0
    }

    /// All terms together.
    pub fn total(&self) -> TermCount {
        self.0.iter().fold(
            TermCount {
                term: "all".into(),
                ..Default::default()
            },
            |mut t, c| {
                t.hits += c.hits;
                t.misses += c.misses;
                t.false_insertions += c.false_insertions;
                t
            },
        )
    }
}

/// Whitespace runs as single spaces, so a term's spacing never matters.
fn spaced(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Non-overlapping occurrences of `term` in `text` (both [`spaced`]), case-sensitive. A term
/// that starts (ends) with a word character doesn't match after (before) one: `main` is
/// not in `domain` or `mains`.
pub fn occurrences(term: &str, text: &str) -> usize {
    if term.is_empty() {
        return 0;
    }
    let starts_word = term.chars().next().is_some_and(is_word_char);
    let ends_word = term.chars().next_back().is_some_and(is_word_char);
    let (mut n, mut from) = (0, 0);
    while let Some(i) = text[from..].find(term).map(|i| from + i) {
        let end = i + term.len();
        let before = text[..i].chars().next_back().is_some_and(is_word_char);
        let after = text[end..].chars().next().is_some_and(is_word_char);
        if (starts_word && before) || (ends_word && after) {
            // Not here; one character on, it may still start (`aa` in `aaa aa`).
            from = i + text[i..].chars().next().map_or(1, char::len_utf8);
        } else {
            n += 1;
            from = end;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_words_case_sensitive() {
        assert_eq!(occurrences("main", "merge main into main."), 2);
        assert_eq!(occurrences("main", "Main domain mains main's"), 0);
        assert_eq!(occurrences("AGT", "agt AGT (AGT)"), 2);
        assert_eq!(occurrences("aa", "aaa aa"), 1);
        assert_eq!(occurrences(".env", "the .env file, x.env"), 2);
        assert_eq!(occurrences("dot env", "a dot env b"), 1);
        assert_eq!(occurrences("", "anything"), 0);
    }

    #[test]
    fn hits_misses_and_false_insertions() {
        let mut t = Terms::new(vec!["main".into(), "kubectl".into(), "AGT".into()]);
        // Right, wrong case, and an extra one.
        t.add("push to main", "push to main");
        t.add("merge into main", "merge into Maine");
        t.add("hello", "hello main");
        // Spacing doesn't matter.
        t.add("run  kubectl\nget", "run kubectl get");
        let by_term: Vec<_> = t
            .counts()
            .iter()
            .map(|c| (c.term.as_str(), c.hits, c.misses, c.false_insertions))
            .collect();
        assert_eq!(
            by_term,
            [("main", 1, 1, 1), ("kubectl", 1, 0, 0), ("AGT", 0, 0, 0)]
        );
        let all = t.total();
        assert_eq!((all.hits, all.misses, all.false_insertions), (2, 1, 1));
        assert_eq!(all.expected(), 3);
    }

    #[test]
    fn terms_of_a_dictionary() {
        let d = Dictionary::parse(
            "terms = [\"main\", \".env\"]\n[profiles.code]\nterms = [\"rebase\", \"main\"]\n[[replace]]\nfrom = \"dot env\"\nto = \".env\"\n[[replace]]\nfrom = \"maine\"\nto = \"Main\"\n",
        )
        .unwrap();
        // Terms first (the profiles' too); a `to` that is already a term isn't counted twice.
        assert_eq!(of(&d), ["main", ".env", "rebase", "Main"]);
    }
}
