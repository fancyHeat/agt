//! Boosting the dictionary's terms ([`agt_core::stt::TranscribeOpts::vocabulary`]) in greedy
//! TDT decoding: shallow fusion with a prefix trie of the terms' pieces. Independent of ONNX,
//! like [`crate::tdt`], so it can be unit-tested; the ONNX joint calls [`Cursor::step`] once
//! per decoder step.
//!
//! # Spelling a term
//!
//! Each term is spelled in the model's SentencePiece pieces (`vocab.txt`; `▁` starts a word,
//! so `main` is `▁main`). The export ships only the vocabulary, not the tokenizer model, so the
//! tokenizer's own segmentation isn't known, and the model can emit any segmentation anyway.
//! Every segmentation with at most one piece more than the fewest possible goes in the trie,
//! up to [`MAX_SPELLINGS`] per form, longest pieces first: with Parakeet v2's 1024 pieces,
//! `main` is `▁m ain` (what the model emits) or `▁m a in`. A term starting with a character
//! no `▁`-piece starts (a digit, punctuation, on v2 also Q, X and Z: `1Password`, `.env`) is
//! spelled with the bare `▁` piece: `▁ 1 P ass w ord`.
//!
//! Two forms of each term: as written, boosted everywhere, and with its first letter
//! capitalized, boosted only at a sentence start (the start of the audio, or after a piece
//! ending in `.`, `?` or `!`): `Main` must not get a bonus mid-sentence, where its `▁M` would
//! help exactly the `Maine` the term is there to beat.
//!
//! # The bonus
//!
//! At each step, the pieces that continue the term being matched, or start one (a match only
//! starts at a word start: the trie's first pieces all begin with `▁`), get [`Cursor::weight`]
//! added to their logit before the argmax. That is the same as adding it to the log
//! probability: log-softmax subtracts one constant from every logit, so no softmax needs
//! computing. (It removes the logits' offset, not their scale: what a weight does depends on
//! the model, which is why the engine scales it per model.)
//!
//! The bonus only changes which piece is emitted, never whether one is: when the model's own
//! choice is blank, blank stays, however close a boosted piece is. A bonus can't insert a
//! term where nothing was said. The bare `▁` never gets it at a word start: it starts every
//! word that begins with a digit, so "ten to fifteen" would come out as "10 to 15" with
//! `1Password` in the dictionary. A match still starts when the model writes `▁` itself.
//!
//! # Greedy limits, and the one-step check
//!
//! Greedy decoding commits to each piece as it goes: a partly matched term can't be taken
//! back, and its bonus can't be paid back when the match is abandoned, nor saved for the
//! term's completion (that needs a beam: another hypothesis to switch to). So a term's first
//! piece could win where its next ones don't fit: `▁A` (from `AGT`) for "after" gave
//! "ATHANT", `▁P` (from `PR`) for "pick" gave "Pick" and title case after it. And a piece that
//! finishes a term could win where the word goes on: v3 spells `Main` `▁Ma in` and "Maine"
//! `▁Ma ine`, so at a sentence start (where `Main` is boosted) `in` beat `ine` and "Maine and
//! Vermont" came out "Main and Vermont".
//!
//! Hence one check: a piece the bonus chose over the model's that leaves a term unfinished is
//! only emitted if the decoder, run one step ahead with it (where greedy decoding would go
//! on: [`Cursor::step`]), would continue the term with its next piece, bonus included and
//! blank aside. That's one extra decoder call where the bonus changed the model's choice,
//! rarely more than a few per dictation. Only one piece ahead: a term of three pieces or more
//! can still stop halfway (`▁k ub` for "cube").
//!
//! A piece that finishes a term isn't checked, which is what the v3 case above needs. A
//! check that the word doesn't go on (no `e` after `▁Ma in`) was tried: it changed nothing
//! on the homophone set, on v2 or v3, because given `▁Ma in` the model goes on with the next
//! word ("Main and Vermont"); the engine's per-model scale (`stt.boost` × 0.5 on v3) is what
//! keeps v3 there. Beam search is the next step if greedy isn't enough.
use crate::tdt;
use agt_core::stt::Result;
use std::collections::HashMap;

