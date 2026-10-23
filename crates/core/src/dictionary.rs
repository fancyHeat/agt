//! `dictionary.toml`: words and names the user says, and fixed rewrites of what the speech
//! model writes for them. A file of its own, next to `config.toml`: it grows, and later
//! versions will add learned entries to it, while `config.toml` stays hand-edited.
//!
//! ```toml
//! terms = ["AGT"]
//!
//! [profiles.code]
//! apps = ["Code.exe", "WindowsTerminal.exe", "com.microsoft.VSCode"]
//! terms = ["main", "rebase", "kubectl"]
//!
//! [[replace]]
//! from = "dot env"
//! to = ".env"
//! profiles = ["code"]
//! ```
//!
//! - `terms`: words and names for the speech model to expect, everywhere, in file order
//!   ([`Dictionary::terms`]). Parakeet favors them (`stt.boost`, see `agt_stt::boost`);
//!   `agt-bench` counts how often each one is transcribed right. Blank entries and exact
//!   duplicates are dropped.
//! - `[profiles.<name>]`: terms that only apply while one of `apps` has focus: the Windows
//!   program's file name (`Code.exe`) or the macOS app's bundle identifier
//!   (`com.microsoft.VSCode`), compared ignoring case. An app in several profiles gets all of
//!   them. At most [`MAX_PROFILES`].
//! - `[[replace]]`: rewrites of the text about to be pasted, made after polish (or on the
//!   transcript when polish is off or fails), so polish never rewrites them; with `profiles`
//!   only in those profiles' apps ([`Scope::replace`]). A `profiles` entry no
//!   `[profiles.<name>]` defines is an error, and so is an empty list.
//!
//! Each dictation resolves the dictionary once, at key-down, for the app that has focus then:
//! a [`Scope`], whose terms go to the engine as [`crate::stt::TranscribeOpts::vocabulary`].
//! Each profile's term list (the global terms, then its own) is built at load, so resolving
//! is a lookup and an `Arc` clone (a merge only for an app in several profiles).
//!
//! A missing file is an empty dictionary, which changes nothing. Unknown keys are rejected so
//! typos surface (as in `config.toml`), and errors say where (line and column) without
//! quoting the file.
//!
//! # Replacements
//!
//! - `from` matches whole words and phrases, ignoring case. Word characters are Unicode
//!   letters and digits and the apostrophes `'` and `’`; anything else is a boundary, so
//!   `maine` matches in `Maine.` and `(MAINE)` but not in `Maines` or `Maine's`.
//! - Between two words of `from`, a run of whitespace, hyphens, dashes and commas matches
//!   any such run or none at all, since the speech model and polish don't always write a
//!   phrase the same way: `true tide` (or `true-tide`) matches `True Tide`, `true-tide`,
//!   `true, tide` and `truetide`. Whitespace elsewhere (next to punctuation) matches any run
//!   of whitespace; other characters match themselves (`from = "node js"` doesn't match
//!   `node.js`).
//! - The leftmost match wins, and of those starting at the same place the longest. It's one
//!   pass: replaced text is never matched again (no chains like a → b → c).
//! - `to` is inserted as written, with no case carried over from what it replaced (`Maine`,
//!   `maine` and `MAINE` all become `to`), except that `\n` in it (a backslash and an `n`)
//!   is a new line, and a `to` whose first word is all lowercase letters gets a capital
//!   where what it replaces starts a sentence with one (at the start, on a new line, or after
//!   `.`, `!` or `?`): `Maine is` there becomes `Main is`. That a rewrite applies whatever the
//!   case is also why a rewrite of a real word applies everywhere, including where the word
//!   was meant.
//! - A blank `from` or `to` is an error, and so is a `from` given twice (ignoring case and
//!   what's between its words), even for different profiles: in an app in both, which one
//!   applied would depend on their order.
//!
//! Edge cases, by design (speech engines write NFC text, where they hardly come up):
//!
//! - Only a word character at an end of `from` needs a boundary there: punctuation at either
//!   end matches with none (`.net` matches in `asp.net`; `e.g.` turns `e.g.x` into `…x`).
//! - Apostrophes are word characters, so a word in single quotes (`'Maine'`, or `‘Maine’`,
//!   whose closing `’` is one too) is part of a longer word and not matched.
//! - Case is compared by lowercasing each character, not by full case folding: `straße`
//!   doesn't match `STRASSE`, and a final `ς` doesn't match `Σ` (which lowercases to `σ`).
//! - Combining marks aren't word characters: a decomposed `café` (`e` + U+0301) is two words
//!   to the matcher, and only matches a `from` written the same way.
//!
//! Rules are compiled at load into lowercased pieces indexed by their first character, so
//! applying them is one scan of the text, and no allocation at all when nothing matches.

use crate::config::{line_column, located, redact_values, write_whole};
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fs, io,
    path::Path,
    sync::Arc,
};

/// Written by "Edit dictionary..." when there is no file yet. Kept as text so the comments
/// explain the format; it parses to an empty dictionary.
pub const TEMPLATE: &str = r#"# AGT dictionary. AGT applies your changes as soon as you save this file. If it has an
# error, AGT says so in its menu and keeps using the last version that worked since it
# started (none, if the file already had the error when AGT started) until it's fixed.

# Words and names you use, for the speech model to favor (Parakeet; how much: `boost` in
# config.toml's [stt]). Favored everywhere; for words that only make sense in some apps, use
# a profile (below). For example: terms = ["AGT", "Kubernetes"]
terms = []

