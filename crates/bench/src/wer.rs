//! Word error rate with a small, documented text normalizer (applied to both sides):
//!
//! 1. lowercase; `&` -> "and", `%` -> "percent", `$N` -> "N dollars";
//! 2. numbers in digits become words: `7` -> "seven", `1,250` -> "one thousand two hundred
//!    fifty", `3.5` -> "three point five", `21st` -> "twenty first", and four-digit numbers
//!    without a comma between 1100 and 2099 (except 2000-2009) are read as years:
//!    `1984` -> "nineteen eighty four", `2021` -> "twenty twenty one";
//! 3. hyphens split words; other punctuation is dropped (apostrophes inside words are kept);
//! 4. hesitations are dropped: uh, um, uhm, umm, hmm, hm, mm, mmm, mhm, er, erm, ah;
//! 5. a few spoken forms are unified: mr/mister, mrs/missus, dr/doctor, ok/okay,
//!    "-isation" -> "-ization" (British spelling in VoxPopuli references).
//!
//! It's deliberately simpler than Whisper's English normalizer: what remains (e.g. "q and a"
//! vs "Q&A" is handled, but spelled-out "twenty twenty one" vs "2021" only when the year
//! rule applies) costs every engine about equally.

const FILLERS: &[&str] = &[
    "uh", "um", "uhm", "umm", "hmm", "hm", "mm", "mmm", "mhm", "er", "erm", "ah",
];

/// (substitutions + deletions + insertions, reference word count).
pub fn word_errors(reference: &str, hypothesis: &str) -> (usize, usize) {
    let (r, h) = (normalize(reference), normalize(hypothesis));
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(rw != hw))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    (prev[h.len()], r.len())
}

/// Word errors by kind, after normalizing both sides like [`word_errors`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Errors {
    pub sub: usize,
    pub del: usize,
    pub ins: usize,
    /// Reference words.
    pub words: usize,
}

impl Errors {
    pub fn of(reference: &str, hypothesis: &str) -> Self {
        let (r, h) = (normalize(reference), normalize(hypothesis));
        let mut e = Errors {
            words: r.len(),
            ..Self::default()
        };
        for op in edit_script(&r, &h) {
            match op {
                Edit::Same(..) => {}
                Edit::Sub(..) => e.sub += 1,
                Edit::Del(_) => e.del += 1,
                Edit::Ins(_) => e.ins += 1,
            }
        }
        e
    }

    pub fn total(&self) -> usize {
        self.sub + self.del + self.ins
    }

    pub fn add(&mut self, o: Errors) {
        (self.sub, self.del, self.ins) = (self.sub + o.sub, self.del + o.del, self.ins + o.ins);
        self.words += o.words;
    }
}

/// One step of turning `a` into `b`: indices into `a` and/or `b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Same(usize, usize),
    Sub(usize, usize),
    Del(usize),
    Ins(usize),
}

/// A minimal (Levenshtein) edit script from `a` to `b`, in order.
pub fn edit_script<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Edit> {
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            d[i][j] = (d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]))
                .min(d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1);
        }
    }
    let (mut i, mut j, mut ops) = (n, m, Vec::new());
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && d[i][j] == d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]) {
            (i, j) = (i - 1, j - 1);
            ops.push(if a[i] == b[j] {
                Edit::Same(i, j)
            } else {
                Edit::Sub(i, j)
            });
        } else if i > 0 && d[i][j] == d[i - 1][j] + 1 {
            i -= 1;
            ops.push(Edit::Del(i));
        } else {
            j -= 1;
            ops.push(Edit::Ins(j));
        }
    }
    ops.reverse();
    ops
}

pub fn normalize(s: &str) -> Vec<String> {
    let s = s
        .to_lowercase()
        .replace('&', " and ")
        .replace('%', " percent ");
    let mut out = Vec::new();
    for raw in s.split(|c: char| c.is_whitespace() || c == '-' || c == '—' || c == '–') {
        let dollars = raw.starts_with('$');
        // Keep digits' separators for now; strip other punctuation (keep inner apostrophes).
        let w: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '\'' | '.' | ','))
            .collect();
        let w = w.trim_matches(|c| matches!(c, '\'' | '.' | ','));
        if w.is_empty() {
            continue;
        }
        if w.starts_with(|c: char| c.is_ascii_digit())
            && let Some(words) = number_words(w)
        {
            out.extend(words);
            if dollars {
                out.push("dollars".into());
            }
            continue;
        }
        let w: String = w.chars().filter(|c| !matches!(c, '.' | ',')).collect();
        let w = w.trim_matches('\'');
        if w.is_empty() || FILLERS.contains(&w) {
            continue;
        }
        let w = match w {
            "mr" => "mister".to_owned(),
            "mrs" => "missus".to_owned(),
            "dr" => "doctor".to_owned(),
            "ok" => "okay".to_owned(),
            _ => w.replace("isation", "ization"),
        };
        out.push(w);
    }
    out
}

