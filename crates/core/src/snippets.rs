//! Snippets (`[snippets]` in config.toml): a phrase you say that AGT types as something else,
//! e.g. a word the speech model mishears ("Reddit True Tide" for "retatrutide") or an email
//! address. Applied to the text about to be pasted, after polish ([`Snippets::apply`]).
//!
//! The phrase is matched as whole words, ignoring capitals and what sits between its words
//! (spaces, hyphens, commas, apostrophes, or nothing: "true-tide" and "truetide" match
//! "true tide"), since the speech model and polish don't always write a phrase the same way.

/// The snippets of a `[snippets] list`, longest phrase first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snippets {
    list: Vec<Snippet>,
}

#[derive(Debug, Clone, PartialEq)]
struct Snippet {
    /// The phrase's words, lowercase.
    words: Vec<Vec<char>>,
    /// What's typed in its place.
    text: String,
    /// `text` starts with an all-lowercase word: capitalized where it starts a sentence.
    capitalize: bool,
}

impl Snippets {
    /// One snippet per line, `phrase = text` (see [`entries`]). `\n` in the text types a
    /// new line.
    pub fn parse(list: &str) -> Self {
        let mut list: Vec<Snippet> = entries(list)
            .into_iter()
            .map(|e| {
                let text = e.text.replace("\\n", "\n");
                let first = text.split_whitespace().next().unwrap_or_default();
                let capitalize = first.chars().all(|c| c.is_alphabetic() && c.is_lowercase());
                Snippet {
                    words: words(&e.phrase),
                    text,
                    capitalize,
                }
            })
            .collect();
        // "Reddit True Tide" before "true tide"; equal lengths keep their order.
        list.sort_by_key(|s| std::cmp::Reverse(s.words.iter().map(Vec::len).sum::<usize>()));
        Self { list }
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// `text` with every phrase replaced, scanning left to right (what was typed in place of
    /// one isn't scanned again), and how many were.
    pub fn apply(&self, text: &str) -> (String, usize) {
        if self.list.is_empty() {
            return (text.to_owned(), 0);
        }
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut replaced = 0;
        let mut i = 0;
        while let Some(c) = text[i..].chars().next() {
            if c.is_alphanumeric() && word_starts(text, i) {
                let found = self
                    .list
                    .iter()
                    .find_map(|s| s.match_at(text, i).map(|end| (s, end)));
                if let Some((s, end)) = found {
                    out.push_str(&text[copied..i]);
                    let first_upper = c.is_uppercase();
                    match s.text.chars().next() {
                        Some(f) if s.capitalize && first_upper && sentence_starts(text, i) => {
                            out.extend(f.to_uppercase());
                            out.push_str(&s.text[f.len_utf8()..]);
                        }
                        _ => out.push_str(&s.text),
                    }
                    replaced += 1;
                    copied = end;
                    i = end;
                    continue;
                }
            }
            i += c.len_utf8();
        }
        out.push_str(&text[copied..]);
        (out, replaced)
    }
}

impl Snippet {
    /// Where the phrase ends if it's at `start` (where a word starts) in `text`.
    fn match_at(&self, text: &str, start: usize) -> Option<usize> {
        let mut chars = text[start..].char_indices().peekable();
        for (n, word) in self.words.iter().enumerate() {
            if n > 0 {
                while chars.next_if(|&(_, c)| between_words(c)).is_some() {}
            }
            let mut want = word.iter();
            let mut left = word.len();
            while left > 0 {
                let (_, c) = chars.next()?;
                if !c.is_alphanumeric() {
                    return None;
                }
                for lower in c.to_lowercase() {
                    if want.next() != Some(&lower) {
                        return None;
                    }
                    left -= 1;
                }
            }
        }
        let end = chars.peek().map_or(text.len(), |&(i, _)| start + i);
        // Whole words only: "tide" isn't in "tidewater". An apostrophe may follow
        // ("retatrutide's").
        let next = text[end..].chars().next();
        (!next.is_some_and(char::is_alphanumeric)).then_some(end)
    }
}

/// A line of a `[snippets] list` that holds a snippet, as written there.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Which line of the list (0-based).
    pub line: usize,
    /// What you say.
    pub phrase: String,
    /// What's typed instead (`\n` still written out).
    pub text: String,
}