# Terms favored only while one of `apps` has focus: on Windows the program's file name, on
# macOS the app's bundle identifier, ignoring case. AGT records the app of each dictation
# (agt.db, `dictations.app_id`; see the README). An app can be in several profiles. Remove
# the # signs to use this one:
#
# [profiles.code]
# apps = [
#     # Windows: VS Code (+ Insiders), Cursor, Zed, Visual Studio, Windows Terminal, consoles,
#     # JetBrains IDEs, Android Studio
#     "Code.exe", "Code - Insiders.exe", "Cursor.exe", "Zed.exe", "devenv.exe",
#     "WindowsTerminal.exe",
#     "powershell.exe", "pwsh.exe", "cmd.exe", "idea64.exe", "pycharm64.exe",
#     "webstorm64.exe", "clion64.exe", "goland64.exe", "rider64.exe", "rustrover64.exe",
#     "phpstorm64.exe", "studio64.exe",
#     # macOS: VS Code (+ Insiders), Cursor, Zed, Terminal, iTerm2, Ghostty, Warp, JetBrains IDEs
#     # (Community editions too), Android Studio
#     "com.microsoft.VSCode", "com.microsoft.VSCodeInsiders", "com.todesktop.230313mzl4w4u92",
#     "dev.zed.Zed",
#     "com.apple.Terminal", "com.googlecode.iterm2", "com.mitchellh.ghostty",
#     "dev.warp.Warp-Stable", "com.jetbrains.intellij", "com.jetbrains.pycharm",
#     "com.jetbrains.WebStorm", "com.jetbrains.CLion", "com.jetbrains.goland",
#     "com.jetbrains.rider", "com.jetbrains.rustrover", "com.jetbrains.PhpStorm",
#     "com.jetbrains.intellij.ce", "com.jetbrains.pycharm.ce", "com.google.android.studio",
# ]
# terms = ["main", "rebase", "kubectl", "PR"]

# Fixed rewrites of what AGT pastes, made after the AI clean-up. Only for things that are
# never ambiguous: a rewrite of a real word (e.g. "Maine") applies everywhere, unless you
# limit it to profiles. `from` matches whole words, ignoring case and what's between them (a
# space, a hyphen, a comma or nothing: "dot env" also matches "dot-env" and "dotenv"). `to`
# is pasted as written, except that \n in it starts a new line, and a lowercase word starts
# with a capital where it starts a sentence. Settings... > Replacements adds and changes
# them too. Copy these lines for each (`profiles` is optional):
#
# [[replace]]
# from = "dot env"
# to = ".env"
# profiles = ["code"]
"#;

/// Profiles a dictionary may have (they're a bit mask).
pub const MAX_PROFILES: usize = 64;

/// One `[[replace]]` entry, as written (trimmed, though `to` keeps its new lines).
#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub from: String,
    pub to: String,
    /// The profiles it's limited to ([`Dictionary::profiles`] indexes, as a bit mask); 0:
    /// everywhere.
    profiles: u64,
    pieces: Vec<Piece>,
    /// What's inserted: `to` with each `\n` a new line.
    insert: String,
    /// `to` starts with an all-lowercase word: capitalized where it starts a sentence.
    capitalize: bool,
}

impl Replacement {
    /// What it types: `to` with each `\n` a new line (and maybe a capital, see the module
    /// docs).
    pub fn inserted(&self) -> &str {
        &self.insert
    }
}

/// A `[profiles.<name>]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    /// As written (trimmed, without blanks); matched ignoring case.
    pub apps: Vec<String>,
    /// Its own terms, in file order.
    pub terms: Vec<String>,
    /// The global terms, then its own (without duplicates): what an app in this profile
    /// alone boosts.
    all_terms: Arc<[String]>,
}

/// A compiled `from`: what the text must have, in order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Piece {
    /// A whole word (lowercased); just its letters if a [`Piece::Gap`] follows.
    Word(String),
    /// Between two words: any run of [`separates_words`] characters, or none.
    Gap,
    /// Any run of whitespace.
    Space,
    /// One character that isn't a word character (lowercased).
    Char(char),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dictionary {
    /// The global terms.
    terms: Arc<[String]>,
    /// By name.
    profiles: Vec<Profile>,
    /// App (lowercased) → the profiles it's in, as a bit mask of `profiles` indexes.
    apps: HashMap<String, u64>,
    replacements: Vec<Replacement>,
    /// Indexes of `replacements` by the first character (lowercased) of their match.
    index: HashMap<char, Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DictionaryError {
    Io(String),
    /// Where (1-based line and column), and toml's message with any quoted value removed:
    /// errors are recorded in agt.db, which never holds dictionary text.
    Parse {
        message: String,
        at: Option<(usize, usize)>,
    },
}

impl std::fmt::Display for DictionaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read dictionary.toml: {e}"),
            Self::Parse { message, at } => {
                f.write_str(&located("invalid dictionary.toml", message, *at))
            }
        }
    }
}

impl std::error::Error for DictionaryError {}

/// The file as written.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct File {
    terms: Vec<String>,
    profiles: BTreeMap<toml::Spanned<String>, ProfileEntry>,
    replace: Vec<Entry>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfileEntry {
    apps: Vec<String>,
    terms: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    from: toml::Spanned<String>,
    to: toml::Spanned<String>,
    profiles: Option<toml::Spanned<Vec<toml::Spanned<String>>>>,
}

/// The file's own keys: kept in error messages, which say what was expected.
const KEYS: &[&str] = &["terms", "profiles", "apps", "replace", "from", "to"];

/// `list` trimmed, without blanks and exact duplicates, in order.
fn cleaned(list: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in list {
        let item = item.trim();
        if !item.is_empty() && !out.iter().any(|t| t == item) {
            out.push(item.to_owned());
        }
    }
    out
}

/// `lists` joined in order, without duplicates.
fn merged<'a>(lists: impl IntoIterator<Item = &'a [String]>) -> Arc<[String]> {
    let mut out: Vec<String> = Vec::new();
    for t in lists.into_iter().flatten() {
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    out.into()
}

/// toml's message without anything the user wrote: quoted values (as for `config.toml`) and
/// also names in backticks, which in this file are the user's words (``unknown field
/// `maine` `` for `maine = "main"`). The file's own [`KEYS`] and punctuation (``expected
/// `"` ``) stay.
fn redact(message: &str) -> String {
    let message = redact_values(message);
    let parts: Vec<&str> = message.split('`').collect();
    let mut out = String::with_capacity(message.len());
    for (i, part) in parts.iter().enumerate() {
        let quoted = i % 2 == 1;
        if !quoted {
            out.push_str(part);
        } else if i == parts.len() - 1 {
            // A lone backtick: what follows isn't quoted.
            out.push('`');
            out.push_str(part);
        } else if KEYS.contains(part) || !part.chars().any(char::is_alphanumeric) {
            out.push('`');
            out.push_str(part);
            out.push('`');
        } else {
            out.push_str("(redacted)");
        }
    }
    out
}

/// Word characters (see the module docs); everything else is a boundary.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '’'
}