/// Spellings kept per form of a term (see the module docs).
pub const MAX_SPELLINGS: usize = 8;
/// Pieces a spelling may have beyond the fewest possible.
const SLACK: usize = 1;

/// What boosting needs to know about a model's vocabulary; built once per model.
pub struct Pieces {
    /// Piece → token id, without the special tokens (`<blk>`, `<unk>`, `<|...|>`).
    ids: HashMap<String, u32>,
    /// The longest piece, in chars.
    longest: usize,
    /// Per token: ends in `.`, `?` or `!`, so a sentence starts after it.
    ends_sentence: Vec<bool>,
    /// The bare `▁` piece, if the model has one: what it writes before a word that no
    /// `▁`-piece starts (a digit, punctuation, a rare letter). Never boosted (see
    /// [`Cursor::pick`]).
    bare: Option<u32>,
}

impl Pieces {
    pub fn new(vocab: &[String]) -> Self {
        let special = |p: &str| p.starts_with('<') && p.ends_with('>');
        let mut ids = HashMap::with_capacity(vocab.len());
        let mut longest = 0;
        for (id, p) in vocab.iter().enumerate() {
            if !p.is_empty() && !special(p) {
                ids.insert(p.clone(), id as u32);
                longest = longest.max(p.chars().count());
            }
        }
        let ends_sentence = vocab
            .iter()
            .map(|p| !special(p) && p.ends_with(['.', '?', '!']))
            .collect();
        Self {
            bare: ids.get("\u{2581}").copied(),
            ids,
            longest,
            ends_sentence,
        }
    }

    /// The spellings of `text` (already in piece form: `▁` for spaces) with at most
    /// [`SLACK`] pieces more than the fewest, longest pieces first, at most `max`. A bare `▁`
    /// piece only if there's no other way: the model writes `▁m ain`, never `▁ m ain`.
    fn spellings(&self, text: &str, max: usize) -> Vec<Vec<u32>> {
        let bare = self.bare;
        let without: Vec<Vec<u32>> = self
            .all_spellings(text, max)
            .into_iter()
            .filter(|s| bare.is_none_or(|b| !s.contains(&b)))
            .collect();
        match without.is_empty() {
            true => self.all_spellings(text, max),
            false => without,
        }
    }

    fn all_spellings(&self, text: &str, max: usize) -> Vec<Vec<u32>> {
        // Char boundaries, and the fewest pieces from each one to the end.
        let bounds: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain([text.len()])
            .collect();
        let n = bounds.len() - 1;
        let mut fewest = vec![usize::MAX; n + 1];
        fewest[n] = 0;
        for i in (0..n).rev() {
            for j in i + 1..=n.min(i + self.longest) {
                if fewest[j] != usize::MAX && self.ids.contains_key(&text[bounds[i]..bounds[j]]) {
                    fewest[i] = fewest[i].min(fewest[j] + 1);
                }
            }
        }
        let mut out = Vec::new();
        if fewest[0] == usize::MAX {
            return out; // a character no piece has
        }
        let mut spell = Spell {
            pieces: self,
            text,
            bounds: &bounds,
            fewest: &fewest,
            budget: fewest[0] + SLACK,
            max,
            path: Vec::new(),
            out: &mut out,
        };
        spell.from(0);
        out
    }
}

/// The depth-first search behind [`Pieces::spellings`].
struct Spell<'a> {
    pieces: &'a Pieces,
    text: &'a str,
    bounds: &'a [usize],
    fewest: &'a [usize],
    /// Most pieces a spelling may have.
    budget: usize,
    max: usize,
    path: Vec<u32>,
    out: &'a mut Vec<Vec<u32>>,
}

