//! The settings window: native controls (AppKit on macOS, Win32 on Windows) for settings
//! the app describes as [`Page`]s of [`Row`]s. The app keeps the meaning; this crate only
//! shows rows and reports what the user did to them ([`Event`], by [`Row::id`]).
//!
//! After every change (and whenever [`Model::changed_elsewhere`] says so) the window asks
//! for the pages again and shows them: a choice may add or remove rows (e.g. the polish
//! provider's), and the file is the only place settings live.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(windows, target_os = "macos")))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;

use agt_platform::Key;
use std::time::Duration;

pub type Error = Box<dyn std::error::Error>;

/// One page of the window (a toolbar tab on macOS, a sidebar entry on Windows).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub title: String,
    /// SF Symbol shown in the macOS toolbar; Windows shows a look-alike icon in its page
    /// list (none for a symbol it doesn't know).
    pub symbol: &'static str,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// "" for none.
    pub title: String,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Reported back in [`Event`]s; unique within one [`Model::pages`] result.
    pub id: usize,
    /// Shown left of the control ("" for none, e.g. for a checkbox, which has its own text).
    pub label: String,
    pub control: Control,
    /// A line or two of help under the control ("" for none).
    pub note: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// An on/off setting with its own text: a checkbox on macOS, a switch on Windows.
    Toggle { text: String, on: bool },
    /// A pop-up list; `selected` indexes `options`.
    Choice {
        options: Vec<String>,
        selected: usize,
    },
    /// A one-line text field, reported when editing ends (Return, or focus moves on) and
    /// the text differs. `secret`: shown as dots.
    Text {
        value: String,
        placeholder: String,
        secret: bool,
    },
    /// A few lines of text (e.g. a prompt), reported like [`Control::Text`].
    LongText { value: String },
    /// Read-only text.
    Value(String),
    /// A push button with this title.
    Button(String),
    /// A hotkey, set by pressing it: a button showing `label` (its keys in plain words) that
    /// listens for a press when clicked, reported as [`Event::Hotkey`]. `listening`: listens
    /// as soon as the row is shown (a row "Add hotkey" just added). `removable`: a minus
    /// button beside it, reported as [`Event::Click`].
    Hotkey {
        label: String,
        listening: bool,
        removable: bool,
    },
    /// Read-only figures in a row, each a large value over a small caption, across the
    /// whole width (the row's label is ignored).
    Stats(Vec<Stat>),
    /// A read-only table across the whole width (the row's label is ignored): the first
    /// column left-aligned, the others right-aligned (numbers), digits evenly spaced.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub value: String,
    pub caption: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Toggle(usize, bool),
    /// Row, option index.
    Choose(usize, usize),
    Text(usize, String),
    Click(usize),
    /// Row, the keys pressed (platform key codes, in the order they went down), or `None`:
    /// listening was cancelled (Esc, anything else done in the window) or, for a row that
    /// asked to listen, couldn't start.
    Hotkey(usize, Option<Vec<u16>>),
}

pub trait Model {
    /// What the window shows, built afresh (from the file, after a change).
    fn pages(&mut self) -> Vec<Page>;
    /// The user changed a row of the latest [`Self::pages`]. `Err`: a message to show.
    fn handle(&mut self, event: Event) -> Result<(), String>;
    /// Asked every couple of seconds: whether the settings changed some other way (the
    /// file was edited), so the pages need showing again.
    fn changed_elsewhere(&mut self) -> bool;
}

/// Records in `pages` what `event` changed, which its control already shows: pages the
/// model gives afterwards that equal these need no rebuild (and the focus, caret and open
/// lists stay as they are). Not a hotkey: the model may refuse the keys, so its row shows what
/// the model gives.
pub(crate) fn note(pages: &mut [Page], event: &Event) {
    let id = match event {
        Event::Toggle(id, _)
        | Event::Choose(id, _)
        | Event::Text(id, _)
        | Event::Click(id)
        | Event::Hotkey(id, _) => *id,
    };
    let row = pages
        .iter_mut()
        .flat_map(|p| &mut p.sections)
        .flat_map(|s| &mut s.rows)
        .find(|r| r.id == id);
    match (row.map(|r| &mut r.control), event) {
        (Some(Control::Toggle { on, .. }), Event::Toggle(_, new)) => *on = *new,
        (Some(Control::Choice { selected, .. }), Event::Choose(_, new)) => *selected = *new,
        (Some(Control::Text { value, .. } | Control::LongText { value }), Event::Text(_, new)) => {
            *value = new.clone()
        }
        _ => {}
    }
}

/// Shows the window, on the page titled `page` (else the first, see [`activate`]), and runs until it's
/// closed. Must be called on the main thread, once. `icon`: the window's, on Windows (macOS
/// shows none).
pub fn run(
    title: &str,
    model: Box<dyn Model>,
    page: Option<&str>,
    icon: Option<Icon>,
) -> Result<(), Error> {
    platform::run(title, model, page, icon)
}

/// Draws the app's icon `n` pixels square: `n * n` RGBA pixels, row by row.
pub type Icon = fn(n: u32) -> Vec<u8>;

/// Brings the window of another process running [`run`] (`pid`) to the front, showing the
/// page titled `page` if given (a title no page has: the first page). False if it couldn't
/// be found.
pub fn activate(pid: u32, page: Option<&str>) -> bool {
    platform::activate(pid, page)
}

/// The index of the page titled `title` (a title no page has: the first page); `None` for
/// no title.
pub(crate) fn page_index(pages: &[Page], title: Option<&str>) -> Option<usize> {
    let title = title?;
    Some(pages.iter().position(|p| p.title == title).unwrap_or(0))
}