/// The lowercase form of `c` used to compare and index (its first char, for the few that
/// lowercase to more than one).
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// What may sit between two words of a phrase (besides nothing): see [`Piece::Gap`].
fn separates_words(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '‐' | '–' | '—' | ',')
}

/// `from` (trimmed, not blank) as pieces.
fn compile(from: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut rest = from;
    while let Some(c) = rest.chars().next() {
        let gap = rest.find(|c| !separates_words(c)).unwrap_or(rest.len());
        let len = if gap > 0
            && matches!(pieces.last(), Some(Piece::Word(_)))
            && rest[gap..].starts_with(is_word_char)
        {
            pieces.push(Piece::Gap);
            gap
        } else if c.is_whitespace() {
            pieces.push(Piece::Space);
            rest.find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len())
        } else if is_word_char(c) {
            let len = rest.find(|c| !is_word_char(c)).unwrap_or(rest.len());
            let word = rest[..len].chars().flat_map(char::to_lowercase).collect();
            pieces.push(Piece::Word(word));
            len
        } else {
            pieces.push(Piece::Char(lower(c)));
            c.len_utf8()
        };
        rest = &rest[len..];
    }
    pieces
}

/// Where `pieces` match `text` from byte `at` (a word's start or a non-word character): the
/// end of the match.
fn match_at(pieces: &[Piece], text: &str, at: usize) -> Option<usize> {
    let mut pos = at;
    for (k, piece) in pieces.iter().enumerate() {
        let rest = &text[pos..];
        pos += match piece {
            // The next word may follow right away ("truetide"): only this one's letters.
            Piece::Word(w) if pieces.get(k + 1) == Some(&Piece::Gap) => word_prefix(rest, w)?,
            // A whole word: what follows a word or a non-word character starts a new one.
            Piece::Word(w) => {
                let len = rest.find(|c| !is_word_char(c)).unwrap_or(rest.len());
                let same = rest[..len]
                    .chars()
                    .flat_map(char::to_lowercase)
                    .eq(w.chars());
                if len == 0 || !same {
                    return None;
                }
                len
            }
            Piece::Gap => rest.find(|c| !separates_words(c)).unwrap_or(rest.len()),
            Piece::Space => match rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len())
            {
                0 => return None,
                len => len,
            },
            Piece::Char(c) => match rest.chars().next() {
                Some(t) if lower(t) == *c && !is_word_char(t) => t.len_utf8(),
                _ => return None,
            },
        };
    }
    Some(pos)
}

/// The length of `w` (a lowercased word) at the start of `text`, made of word characters
/// whatever comes after them.
fn word_prefix(text: &str, w: &str) -> Option<usize> {
    let mut want = w.chars().peekable();
    let mut len = 0;
    for c in text.chars() {
        if want.peek().is_none() {
            break;
        }
        if !is_word_char(c) {
            return None;
        }
        for l in c.to_lowercase() {
            if want.next() != Some(l) {
                return None;
            }
        }
        len += c.len_utf8();
    }
    want.peek().is_none().then_some(len)
}

/// Whether `at` in `text` starts a sentence: the text's start, a new line, or after `.`, `!`
/// or `?` (an opening quote or bracket in between).
fn sentence_starts(text: &str, at: usize) -> bool {
    let before = text[..at].trim_end_matches(|c: char| {
        (c.is_whitespace() && c != '\n') || matches!(c, '"' | '“' | '\'' | '‘' | '(' | '[')
    });
    before.is_empty() || before.ends_with(['.', '!', '?', '\n'])
}

/// Puts `r`'s text in `out` in place of what it matched at `at` in `text` (see the module
/// docs for the capital).
fn push_insert(out: &mut String, r: &Replacement, text: &str, at: usize) {
    match r.insert.chars().next() {
        Some(first)
            if r.capitalize
                && text[at..].starts_with(char::is_uppercase)
                && sentence_starts(text, at) =>
        {
            out.extend(first.to_uppercase());
            out.push_str(&r.insert[first.len_utf8()..]);
        }
        _ => out.push_str(&r.insert),
    }
}

impl Dictionary {
    /// Parses the file's text (see the module docs for what's rejected).
    pub fn parse(text: &str) -> Result<Self, DictionaryError> {
        let error = |message: &str, span: Option<std::ops::Range<usize>>| DictionaryError::Parse {
            message: redact(message.trim()),
            at: line_column(span, text),
        };
        let file: File = toml::from_str(text).map_err(|e| error(e.message(), e.span()))?;
        if file.profiles.len() > MAX_PROFILES {
            return Err(error(&format!("more than {MAX_PROFILES} `profiles`"), None));
        }
        // agt.db joins a dictation's profile names with commas.
        if let Some(name) = file.profiles.keys().find(|n| n.get_ref().contains(',')) {
            return Err(error(
                "a profile name can't have a comma in it",
                Some(name.span()),
            ));
        }
        let terms: Arc<[String]> = cleaned(file.terms).into();
        let mut apps: HashMap<String, u64> = HashMap::new();
        let profiles: Vec<Profile> = file
            .profiles
            .into_iter()
            .enumerate()
            .map(|(i, (name, p))| {
                let own = cleaned(p.terms);
                let app_ids = cleaned(p.apps);
                for app in &app_ids {
                    *apps.entry(app.to_lowercase()).or_default() |= 1 << i;
                }
                Profile {
                    name: name.into_inner(),
                    apps: app_ids,
                    all_terms: merged([&terms[..], &own[..]]),
                    terms: own,
                }
            })
            .collect();
        let mut dictionary = Self {
            terms,
            apps,
            ..Self::default()
        };
        let mut seen: HashMap<Vec<Piece>, ()> = HashMap::new();
        for entry in file.replace {
            let mut mask = 0u64;
            if let Some(names) = &entry.profiles {
                if names.get_ref().is_empty() {
                    return Err(error(
                        "`profiles` is empty (leave it out to apply everywhere)",
                        Some(names.span()),
                    ));
                }
                for name in names.get_ref() {
                    match profiles.iter().position(|p| p.name == *name.get_ref()) {
                        Some(i) => mask |= 1 << i,
                        None => {
                            return Err(error(
                                "`profiles` names a profile that no [profiles.<name>] table defines",
                                Some(name.span()),
                            ));
                        }
                    }
                }
            }
            let from = entry.from.get_ref().trim();
            // New lines are what it types (`to = "\n"`): kept.
            let to = entry
                .to
                .get_ref()
                .trim_matches(|c: char| c.is_whitespace() && c != '\n');
            if from.is_empty() {
                return Err(error("`from` is empty", Some(entry.from.span())));
            }
            if to.is_empty() {
                return Err(error(
                    "`to` is empty (a replacement can't delete words)",
                    Some(entry.to.span()),
                ));
            }
            let pieces = compile(from);
            if seen.insert(pieces.clone(), ()).is_some() {
                return Err(error(
                    "this `from` is already replaced above (case and what's between its words don't count)",
                    Some(entry.from.span()),
                ));
            }
            let first = match &pieces[0] {
                Piece::Word(w) => w.chars().next().expect("a word is not empty"),
                Piece::Char(c) => *c,
                Piece::Space => unreachable!("`from` is trimmed"),
                Piece::Gap => unreachable!("a gap follows a word"),
            };
            let i = dictionary.replacements.len();
            dictionary.index.entry(first).or_default().push(i);
            let insert = to.replace("\\n", "\n");
            let capitalize = insert
                .split_whitespace()
                .next()
                .is_some_and(|w| w.chars().all(|c| c.is_alphabetic() && c.is_lowercase()));
            dictionary.replacements.push(Replacement {
                from: from.to_owned(),
                to: to.to_owned(),
                profiles: mask,
                pieces,
                insert,
                capitalize,
            });
        }
        dictionary.profiles = profiles;
        Ok(dictionary)
    }