impl Spell<'_> {
    /// Every spelling of the text from char `i`, after `path`.
    fn from(&mut self, i: usize) {
        let n = self.bounds.len() - 1;
        if i == n {
            self.out.push(self.path.clone());
            return;
        }
        for j in (i + 1..=n.min(i + self.pieces.longest)).rev() {
            if self.out.len() >= self.max {
                return;
            }
            let piece = &self.text[self.bounds[i]..self.bounds[j]];
            let Some(&id) = self.pieces.ids.get(piece) else {
                continue;
            };
            if self.fewest[j] == usize::MAX || self.path.len() + 1 + self.fewest[j] > self.budget {
                continue;
            }
            self.path.push(id);
            self.from(j);
            self.path.pop();
        }
    }
}

/// The trie's mid-sentence root: terms as written.
const ROOT: u32 = 0;
/// The root at a sentence start: terms as written and capitalized.
const START_ROOT: u32 = 1;

#[derive(Default)]
struct Node {
    /// (token, node), sorted by token.
    children: Vec<(u32, u32)>,
    /// A term ends here.
    end: bool,
}

/// The terms' spellings as a prefix trie of token ids. Built once per term list (the engine
/// caches it), read by every decoding step.
pub struct Trie {
    nodes: Vec<Node>,
    /// Terms with at least one spelling (a term with a character no piece has has none).
    pub spelled: usize,
}

impl Trie {
    pub fn new(terms: &[String], pieces: &Pieces) -> Self {
        let mut trie = Self {
            nodes: vec![Node::default(), Node::default()],
            spelled: 0,
        };
        for term in terms {
            let words: Vec<&str> = term.split_whitespace().collect();
            if words.is_empty() {
                continue;
            }
            let written = format!("\u{2581}{}", words.join("\u{2581}"));
            let spellings = pieces.spellings(&written, MAX_SPELLINGS);
            trie.spelled += usize::from(!spellings.is_empty());
            for s in &spellings {
                trie.insert(ROOT, s);
                trie.insert(START_ROOT, s);
            }
            let capitalized = capitalize(&written);
            if capitalized != written {
                for s in pieces.spellings(&capitalized, MAX_SPELLINGS) {
                    trie.insert(START_ROOT, &s);
                }
            }
        }
        trie
    }

    fn insert(&mut self, root: u32, spelling: &[u32]) {
        let mut node = root;
        for &token in spelling {
            node = match self.child(node, token) {
                Some(next) => next,
                None => {
                    let next = self.nodes.len() as u32;
                    self.nodes.push(Node::default());
                    let children = &mut self.nodes[node as usize].children;
                    let at = children.partition_point(|&(t, _)| t < token);
                    children.insert(at, (token, next));
                    next
                }
            };
        }
        self.nodes[node as usize].end = true;
    }

    fn child(&self, node: u32, token: u32) -> Option<u32> {
        let children = &self.nodes[node as usize].children;
        children
            .binary_search_by_key(&token, |&(t, _)| t)
            .ok()
            .map(|i| children[i].1)
    }

    fn children(&self, node: u32) -> &[(u32, u32)] {
        &self.nodes[node as usize].children
    }

    /// No term has a spelling: boosting would change nothing.
    pub fn is_empty(&self) -> bool {
        self.spelled == 0
    }
}

/// `▁main` → `▁Main` (unchanged if it doesn't start with a lowercase letter).
fn capitalize(text: &str) -> String {
    let rest = text.trim_start_matches('\u{2581}');
    let mut chars = rest.chars();
    match chars.next() {
        Some(first) => format!("\u{2581}{}{}", first.to_uppercase(), chars.as_str()),
        None => text.to_owned(),
    }
}

/// Where a decoder step is (see [`Cursor::step`]).
#[derive(Debug, Clone, Copy)]
pub struct At {
    /// The frame.
    pub t: usize,
    /// The step's duration index.
    pub duration: usize,
    /// Tokens already emitted on this frame (before this step).
    pub emitted_here: usize,
    /// Frames in all.
    pub frames: usize,
}