/// Words for a token starting with a digit, or `None` if it isn't a number we understand.
fn number_words(tok: &str) -> Option<Vec<String>> {
    // Ordinal suffix.
    let (num, ordinal) = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suf| tok.strip_suffix(suf).map(|n| (n, true)))
        .unwrap_or((tok, false));
    let has_comma = num.contains(',');
    let num = num.replace(',', "");
    let (int, frac) = match num.split_once('.') {
        Some((i, f)) => (i.to_owned(), Some(f.to_owned())),
        None => (num, None),
    };
    if int.is_empty() || !int.chars().all(|c| c.is_ascii_digit()) || int.len() > 12 {
        return None;
    }
    let n: u64 = int.parse().ok()?;
    let mut words = if frac.is_none() && !has_comma && !ordinal && int.len() == 4 && is_year(n) {
        let (hi, lo) = (n / 100, n % 100);
        let mut w = cardinal(hi);
        if lo == 0 {
            w.push("hundred".into());
        } else {
            if lo < 10 {
                w.push("oh".into());
            }
            w.extend(cardinal(lo));
        }
        w
    } else {
        cardinal(n)
    };
    if let Some(f) = frac {
        if !f.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if !f.is_empty() {
            words.push("point".into());
            words.extend(
                f.chars()
                    .map(|c| ONES[c as usize - '0' as usize].to_owned()),
            );
        }
    }
    if ordinal {
        let last = words.pop()?;
        words.push(ordinal_of(&last));
    }
    Some(words)
}

fn is_year(n: u64) -> bool {
    (1100..=2099).contains(&n) && !(2000..=2009).contains(&n)
}

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

fn cardinal(n: u64) -> Vec<String> {
    if n == 0 {
        return vec!["zero".into()];
    }
    let mut out = Vec::new();
    let mut rest = n;
    for (scale, name) in [
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ] {
        if rest >= scale {
            out.extend(below_thousand(rest / scale));
            out.push(name.into());
            rest %= scale;
        }
    }
    out.extend(below_thousand(rest));
    out
}

fn below_thousand(n: u64) -> Vec<String> {
    let mut out = Vec::new();
    let (h, r) = (n / 100, n % 100);
    if h > 0 {
        out.push(ONES[h as usize].into());
        out.push("hundred".into());
    }
    if r >= 20 {
        out.push(TENS[(r / 10) as usize].into());
        if r % 10 > 0 {
            out.push(ONES[(r % 10) as usize].into());
        }
    } else if r > 0 {
        out.push(ONES[r as usize].into());
    }
    out
}

fn ordinal_of(w: &str) -> String {
    match w {
        "one" => "first".into(),
        "two" => "second".into(),
        "three" => "third".into(),
        "five" => "fifth".into(),
        "eight" => "eighth".into(),
        "nine" => "ninth".into(),
        "twelve" => "twelfth".into(),
        w if w.ends_with('y') => format!("{}ieth", &w[..w.len() - 1]),
        w => format!("{w}th"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s).join(" ")
    }

    #[test]
    fn wer_counts_edits_ignoring_case_and_punctuation() {
        assert_eq!(word_errors("Hello, world.", "hello world"), (0, 2));
        assert_eq!(word_errors("the cat sat", "the bat sat down"), (2, 3));
        assert_eq!(word_errors("a b c", ""), (3, 3));
        assert_eq!(word_errors("Don't stop", "don't stop"), (0, 2));
        assert_eq!(word_errors("double check", "double-check"), (0, 2));
    }

    #[test]
    fn errors_by_kind() {
        let kinds = |r, h| {
            let e = Errors::of(r, h);
            (e.sub, e.del, e.ins, e.words)
        };
        assert_eq!(kinds("the cat sat", "The bat sat down."), (1, 0, 1, 3));
        assert_eq!(kinds("we went home today", "We went today."), (0, 1, 0, 4));
        assert_eq!(kinds("a b", "a b"), (0, 0, 0, 2));
        assert_eq!(
            edit_script(&["a", "b"], &["a", "c", "b"]),
            [Edit::Same(0, 0), Edit::Ins(1), Edit::Same(1, 2)]
        );
    }

    #[test]
    fn numbers_become_words() {
        assert_eq!(n("7 days"), "seven days");
        assert_eq!(
            n("the last 10 to 15 years."),
            "the last ten to fifteen years"
        );
        assert_eq!(n("1,250"), "one thousand two hundred fifty");
        assert_eq!(n("3.5%"), "three point five percent");
        assert_eq!(n("$20"), "twenty dollars");
        assert_eq!(n("the 21st"), "the twenty first");
        assert_eq!(n("in 1984"), "in nineteen eighty four");
        assert_eq!(n("in 2021"), "in twenty twenty one");
        assert_eq!(n("in 1905"), "in nineteen oh five");
        assert_eq!(n("in 1900"), "in nineteen hundred");
        assert_eq!(n("in 2005"), "in two thousand five");
        assert_eq!(n("4th 20th 2nd"), "fourth twentieth second");
    }

    #[test]
    fn fillers_and_spoken_forms() {
        assert_eq!(n("Well, uh, Mr. Smith, um... OK"), "well mister smith okay");
        assert_eq!(n("Q&A"), "q and a");
        assert_eq!(n("organisations"), "organizations");
    }
}
