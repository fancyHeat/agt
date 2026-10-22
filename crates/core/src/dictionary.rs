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
//! - `[[replace]]`: rewrites applied to every transcript before polish, or with `profiles`
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
//! - Whitespace inside `from` matches any run of whitespace; other characters match
//!   themselves (`from = "node js"` doesn't match `node.js`).
//! - The leftmost match wins, and of those starting at the same place the longest. It's one
//!   pass: replaced text is never matched again (no chains like a → b → c).
//! - `to` is inserted exactly as written, with no case carried over from what it replaced:
//!   `Maine`, `maine` and `MAINE` all become `to`. That is also why a rewrite of a real word
//!   applies everywhere, including where the word was meant.
//! - A blank `from` or `to` is an error, and so is a `from` given twice (ignoring case and
//!   spacing), even for different profiles: in an app in both, which one applied would
//!   depend on their order.
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

use crate::config::{line_column, located, redact_values};
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
#     # Windows: VS Code, Cursor, Zed, Visual Studio, Windows Terminal, consoles, JetBrains
#     "Code.exe", "Cursor.exe", "Zed.exe", "devenv.exe", "WindowsTerminal.exe",
#     "powershell.exe", "pwsh.exe", "cmd.exe", "idea64.exe", "pycharm64.exe",
#     "webstorm64.exe", "clion64.exe", "goland64.exe", "rider64.exe", "rustrover64.exe",
#     "phpstorm64.exe",
#     # macOS: VS Code, Cursor, Zed, Terminal, iTerm2, Ghostty, Warp, JetBrains
#     "com.microsoft.VSCode", "com.todesktop.230313mzl4w4u92", "dev.zed.Zed",
#     "com.apple.Terminal", "com.googlecode.iterm2", "com.mitchellh.ghostty",
#     "dev.warp.Warp-Stable", "com.jetbrains.intellij", "com.jetbrains.pycharm",
#     "com.jetbrains.WebStorm", "com.jetbrains.CLion", "com.jetbrains.goland",
#     "com.jetbrains.rider", "com.jetbrains.rustrover", "com.jetbrains.PhpStorm",
# ]
# terms = ["main", "rebase", "kubectl", "PR"]

# Fixed rewrites applied to every transcript before polish. Only for things that are never
# ambiguous: a rewrite of a real word (e.g. "Maine") applies everywhere, unless you limit it
# to profiles. `from` matches whole words, ignoring case; `to` is pasted exactly as written.
# Copy these lines for each (`profiles` is optional):
#
# [[replace]]
# from = "dot env"
# to = ".env"
# profiles = ["code"]
"#;

/// Profiles a dictionary may have (they're a bit mask).
pub const MAX_PROFILES: usize = 64;

/// One `[[replace]]` entry, as written (trimmed).
#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub from: String,
    pub to: String,
    /// The profiles it's limited to ([`Dictionary::profiles`] indexes, as a bit mask); 0:
    /// everywhere.
    profiles: u64,
    pieces: Vec<Piece>,
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
    /// A whole word (lowercased).
    Word(String),
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
    profiles: BTreeMap<String, ProfileEntry>,
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

/// `from` (trimmed, not blank) as pieces.
fn compile(from: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut rest = from;
    while let Some(c) = rest.chars().next() {
        let len = if c.is_whitespace() {
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
    for piece in pieces {
        let rest = &text[pos..];
        pos += match piece {
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
                    name,
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
            let to = entry.to.get_ref().trim();
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
                    "this `from` is already replaced above (case and spacing don't count)",
                    Some(entry.from.span()),
                ));
            }
            let first = match &pieces[0] {
                Piece::Word(w) => w.chars().next().expect("a word is not empty"),
                Piece::Char(c) => *c,
                Piece::Space => unreachable!("`from` is trimmed"),
            };
            let i = dictionary.replacements.len();
            dictionary.index.entry(first).or_default().push(i);
            dictionary.replacements.push(Replacement {
                from: from.to_owned(),
                to: to.to_owned(),
                profiles: mask,
                pieces,
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
                    out.push_str(&self.replacements[r].to);
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
        let d = rules(&[("maine", "main")]);
        for (text, want, n) in [
            ("merge it into Maine.", "merge it into main.", 1),
            ("(MAINE) and maine", "(main) and main", 2),
            ("Maines and Maine's", "Maines and Maine's", 0),
            ("remaine", "remaine", 0),
            ("Maine,Maine", "main,main", 2),
            ("Maine", "main", 1),
        ] {
            assert_eq!(apply(&d, text), (want.into(), n), "{text}");
        }
    }

    #[test]
    fn whitespace_in_from_matches_any_run() {
        let d = rules(&[("dot  env", ".env")]);
        assert_eq!(apply(&d, "copy dot env now"), ("copy .env now".into(), 1));
        assert_eq!(apply(&d, "copy Dot\n\t env"), ("copy .env".into(), 1));
        assert_eq!(apply(&d, "dotenv dot-env"), ("dotenv dot-env".into(), 0));
        assert_eq!(apply(&d, "dot. env"), ("dot. env".into(), 0));
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
}