/// Where one decoding is in the trie. One per `transcribe` call (a new call starts at a
/// sentence start, outside any term).
#[derive(Clone)]
pub struct Cursor<'a> {
    trie: &'a Trie,
    pieces: &'a Pieces,
    /// The bonus, in nats (log probability).
    pub weight: f32,
    /// The node of the term being matched, if any.
    node: Option<u32>,
    sentence_start: bool,
}

impl<'a> Cursor<'a> {
    pub fn new(trie: &'a Trie, pieces: &'a Pieces, weight: f32) -> Self {
        Self {
            trie,
            pieces,
            weight,
            node: None,
            sentence_start: true,
        }
    }

    fn root(&self) -> u32 {
        match self.sentence_start {
            true => START_ROOT,
            false => ROOT,
        }
    }

    /// One boosted decoder step, as the ONNX joint runs it: [`Self::choose`], with the
    /// lookahead on the frame greedy decoding goes on with after emitting a piece at `at`
    /// ([`tdt::next_frame`]): `run(frame, piece)` runs the decoder there with `piece` as the
    /// last token and returns its token logits. No audio left: no lookahead, and no piece the
    /// model didn't choose.
    pub fn step(
        &mut self,
        logits: &[f32],
        best: usize,
        blank: usize,
        at: At,
        run: impl FnOnce(usize, usize) -> Result<Vec<f32>>,
    ) -> Result<usize> {
        self.choose(logits, best, blank, |piece| {
            let next = tdt::next_frame(at.t, at.duration, true, at.emitted_here + 1);
            if next >= at.frames {
                return Ok(None);
            }
            run(next, piece).map(Some)
        })
    }

    /// One decoder step: the token to emit, given the step's token logits (blank included)
    /// and their argmax `best`, and the trie position moved on past it. `best`, unless the
    /// bonus puts a term's piece ahead of it and that piece passes the one-step check (see
    /// the module docs): `lookahead(piece)` returns the token logits of the step after
    /// emitting it (`None`: no audio left, so the piece is not emitted).
    pub fn choose(
        &mut self,
        logits: &[f32],
        best: usize,
        blank: usize,
        lookahead: impl FnOnce(usize) -> Result<Option<Vec<f32>>>,
    ) -> Result<usize> {
        let mut token = self.pick(logits, best, blank);
        if token != best && self.unfinished(token) {
            let ok = match lookahead(token)? {
                Some(next) => self.continues(token, &next, blank),
                None => false,
            };
            if !ok {
                token = best;
            }
        }
        if token != blank {
            self.advance(token);
        }
        Ok(token)
    }

    /// `best`, or another piece the bonus puts ahead of it. Never turns a blank into a piece,
    /// and never boosts the bare `▁` at a word start (it starts every word with a digit or
    /// punctuation: spoken numbers would come out as digits everywhere).
    fn pick(&self, logits: &[f32], best: usize, blank: usize) -> usize {
        if best == blank {
            return best;
        }
        let (mut pick, mut score) = (best, logits[best]);
        let in_term = self.node.map_or(&[][..], |n| self.trie.children(n));
        let starts = self
            .trie
            .children(self.root())
            .iter()
            .filter(|&&(t, _)| Some(t) != self.pieces.bare);
        for &(token, _) in in_term.iter().chain(starts) {
            let s = logits
                .get(token as usize)
                .map_or(f32::MIN, |l| l + self.weight);
            if s > score {
                (pick, score) = (token as usize, s);
            }
        }
        pick
    }

    /// The node `token` leads to from here: on in the term being matched, or the start of
    /// one.
    fn next_node(&self, token: usize) -> Option<u32> {
        let t = token as u32;
        self.node
            .and_then(|n| self.trie.child(n, t))
            .or_else(|| self.trie.child(self.root(), t))
    }