/// The snippets of a `[snippets] list`, in its order. Lines starting with `#`, without `=`,
/// or with nothing on either side hold none.
pub fn entries(list: &str) -> Vec<Entry> {
    list.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with('#'))
        .filter_map(|(line, l)| {
            let (phrase, text) = l.split_once('=')?;
            let (phrase, text) = (phrase.trim(), text.trim());
            (!words(phrase).is_empty() && !text.is_empty()).then(|| Entry {
                line,
                phrase: phrase.into(),
                text: text.into(),
            })
        })
        .collect()
}

/// `list` with the snippet `phrase = text`: on the line of a snippet with the same words if
/// there is one ("reddit true-tide" is "Reddit True Tide"), else on a new last line.
pub fn add(list: &str, phrase: &str, text: &str) -> String {
    let line = format!("{} = {}", phrase.trim(), text.trim());
    let same = entries(list)
        .into_iter()
        .find(|e| words(&e.phrase) == words(phrase));
    let mut lines: Vec<&str> = list.lines().collect();
    match same {
        Some(e) => lines[e.line] = &line,
        None => lines.push(&line),
    }
    joined(&lines)
}

/// `list` with the snippet on line `entry.line` typing `text` instead, or without that line if
/// `text` is empty.
pub fn set_text(list: &str, entry: &Entry, text: &str) -> String {
    let line = format!("{} = {}", entry.phrase, text.trim());
    let mut lines: Vec<&str> = list.lines().collect();
    if entry.line < lines.len() {
        if text.trim().is_empty() {
            lines.remove(entry.line);
        } else {
            lines[entry.line] = &line;
        }
    }
    joined(&lines)
}

/// One line each, each ending with a new line.
fn joined(lines: &[&str]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// The words of a phrase, lowercase: its runs of letters and digits.
fn words(phrase: &str) -> Vec<Vec<char>> {
    phrase
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.chars().flat_map(char::to_lowercase).collect())
        .collect()
}

/// What may sit between a phrase's words in the text (besides nothing).
fn between_words(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '‐' | '–' | '—' | ',' | '\'' | '’')
}

/// Whether a word starts at `i` (a letter or digit): not inside one, and not its ending
/// after an apostrophe ("t" in "don't").
fn word_starts(text: &str, i: usize) -> bool {
    let mut before = text[..i].chars().rev();
    match before.next() {
        None => true,
        Some(c) if c.is_alphanumeric() => false,
        Some('\'' | '’') => !before.next().is_some_and(char::is_alphanumeric),
        Some(_) => true,
    }
}