    /// Loads `path`; a missing file is an empty dictionary.
    pub fn load(path: &Path) -> Result<Self, DictionaryError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(DictionaryError::Io(e.to_string())),
        }
    }

    /// Writes [`TEMPLATE`] to `path` unless something is there already.
    pub fn create_if_missing(path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => io::Write::write_all(&mut f, TEMPLATE.as_bytes()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// No terms and no replacements: nothing changes.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.profiles.iter().all(|p| p.terms.is_empty())
            && self.replacements.is_empty()
    }

    /// The global terms (not the profiles'), in file order.
    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Every term, global and the profiles', in file order, without duplicates.
    pub fn all_terms(&self) -> Arc<[String]> {
        merged(
            [&self.terms[..]]
                .into_iter()
                .chain(self.profiles.iter().map(|p| &p.terms[..])),
        )
    }

    /// The `[profiles.<name>]` tables, by name.
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    /// The `[[replace]]` entries, in file order.
    pub fn replacements(&self) -> &[Replacement] {
        &self.replacements
    }

    /// The names of the profiles `replacement` (one of [`Self::replacements`]) is limited to;
    /// none: it applies everywhere.
    pub fn profiles_of(&self, replacement: &Replacement) -> Vec<&str> {
        self.profiles
            .iter()
            .enumerate()
            .filter(|(i, _)| replacement.profiles & 1 << i != 0)
            .map(|(_, p)| p.name.as_str())
            .collect()
    }

    /// `text` with every replacement that applies everywhere (no `profiles`) applied: what
    /// an app in no profile gets ([`Scope::replace`]).
    pub fn replace<'a>(&self, text: &'a str) -> (Cow<'a, str>, usize) {
        self.replace_in(text, 0)
    }

    /// `text` with the replacements for the profiles in `profiles` (a bit mask) applied
    /// (see the module docs), and how many were made. Borrowed when nothing matched.
    fn replace_in<'a>(&self, text: &'a str, profiles: u64) -> (Cow<'a, str>, usize) {
        if self.replacements.is_empty() {
            return (Cow::Borrowed(text), 0);
        }
        let applies = |r: &Replacement| r.profiles == 0 || r.profiles & profiles != 0;
        let mut out = String::new();
        let (mut copied, mut count, mut i) = (0, 0, 0);
        while let Some(c) = text[i..].chars().next() {
            // A match starts at a word's start or at a non-word character, never inside a word.
            let step = if is_word_char(c) {
                text[i..]
                    .find(|c| !is_word_char(c))
                    .unwrap_or(text.len() - i)
            } else {
                c.len_utf8()
            };
            // The longest match here; of equally long ones, the first in the file.
            let best = self.index.get(&lower(c)).and_then(|candidates| {
                candidates
                    .iter()
                    .filter(|&&r| applies(&self.replacements[r]))
                    .filter_map(|&r| {
                        match_at(&self.replacements[r].pieces, text, i).map(|end| (end, r))
                    })
                    .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
            });
            match best {
                Some((end, r)) => {
                    if count == 0 {
                        out.reserve(text.len() + 16);
                    }
                    out.push_str(&text[copied..i]);
                    push_insert(&mut out, &self.replacements[r], text, i);
                    count += 1;
                    (copied, i) = (end, end);
                }
                None => i += step,
            }
        }
        if count == 0 {
            return (Cow::Borrowed(text), 0);
        }
        out.push_str(&text[copied..]);
        (Cow::Owned(out), count)
    }
}

/// A dictionary as it applies in one app: its terms and replacements there. Resolved once
/// per dictation, at key-down, and carried with it ([`Scope::new`] costs a hash lookup).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    dictionary: Arc<Dictionary>,
    app: Option<String>,
    /// The app's profiles (bit mask of [`Dictionary::profiles`] indexes).
    profiles: u64,
    terms: Arc<[String]>,
}

