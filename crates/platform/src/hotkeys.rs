//! The configured hotkeys: any of them dictates.

use crate::Hotkey;
use std::fmt;

/// The configured hotkeys (`[hotkey] key`, one or a list), at least one. None is the same as
/// another or contains another: holding the longer one would start the shorter one's
/// dictation first. Sharing keys is fine (`LeftShift+A` and `LeftShift+B`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkeys(Vec<Hotkey>);

impl Hotkeys {
    /// Parses the file's names in order (see [`Hotkey::parse`]). An entry that isn't a hotkey
    /// fails with [`Hotkey::parse`]'s error; one that breaks the rules with
    /// [`Self::check_new`]'s.
    pub fn parse(specs: &[String]) -> Result<Hotkeys, String> {
        if specs.is_empty() {
            return Err("At least one hotkey is needed.".into());
        }
        let mut hotkeys = Hotkeys(Vec::with_capacity(specs.len()));
        for spec in specs {
            let new = Hotkey::parse(spec)?;
            hotkeys.check_new(&new, None)?;
            hotkeys.0.push(new);
        }
        Ok(hotkeys)
    }

    pub fn list(&self) -> &[Hotkey] {
        &self.0
    }

    /// Whether `new` can join the list, in place of entry `replacing` if given (re-recording
    /// a row): it must not be the same keys as another entry, contain one or be contained in
    /// one. Errors are plain sentences for the settings window, with the shorter hotkey first.
    pub fn check_new(&self, new: &Hotkey, replacing: Option<usize>) -> Result<(), String> {
        for (i, old) in self.0.iter().enumerate() {
            if Some(i) == replacing {
                continue;
            }
            if old.same_keys(new) {
                return Err("That's already a hotkey.".into());
            }
            let (short, long) = if new.contains(old) {
                (old, new)
            } else if old.contains(new) {
                (new, old)
            } else {
                continue;
            };
            return Err(format!(
                "{} is already a hotkey, and {} starts with it.",
                short.label(),
                long.label()
            ));
        }
        Ok(())
    }
}

/// As the file names them, joined with ", " (`Fn, LeftControl+Space`).
impl fmt::Display for Hotkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, hotkey) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{hotkey}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_rules() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Hotkeys::parse(&[]).unwrap_err(),
            "At least one hotkey is needed."
        );
        assert_eq!(
            Hotkeys::parse(&s(&["F13", "f13"])).unwrap_err(),
            "That's already a hotkey."
        );
        // The brief's F13 and F13+A: F13+A is refused (two keys that aren't modifiers).
        assert_eq!(
            Hotkeys::parse(&s(&["LeftShift", "LeftShift+A"])).unwrap_err(),
            "Left Shift is already a hotkey, and Left Shift + A starts with it."
        );
        let ok = Hotkeys::parse(&s(&["LeftShift+A", "LeftShift+B"])).unwrap();
        assert_eq!(ok.to_string(), "LeftShift+A, LeftShift+B");
        assert!(
            ok.check_new(&Hotkey::parse("LeftShift+A").unwrap(), Some(0))
                .is_ok()
        );
        assert!(
            ok.check_new(&Hotkey::parse("LeftShift+A").unwrap(), Some(1))
                .is_err()
        );
    }

    #[test]
    fn list_order_and_errors() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let one = Hotkeys::parse(&s(&["LeftShift+A"])).unwrap();
        assert_eq!(one.list(), [Hotkey::parse("LeftShift+A").unwrap()]);
        assert_eq!(one.to_string(), "LeftShift+A");
        // The shorter one comes first whichever is new.
        assert_eq!(
            one.check_new(&Hotkey::parse("LeftShift").unwrap(), None)
                .unwrap_err(),
            "Left Shift is already a hotkey, and Left Shift + A starts with it."
        );
        assert_eq!(
            one.check_new(&Hotkey::parse("a+leftshift").unwrap(), None)
                .unwrap_err(),
            "That's already a hotkey."
        );
        // An entry that isn't a hotkey fails with its own error, unchanged.
        assert_eq!(
            Hotkeys::parse(&s(&["F13", "Hyper"])).unwrap_err(),
            Hotkey::parse("Hyper").unwrap_err()
        );
    }
}
