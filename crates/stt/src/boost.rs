//! Boosting the dictionary's terms ([`agt_core::stt::TranscribeOpts::vocabulary`]) in greedy
//! TDT decoding: shallow fusion with a prefix trie of the terms' pieces. Independent of ONNX,
//! like [`crate::tdt`], so it can be unit-tested.
//!
//! # Spelling a term
//!
//! Each term is spelled in the model's SentencePiece pieces (`vocab.txt`; `▁` starts a word,
//! so `main` is `▁main`). The export ships only the vocabulary, not the tokenizer model, so the
//! tokenizer's own segmentation isn't known, and the model can emit any segmentation anyway.
//! Every segmentation with at most one piece more than the fewest possible goes in the trie,
//! up to [`MAX_SPELLINGS`] per form, longest pieces first: with Parakeet v2's 1024 pieces,
//! `main` is `▁m ain` (what the model emits) or `▁m a in`.
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
//! probability: log-softmax subtracts one constant from every logit, so the weight is in
//! nats whatever the logits' offset, and no softmax needs computing.
//!
//! The bonus only changes which piece is emitted, never whether one is: when the model's own
//! choice is blank, blank stays, however close a boosted piece is. A bonus can't insert a
//! term where nothing was said.
//!
//! # Greedy limits, and the one-step check
//!
//! Greedy decoding commits to each piece as it goes: a partly matched term can't be taken
//! back, and its bonus can't be paid back when the match is abandoned, nor saved for the
//! term's completion (that needs a beam: another hypothesis to switch to). So a term's first
//! piece could win where its next ones don't fit: `▁A` (from `AGT`) for "after" gave
//! "ATHANT", `▁P` (from `PR`) for "pick" gave "Pick" and title case after it.
//!
//! Hence one check: a piece the bonus chose over the model's that leaves a term unfinished is
//! only emitted if, given it, the model's next piece (bonus included) would continue the term
//! ([`Cursor::needs_check`], [`Cursor::continues`]): the decoder runs one step ahead, which
//! costs one extra decoder call where the bonus changed the model's choice, rarely more than
//! a few per dictation. Only one piece ahead: a term of three pieces or more can still stop
//! halfway (`▁k ub` for "cube"). Beam search is the next step if that isn't enough.

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
            ids,
            longest,
            ends_sentence,
        }
    }

    /// The spellings of `text` (already in piece form: `▁` for spaces) with at most
    /// [`SLACK`] pieces more than the fewest, longest pieces first, at most `max`. A bare `▁`
    /// piece only if there's no other way: the model writes `▁m ain`, never `▁ m ain`, and
    /// a bonus on the bare piece would apply at every word start.
    fn spellings(&self, text: &str, max: usize) -> Vec<Vec<u32>> {
        let bare = self.ids.get("\u{2581}").copied();
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

    /// The token to emit at a step whose token logits (blank included) are `logits` and
    /// whose argmax is `best`: `best`, or another piece the bonus puts ahead of it (see the
    /// module docs). One that [`Self::needs_check`] must be checked before it's emitted.
    pub fn pick(&self, logits: &[f32], best: usize, blank: usize) -> usize {
        if best == blank {
            return best; // never turns a blank into a piece
        }
        let (mut pick, mut score) = (best, logits[best]);
        let in_term = self.node.map_or(&[][..], |n| self.trie.children(n));
        for &(token, _) in in_term.iter().chain(self.trie.children(self.root())) {
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
    pub fn advance(&mut self, token: usize) {
        self.node = self.next_node(token);
        self.sentence_start = self.pieces.ends_sentence.get(token) == Some(&true);
    }

    /// Emitting `token` (a piece [`Self::pick`] chose over the model's) would leave a term
    /// unfinished: check with [`Self::continues`] first.
    pub fn needs_check(&self, token: usize) -> bool {
        self.next_node(token)
            .is_some_and(|n| !self.trie.nodes[n as usize].end)
    }

    /// Whether, after `token`, the next piece would continue the term: `logits` are the token
    /// logits (blank included) of the step after it, and a continuation, with the bonus, must
    /// beat every other piece. Blank doesn't count: the next piece may come a frame later.
    pub fn continues(&self, token: usize, logits: &[f32], blank: usize) -> bool {
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
    use agt_core::stt::Result;

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
        "<unk>",        // 14
        "<blk>",        // 15
    ];
    const BLANK: usize = 15;

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
        assert!(p.spellings("\u{2581}mx", 8).is_empty());
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
        assert_eq!(c.pick(&logits(1, &[(0, -1.5)]), 1, BLANK), 0);
        // ▁M leads by 2.5: the bonus isn't enough.
        assert_eq!(c.pick(&logits(1, &[(0, -2.5)]), 1, BLANK), 1);
        // Blank wins: stays blank, however close ▁m is.
        assert_eq!(c.pick(&logits(BLANK, &[(0, -0.01)]), BLANK, BLANK), BLANK);
        // Weight 0: the model's choice.
        let off = Cursor::new(&t, &p, 0.0);
        assert_eq!(off.pick(&logits(1, &[(0, -0.01)]), 1, BLANK), 1);
    }

    #[test]
    fn capitalized_form_is_boosted_only_at_sentence_start() {
        let p = pieces();
        let t = trie(&["main"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        // At the start both ▁m and ▁M are boosted: the model's choice between them stands.
        assert_eq!(c.pick(&logits(1, &[(0, -1.0)]), 1, BLANK), 1);
        c.advance(6); // "▁to"
        assert_eq!(c.pick(&logits(1, &[(0, -1.0)]), 1, BLANK), 0);
        c.advance(7); // "." starts a sentence again
        assert_eq!(c.pick(&logits(1, &[(0, -1.0)]), 1, BLANK), 1);
    }

    #[test]
    fn cursor_follows_the_term_and_resets_off_it() {
        let p = pieces();
        let t = trie(&["main", "kub"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        c.advance(6); // ▁to
        c.advance(0); // ▁m: in "main"
        // "ain" and "a" both continue it: the model's order between them holds.
        assert_eq!(c.pick(&logits(3, &[(2, -1.0)]), 3, BLANK), 3);
        // Over a piece that leaves the term ("e" after ▁m isn't a spelling), "ain" wins.
        assert_eq!(c.pick(&logits(5, &[(2, -1.0)]), 5, BLANK), 2);
        c.advance(5); // "e": off the term; mid-word, so no new match starts
        assert_eq!(c.node, None);
        assert_eq!(c.pick(&logits(5, &[(2, -1.0)]), 5, BLANK), 5);
        // A word start can begin a match right away, even mid-term.
        c.advance(0);
        c.advance(8); // ▁k: leaves "main", starts "kub"
        assert!(c.node.is_some());
        assert_eq!(c.pick(&logits(12, &[(9, -1.0)]), 12, BLANK), 9);
    }

    #[test]
    fn an_unfinished_term_is_checked_one_piece_ahead() {
        let p = pieces();
        let t = trie(&["main", "m"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        c.advance(6);
        // ▁m alone is a term too ("m"): nothing to check. Without it, ▁m needs one.
        assert!(!c.needs_check(0));
        let t = trie(&["main"], &p);
        let mut c = Cursor::new(&t, &p, 2.0);
        c.advance(6);
        assert!(c.needs_check(0));
        assert!(!c.needs_check(5)); // not in the trie at all
        // After ▁m: "ain" (with the bonus) beats "e", the model's next choice: it goes on.
        assert!(c.continues(0, &logits(5, &[(2, -1.5)]), BLANK));
        // Blank doesn't count against it.
        assert!(c.continues(0, &logits(BLANK, &[(2, -3.0), (5, -3.5)]), BLANK));
        // "e" leads by more than the bonus: it doesn't.
        assert!(!c.continues(0, &logits(5, &[(2, -2.5)]), BLANK));
        // The last piece finishes the term: no check.
        c.advance(0);
        assert!(!c.needs_check(2));
        assert!(c.needs_check(3)); // ▁m a … in
    }

    /// Steps of logits through [`tdt::greedy_decode`], boosted like the ONNX joint does: a
    /// piece that needs a check is checked against the next step's logits.
    struct Boosted<'a> {
        steps: Vec<Vec<f32>>,
        cursor: Cursor<'a>,
    }

    impl Joint for Boosted<'_> {
        fn step(&mut self, _t: usize, _last: usize) -> Result<(usize, usize)> {
            let l = match self.steps.is_empty() {
                true => logits(BLANK, &[]),
                false => self.steps.remove(0),
            };
            let best = tdt::argmax(&l);
            let mut token = self.cursor.pick(&l, best, BLANK);
            if token != best && self.cursor.needs_check(token) {
                let next = self.steps.first().cloned().unwrap_or(logits(BLANK, &[]));
                if !self.cursor.continues(token, &next, BLANK) {
                    token = best;
                }
            }
            if token != BLANK {
                self.cursor.advance(token);
            }
            // Duration index 0 for pieces (stay on the frame), 1 for blank (next frame).
            Ok((token, usize::from(token == BLANK)))
        }
        fn accept(&mut self) {}
    }

    fn decode(steps: &[Vec<f32>], t: &Trie, p: &Pieces, weight: f32) -> String {
        let mut j = Boosted {
            steps: steps.to_vec(),
            cursor: Cursor::new(t, p, weight),
        };
        let tokens = tdt::greedy_decode(3, BLANK, &mut j).unwrap();
        tdt::detokenize(tokens.iter().map(|&i| VOCAB[i]))
    }

    #[test]
    fn decodes_push_to_main_not_maine() {
        let p = pieces();
        let t = trie(&["main"], &p);
        let steps = [
            logits(11, &[]),             // ▁push
            logits(6, &[]),              // ▁to
            logits(1, &[(0, -1.0)]),     // ▁M over ▁m by 1
            logits(2, &[]),              // ain
            logits(BLANK, &[(5, -1.0)]), // blank, then on the next frame
            logits(7, &[]),              // .
        ];
        assert_eq!(decode(&steps, &t, &p, 0.0), "push to Main.");
        assert_eq!(decode(&steps, &t, &p, 2.0), "push to main.");
    }

    #[test]
    fn a_first_piece_the_word_doesnt_go_on_from_is_not_emitted() {
        let p = pieces();
        let t = trie(&["main"], &p);
        // "push to Mute": ▁M then "t" (standing in for "ute"): ▁m would win the first
        // step, but "ain" doesn't come after it, so the model's ▁M stays.
        let steps = [
            logits(11, &[]),
            logits(6, &[]),
            logits(1, &[(0, -1.0)]),
            logits(12, &[(2, -6.0)]),
            logits(BLANK, &[]),
        ];
        assert_eq!(decode(&steps, &t, &p, 0.0), "push to Mt");
        assert_eq!(decode(&steps, &t, &p, 2.0), "push to Mt");
    }
}