impl Scope {
    /// `dictionary` in `app` (an exe file name or a bundle identifier; `None`: unknown, so
    /// in no profile).
    pub fn new(dictionary: Arc<Dictionary>, app: Option<String>) -> Self {
        let profiles = app
            .as_deref()
            .and_then(|a| dictionary.apps.get(&a.to_lowercase()))
            .copied()
            .unwrap_or(0);
        let terms = match profiles.count_ones() {
            0 => dictionary.terms.clone(),
            1 => dictionary.profiles[profiles.trailing_zeros() as usize]
                .all_terms
                .clone(),
            _ => merged(
                [&dictionary.terms[..]].into_iter().chain(
                    dictionary
                        .profiles
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| profiles & 1 << i != 0)
                        .map(|(_, p)| &p.terms[..]),
                ),
            ),
        };
        Self {
            dictionary,
            app,
            profiles,
            terms,
        }
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// The focused app at key-down, if known.
    pub fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }

    /// The names of the app's profiles, by name.
    pub fn profile_names(&self) -> impl Iterator<Item = &str> {
        self.dictionary
            .profiles
            .iter()
            .enumerate()
            .filter(|(i, _)| self.profiles & 1 << i != 0)
            .map(|(_, p)| p.name.as_str())
    }

    /// The global terms, then the app's profiles' (in profile order), without duplicates.
    pub fn terms(&self) -> &Arc<[String]> {
        &self.terms
    }

    /// `text` with the replacements that apply here applied (see the module docs), and
    /// how many were made. Borrowed when nothing matched.
    pub fn replace<'a>(&self, text: &'a str) -> (Cow<'a, str>, usize) {
        self.dictionary.replace_in(text, self.profiles)
    }
}

// Changing the file (the settings window's Replacements page). The file stays the user's:
// comments, order and the other entries are kept as written.

/// `text` (a `dictionary.toml`) replacing `from` with `to`: the `[[replace]]` whose `from` is
/// the same (case and spacing don't count, as for duplicates) gets `to`, keeping its
/// `profiles`; otherwise one that applies everywhere is added at the end of the file. Fails,
/// with nothing changed, if `text` has an error or the result would have one (a blank `from`
/// or `to`).
pub fn with_replacement(text: &str, from: &str, to: &str) -> Result<String, DictionaryError> {
    let (from, to) = (from.trim(), to.trim());
    edit(text, |doc| {
        let list = replace_tables(doc)?;
        if let Some(t) = list.iter_mut().find(|t| same_from(t, from)) {
            t["to"] = toml_edit::value(to);
            return Ok(());
        }
        let mut table = toml_edit::Table::new();
        table["from"] = toml_edit::value(from);
        table["to"] = toml_edit::value(to);
        list.push(table);
        // Comments at the end of the file (the template's explain the format) would
        // otherwise come after the new table.
        let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
        if !trailing.trim().is_empty() {
            let list = replace_tables(doc)?;
            let last = list.len() - 1;
            let table = list.get_mut(last).expect("just pushed");
            table
                .decor_mut()
                .set_prefix(format!("{}\n\n", trailing.trim_end()));
            doc.set_trailing("");
        }
        Ok(())
    })
}

/// `text` without the `[[replace]]` whose `from` is the same as `from` (see
/// [`with_replacement`]); unchanged if there's none.
pub fn without_replacement(text: &str, from: &str) -> Result<String, DictionaryError> {
    let from = from.trim();
    edit(text, |doc| {
        let list = replace_tables(doc)?;
        list.retain(|t| !same_from(t, from));
        if list.is_empty() {
            doc.remove("replace");
        }
        Ok(())
    })
}

/// Writes `text` to `path` in one step (a new file, renamed over it): the running AGT reloads
/// it once, whole.
pub fn save(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    write_whole(path, text)
}

/// `text` changed by `change`, if both it and the result are valid dictionaries.
fn edit(
    text: &str,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), DictionaryError>,
) -> Result<String, DictionaryError> {
    Dictionary::parse(text)?;
    let mut doc: toml_edit::DocumentMut =
        text.parse()
            .map_err(|e: toml_edit::TomlError| DictionaryError::Parse {
                message: redact(e.message()),
                at: line_column(e.span(), text),
            })?;
    change(&mut doc)?;
    let out = doc.to_string();
    Dictionary::parse(&out)?;
    Ok(out)
}

/// The file's `[[replace]]` tables (none yet: an empty list).
fn replace_tables(
    doc: &mut toml_edit::DocumentMut,
) -> Result<&mut toml_edit::ArrayOfTables, DictionaryError> {
    doc.entry("replace")
        .or_insert(toml_edit::Item::ArrayOfTables(Default::default()))
        .as_array_of_tables_mut()
        .ok_or_else(|| DictionaryError::Parse {
            message: "`replace` isn't written as [[replace]] tables, so it can only be changed in the file".into(),
            at: None,
        })
}

