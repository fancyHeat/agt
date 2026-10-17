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
    /// SF Symbol shown in the macOS toolbar (ignored elsewhere).
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
    /// A checkbox with its own text.
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

/// Shows the window and runs until it's closed. Must be called on the main thread, once.
pub fn run(title: &str, model: Box<dyn Model>) -> Result<(), Error> {
    platform::run(title, model)
}

/// Brings the window of another process running [`run`] (`pid`) to the front. False if it
/// couldn't be found.
pub fn activate(pid: u32) -> bool {
    platform::activate(pid)
}