/// Whether `i` starts a sentence: the text's start, a new line, or after `.`, `!` or `?`
/// (an opening quote or bracket in between).
fn sentence_starts(text: &str, i: usize) -> bool {
    let before = text[..i].trim_end_matches(|c: char| {
        (c.is_whitespace() && c != '\n') || matches!(c, '"' | '“' | '\'' | '‘' | '(' | '[')
    });
    before.is_empty() || before.ends_with(['.', '!', '?', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(list: &str, text: &str) -> String {
        Snippets::parse(list).apply(text).0
    }

    #[test]
    fn replaces_a_phrase_however_it_was_written() {
        let list = "Reddit True Tide = retatrutide";
        for said in [
            "Reddit True Tide",
            "reddit true tide",
            "Reddit true-tide",
            "Reddit truetide",
            "reddit, true tide",
            "Reddit  True\tTide",
        ] {
            assert_eq!(
                apply(list, &format!("I take {said} weekly.")),
                "I take retatrutide weekly.",
                "{said}"
            );
        }
    }

    #[test]
    fn whole_words_only() {
        let list = "tide = TIDE";
        assert_eq!(apply(list, "tidewater riptide"), "tidewater riptide");
        assert_eq!(apply(list, "the tide's in"), "the TIDE's in");
        assert_eq!(apply(list, "(tide)"), "(TIDE)");
        assert_eq!(apply("t = T", "don't stop"), "don't stop");
        assert_eq!(apply("true tide = x", "true tidewater"), "true tidewater");
    }

    #[test]
    fn capitalized_where_the_transcript_starts_a_sentence_with_a_capital() {
        let list = "Reddit True Tide = retatrutide";
        assert_eq!(
            apply(
                list,
                "Reddit True Tide works. Reddit true tide too? \"Reddit true tide\""
            ),
            "Retatrutide works. Retatrutide too? \"Retatrutide\""
        );
        assert_eq!(apply(list, "On Reddit True Tide"), "On retatrutide");
        assert_eq!(apply(list, "reddit true tide works"), "retatrutide works");
        assert_eq!(apply(list, "Hi.\nReddit True Tide"), "Hi.\nRetatrutide");
        // Typed as written when it doesn't start with a lowercase word.
        assert_eq!(apply("eye phone = iPhone", "Eye phone."), "iPhone.");
        assert_eq!(
            apply("my email = grant@example.com", "My email."),
            "grant@example.com."
        );
    }

    #[test]
    fn longest_phrase_wins_and_output_is_not_rescanned() {
        let list = "true tide = X\nReddit True Tide = retatrutide";
        assert_eq!(apply(list, "reddit true tide, true tide"), "retatrutide, X");
        let list = "Overdrive = Overdrive Automations";
        let (out, n) = Snippets::parse(list).apply("Overdrive and overdrive");
        assert_eq!(out, "Overdrive Automations and Overdrive Automations");
        assert_eq!(n, 2);
    }

    #[test]
    fn parses_lines() {
        let s = Snippets::parse(
            "# comment\n\n  tirz = tirzepatide  \r\nno equals sign\n = nothing said\nempty =\n\
             sig = Thanks,\\nGrant\nmath = a = b\n",
        );
        assert_eq!(s.len(), 3);
        assert_eq!(apply("sig = Thanks,\\nGrant", "sig"), "Thanks,\nGrant");
        assert_eq!(apply("math = a = b", "math"), "a = b");
        assert!(Snippets::parse("").is_empty());
        assert_eq!(Snippets::parse("").apply("text"), ("text".into(), 0));
    }

    #[test]
    fn entries_add_and_set_text_keep_the_other_lines() {
        let list = "# mine\nReddit True Tide = retatrutide\nnot a snippet\ntirz = tirzepatide";
        let e = entries(list);
        let phrases: Vec<_> = e.iter().map(|e| (e.line, e.phrase.as_str())).collect();
        assert_eq!(phrases, [(1, "Reddit True Tide"), (3, "tirz")]);
        // Added last; the same words again change that line instead.
        let added = add(list, " sig ", " Thanks,\\nGrant ");
        assert_eq!(
            added,
            "# mine\nReddit True Tide = retatrutide\nnot a snippet\ntirz = tirzepatide\nsig = Thanks,\\nGrant\n"
        );
        assert_eq!(
            add(list, "reddit true-tide", "Retatrutide"),
            "# mine\nreddit true-tide = Retatrutide\nnot a snippet\ntirz = tirzepatide\n"
        );
        assert_eq!(add("", "a b", "c"), "a b = c\n");
        // Changed in place, or removed when emptied.
        assert_eq!(
            set_text(list, &e[1], "tirzepatide (Zepbound)"),
            "# mine\nReddit True Tide = retatrutide\nnot a snippet\ntirz = tirzepatide (Zepbound)\n"
        );
        assert_eq!(
            set_text(list, &e[0], " "),
            "# mine\nnot a snippet\ntirz = tirzepatide\n"
        );
    }

    #[test]
    fn non_ascii() {
        assert_eq!(apply("café olé = X", "Le CAFÉ-OLÉ."), "Le X.");
        assert_eq!(apply("über = uber", "Über."), "Uber.");
    }
}