/// `table`'s `from` is `from`, as duplicates are told apart.
fn same_from(table: &toml_edit::Table, from: &str) -> bool {
    table
        .get("from")
        .and_then(|f| f.as_str())
        .is_some_and(|f| compile(f.trim()) == compile(from))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(text: &str) -> Dictionary {
        Dictionary::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    /// One `[[replace]]` per pair.
    fn rules(pairs: &[(&str, &str)]) -> Dictionary {
        let text: String = pairs
            .iter()
            .map(|(from, to)| format!("[[replace]]\nfrom = {from:?}\nto = {to:?}\n"))
            .collect();
        dict(&text)
    }

    fn apply(d: &Dictionary, text: &str) -> (String, usize) {
        let (out, n) = d.replace(text);
        (out.into_owned(), n)
    }

    #[test]
    fn template_and_missing_file_are_empty() {
        assert!(dict(TEMPLATE).is_empty());
        assert!(dict("").is_empty());
        let missing = std::env::temp_dir().join("agt-no-such-dictionary.toml");
        assert_eq!(Dictionary::load(&missing).unwrap(), Dictionary::default());
        let (out, n) = Dictionary::default().replace("Maine is nice");
        assert!(matches!(out, Cow::Borrowed(_)) && n == 0);
    }

    #[test]
    fn terms_keep_file_order_without_blanks_or_duplicates() {
        let d = dict(r#"terms = ["main", " kubectl ", "", "AGT", "main", "Main"]"#);
        assert_eq!(d.terms(), ["main", "kubectl", "AGT", "Main"]);
        assert!(d.replacements().is_empty() && !d.is_empty());
    }

    #[test]
    fn whole_words_ignoring_case() {
        // Mid-sentence: no capital (see `a_lowercase_to_starts_a_sentence_with_a_capital`).
        let d = rules(&[("maine", "main")]);
        for (text, want, n) in [
            ("merge it into Maine.", "merge it into main.", 1),
            ("so (MAINE) and maine", "so (main) and main", 2),
            ("Maines and Maine's", "Maines and Maine's", 0),
            ("remaine", "remaine", 0),
            ("so Maine,Maine", "so main,main", 2),
            ("into Maine", "into main", 1),
        ] {
            assert_eq!(apply(&d, text), (want.into(), n), "{text}");
        }
    }

    #[test]
    fn what_is_between_words_does_not_count() {
        let d = rules(&[("dot  env", ".env")]);
        assert_eq!(apply(&d, "copy dot env now"), ("copy .env now".into(), 1));
        assert_eq!(apply(&d, "copy Dot\n\t env"), ("copy .env".into(), 1));
        assert_eq!(apply(&d, "dotenv dot-env"), (".env .env".into(), 2));
        assert_eq!(apply(&d, "dot. env"), ("dot. env".into(), 0));

        // However the speech model or polish wrote it, and however `from` is written.
        for from in [
            "Reddit True Tide",
            "reddit true-tide",
            "Reddit, True – Tide",
        ] {
            let d = rules(&[(from, "retatrutide")]);
            for said in [
                "Reddit True Tide",
                "reddit true tide",
                "Reddit true-tide",
                "Reddit truetide",
                "redditTrueTide",
                "reddit, true tide",
                "Reddit True — Tide",
                "Reddit  True\nTide",
            ] {
                let text = format!("take {said} daily");
                assert_eq!(
                    apply(&d, &text),
                    ("take retatrutide daily".into(), 1),
                    "{from}: {said}"
                );
            }
            // Still whole words, and other punctuation still counts.
            for said in [
                "Reddit true tides",
                "Reddit untrue tide",
                "Reddits true tide",
                "Reddit true tide's",
                "Reddit true.tide",
                "Reddit true; tide",
            ] {
                let text = format!("take {said} daily");
                assert_eq!(apply(&d, &text), (text.clone(), 0), "{from}: {said}");
            }
        }
        // So these are the same `from`.
        let e = Dictionary::parse(
            "[[replace]]\nfrom = \"true tide\"\nto = \"a\"\n[[replace]]\nfrom = \"True-Tide\"\nto = \"b\"\n",
        )
        .unwrap_err();
        assert!(e.to_string().contains("already replaced"), "{e}");
    }

    #[test]
    fn a_lowercase_to_starts_a_sentence_with_a_capital() {
        let d = rules(&[
            ("reddit true tide", "retatrutide"),
            ("dot env", ".env"),
            ("agt", "AGT"),
        ]);
        for (text, want) in [
            ("Reddit true tide works.", "Retatrutide works."),
            ("So Reddit true tide works.", "So retatrutide works."),
            (
                "Done. Reddit true tide! Reddit truetide? (Reddit true tide)",
                "Done. Retatrutide! Retatrutide? (Retatrutide)",
            ),
            ("Done.\n\"Reddit true tide\"", "Done.\n\"Retatrutide\""),
            ("line one\nReddit true tide", "line one\nRetatrutide"),
            // What it replaces was lowercase: so is `to`.
            ("Done. reddit true tide", "Done. retatrutide"),
            // `to` doesn't start with a lowercase word: as written.
            ("Dot env first. Agt next.", ".env first. AGT next."),
        ] {
            assert_eq!(apply(&d, text).0, want, "{text}");
        }
        let d = rules(&[
            ("my email", "me@example.com"),
            ("my site", "example.com/me"),
        ]);
        assert_eq!(
            apply(&d, "My email. My site.").0,
            "me@example.com. example.com/me."
        );
    }

    #[test]
    fn backslash_n_in_to_is_a_new_line() {
        // Written `\n` (a TOML literal string, or typed in the settings window), or a TOML
        // escape: a new line either way. `to` keeps what was written.
        let d = dict(
            "[[replace]]\nfrom = \"sign off\"\nto = 'Best,\\nGrant'\n\n\
             [[replace]]\nfrom = \"new para\"\nto = \"\\n\\n\"\n",
        );
        assert_eq!(d.replacements()[0].to, "Best,\\nGrant");
        assert_eq!(apply(&d, "thanks sign off").0, "thanks Best,\nGrant");
        assert_eq!(apply(&d, "one. new para two").0, "one. \n\n two");
    }

    #[test]
    fn punctuation_in_from_matches_itself() {
        let d = rules(&[("node.js", "Node.js"), (".net", ".NET")]);
        assert_eq!(apply(&d, "use node.js!"), ("use Node.js!".into(), 1));
        assert_eq!(
            apply(&d, "nodes.js node js"),
            ("nodes.js node js".into(), 0)
        );
        assert_eq!(apply(&d, "on .net."), ("on .NET.".into(), 1));
    }

    #[test]
    fn leftmost_longest_one_pass() {
        let d = rules(&[("dot", "."), ("dot env", ".env"), ("env", "ENV")]);
        // The longer match at the same place wins; its output isn't matched again.
        assert_eq!(
            apply(&d, "dot env and env dot"),
            (".env and ENV .".into(), 3)
        );
        // No chains: a -> b, b -> c leaves a's output alone.
        let d = rules(&[("alpha", "beta"), ("beta", "gamma")]);
        assert_eq!(apply(&d, "alpha beta"), ("beta gamma".into(), 2));
        // Leftmost: the earlier start wins even if a later one would be longer.
        let d = rules(&[("a b", "X"), ("b c d", "Y")]);
        assert_eq!(apply(&d, "a b c d"), ("X c d".into(), 1));
    }

    #[test]
    fn to_is_inserted_verbatim() {
        let d = rules(&[("agt", "AGT"), ("cube control", "kubectl")]);
        assert_eq!(
            apply(&d, "Agt and CUBE CONTROL."),
            ("AGT and kubectl.".into(), 2)
        );
    }

    #[test]
    fn unicode_words() {
        let d = rules(&[("café", "Café"), ("straße", "Strasse")]);
        assert_eq!(
            apply(&d, "CAFÉ, cafés; Straße."),
            ("Café, cafés; Strasse.".into(), 2)
        );
        let d = rules(&[("don’t", "do not")]);
        assert_eq!(apply(&d, "I DON’T, don’ts"), ("I do not, don’ts".into(), 1));
    }

    #[test]
    fn rejected_entries_say_where() {
        for (text, line, what) in [
            (
                "[[replace]]\nfrom = \"  \"\nto = \"x\"\n",
                2,
                "`from` is empty",
            ),
            ("[[replace]]\nfrom = \"x\"\nto = \"\"\n", 3, "`to` is empty"),
            (
                "[[replace]]\nfrom = \"Dot env\"\nto = \"a\"\n\n[[replace]]\nfrom = \"dot  ENV\"\nto = \"b\"\n",
                6,
                "already replaced",
            ),
            ("[[replace]]\nfrom = \"x\"\n", 1, "missing field `to`"),
            (
                "[[replace]]\nfrom = \"x\"\nto = \"y\"\napps = []\n",
                4,
                "unknown field",
            ),
            ("typo = 1\n", 1, "unknown field"),
            ("terms = \"main\"\n", 1, "invalid type"),
        ] {
            let e = Dictionary::parse(text).unwrap_err().to_string();
            assert!(
                e.starts_with(&format!("invalid dictionary.toml at line {line}, column ")),
                "{text}: {e}"
            );
            assert!(e.contains(what), "{text}: {e}");
        }
    }

    /// Errors go to the log and agt.db, which never hold the user's words: whatever toml
    /// quotes back (a value, or a key written where a key isn't expected) is taken out.
    #[test]
    fn errors_never_quote_the_users_words() {
        for text in [
            "terms = \"secretword\"",
            "secretword = \"main\"",
            "terms = []\nsecretword = \"main\"\n",
            "[[replace]]\nfrom = \"x\"\nto = \"y\"\nsecretword = \"main\"\n",
            "[[replace]]\nsecretword = \"main\"\n",
            "[secretword]\n",
            "[[secretword]]\nfrom = \"x\"\nto = \"y\"\n",
            "terms = [\"a\"]\nterms = [\"secretword\"]\n",
            "secretword = 1\nsecretword = 2\n",
            "terms = [secretword]\n",
            "terms = [\"secretword\", 1]\n",
            "[[replace]]\nfrom = \"secretword\"\nto = 1\n",
            "[[replace]]\nfrom = \"secretword\"\nto = \"secretword\"\n[[replace]]\nfrom = \"SECRETWORD\"\nto = \"x\"\n",
            "[[replace]]\nfrom = 'secretword\nto = \"x\"\n",
            "secretword\n",
            "secretword.x = 1\n",
            "\"secretword\" = 1\n",
            "terms = [\"secretword\"\n",
        ] {
            let e = Dictionary::parse(text).unwrap_err().to_string();
            assert!(!e.to_lowercase().contains("secretword"), "{text:?}: {e}");
            assert!(e.contains("line "), "{text:?}: {e}");
        }
        // The file's own keys stay: they say what's wrong.
        let e = Dictionary::parse("maine = \"main\"")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("expected one of `terms`, `profiles`, `replace`"),
            "{e}"
        );
        assert!(!e.contains("maine"), "{e}");
    }

    const PROFILED: &str = r#"
terms = ["AGT", "main"]

[profiles.code]
apps = ["Code.exe", " com.apple.Terminal ", "", "code.exe"]
terms = ["main", "rebase"]

[profiles.shell]
apps = ["WindowsTerminal.exe", "CODE.EXE"]
terms = ["kubectl", "rebase"]

[profiles.empty]

[[replace]]
from = "dot env"
to = ".env"
profiles = ["code"]

[[replace]]
from = "agt"
to = "AGT"
"#;

    #[test]
    fn profiles_scope_terms_and_replacements_by_app() {
        let d = Arc::new(dict(PROFILED));
        let names: Vec<&str> = d.profiles().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["code", "empty", "shell"]);
        assert_eq!(
            d.profiles()[0].apps,
            ["Code.exe", "com.apple.Terminal", "code.exe"]
        );
        assert_eq!(&d.all_terms()[..], ["AGT", "main", "rebase", "kubectl"]);
        let scope = |app: Option<&str>| Scope::new(d.clone(), app.map(str::to_owned));
        let replaced = |s: &Scope| s.replace("agt reads dot env").0.into_owned();

        // No profile (or no app known): the global terms and rules only.
        for app in [None, Some("notepad.exe"), Some("")] {
            let s = scope(app);
            assert_eq!(&s.terms()[..], ["AGT", "main"]);
            assert!(Arc::ptr_eq(s.terms(), &d.terms));
            assert_eq!(s.profile_names().count(), 0);
            assert_eq!(replaced(&s), "AGT reads dot env");
        }
        assert_eq!(d.replace("agt reads dot env").0, "AGT reads dot env");
        // One profile, matched ignoring case: its precompiled list.
        let s = scope(Some("COM.APPLE.TERMINAL"));
        assert_eq!(&s.terms()[..], ["AGT", "main", "rebase"]);
        assert!(Arc::ptr_eq(s.terms(), &d.profiles()[0].all_terms));
        assert_eq!(s.profile_names().collect::<Vec<_>>(), ["code"]);
        assert_eq!(s.app(), Some("COM.APPLE.TERMINAL"));
        assert_eq!(replaced(&s), "AGT reads .env");
        let s = scope(Some("WindowsTerminal.exe"));
        assert_eq!(&s.terms()[..], ["AGT", "main", "kubectl", "rebase"]);
        assert_eq!(replaced(&s), "AGT reads dot env");
        // An app in two profiles gets both.
        let s = scope(Some("Code.exe"));
        assert_eq!(s.profile_names().collect::<Vec<_>>(), ["code", "shell"]);
        assert_eq!(&s.terms()[..], ["AGT", "main", "rebase", "kubectl"]);
        assert_eq!(replaced(&s), "AGT reads .env");
        // Profile terms count as terms; profile-only rules don't apply outside them.
        assert!(!dict("[profiles.x]\nterms = [\"a\"]\n").is_empty());
        assert!(dict("[profiles.x]\napps = [\"a.exe\"]\n").is_empty());
    }

    #[test]
    fn bad_profiles_say_where_without_quoting() {
        for (text, line, what) in [
            (
                "[profiles.code]\n\n[[replace]]\nfrom = \"x\"\nto = \"y\"\nprofiles = [\"code\", \"secretword\"]\n",
                6,
                "no [profiles.<name>] table defines",
            ),
            (
                "[[replace]]\nfrom = \"x\"\nto = \"y\"\nprofiles = []\n",
                4,
                "`profiles` is empty",
            ),
            ("[profiles.code]\nsecretword = 1\n", 2, "unknown field"),
            ("terms = []\n[profiles.\"secret,word\"]\n", 2, "comma"),
            (
                "[profiles.code]\napps = \"secretword\"\n",
                2,
                "invalid type",
            ),
            ("[profiles]\nsecretword = 1\n", 2, "invalid type"),
            (
                "[[replace]]\nfrom = \"x\"\nto = \"y\"\nprofiles = \"secretword\"\n",
                4,
                "invalid type",
            ),
        ] {
            let e = Dictionary::parse(text).unwrap_err().to_string();
            assert!(
                e.starts_with(&format!("invalid dictionary.toml at line {line}, column ")),
                "{text}: {e}"
            );
            assert!(e.contains(what), "{text}: {e}");
            assert!(!e.to_lowercase().contains("secretword"), "{text:?}: {e}");
        }
        let many: String = (0..=MAX_PROFILES)
            .map(|i| format!("[profiles.p{i}]\n"))
            .collect();
        let e = Dictionary::parse(&many).unwrap_err().to_string();
        assert!(e.contains("more than 64"), "{e}");
        let ok: String = (0..MAX_PROFILES)
            .map(|i| format!("[profiles.p{i}]\n"))
            .collect();
        assert_eq!(dict(&ok).profiles().len(), MAX_PROFILES);
    }

    /// The template's commented-out examples work once uncommented.
    #[test]
    fn template_examples_parse() {
        let uncommented: String = TEMPLATE
            .lines()
            .map(|l| {
                let Some(rest) = l.strip_prefix("# ") else {
                    return l;
                };
                let key = rest
                    .split_once(" = ")
                    .is_some_and(|(k, _)| k.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
                match key || rest.starts_with(['[', ']', ' ']) {
                    true => rest,
                    false => l,
                }
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let d = Arc::new(dict(&uncommented));
        assert_eq!(d.profiles().len(), 1);
        assert_eq!(d.replacements().len(), 1);
        for app in ["code.exe", "com.microsoft.vscode", "WindowsTerminal.exe"] {
            let s = Scope::new(d.clone(), Some(app.into()));
            assert!(s.terms().iter().any(|t| t == "main"), "{app}");
            assert_eq!(s.replace("copy dot env").0, "copy .env");
        }
        assert_eq!(d.replace("copy dot env").0, "copy dot env");
    }

    #[test]
    fn create_if_missing_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("agt-dict-test-{}", std::process::id()));
        let path = dir.join("dictionary.toml");
        let _ = fs::remove_dir_all(&dir);
        Dictionary::create_if_missing(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), TEMPLATE);
        fs::write(&path, "terms = [\"x\"]").unwrap();
        Dictionary::create_if_missing(&path).unwrap();
        assert_eq!(Dictionary::load(&path).unwrap().terms(), ["x"]);
        fs::write(&path, "terms = 1").unwrap();
        assert!(Dictionary::load(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replacements_are_added_changed_and_removed_in_the_file() {
        // Added to the template: after its comments, which stay.
        let text = with_replacement(TEMPLATE, " Reddit True Tide ", "retatrutide").unwrap();
        assert!(text.starts_with(TEMPLATE.trim_end()), "{text}");
        assert!(
            text.contains("# profiles = [\"code\"]\n\n[[replace]]"),
            "{text}"
        );
        assert!(
            text.ends_with("\n[[replace]]\nfrom = \"Reddit True Tide\"\nto = \"retatrutide\"\n"),
            "{text}"
        );
        let d = dict(&text);
        assert_eq!(
            apply(&d, "Take Reddit true tide daily."),
            ("Take retatrutide daily.".into(), 1)
        );

        // The same `from` (case and spacing don't count) changes that entry, profiles kept.
        let text = "[profiles.code]\napps = [\"Code.exe\"]\n\n\
                    # Mine.\n[[replace]]\nfrom = \"dot env\"\nto = \"env\"\nprofiles = [\"code\"]\n";
        let changed = with_replacement(text, "Dot  Env", ".env").unwrap();
        assert_eq!(changed, text.replace("to = \"env\"", "to = \".env\""));
        let d = dict(&changed);
        assert_eq!(d.profiles_of(&d.replacements()[0]), ["code"]);

        // A second one comes after it; removing leaves the rest as written.
        let two = with_replacement(&changed, "my email", "me@example.com").unwrap();
        assert_eq!(dict(&two).replacements().len(), 2);
        assert_eq!(
            dict(&two).profiles_of(&dict(&two).replacements()[1]),
            [] as [&str; 0]
        );
        assert_eq!(without_replacement(&two, "MY EMAIL").unwrap(), changed);
        // The last one gone: no `replace` left. None to remove: unchanged.
        let none = without_replacement(&changed, "dot env").unwrap();
        assert_eq!(none, "[profiles.code]\napps = [\"Code.exe\"]\n");
        assert_eq!(without_replacement(&none, "dot env").unwrap(), none);
    }

    #[test]
    fn replacements_are_not_written_into_a_broken_file() {
        // The file's error, or the result's: nothing to write either way.
        assert!(with_replacement("terms = [", "a", "b").is_err());
        assert!(without_replacement("[[replace]]\nfrom = \"a\"\n", "a").is_err());
        assert!(with_replacement("", "a", " ").is_err());
        assert!(with_replacement("", " ", "b").is_err());
        // Written some other way than [[replace]] tables: left to the file.
        let inline = "replace = [{ from = \"a\", to = \"b\" }]\n";
        assert!(dict(inline).replacements().len() == 1);
        assert!(with_replacement(inline, "c", "d").is_err());
    }

    #[test]
    fn saved_files_are_whole() {
        let dir = std::env::temp_dir().join(format!("agt-dictionary-save-{}", std::process::id()));
        let path = dir.join("sub").join("dictionary.toml");
        save(&path, "terms = [\"AGT\"]\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "terms = [\"AGT\"]\n");
        save(&path, "terms = []\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "terms = []\n");
        let left: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file left");
        let _ = fs::remove_dir_all(&dir);
    }
}
