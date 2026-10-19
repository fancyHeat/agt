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
/// lists stay as they are).
pub(crate) fn note(pages: &mut [Page], event: &Event) {
    let id = match event {
        Event::Toggle(id, _) | Event::Choose(id, _) | Event::Text(id, _) | Event::Click(id) => *id,
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