    /// `token` (not blank) was emitted.
    fn advance(&mut self, token: usize) {
        self.node = self.next_node(token);
        self.sentence_start = self.pieces.ends_sentence.get(token) == Some(&true);
    }

    /// Emitting `token` would leave a term unfinished: it's only emitted if the term goes on
    /// ([`Self::continues`]). A piece that finishes a term isn't checked.
    fn unfinished(&self, token: usize) -> bool {
        self.next_node(token)
            .is_some_and(|n| !self.trie.nodes[n as usize].end)
    }

    /// The one-step check of a piece the bonus chose, with `logits` the token logits (blank
    /// included) of the step after it: the next piece, the bonus included, must continue the
    /// term. Blank doesn't count: the next piece may come a frame later.
    fn continues(&self, token: usize, logits: &[f32], blank: usize) -> bool {
        let Some(node) = self.next_node(token) else {
            return false;
        };
        let children = self.trie.children(node);
        let is_child = |t: usize| {
            children
                .binary_search_by_key(&(t as u32), |&(c, _)| c)
                .is_ok()
        };
        let best_child = children
            .iter()
            .filter_map(|&(t, _)| logits.get(t as usize))
            .fold(f32::MIN, |m, &l| m.max(l + self.weight));
        let best_other = logits
            .iter()
            .enumerate()
            .filter(|&(t, _)| t != blank && !is_child(t))
            .fold(f32::MIN, |m, (_, &l)| m.max(l));
        best_child > best_other
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tdt::{self, Joint};

    /// A tiny vocabulary: ids are positions; the last one is blank.
    const VOCAB: &[&str] = &[
        "\u{2581}m",    // 0
        "\u{2581}M",    // 1
        "ain",          // 2
        "a",            // 3
        "in",           // 4
        "e",            // 5
        "\u{2581}to",   // 6
        ".",            // 7
        "\u{2581}k",    // 8
        "ub",           // 9
        "\u{2581}c",    // 10
        "\u{2581}push", // 11
        "t",            // 12
        "\u{2581}",     // 13
        "1",            // 14
        "x",            // 15
        "<unk>",        // 16
        "<blk>",        // 17
    ];
    const BLANK: usize = 17;
    const M: usize = 0;
    const CAP_M: usize = 1;
    const AIN: usize = 2;

    fn pieces() -> Pieces {
        Pieces::new(&VOCAB.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn trie(terms: &[&str], pieces: &Pieces) -> Trie {
        Trie::new(
            &terms.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            pieces,
        )
    }

    /// Logits with `top` the argmax at 0.0, `others` as given, everything else at -10.
    fn logits(top: usize, others: &[(usize, f32)]) -> Vec<f32> {
        let mut l = vec![-10.0; VOCAB.len()];
        for &(t, v) in others {
            l[t] = v;
        }
        l[top] = 0.0;
        l
    }

    /// `choose` with a lookahead that says the term goes on (`ain` next).
    fn choose(c: &mut Cursor, l: &[f32]) -> usize {
        let best = tdt::argmax(l);
        c.choose(l, best, BLANK, |_| Ok(Some(logits(AIN, &[]))))
            .unwrap()
    }

    #[test]
    fn spellings_go_up_to_one_piece_more_than_the_fewest() {
        let p = pieces();
        // ▁m+ain (2), ▁m+a+in (3).
        assert_eq!(
            p.spellings("\u{2581}main", 8),
            vec![vec![0, 2], vec![0, 3, 4]]
        );
        assert_eq!(p.spellings("\u{2581}main", 1), vec![vec![0, 2]]);
        // A character no piece has: no spelling. Special tokens are never pieces.
        assert!(p.spellings("\u{2581}mz", 8).is_empty());
        assert!(p.spellings("<unk>", 8).is_empty());
        // The bare ▁ only when nothing else spells it.
        assert_eq!(
            p.spellings("\u{2581}ain", 8),
            vec![vec![13, 2], vec![13, 3, 4]]
        );
    }

    #[test]
    fn trie_has_capitalized_form_only_at_sentence_start() {
        let p = pieces();
        let t = trie(&["main", "", "zz", " main "], &p);
        assert_eq!(t.spelled, 2); // "main" twice, "zz" unspellable, "" skipped
        assert!(t.child(ROOT, 0).is_some() && t.child(ROOT, 1).is_none());
        assert!(t.child(START_ROOT, 0).is_some() && t.child(START_ROOT, 1).is_some());
        assert!(trie(&["zz"], &p).is_empty());
        assert_eq!(capitalize("\u{2581}main"), "\u{2581}Main");
        assert_eq!(capitalize("\u{2581}AGT"), "\u{2581}AGT");
        assert_eq!(capitalize("\u{2581}1x"), "\u{2581}1x");
    }

    #[test]
    fn bonus_changes_which_piece_never_whether() {
        let p = pieces();
        let t = trie(&["main"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        c.advance(11); // "▁push": mid-sentence from here
        // ▁M leads by 1.5: the bonus on ▁m (2.0) wins.
        assert_eq!(c.pick(&logits(CAP_M, &[(M, -1.5)]), CAP_M, BLANK), M);
        // ▁M leads by 2.5: the bonus isn't enough.
        assert_eq!(c.pick(&logits(CAP_M, &[(M, -2.5)]), CAP_M, BLANK), CAP_M);
        // Blank wins: stays blank, however close ▁m is.
        assert_eq!(c.pick(&logits(BLANK, &[(M, -0.01)]), BLANK, BLANK), BLANK);
        // Weight 0: the model's choice.
        let off = Cursor::new(&t, &p, 0.0);
        assert_eq!(off.pick(&logits(CAP_M, &[(M, -0.01)]), CAP_M, BLANK), CAP_M);
    }

    #[test]
    fn the_bare_word_start_is_never_boosted() {
        // "1x" is spelled ▁ 1 x: the bare ▁ would otherwise get the bonus at every word start
        // (any number would come out in digits).
        let p = pieces();
        let t = trie(&["1x"], &p);
        assert_eq!(t.children(ROOT), [(13, t.child(ROOT, 13).unwrap())]);
        let mut c = Cursor::new(&t, &p, 4.0);
        c.advance(11);
        assert_eq!(c.pick(&logits(6, &[(13, -0.5)]), 6, BLANK), 6);
        // The model's own ▁ still starts a match, whose next pieces are boosted.
        assert_eq!(choose(&mut c, &logits(13, &[])), 13);
        assert_eq!(c.pick(&logits(15, &[(14, -1.0)]), 15, BLANK), 14);
    }

    #[test]
    fn capitalized_form_is_boosted_only_at_sentence_start() {
        let p = pieces();
        let t = trie(&["main"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        // At the start both ▁m and ▁M are boosted: the model's choice between them stands.
        assert_eq!(c.pick(&logits(CAP_M, &[(M, -1.0)]), CAP_M, BLANK), CAP_M);
        c.advance(6); // "▁to"
        assert_eq!(c.pick(&logits(CAP_M, &[(M, -1.0)]), CAP_M, BLANK), M);
        c.advance(7); // "." starts a sentence again
        assert_eq!(c.pick(&logits(CAP_M, &[(M, -1.0)]), CAP_M, BLANK), CAP_M);
    }

    #[test]
    fn cursor_follows_the_term_and_resets_off_it() {
        let p = pieces();
        let t = trie(&["main", "kub"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        c.advance(6); // ▁to
        c.advance(M); // ▁m: in "main"
        // "ain" and "a" both continue it: the model's order between them holds.
        assert_eq!(c.pick(&logits(3, &[(AIN, -1.0)]), 3, BLANK), 3);
        // Over a piece that leaves the term ("e" after ▁m isn't a spelling), "ain" wins.
        assert_eq!(c.pick(&logits(5, &[(AIN, -1.0)]), 5, BLANK), AIN);
        c.advance(5); // "e": off the term; mid-word, so no new match starts
        assert_eq!(c.node, None);
        assert_eq!(c.pick(&logits(5, &[(AIN, -1.0)]), 5, BLANK), 5);
        // A word start can begin a match right away, even mid-term.
        c.advance(M);
        c.advance(8); // ▁k: leaves "main", starts "kub"
        assert!(c.node.is_some());
        assert_eq!(c.pick(&logits(12, &[(9, -1.0)]), 12, BLANK), 9);
    }

    #[test]
    fn a_boosted_piece_is_checked_one_step_ahead() {
        let p = pieces();
        let t = trie(&["main", "maint"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        c.advance(6);
        let at_m = c.clone();
        let l = logits(CAP_M, &[(M, -1.0)]);
        let ask = |c: &mut Cursor, next: Option<Vec<f32>>| {
            let mut asked = None;
            let token = c
                .choose(&l, CAP_M, BLANK, |piece| {
                    asked = Some(piece);
                    Ok(next)
                })
                .unwrap();
            (token, asked)
        };
        // ▁m (boosted) is only emitted if the term goes on: "ain" (with the bonus) beats "e".
        let mut c1 = at_m.clone();
        assert_eq!(ask(&mut c1, Some(logits(5, &[(AIN, -1.5)]))), (M, Some(M)));
        assert!(c1.node.is_some());
        // Blank doesn't count against it.
        let mut c1 = at_m.clone();
        let blank_first = logits(BLANK, &[(AIN, -3.0), (5, -3.5)]);
        assert_eq!(ask(&mut c1, Some(blank_first)).0, M);
        // "e" leads by more than the bonus, or no audio is left: the model's ▁M.
        let mut c1 = at_m.clone();
        assert_eq!(ask(&mut c1, Some(logits(5, &[(AIN, -2.5)]))).0, CAP_M);
        assert_eq!(c1.node, None);
        let mut c1 = at_m.clone();
        assert_eq!(ask(&mut c1, None).0, CAP_M);
        // The model's own choice is never checked.
        let mut c1 = at_m.clone();
        let mut asked = false;
        let own = logits(M, &[]);
        c1.choose(&own, M, BLANK, |_| {
            asked = true;
            Ok(None)
        })
        .unwrap();
        assert!(!asked);
        // A piece that finishes a term is emitted without a lookahead (see the module docs).
        let mut done = at_m.clone();
        done.advance(M);
        let finish = logits(12, &[(AIN, -1.0)]); // the model: ▁m "t"…
        let mut asked = false;
        let token = done
            .choose(&finish, 12, BLANK, |_| {
                asked = true;
                Ok(None)
            })
            .unwrap();
        assert_eq!((token, asked), (AIN, false));
    }

    /// A model as a function of (frame, last token) -> (logits, duration index), decoded by
    /// [`tdt::greedy_decode`] through [`Cursor::step`], as the ONNX joint does. Records each
    /// lookahead the cursor asks for: (frame, piece).
    struct Model<'a, F> {
        model: F,
        cursor: Cursor<'a>,
        frames: usize,
        asked: Vec<(usize, usize)>,
    }

    impl<F: Fn(usize, usize) -> (Vec<f32>, usize)> Joint for Model<'_, F> {
        fn step(&mut self, t: usize, last: usize, here: usize) -> Result<(usize, usize)> {
            let (l, duration) = (self.model)(t, last);
            let at = At {
                t,
                duration,
                emitted_here: here,
                frames: self.frames,
            };
            let (model, asked) = (&self.model, &mut self.asked);
            let token = self
                .cursor
                .step(&l, tdt::argmax(&l), BLANK, at, |frame, piece| {
                    asked.push((frame, piece));
                    Ok(model(frame, piece).0)
                })?;
            Ok((token, duration))
        }
        fn accept(&mut self) {}
    }

    fn decode<F: Fn(usize, usize) -> (Vec<f32>, usize)>(
        model: F,
        frames: usize,
        terms: &[&str],
        weight: f32,
    ) -> (String, Vec<(usize, usize)>) {
        let p = pieces();
        let t = trie(terms, &p);
        let mut j = Model {
            model,
            cursor: Cursor::new(&t, &p, weight),
            frames,
            asked: Vec::new(),
        };
        let tokens = tdt::greedy_decode(frames, BLANK, &mut j).unwrap();
        let text = tdt::detokenize(tokens.iter().map(|&i| VOCAB[i]));
        (text, j.asked)
    }

    /// "push to Main." with ▁M one nat over ▁m on frame 0, which then predicts `dur`; the
    /// model goes on with "ain" after either, then "." on frame 2.
    fn push_to_main(dur: usize) -> impl Fn(usize, usize) -> (Vec<f32>, usize) {
        move |t, last| match (t, last) {
            (0, BLANK) => (logits(11, &[]), 0),
            (0, 11) => (logits(6, &[]), 0),
            (0, 6) => (logits(CAP_M, &[(M, -1.0)]), dur),
            (_, M | CAP_M) => (logits(AIN, &[]), 1),
            (_, AIN) => (logits(7, &[]), 0),
            _ => (logits(BLANK, &[]), 1),
        }
    }

    #[test]
    fn decodes_push_to_main_not_main_capitalized() {
        let (text, asked) = decode(push_to_main(0), 4, &["main"], 0.0);
        assert_eq!((text.as_str(), asked.len()), ("push to Main.", 0));
        // The lookahead runs where greedy decoding goes on: the same frame (duration 0).
        let (text, asked) = decode(push_to_main(0), 4, &["main"], 2.0);
        assert_eq!(
            (text.as_str(), &asked[..]),
            ("push to main.", &[(0, M)][..])
        );
        // Two frames on (duration index 2).
        let (text, asked) = decode(push_to_main(2), 4, &["main"], 2.0);
        assert_eq!(
            (text.as_str(), &asked[..]),
            ("push to main.", &[(2, M)][..])
        );
        // No audio after it: no lookahead, so no piece the model didn't choose.
        let (text, asked) = decode(push_to_main(4), 4, &["main"], 2.0);
        assert_eq!((text.as_str(), asked.len()), ("push to M", 0));
    }

    #[test]
    fn a_first_piece_the_word_doesnt_go_on_from_is_not_emitted() {
        // "push to Mt": after ▁m the model wants "t", not "ain".
        let model = |t, last| match (t, last) {
            (0, BLANK) => (logits(11, &[]), 0),
            (0, 11) => (logits(6, &[]), 0),
            (0, 6) => (logits(CAP_M, &[(M, -1.0)]), 0),
            (0, M | CAP_M) => (logits(12, &[(AIN, -6.0)]), 1),
            _ => (logits(BLANK, &[]), 1),
        };
        let (text, asked) = decode(model, 2, &["main"], 2.0);
        assert_eq!((text.as_str(), &asked[..]), ("push to Mt", &[(0, M)][..]));
    }

    #[test]
    fn the_tenth_symbol_on_a_frame_looks_ahead_on_the_next() {
        let p = pieces();
        let t = trie(&["main"], &p);
        let l = logits(CAP_M, &[(M, -1.0)]);
        for (here, frame) in [(0, 3), (8, 3), (9, 4)] {
            let mut c = Cursor::new(&t, &p, 2.0);
            c.advance(6);
            let mut asked = None;
            let at = At {
                t: 3,
                duration: 0,
                emitted_here: here,
                frames: 10,
            };
            c.step(&l, CAP_M, BLANK, at, |f, _| {
                asked = Some(f);
                Ok(logits(AIN, &[]))
            })
            .unwrap();
            assert_eq!(asked, Some(frame), "{here} emitted on the frame before");
        }
    }
}