/// A hotkey row's text while it listens, until a key is held.
pub(crate) const LISTENING: &str = "Press a key or combo... (Esc cancels)";

/// The longest a key recorder done listening waits for the keys it kept from apps to be
/// released (see [`agt_platform::CaptureHandle::keys_up`]).
pub(crate) const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// A listening hotkey row's text for the keys held so far (platform key codes, in the order
/// they went down): their labels joined with " + ", a key AGT doesn't know as `Key {code}`.
pub(crate) fn held_label(codes: &[u16]) -> String {
    codes
        .iter()
        .map(|&c| Key::from_code(c).map_or_else(|| format!("Key {c}"), |k| k.label().into()))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// The keys of hotkey row `id` on `page`, as the model gave them.
pub(crate) fn hotkey_label(page: &Page, id: usize) -> Option<&str> {
    page.sections
        .iter()
        .flat_map(|s| &s.rows)
        .find(|r| r.id == id)
        .and_then(|r| match &r.control {
            Control::Hotkey { label, .. } => Some(label.as_str()),
            _ => None,
        })
}

/// What `page` (the page shown afresh, if any) means for listening while row `listening`
/// listens (if one does): whether that row is gone (the page has no enabled hotkey row with
/// its id), and the row to start listening on (the first enabled one that asks to, unless
/// it's that one).
pub(crate) fn listening_as_shown(
    page: Option<&Page>,
    listening: Option<usize>,
) -> (bool, Option<usize>) {
    let rows: Vec<_> = page
        .into_iter()
        .flat_map(|p| &p.sections)
        .flat_map(|section| &section.rows)
        .filter(|r| r.enabled)
        .filter_map(|r| match r.control {
            Control::Hotkey {
                listening: wants, ..
            } => Some((r.id, wants)),
            _ => None,
        })
        .collect();
    let gone = listening.is_some_and(|id| !rows.iter().any(|&(r, _)| r == id));
    let wanted = rows
        .iter()
        .find(|&&(_, wants)| wants)
        .map(|&(id, _)| id)
        .filter(|&id| listening != Some(id));
    (gone, wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_ignores_hotkey_events() {
        let row = Row {
            id: 0,
            label: "Hotkey".into(),
            note: String::new(),
            enabled: true,
            control: Control::Hotkey {
                label: "Fn".into(),
                listening: false,
                removable: true,
            },
        };
        let mut pages = vec![Page {
            title: "Settings".into(),
            symbol: "",
            sections: vec![Section {
                title: String::new(),
                rows: vec![row.clone()],
            }],
        }];
        note(&mut pages, &Event::Hotkey(0, Some(vec![1])));
        assert_eq!(pages[0].sections[0].rows[0], row);
    }

    fn hotkey(id: usize, listening: bool, enabled: bool) -> Row {
        Row {
            id,
            label: String::new(),
            note: String::new(),
            enabled,
            control: Control::Hotkey {
                label: format!("F{}", 13 + id),
                listening,
                removable: false,
            },
        }
    }

    fn page(rows: Vec<Row>) -> Page {
        Page {
            title: "Settings".into(),
            symbol: "",
            sections: vec![Section {
                title: String::new(),
                rows,
            }],
        }
    }

    #[test]
    fn held_keys_in_plain_words() {
        let code = |name| Key::parse(name).unwrap().code();
        let (shift, space) = (code("LeftShift"), code("Space"));
        assert_eq!(held_label(&[shift]), "Left Shift");
        assert_eq!(held_label(&[shift, space]), "Left Shift + Space");
        assert_eq!(Key::from_code(0xFFFF), None);
        assert_eq!(held_label(&[shift, 0xFFFF]), "Left Shift + Key 65535");
    }

    #[test]
    fn hotkey_label_is_the_models() {
        let mut button = hotkey(1, false, true);
        button.control = Control::Button("Add hotkey".into());
        let p = page(vec![hotkey(0, false, true), button]);
        assert_eq!(hotkey_label(&p, 0), Some("F13"));
        assert_eq!(hotkey_label(&p, 1), None);
        assert_eq!(hotkey_label(&p, 2), None);
    }

    #[test]
    fn listening_follows_the_page_shown() {
        let one = page(vec![hotkey(0, false, true)]);
        assert_eq!(listening_as_shown(Some(&one), None), (false, None));
        // The listening row is still there: listening goes on.
        assert_eq!(listening_as_shown(Some(&one), Some(0)), (false, None));
        // A row that asks to listen starts, unless it listens already.
        let asks = page(vec![hotkey(0, false, true), hotkey(1, true, true)]);
        assert_eq!(listening_as_shown(Some(&asks), None), (false, Some(1)));
        assert_eq!(listening_as_shown(Some(&asks), Some(1)), (false, None));
        assert_eq!(listening_as_shown(Some(&asks), Some(0)), (false, Some(1)));
        // Gone: no such row, a disabled one, a row that isn't a hotkey, no page.
        assert_eq!(listening_as_shown(Some(&one), Some(5)), (true, None));
        let disabled = page(vec![hotkey(0, true, false)]);
        assert_eq!(listening_as_shown(Some(&disabled), Some(0)), (true, None));
        let mut button = hotkey(0, false, true);
        button.control = Control::Button("Add hotkey".into());
        assert_eq!(
            listening_as_shown(Some(&page(vec![button])), Some(0)),
            (true, None)
        );
        assert_eq!(listening_as_shown(None, Some(0)), (true, None));
        assert_eq!(listening_as_shown(None, None), (false, None));
        // A disabled row doesn't start listening.
        assert_eq!(listening_as_shown(Some(&disabled), None), (false, None));
    }
}
