//! The meeting notes windows: the side pill and the notes window, two web pages
//! (`ui/pill.html`, `ui/notes.html`, embedded) in the system webview (wry: WKWebView on macOS,
//! WebView2 on Windows), and the messages both ways (the IPC contract in the meeting notes
//! design). [`crate::meeting`] decides what they show; this module only shows it. Everything
//! here runs on the UI thread.
//!
//! - Pages post JSON with `window.ipc.postMessage`; the handlers parse it ([`FromNotes`],
//!   [`FromPill`]) and post it to the event loop. Rust answers with `window.agt.receive(msg)`
//!   ([`ToNotes`], [`ToPill`]) through `evaluate_script`. Until a page has loaded, what it
//!   would get is queued (levels are dropped).
//! - The pill never takes focus: on macOS a non-activating `NSPanel` (like the dictation
//!   overlay) shown with `orderFrontRegardless`, on Windows a `WS_EX_NOACTIVATE` tool window
//!   shown with `SW_SHOWNOACTIVATE`. Clicking it doesn't activate AGT either, so the call app
//!   keeps the keyboard. It is left out of screen sharing and recordings (macOS
//!   `sharingType = none`, Windows `WDA_EXCLUDEFROMCAPTURE`), floats above other windows and
//!   (macOS) joins every Space, full-screen ones included.
//! - Hover and drag are Rust's job: WKWebView may not report the mouse entering a window of an
//!   app that isn't active, and a native drag started from an asynchronous message can miss
//!   its mouse-down. So while the pill shows, a timer ([`TICK`], about 15 times a second, in
//!   the event loop's `WaitUntil`: no thread, no busy loop) reads the global cursor position
//!   and decides whether the pill is expanded ([`Hover`]); after the page's `drag` it follows
//!   the cursor vertically, at [`DRAG_TICK`], while the primary button stays down. The same
//!   timer sends the levels. While the pill is hidden nothing runs.
//! - The notes window is a normal window, created on first use. Closing it hides it (after
//!   `window.agt.flush()`, so an edit still waiting for its autosave is sent): the page keeps
//!   its state, and opening it again is instant. On macOS AGT then gives the focus back to the
//!   app before it, as closing an app's last window would.
//! - Each webview shows its page and nothing else ([`own_page`]): no navigation away (a URL or
//!   file dropped outside a text field would otherwise replace the page, which wry gives
//!   `window.ipc` too, and keep getting the transcript), no new windows, and IPC only from the
//!   page itself. Text dropped into the notes editor still lands there. macOS loads the pages
//!   with `with_html`; Windows serves them on a custom protocol ([`SERVED`]).

use crate::{AppEvent, meeting::Msg};
use agt_audio::Level;
use agt_meeting::{Line, Meeting, MeetingInfo, SystemAudio};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};
use winit::{
    dpi::{LogicalPosition, LogicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoopProxy},
    window::{Window, WindowId},
};
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder};

const NOTES_HTML: &str = include_str!("../ui/notes.html");
const PILL_HTML: &str = include_str!("../ui/pill.html");

/// The two pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Notes,
    Pill,
}

impl Page {
    fn html(self) -> &'static str {
        match self {
            Page::Notes => NOTES_HTML,
            Page::Pill => PILL_HTML,
        }
    }

    /// Its file name on the custom protocol ([`SERVED`]).
    fn file(self) -> &'static str {
        match self {
            Page::Notes => "notes.html",
            Page::Pill => "pill.html",
        }
    }
}

/// How the pages are loaded. macOS: `with_html`, a document at `about:blank` (null origin),
/// measured there for its navigation and for its IPC messages. Windows: WebView2 loads
/// `with_html` through `NavigateToString`, and then reports a `data:` URL holding the whole page
/// as the source of every IPC message; wry turns that into an `http::Uri`, which can't be over
/// 64 KB, so nothing the notes page (over 90 KB) sends would arrive. So on Windows each page is
/// served on the [`SCHEME`] custom protocol and loaded from its URL, which WebView2 shows as
/// `https://agt.localhost/<file>` ([`page_url`]). Untested on Windows.
const SERVED: bool = cfg!(windows);
/// The custom protocol's name.
const SCHEME: &str = "agt";

/// How often the pill looks at the cursor and the meters move: about 15 times a second.
const TICK: Duration = Duration::from_millis(66);
/// While the pill is dragged it follows the cursor once a frame.
const DRAG_TICK: Duration = Duration::from_millis(16);
/// The expanded pill collapses this long after the cursor left it.
const COLLAPSE_AFTER: Duration = Duration::from_millis(600);

/// The notes window: its title, size and smallest size, in points.
const NOTES_TITLE: &str = "AGT Meeting notes";
const NOTES_SIZE: (f64, f64) = (460.0, 760.0);
const NOTES_MIN: (f64, f64) = (380.0, 520.0);
/// Its distance from the screen's right edge: clear of the pill.
const NOTES_RIGHT: f64 = 72.0;
/// ... and from the top, on screens with room to spare.
const NOTES_TOP: f64 = 48.0;

// ------------------------------------------------------------------ messages

/// From the notes page.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromNotes {
    /// The page loaded (again).
    Ready,
    Start,
    Stop,
    Pause,
    Resume,
    /// The user's notes for meeting `id` (debounced).
    Notes {
        id: String,
        notes: String,
    },
    Title {
        id: String,
        title: String,
    },
    /// Write the AI notes for `id`.
    Summarize {
        id: String,
    },
    /// Put `text` on the clipboard (the page sends the meeting's Markdown), or meeting `id`'s
    /// Markdown as it is on disk.
    Copy {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    OpenFolder {
        id: String,
    },
    /// The page shows the meetings list: send it.
    List,
    Open {
        id: String,
    },
    /// The system audio permission's settings page.
    OpenSettings,
}

/// From the pill page.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromPill {
    OpenNotes,
    Stop,
    Pause,
    Resume,
    /// "Take notes" on the prompt.
    Start,
    /// The prompt's x.
    Dismiss,
    /// The mouse entered (`on`) or left the page. Only ever expands it ([`Hover`]).
    Hover {
        on: bool,
    },
    /// Mouse down on the grip or empty space: follow the cursor while the button is down.
    Drag,
}

/// What the pages send, and the pill page finishing loading (it sends no `ready`).
#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    Notes(FromNotes),
    Pill(FromPill),
    PillLoaded,
}

/// To the notes page.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToNotes {
    /// Show this meeting; `live` while it records (or finishes).
    Meeting {
        meeting: MeetingOut,
        live: Option<Live>,
    },
    /// A new line of meeting `id`.
    Line {
        id: String,
        line: Line,
    },
    /// Meeting `id` is recording, paused or finishing; `None`: it stopped.
    Live {
        id: String,
        live: Option<Live>,
    },
    /// The live tracks' levels, 0..1 ([`level`]).
    Levels {
        me: f32,
        them: f32,
    },
    Summary {
        id: String,
        state: SummaryState,
        #[serde(skip_serializing_if = "Option::is_none")]
        markdown: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        /// The title the notes gave the meeting (it had none).
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Meetings {
        list: Vec<MeetingInfo>,
    },
    /// Something went wrong (or is worth knowing) that the user should hear of: one line
    /// under the header, until dismissed. `id`: about that meeting; `None`: about none (a start
    /// that failed). The page keeps the last one per meeting (and for none).
    Notice {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        text: String,
    },
    /// The notice `text` about `id` no longer holds (the speech model has loaded): the page
    /// drops it, unless a newer one took its place.
    ClearNotice {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        text: String,
    },
}

impl ToNotes {
    /// The AI notes for `id` are being written.
    pub fn running(id: &str) -> ToNotes {
        ToNotes::Summary {
            id: id.into(),
            state: SummaryState::Running,
            markdown: None,
            by: None,
            title: None,
            error: None,
        }
    }

    /// The AI notes for `id`, or the message to show.
    pub fn summary(id: &str, result: Result<AiNotes, String>) -> ToNotes {
        let (state, notes, error) = match result {
            Ok(notes) => (SummaryState::Done, Some(notes), None),
            Err(e) => (SummaryState::Error, None, Some(e)),
        };
        let (markdown, by, title) = match notes {
            Some(n) => (Some(n.markdown), Some(n.by), n.title),
            None => (None, None, None),
        };
        ToNotes::Summary {
            id: id.into(),
            state,
            markdown,
            by,
            title,
            error,
        }
    }
}

/// AI notes as written (and saved): the Markdown, what wrote it (`Claude (sonnet)`), and the
/// title they gave the meeting, if it had none.
#[derive(Clone, Debug, PartialEq)]
pub struct AiNotes {
    pub markdown: String,
    pub by: String,
    pub title: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryState {
    Running,
    Done,
    Error,
}

/// A meeting as the notes page gets it: `meeting.json`'s fields and the transcript.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeetingOut {
    pub id: String,
    /// Empty until named: the page shows "New meeting..." while it records or its notes are
    /// written, `default_title` otherwise.
    pub title: String,
    pub default_title: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub notes: String,
    pub summary: Option<String>,
    pub summary_by: Option<String>,
    pub lines: Vec<Line>,
}

impl MeetingOut {
    pub fn new(m: Meeting, lines: Vec<Line>) -> MeetingOut {
        MeetingOut {
            default_title: m.default_title(),
            id: m.id,
            title: m.title,
            started_at: m.started_at,
            ended_at: m.ended_at,
            notes: m.notes,
            summary: m.summary,
            summary_by: m.summary_by,
            lines,
        }
    }
}

/// A recording meeting's state, for the notes page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Live {
    /// False once stopped: what's left is being transcribed.
    pub recording: bool,
    pub paused: bool,
    /// Since the start, pauses included.
    pub elapsed_ms: u64,
    pub system: SystemValue,
    /// Why there is no system audio, with `unsupported` ("system audio needs macOS 14.2 or
    /// later").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_reason: Option<String>,
}

impl Live {
    /// With the session's [`SystemAudio`] and the permission as
    /// [`agt_platform::system_audio_permission`] last said ([`system_value`]).
    pub fn new(
        recording: bool,
        paused: bool,
        elapsed_ms: u64,
        system: &SystemAudio,
        permission: Option<bool>,
    ) -> Live {
        let system_reason = match system {
            SystemAudio::Unsupported(why) if !why.trim().is_empty() => Some(why.trim().into()),
            _ => None,
        };
        Live {
            recording,
            paused,
            elapsed_ms,
            system: system_value(system, permission),
            system_reason,
        }
    }
}

/// How the "Them" track is doing, as the notes page shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SystemValue {
    Ok,
    /// "AGT can't hear the other side yet." with a button to the settings.
    NoPermission,
    /// The mic only: "AGT records your microphone only: <system_reason>."
    Unsupported,
    /// Allowed, but nothing comes: "AGT doesn't hear anything from the other side yet."
    Silent,
}

/// The page's value for the session's [`SystemAudio`], given
/// [`agt_platform::system_audio_permission`]. A track that can't run at all is `unsupported`
/// whatever the permission says (allowing it wouldn't help). Otherwise a permission known to
/// be missing is `no-permission`, and so is silence while it isn't known (the tap delivers
/// zeros without it, and the check is a private API that may say nothing). Silence with the
/// permission granted is `silent`: nothing plays, or the capture is stuck, and the settings
/// wouldn't help.
pub fn system_value(system: &SystemAudio, permission: Option<bool>) -> SystemValue {
    match (system, permission) {
        (SystemAudio::Unsupported(_), _) => SystemValue::Unsupported,
        (_, Some(false)) => SystemValue::NoPermission,
        (SystemAudio::Silent, None) => SystemValue::NoPermission,
        (SystemAudio::Silent, Some(true)) => SystemValue::Silent,
        (SystemAudio::Ok, _) => SystemValue::Ok,
    }
}

/// To the pill page.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToPill {
    Pill {
        state: PillState,
        expanded: bool,
        elapsed_ms: u64,
        /// The app using the mic, for the prompt.
        #[serde(skip_serializing_if = "Option::is_none")]
        app: Option<String>,
    },
    /// The max of the two drives the bars; 0..1 ([`level`]).
    Levels { me: f32, them: f32 },
    /// Something went wrong (the notes page has the details): the pill flashes, and shows
    /// `text` on hover.
    Notice { text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PillState {
    Recording,
    Paused,
    /// "Meeting detected": another app uses the mic.
    Prompt,
    /// Stopped; the last segments are being transcribed.
    Finishing,
}

/// What the pill shows.
#[derive(Clone, Debug, PartialEq)]
pub struct PillView {
    pub state: PillState,
    pub elapsed_ms: u64,
    pub app: Option<String>,
}

/// The URL `page`'s webview reports for it, in its navigation and as its IPC messages'
/// sender: `about:blank` for `with_html`, or (`served`, see [`SERVED`]) its URL on the custom
/// protocol as WebView2 has it.
fn page_url(page: Page, served: bool) -> String {
    if served {
        format!("https://{SCHEME}.localhost/{}", page.file())
    } else {
        "about:blank".into()
    }
}

/// Whether `url` is `page` itself ([`page_url`]); nothing else is (a dropped link or file, a
/// page navigating away, the other page).
fn own_page(page: Page, url: &str) -> bool {
    own_page_on(page, url, SERVED)
}

fn own_page_on(page: Page, url: &str, served: bool) -> bool {
    url == page_url(page, served)
}

/// `builder` loading `page`: `with_html`, or on Windows from the custom protocol ([`SERVED`]).
fn load(builder: WebViewBuilder<'_>, page: Page) -> WebViewBuilder<'_> {
    if !SERVED {
        return builder.with_html(page.html());
    }
    let builder = builder
        .with_custom_protocol(SCHEME.into(), move |_, req| serve(page, req.uri().path()))
        // WebView2 navigates to this as https://agt.localhost/... (see `https`).
        .with_url(format!("{SCHEME}://localhost/{}", page.file()));
    https(builder)
}

/// WebView2 serves custom protocols on `https://<scheme>.localhost` (a secure context: no
/// mixed content) instead of wry's default `http://`.
#[cfg(windows)]
fn https(builder: WebViewBuilder<'_>) -> WebViewBuilder<'_> {
    use wry::WebViewBuilderExtWindows as _;
    builder.with_https_scheme(true)
}

#[cfg(not(windows))]
fn https(builder: WebViewBuilder<'_>) -> WebViewBuilder<'_> {
    builder
}

/// The custom protocol's answer for `path` ([`SERVED`]): the page at its own path, nothing
/// anywhere else.
fn serve(page: Page, path: &str) -> wry::http::Response<Cow<'static, [u8]>> {
    let empty = || Cow::Borrowed(&[][..]);
    let response = if path.strip_prefix('/') == Some(page.file()) {
        wry::http::Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Cow::Borrowed(page.html().as_bytes()))
    } else {
        wry::http::Response::builder().status(404).body(empty())
    };
    response.unwrap_or_else(|e| {
        tracing::error!("meeting window: cannot answer for {path}: {e}");
        wry::http::Response::new(empty())
    })
}

/// `builder` locked to `page` ([`own_page`]): navigation elsewhere and new windows refused.
fn locked(builder: WebViewBuilder<'_>, page: Page) -> WebViewBuilder<'_> {
    builder
        .with_navigation_handler(move |url| {
            let own = own_page(page, &url);
            if !own {
                let shown: String = url.chars().take(80).collect();
                tracing::info!("meeting window: not leaving the page for {shown}");
            }
            own
        })
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
}

/// A page's message, if it came from `page` itself (see [`own_page`]).
fn from_page<T: serde::de::DeserializeOwned>(
    page: Page,
    req: &wry::http::Request<String>,
) -> Option<T> {
    let sender = req.uri().to_string();
    if !own_page(page, &sender) {
        tracing::warn!("meeting window: ignoring a message from another page");
        return None;
    }
    match serde_json::from_str(req.body()) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::debug!("meeting window: unknown message: {e}");
            None
        }
    }
}

/// The script that hands `msg` to a page.
fn receive(msg: &impl Serialize) -> String {
    match serde_json::to_string(msg) {
        Ok(json) => format!("window.agt.receive({json})"),
        Err(e) => {
            tracing::error!("cannot encode a message for a meeting window: {e}");
            "window.agt.receive(null)".into()
        }
    }
}

/// A track's level for the meters, 0..1: its RMS in dBFS from -60 (0) to -15 (1), so speech
/// on a call (around -30 dBFS) sits in the middle. Two decimals: the pages need no more.
pub fn level(rms: f32) -> f32 {
    // Also NaN.
    if rms.is_nan() || rms <= 0.0 {
        return 0.0;
    }
    let v = ((20.0 * rms.log10() + 60.0) / 45.0).clamp(0.0, 1.0);
    (v * 100.0).round() / 100.0
}

// ------------------------------------------------------------------ pill geometry

/// A rectangle in screen points; `x`, `y` is its smallest corner. That is the top left on
/// Windows (y down) and the bottom left on macOS (y up); nothing here depends on which.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// Edges included: the cursor reaches the screen's last point.
    fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.x + self.w && p.y >= self.y && p.y <= self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// The pill's window sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Recording or paused, and finishing: 44 x 120 with its right 8 points off-screen, so it
    /// reads as half out of the edge.
    Collapsed,
    /// Hovered: 52 x 212, fully on screen 8 points from the edge.
    Expanded,
    /// The detection card: 300 x 88, 8 points from the edge.
    Prompt,
}

impl Shape {
    /// Width, height, and how far the right side is from the screen's edge (negative:
    /// beyond it).
    fn size(self) -> (f64, f64, f64) {
        match self {
            Shape::Collapsed => (44.0, 120.0, -8.0),
            Shape::Expanded => (52.0, 212.0, 8.0),
            Shape::Prompt => (300.0, 88.0, 8.0),
        }
    }
}

/// Where the pill's middle goes until the user drags it: 40% down the screen's visible frame
/// (`y_up`: macOS, whose y counts from the bottom).
pub fn default_center(screen: Rect, y_up: bool) -> f64 {
    let from_top = 0.4 * screen.h;
    if y_up {
        screen.y + screen.h - from_top
    } else {
        screen.y + from_top
    }
}

/// `center` (the pill's middle) moved so even the expanded pill fits on `screen`.
fn clamp_center(center: f64, screen: Rect) -> f64 {
    let half = Shape::Expanded.size().1 / 2.0;
    center.clamp(
        screen.y + half,
        (screen.y + screen.h - half).max(screen.y + half),
    )
}

/// The pill's window for `shape` against the right edge of `screen` (its visible frame: the
/// menu bar, Dock and taskbar left out), centered on `center` but kept on screen.
pub fn pill_rect(shape: Shape, screen: Rect, center: f64) -> Rect {
    let (w, h, inset) = shape.size();
    let top = (screen.y + screen.h - h).max(screen.y);
    Rect {
        x: screen.x + screen.w - inset - w,
        y: (center - h / 2.0).clamp(screen.y, top).round(),
        w,
        h,
    }
}

/// Where the cursor expands the pill and where it keeps it expanded.
pub struct Zones {
    /// The collapsed pill's visible part.
    collapsed: Rect,
    /// The expanded pill, stretched to the screen's edge: a cursor parked in the last few
    /// points (between the pill and the edge) is still on it, so it doesn't open and close.
    expanded: Rect,
}

impl Zones {
    pub fn new(screen: Rect, center: f64) -> Zones {
        let right = screen.x + screen.w;
        let mut collapsed = pill_rect(Shape::Collapsed, screen, center);
        collapsed.w = right - collapsed.x;
        let mut expanded = pill_rect(Shape::Expanded, screen, center);
        expanded.w = right - expanded.x;
        Zones {
            collapsed,
            expanded,
        }
    }
}

/// Whether the pill is expanded, from where the cursor is: on the collapsed pill expands it;
/// off the expanded one for [`COLLAPSE_AFTER`] collapses it.
#[derive(Debug, Default)]
pub struct Hover {
    expanded: bool,
    /// When the cursor left the expanded pill.
    left_at: Option<Instant>,
}

impl Hover {
    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// The cursor is at `cursor` (`None`: unknown, counts as away). Returns whether
    /// [`Self::expanded`] changed.
    pub fn update(&mut self, now: Instant, cursor: Option<Point>, zones: &Zones) -> bool {
        let on = |r: &Rect| cursor.is_some_and(|c| r.contains(c));
        if !self.expanded {
            if on(&zones.collapsed) {
                self.expanded = true;
                self.left_at = None;
                return true;
            }
        } else if on(&zones.expanded) {
            self.left_at = None;
        } else {
            let left = *self.left_at.get_or_insert(now);
            if now.duration_since(left) >= COLLAPSE_AFTER {
                self.expanded = false;
                self.left_at = None;
                return true;
            }
        }
        false
    }

    /// The page saw the mouse come in (`on`) or go. Coming in expands; going is left to
    /// [`Self::update`], which knows where the cursor really is. Returns whether it changed.
    pub fn page(&mut self, on: bool) -> bool {
        if !on {
            return false;
        }
        self.left_at = None;
        !std::mem::replace(&mut self.expanded, true)
    }

    pub fn reset(&mut self) {
        *self = Hover::default();
    }
}

// ------------------------------------------------------------------ windows

/// The pill and the notes window, each created on first use.
pub struct Ui {
    proxy: EventLoopProxy<AppEvent>,
    pill: Option<Pill>,
    notes: Option<Notes>,
    /// The tracks' levels while a meeting records (not paused): sent to the pill and an open
    /// notes window every [`TICK`].
    meters: Option<(Level, Level)>,
    next: Instant,
}

struct Pill {
    window: native::PillWindow,
    loaded: bool,
    /// `None`: hidden.
    view: Option<PillView>,
    hover: Hover,
    /// The middle's position: the default until dragged, then where it was left (for the
    /// rest of the session).
    center: Option<f64>,
    /// Dragging: the cursor's distance from the middle when it started.
    drag: Option<f64>,
    frame: Option<Rect>,
    /// macOS: until when to give back an activation wry asked for (see
    /// [`native::PillWindow::new`]).
    guard_focus_until: Option<Instant>,
}

struct Notes {
    window: Window,
    webview: WebView,
    ready: bool,
    /// Scripts waiting for the page to load.
    queue: Vec<String>,
    visible: bool,
}

impl Ui {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Ui {
        Ui {
            proxy,
            pill: None,
            notes: None,
            meters: None,
            next: Instant::now(),
        }
    }

    // --------------------------------------------------------------- pill

    /// Shows `view` on the pill (creating it the first time), or hides it.
    pub fn set_pill(&mut self, event_loop: &ActiveEventLoop, view: Option<PillView>) {
        if view.is_some() && self.pill.is_none() {
            match self.create_pill(event_loop) {
                Ok(p) => self.pill = Some(p),
                Err(e) => return tracing::error!("cannot show the meeting pill: {e}"),
            }
        }
        self.show_pill(view);
    }

    /// Shows `view` on the pill if there is one (a meeting's pill exists from its start).
    pub fn update_pill(&mut self, view: PillView) {
        self.show_pill(Some(view));
    }

    pub fn hide_pill(&mut self) {
        self.show_pill(None);
    }

    fn show_pill(&mut self, view: Option<PillView>) {
        let Some(pill) = &mut self.pill else { return };
        match view {
            Some(view) => {
                let was_hidden = pill.view.is_none();
                if !matches!(view.state, PillState::Recording | PillState::Paused) {
                    pill.hover.reset();
                }
                if pill.view.as_ref() == Some(&view) {
                    return;
                }
                pill.view = Some(view);
                pill.place();
                if was_hidden {
                    pill.window.show();
                    self.next = Instant::now();
                }
                pill.send_view();
            }
            None => pill.hide(),
        }
    }

    fn create_pill(&self, event_loop: &ActiveEventLoop) -> Result<Pill, String> {
        let (ipc, loaded) = (self.proxy.clone(), self.proxy.clone());
        let builder = load(locked(WebViewBuilder::new(), Page::Pill), Page::Pill)
            .with_transparent(true)
            // The first click on a window of an app that isn't active is a click, not just
            // a focus change.
            .with_accept_first_mouse(true)
            .with_ipc_handler(move |req| {
                if let Some(m) = from_page(Page::Pill, &req) {
                    post(&ipc, UiEvent::Pill(m));
                }
            })
            .with_on_page_load_handler(move |event, _| {
                if let PageLoadEvent::Finished = event {
                    post(&loaded, UiEvent::PillLoaded);
                }
            });
        let (window, guards) = native::PillWindow::new(event_loop, builder)?;
        Ok(Pill {
            window,
            loaded: false,
            view: None,
            hover: Hover::default(),
            center: None,
            drag: None,
            frame: None,
            guard_focus_until: guards.then(|| Instant::now() + Duration::from_millis(500)),
        })
    }

    /// The pill page loaded: it gets its state now (it sends no `ready`).
    pub fn pill_loaded(&mut self) {
        if let Some(pill) = &mut self.pill {
            pill.loaded = true;
            pill.send_view();
        }
    }

    pub fn pill_hover(&mut self, on: bool) {
        if let Some(pill) = &mut self.pill
            && pill.hoverable()
            && pill.hover.page(on)
        {
            pill.place();
            pill.send_view();
        }
    }

    /// Flashes the pill, if it shows, for something that went wrong ([`ToPill::Notice`]).
    pub fn flash_pill(&self, text: &str) {
        if let Some(pill) = &self.pill
            && pill.view.is_some()
        {
            pill.send(&ToPill::Notice { text: text.into() });
        }
    }

    /// The page's mouse-down on the grip: follow the cursor while the button is down.
    pub fn pill_drag(&mut self) {
        let Some(pill) = &mut self.pill else { return };
        let (Some(_), Some(cursor), Some(frame)) =
            (&pill.view, native::cursor(&pill.window), pill.frame)
        else {
            return;
        };
        pill.drag = Some(cursor.y - (frame.y + frame.h / 2.0));
        self.next = Instant::now();
    }

    // --------------------------------------------------------------- notes window

    /// Shows the notes window (creating it the first time) and gives it the keyboard.
    /// Returns false if it can't be created.
    pub fn show_notes(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if self.notes.is_none() {
            match self.create_notes(event_loop) {
                Ok(n) => self.notes = Some(n),
                Err(e) => {
                    tracing::error!("cannot open the meeting notes window: {e}");
                    return false;
                }
            }
        }
        let Some(notes) = &mut self.notes else {
            return false;
        };
        notes.window.set_visible(true);
        notes.window.focus_window();
        notes.visible = true;
        // A deliberate activation: not the one wry asks for, to be given back.
        if let Some(pill) = &mut self.pill {
            pill.guard_focus_until = None;
        }
        self.next = Instant::now();
        true
    }

    fn create_notes(&self, event_loop: &ActiveEventLoop) -> Result<Notes, String> {
        let (mut w, mut h) = NOTES_SIZE;
        let mut attrs = Window::default_attributes()
            .with_title(NOTES_TITLE)
            .with_min_inner_size(LogicalSize::new(NOTES_MIN.0, NOTES_MIN.1))
            .with_visible(false);
        // At the right of the main screen, clear of the pill, on screen even on a small one.
        if let Some(m) = event_loop.primary_monitor() {
            let scale = m.scale_factor();
            let (pos, size) = (
                m.position().to_logical::<f64>(scale),
                m.size().to_logical::<f64>(scale),
            );
            w = w.min(size.width - NOTES_RIGHT).max(NOTES_MIN.0);
            h = h.min(size.height - 2.0 * NOTES_TOP).max(NOTES_MIN.1);
            let top = ((size.height - h) / 2.0).max(NOTES_TOP);
            attrs = attrs.with_position(LogicalPosition::new(
                pos.x + size.width - NOTES_RIGHT - w,
                pos.y + top,
            ));
        }
        let attrs = attrs.with_inner_size(LogicalSize::new(w, h));
        let window = event_loop.create_window(attrs).map_err(|e| e.to_string())?;
        native::install_menu();
        let ipc = self.proxy.clone();
        let webview = load(locked(WebViewBuilder::new(), Page::Notes), Page::Notes)
            .with_ipc_handler(move |req| {
                if let Some(m) = from_page(Page::Notes, &req) {
                    post(&ipc, UiEvent::Notes(m));
                }
            })
            .build(&window)
            .map_err(|e| e.to_string())?;
        Ok(Notes {
            window,
            webview,
            ready: false,
            queue: Vec::new(),
            visible: false,
        })
    }

    /// The notes window exists (shown or hidden): its page keeps the state it was sent.
    pub fn notes_exists(&self) -> bool {
        self.notes.is_some()
    }

    pub fn notes_visible(&self) -> bool {
        self.notes.as_ref().is_some_and(|n| n.visible)
    }

    /// Sends `msg` to the notes page, if there is one (queued until it has loaded).
    pub fn send_notes(&mut self, msg: &ToNotes) {
        if let Some(notes) = &mut self.notes {
            notes.send(msg);
        }
    }

    /// The notes page loaded. Returns whether it got anything queued for it.
    pub fn notes_ready(&mut self) -> bool {
        let Some(notes) = &mut self.notes else {
            return false;
        };
        notes.ready = true;
        let queued = !notes.queue.is_empty();
        for script in std::mem::take(&mut notes.queue) {
            notes.eval(&script);
        }
        queued
    }

    /// Saves what the page still holds (`window.agt.flush()`), then hides the window and
    /// (macOS) gives the focus back if AGT has no other window ([`native::notes_hidden`]).
    pub fn hide_notes(&mut self) {
        if let Some(notes) = &mut self.notes {
            notes.eval("window.agt.flush()");
            notes.window.set_visible(false);
            if std::mem::replace(&mut notes.visible, false) {
                native::notes_hidden();
            }
        }
    }

    /// A window event: closing the notes window hides it. Returns whether it was one of ours.
    pub fn window_event(&mut self, id: WindowId, event: &WindowEvent) -> bool {
        if self.notes.as_ref().is_none_or(|n| n.window.id() != id) {
            return false;
        }
        if let WindowEvent::CloseRequested = event {
            self.hide_notes();
        }
        true
    }

    // --------------------------------------------------------------- the timer

    /// The tracks' levels to send while a meeting records (`None`: not recording, or paused).
    pub fn set_meters(&mut self, meters: Option<(Level, Level)>) {
        self.meters = meters;
        self.next = Instant::now();
    }

    /// When [`Self::tick`] is due next; `None` while nothing needs it (no pill, no meters).
    pub fn next_tick(&self) -> Option<Instant> {
        let pill = self.pill.as_ref().is_some_and(|p| p.ticking());
        let meters = self.meters.is_some() && self.notes_visible();
        (pill || meters).then_some(self.next)
    }

    /// Hover, drag and levels; call it when [`Self::next_tick`] is due.
    pub fn tick(&mut self, now: Instant) {
        let levels = self
            .meters
            .as_ref()
            .map(|(me, them)| (level(me.get()), level(them.get())));
        let mut wait = TICK;
        if let Some(pill) = &mut self.pill {
            pill.tick(now, levels);
            if pill.drag.is_some() {
                wait = DRAG_TICK;
            }
        }
        if let (Some((me, them)), Some(notes)) = (levels, &mut self.notes)
            && notes.visible
        {
            notes.send(&ToNotes::Levels { me, them });
        }
        self.next = now + wait;
    }

    /// Before quitting: hides both windows (the notes page saves what it holds). Returns
    /// whether the notes page may still send something, so the event loop should keep
    /// running a moment.
    pub fn prepare_exit(&mut self) -> bool {
        if let Some(pill) = &mut self.pill {
            pill.hide();
        }
        let flushing = self.notes.as_ref().is_some_and(|n| n.ready);
        self.hide_notes();
        flushing
    }
}

/// Posts a page's message to the event loop.
fn post(proxy: &EventLoopProxy<AppEvent>, e: UiEvent) {
    let _ = proxy.send_event(AppEvent::Meeting(Msg::Ui(e)));
}

impl Pill {
    fn hoverable(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| matches!(v.state, PillState::Recording | PillState::Paused))
    }

    /// Shown and something to follow: the cursor (hover, drag) or the levels; or an
    /// activation to give back.
    fn ticking(&self) -> bool {
        self.view.is_some()
            && (self.hoverable() || self.drag.is_some() || self.guard_focus_until.is_some())
    }

    fn shape(&self) -> Shape {
        match self.view.as_ref().map(|v| v.state) {
            Some(PillState::Prompt) => Shape::Prompt,
            _ if self.hover.expanded() && self.hoverable() => Shape::Expanded,
            _ => Shape::Collapsed,
        }
    }

    /// Sizes and places the window for the current state (the screen is read again each
    /// time: displays come and go).
    fn place(&mut self) {
        let Some(screen) = native::screen(&self.window) else {
            return;
        };
        let center = clamp_center(
            self.center
                .unwrap_or_else(|| default_center(screen, native::Y_UP)),
            screen,
        );
        let rect = pill_rect(self.shape(), screen, center);
        if self.frame != Some(rect) {
            self.window.set_frame(rect);
            self.frame = Some(rect);
        }
    }

    fn hide(&mut self) {
        if self.view.take().is_some() {
            self.window.hide();
        }
        self.drag = None;
        self.hover.reset();
    }

    fn send_view(&mut self) {
        let Some(view) = &self.view else { return };
        let msg = ToPill::Pill {
            state: view.state,
            expanded: self.shape() == Shape::Expanded,
            elapsed_ms: view.elapsed_ms,
            app: view.app.clone(),
        };
        self.send(&msg);
    }

    fn send(&self, msg: &ToPill) {
        if self.loaded
            && let Err(e) = self.window.webview().evaluate_script(&receive(msg))
        {
            tracing::debug!("pill: {e}");
        }
    }

    fn tick(&mut self, now: Instant, levels: Option<(f32, f32)>) {
        if self.guard_focus_until.is_some_and(|until| now < until) {
            if native::give_back_activation() {
                self.guard_focus_until = None;
            }
        } else {
            self.guard_focus_until = None;
        }
        let Some(state) = self.view.as_ref().map(|v| v.state) else {
            return;
        };
        let cursor = native::cursor(&self.window);
        if let Some(grab) = self.drag {
            match (
                native::primary_button_down(),
                cursor,
                native::screen(&self.window),
            ) {
                (true, Some(c), Some(screen)) => {
                    self.center = Some(clamp_center(c.y - grab, screen));
                    self.place();
                }
                (true, ..) => {}
                (false, ..) => self.drag = None,
            }
        } else if self.hoverable()
            && let Some(screen) = native::screen(&self.window)
        {
            let center = clamp_center(
                self.center
                    .unwrap_or_else(|| default_center(screen, native::Y_UP)),
                screen,
            );
            if self.hover.update(now, cursor, &Zones::new(screen, center)) {
                self.place();
                self.send_view();
            }
        }
        if state == PillState::Recording
            && let Some((me, them)) = levels
        {
            self.send(&ToPill::Levels { me, them });
        }
    }
}

impl Notes {
    /// Levels are only worth anything now: dropped while the page loads.
    fn send(&mut self, msg: &ToNotes) {
        if self.ready {
            self.eval(&receive(msg));
        } else if !matches!(msg, ToNotes::Levels { .. }) {
            self.queue.push(receive(msg));
        }
    }

    fn eval(&self, script: &str) {
        if let Err(e) = self.webview.evaluate_script(script) {
            tracing::debug!("notes window: {e}");
        }
    }
}

// ------------------------------------------------------------------ platform

#[cfg(target_os = "macos")]
mod native {
    //! A non-activating `NSPanel` for the pill (wry's webview inside it), and the cursor in
    //! Cocoa's global coordinates (points, y up from the bottom of the primary display),
    //! which is also what the panel's frame and the screens' frames are in.

    use super::{Point, Rect};
    use objc2::{MainThreadMarker, MainThreadOnly as _, rc::Retained, runtime::Sel, sel};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSColor, NSEvent, NSEventModifierFlags, NSMenu,
        NSMenuItem, NSPanel, NSScreen, NSStatusWindowLevel, NSView, NSWindowCollectionBehavior,
        NSWindowSharingType, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use std::{ffi::c_void, ptr::NonNull};
    use winit::{
        event_loop::ActiveEventLoop,
        raw_window_handle::{
            AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle,
        },
    };
    use wry::{WebView, WebViewBuilder};

    pub const Y_UP: bool = true;

    pub struct PillWindow {
        mtm: MainThreadMarker,
        panel: Retained<NSPanel>,
        webview: WebView,
    }

    /// The panel's content view, as the window handle wry builds into.
    struct ViewHandle(NonNull<c_void>);

    impl HasWindowHandle for ViewHandle {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(self.0));
            // SAFETY: the view is the panel's content view, alive for the whole build.
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    impl PillWindow {
        /// The hidden panel with the page in it. Also returns whether AGT wasn't active when
        /// wry built the webview: wry then asks for AGT to be activated (macOS 14 may refuse,
        /// earlier versions don't), which [`give_back_activation`] undoes.
        pub fn new(
            _: &ActiveEventLoop,
            builder: WebViewBuilder,
        ) -> Result<(PillWindow, bool), String> {
            let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(44.0, 120.0));
            let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                rect,
                NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                NSBackingStoreType::Buffered,
                false,
            );
            // SAFETY: we own the panel; closing must not free it behind our back.
            unsafe { panel.setReleasedWhenClosed(false) };
            panel.setOpaque(false);
            panel.setBackgroundColor(Some(&NSColor::clearColor()));
            panel.setHasShadow(false);
            // Not `setFloatingPanel(true)`: that would reset the level to floating.
            panel.setLevel(NSStatusWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
            // Not in screen sharing, screenshots or recordings.
            panel.setSharingType(NSWindowSharingType::None);
            // Panels hide when their app deactivates, and AGT is never active.
            panel.setHidesOnDeactivate(false);
            panel.setBecomesKeyOnlyIfNeeded(true);
            let view = NSView::new(mtm);
            panel.setContentView(Some(&view));
            let handle = ViewHandle(NonNull::from(&*view).cast());
            let was_active = NSApplication::sharedApplication(mtm).isActive();
            let webview = builder.build(&handle).map_err(|e| e.to_string())?;
            let window = PillWindow {
                mtm,
                panel,
                webview,
            };
            Ok((window, !was_active))
        }

        pub fn webview(&self) -> &WebView {
            &self.webview
        }

        pub fn set_frame(&self, r: Rect) {
            let frame = NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h));
            self.panel.setFrame_display(frame, true);
        }

        /// In front, without activating AGT or taking the keyboard.
        pub fn show(&self) {
            self.panel.orderFrontRegardless();
        }

        pub fn hide(&self) {
            self.panel.orderOut(None);
        }
    }

    /// The main display's visible frame (no menu bar, no Dock).
    pub fn screen(window: &PillWindow) -> Option<Rect> {
        let f = NSScreen::screens(window.mtm).firstObject()?.visibleFrame();
        Some(Rect {
            x: f.origin.x,
            y: f.origin.y,
            w: f.size.width,
            h: f.size.height,
        })
    }

    pub fn cursor(_: &PillWindow) -> Option<Point> {
        let p = NSEvent::mouseLocation();
        Some(Point { x: p.x, y: p.y })
    }

    pub fn primary_button_down() -> bool {
        NSEvent::pressedMouseButtons() & 1 != 0
    }

    /// The app's main menu: Edit (undo, redo, cut, copy, paste, select all) and Close Window
    /// (Cmd+W), so the notes window's text fields get the usual shortcuts: WebKit takes them
    /// from the menu, and without one Cmd+C and Cmd+V do nothing. Never shown (AGT has no menu
    /// bar). winit's default menu isn't installed (`with_default_menu(false)` in `main`): its
    /// Cmd+Q in the notes window would end AGT on the spot, without stopping a meeting.
    pub fn install_menu() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        if app.mainMenu().is_some() {
            return;
        }
        let item = |title: &str, action: Sel, key: &str, mods: NSEventModifierFlags| {
            // SAFETY: standard responder-chain actions, valid selectors.
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::from_str(key),
                )
            };
            item.setKeyEquivalentModifierMask(mods);
            item
        };
        let submenu = |title: &str, items: &[Retained<NSMenuItem>]| {
            let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
            for i in items {
                menu.addItem(i);
            }
            let holder = NSMenuItem::new(mtm);
            holder.setSubmenu(Some(&menu));
            holder
        };
        let cmd = NSEventModifierFlags::Command;
        let main = NSMenu::new(mtm);
        // The first one is the app menu.
        main.addItem(&submenu(
            "AGT",
            &[item("Close Window", sel!(performClose:), "w", cmd)],
        ));
        main.addItem(&submenu(
            "Edit",
            &[
                item("Undo", sel!(undo:), "z", cmd),
                item("Redo", sel!(redo:), "z", cmd | NSEventModifierFlags::Shift),
                NSMenuItem::separatorItem(mtm),
                item("Cut", sel!(cut:), "x", cmd),
                item("Copy", sel!(copy:), "c", cmd),
                item("Paste", sel!(paste:), "v", cmd),
                item("Select All", sel!(selectAll:), "a", cmd),
            ],
        ));
        app.setMainMenu(Some(&main));
    }

    /// The notes window just hid. AGT is a menu bar app: without a window of its own showing,
    /// it would stay the active app with nowhere to type, so the keyboard would go nowhere
    /// until the user clicked another app. If no other AGT window could take the focus (the
    /// pill and the dictation overlay never do), AGT deactivates, and macOS activates the app
    /// before it. `hide:` would hide the pill too.
    pub fn notes_hidden() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        if !app.isActive() {
            return;
        }
        let other = app
            .windows()
            .to_vec()
            .iter()
            .any(|w| w.isVisible() && w.canBecomeMainWindow());
        if !other {
            tracing::debug!("notes window closed; giving the focus back");
            app.deactivate();
        }
    }

    /// If AGT became active (wry's activation went through), gives the focus back to the app
    /// that had it. Returns whether it did.
    pub fn give_back_activation() -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        if app.isActive() {
            tracing::debug!("the meeting pill's webview activated AGT; deactivating");
            app.deactivate();
            return true;
        }
        false
    }
}

#[cfg(windows)]
mod native {
    //! A borderless, always-on-top tool window that is never activated (not when shown, not
    //! when clicked), and the cursor in logical points of the primary monitor (y down).

    use super::{Point, Rect};
    use windows_sys::Win32::{
        Foundation::{HWND, POINT},
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON},
            WindowsAndMessaging::{
                GWL_EXSTYLE, GetCursorPos, GetWindowLongPtrW, SW_SHOWNOACTIVATE,
                SetWindowDisplayAffinity, SetWindowLongPtrW, ShowWindow, WDA_EXCLUDEFROMCAPTURE,
                WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            },
        },
    };
    use winit::{
        dpi::{LogicalPosition, LogicalSize},
        event_loop::ActiveEventLoop,
        platform::windows::WindowAttributesExtWindows as _,
        raw_window_handle::{HasWindowHandle as _, RawWindowHandle},
        window::{Window, WindowLevel},
    };
    use wry::{WebView, WebViewBuilder};

    pub const Y_UP: bool = false;

    pub struct PillWindow {
        window: Window,
        webview: WebView,
        hwnd: HWND,
    }

    impl PillWindow {
        pub fn new(
            event_loop: &ActiveEventLoop,
            builder: WebViewBuilder,
        ) -> Result<(PillWindow, bool), String> {
            let attrs = Window::default_attributes()
                .with_title("AGT meeting pill")
                .with_visible(false)
                .with_active(false)
                .with_decorations(false)
                .with_transparent(true)
                .with_resizable(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_skip_taskbar(true)
                .with_inner_size(LogicalSize::new(44.0, 120.0));
            let window = event_loop.create_window(attrs).map_err(|e| e.to_string())?;
            let hwnd = match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
                RawWindowHandle::Win32(h) => h.hwnd.get() as HWND,
                _ => return Err("not a Win32 window".into()),
            };
            // SAFETY: our own live window.
            unsafe {
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    ex | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize,
                );
                // Windows 10 2004 and later; older ones just show it in captures.
                SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let webview = builder.build(&window).map_err(|e| e.to_string())?;
            Ok((
                PillWindow {
                    window,
                    webview,
                    hwnd,
                },
                false,
            ))
        }

        pub fn webview(&self) -> &WebView {
            &self.webview
        }

        pub fn set_frame(&self, r: Rect) {
            // Both without activating (winit passes SWP_NOACTIVATE).
            self.window
                .set_outer_position(LogicalPosition::new(r.x, r.y));
            let _ = self.window.request_inner_size(LogicalSize::new(r.w, r.h));
        }

        pub fn show(&self) {
            // SAFETY: our own live window.
            unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        }

        pub fn hide(&self) {
            self.window.set_visible(false);
        }
    }

    /// The primary monitor (the whole of it: the taskbar is rarely on the right).
    pub fn screen(window: &PillWindow) -> Option<Rect> {
        let m = window.window.primary_monitor()?;
        let scale = m.scale_factor();
        let (pos, size) = (
            m.position().to_logical::<f64>(scale),
            m.size().to_logical::<f64>(scale),
        );
        Some(Rect {
            x: pos.x,
            y: pos.y,
            w: size.width,
            h: size.height,
        })
    }

    /// In the primary monitor's logical points (exact on it, which is where the pill is).
    pub fn cursor(window: &PillWindow) -> Option<Point> {
        let scale = window.window.primary_monitor()?.scale_factor();
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY: a valid out pointer.
        if unsafe { GetCursorPos(&mut p) } == 0 {
            return None;
        }
        Some(Point {
            x: f64::from(p.x) / scale,
            y: f64::from(p.y) / scale,
        })
    }

    pub fn primary_button_down() -> bool {
        // SAFETY: no pointers.
        (unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON)) } as u16) & 0x8000 != 0
    }

    pub fn give_back_activation() -> bool {
        false
    }

    /// Windows activates the next window itself when the notes window hides.
    pub fn notes_hidden() {}

    /// Windows has no app menu; text editing shortcuts work without one.
    pub fn install_menu() {}
}

#[cfg(not(any(target_os = "macos", windows)))]
mod native {
    //! No pill on other systems (AGT doesn't run there); this keeps the crate building.

    use super::{Point, Rect};
    use winit::event_loop::ActiveEventLoop;
    use wry::{WebView, WebViewBuilder};

    pub const Y_UP: bool = false;

    pub struct PillWindow {
        webview: WebView,
    }

    impl PillWindow {
        pub fn new(_: &ActiveEventLoop, _: WebViewBuilder) -> Result<(PillWindow, bool), String> {
            Err("not supported on this system".into())
        }
        pub fn webview(&self) -> &WebView {
            &self.webview
        }
        pub fn set_frame(&self, _: Rect) {}
        pub fn show(&self) {}
        pub fn hide(&self) {}
    }

    pub fn screen(_: &PillWindow) -> Option<Rect> {
        None
    }
    pub fn cursor(_: &PillWindow) -> Option<Point> {
        None
    }
    pub fn primary_button_down() -> bool {
        false
    }
    pub fn give_back_activation() -> bool {
        false
    }
    pub fn notes_hidden() {}
    pub fn install_menu() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use agt_meeting::{Line, Meeting, MeetingInfo, Speaker, SystemAudio};
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    fn notes(s: &str) -> FromNotes {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }
    fn pill(s: &str) -> FromPill {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }
    fn out(msg: &impl serde::Serialize) -> Value {
        serde_json::to_value(msg).unwrap()
    }

    #[test]
    fn ipc_messages_parse() {
        assert_eq!(notes(r#"{"type":"ready"}"#), FromNotes::Ready);
        assert_eq!(notes(r#"{"type":"start"}"#), FromNotes::Start);
        assert_eq!(notes(r#"{"type":"stop"}"#), FromNotes::Stop);
        assert_eq!(notes(r#"{"type":"pause"}"#), FromNotes::Pause);
        assert_eq!(notes(r#"{"type":"resume"}"#), FromNotes::Resume);
        assert_eq!(
            notes(r#"{"type":"notes","id":"m1","notes":"Ask about Q3"}"#),
            FromNotes::Notes {
                id: "m1".into(),
                notes: "Ask about Q3".into()
            }
        );
        assert_eq!(
            notes(r#"{"type":"title","id":"m1","title":"Standup"}"#),
            FromNotes::Title {
                id: "m1".into(),
                title: "Standup".into()
            }
        );
        assert_eq!(
            notes(r#"{"type":"summarize","id":"m1"}"#),
            FromNotes::Summarize { id: "m1".into() }
        );
        assert_eq!(
            notes(r##"{"type":"copy","text":"# Standup\n"}"##),
            FromNotes::Copy {
                text: Some("# Standup\n".into()),
                id: None
            }
        );
        assert_eq!(
            notes(r#"{"type":"copy","id":"m1"}"#),
            FromNotes::Copy {
                text: None,
                id: Some("m1".into())
            }
        );
        assert_eq!(
            notes(r#"{"type":"open_folder","id":"m1"}"#),
            FromNotes::OpenFolder { id: "m1".into() }
        );
        assert_eq!(notes(r#"{"type":"list"}"#), FromNotes::List);
        assert_eq!(
            notes(r#"{"type":"open","id":"m1"}"#),
            FromNotes::Open { id: "m1".into() }
        );
        assert_eq!(
            notes(r#"{"type":"open_settings"}"#),
            FromNotes::OpenSettings
        );
        // Extra fields are ignored; unknown types and missing fields are errors.
        assert_eq!(notes(r#"{"type":"stop","from":"chip"}"#), FromNotes::Stop);
        assert!(serde_json::from_str::<FromNotes>(r#"{"type":"explode"}"#).is_err());
        assert!(serde_json::from_str::<FromNotes>(r#"{"type":"notes","id":"m1"}"#).is_err());
        assert!(serde_json::from_str::<FromNotes>("not json").is_err());

        assert_eq!(pill(r#"{"type":"open_notes"}"#), FromPill::OpenNotes);
        assert_eq!(pill(r#"{"type":"stop"}"#), FromPill::Stop);
        assert_eq!(pill(r#"{"type":"pause"}"#), FromPill::Pause);
        assert_eq!(pill(r#"{"type":"resume"}"#), FromPill::Resume);
        assert_eq!(pill(r#"{"type":"start"}"#), FromPill::Start);
        assert_eq!(pill(r#"{"type":"dismiss"}"#), FromPill::Dismiss);
        assert_eq!(
            pill(r#"{"type":"hover","on":true}"#),
            FromPill::Hover { on: true }
        );
        assert_eq!(
            pill(r#"{"type":"hover","on":false}"#),
            FromPill::Hover { on: false }
        );
        assert_eq!(pill(r#"{"type":"drag"}"#), FromPill::Drag);
        assert!(serde_json::from_str::<FromPill>(r#"{"type":"notes"}"#).is_err());
    }

    fn line() -> Line {
        Line {
            id: 1,
            speaker: Speaker::Them,
            start_ms: 0,
            end_ms: 1200,
            text: "Hi all".into(),
        }
    }

    #[test]
    fn ipc_out_shapes() {
        assert_eq!(
            out(&ToNotes::Line {
                id: "m1".into(),
                line: line()
            }),
            json!({"type":"line","id":"m1","line":{"id":1,"speaker":"them","start_ms":0,"end_ms":1200,"text":"Hi all"}})
        );
        let live = Live {
            recording: true,
            paused: false,
            elapsed_ms: 754_000,
            system: SystemValue::NoPermission,
            system_reason: None,
        };
        assert_eq!(
            out(&ToNotes::Live {
                id: "m1".into(),
                live: Some(live)
            }),
            json!({"type":"live","id":"m1","live":{"recording":true,"paused":false,"elapsed_ms":754000,"system":"no-permission"}})
        );
        assert_eq!(
            out(&ToNotes::Live {
                id: "m1".into(),
                live: None
            }),
            json!({"type":"live","id":"m1","live":null})
        );
        let m = Meeting {
            id: "m1".into(),
            title: "Standup".into(),
            started_at: 1_792_161_005_000,
            notes: "Ask about Q3".into(),
            audio: vec!["mic.wav".into()],
            ..Meeting::default()
        };
        let default_title = m.default_title();
        assert_eq!(
            out(&ToNotes::Meeting {
                meeting: MeetingOut::new(m, vec![line()]),
                live: None
            }),
            json!({"type":"meeting","meeting":{"id":"m1","title":"Standup","default_title":default_title,
                "started_at":1_792_161_005_000_i64,
                "ended_at":null,"notes":"Ask about Q3","summary":null,"summary_by":null,
                "lines":[{"id":1,"speaker":"them","start_ms":0,"end_ms":1200,"text":"Hi all"}]},
                "live":null})
        );
        assert_eq!(
            out(&ToNotes::Levels { me: 0.5, them: 0.0 }),
            json!({"type":"levels","me":0.5,"them":0.0})
        );
        assert_eq!(
            out(&ToNotes::running("m1")),
            json!({"type":"summary","id":"m1","state":"running"})
        );
        let notes = |title: Option<&str>| AiNotes {
            markdown: "## Summary".into(),
            by: "Claude (sonnet)".into(),
            title: title.map(Into::into),
        };
        assert_eq!(
            out(&ToNotes::summary("m1", Ok(notes(None)))),
            json!({"type":"summary","id":"m1","state":"done","markdown":"## Summary","by":"Claude (sonnet)"})
        );
        // The title the notes gave an untitled meeting.
        assert_eq!(
            out(&ToNotes::summary(
                "m1",
                Ok(notes(Some("Q3 pricing review")))
            )),
            json!({"type":"summary","id":"m1","state":"done","markdown":"## Summary","by":"Claude (sonnet)",
                "title":"Q3 pricing review"})
        );
        assert_eq!(
            out(&ToNotes::summary(
                "m1",
                Err("Claude took too long. Try again.".into())
            )),
            json!({"type":"summary","id":"m1","state":"error","error":"Claude took too long. Try again."})
        );
        let info = MeetingInfo {
            id: "m1".into(),
            title: String::new(),
            default_title: "Meeting Oct 16, 2:30 PM".into(),
            started_at: 5,
            ended_at: Some(9),
            has_summary: true,
        };
        assert_eq!(
            out(&ToNotes::Meetings { list: vec![info] }),
            json!({"type":"meetings","list":[{"id":"m1","title":"","default_title":"Meeting Oct 16, 2:30 PM",
                "started_at":5,"ended_at":9,"has_summary":true}]})
        );
        assert_eq!(
            out(&ToNotes::Notice {
                id: Some("m1".into()),
                text: "The microphone stopped: unplugged".into()
            }),
            json!({"type":"notice","id":"m1","text":"The microphone stopped: unplugged"})
        );
        assert_eq!(
            out(&ToNotes::Notice {
                id: None,
                text: "Speech model failed to load: model unavailable".into()
            }),
            json!({"type":"notice","text":"Speech model failed to load: model unavailable"})
        );
        assert_eq!(
            out(&ToNotes::ClearNotice {
                id: Some("m1".into()),
                text: "Speech model still loading".into()
            }),
            json!({"type":"clear_notice","id":"m1","text":"Speech model still loading"})
        );

        assert_eq!(
            out(&ToPill::Pill {
                state: PillState::Prompt,
                expanded: false,
                elapsed_ms: 0,
                app: Some("Google Chrome".into())
            }),
            json!({"type":"pill","state":"prompt","expanded":false,"elapsed_ms":0,"app":"Google Chrome"})
        );
        assert_eq!(
            out(&ToPill::Pill {
                state: PillState::Finishing,
                expanded: true,
                elapsed_ms: 61_000,
                app: None
            }),
            json!({"type":"pill","state":"finishing","expanded":true,"elapsed_ms":61000})
        );
        assert_eq!(
            out(&ToPill::Levels {
                me: 1.0,
                them: 0.25
            }),
            json!({"type":"levels","me":1.0,"them":0.25})
        );
        assert_eq!(
            out(&ToPill::Notice {
                text: "The microphone stopped".into()
            }),
            json!({"type":"notice","text":"The microphone stopped"})
        );
        // What evaluate_script runs: a call with the JSON as its argument.
        assert_eq!(
            receive(&ToPill::Levels {
                me: 1.0,
                them: 0.25
            }),
            r#"window.agt.receive({"type":"levels","me":1.0,"them":0.25})"#
        );
    }

    #[test]
    fn only_own_page() {
        // macOS (`with_html`): the document at about:blank, for either page.
        for page in [Page::Notes, Page::Pill] {
            assert!(own_page_on(page, "about:blank", false), "{page:?}");
        }
        // Windows (the custom protocol): each page at its own URL only.
        assert!(own_page_on(
            Page::Notes,
            "https://agt.localhost/notes.html",
            true
        ));
        assert!(own_page_on(
            Page::Pill,
            "https://agt.localhost/pill.html",
            true
        ));
        assert!(!own_page_on(
            Page::Notes,
            "https://agt.localhost/pill.html",
            true
        ));
        assert!(!own_page_on(Page::Notes, "about:blank", true));
        assert!(!own_page_on(
            Page::Notes,
            "https://agt.localhost/notes.html",
            false
        ));
        for other in [
            "",
            "https://example.com/",
            "http://localhost:1/",
            "file:///Users/me/Desktop/notes.txt",
            "about:blank#x",
            "about:srcdoc",
            "javascript:alert(1)",
            // NavigateToString's form: no longer the page on either system.
            "data:text/html;charset=utf-8;base64,PGh0bWw+",
            "data:image/png;base64,AAAA",
            "http://agt.localhost/notes.html",
            "https://agt.localhost/notes.html#x",
            "https://agt.localhost/notes.html?x=1",
            "https://agt.localhost/",
            "https://evil.agt.localhost/notes.html",
            "https://agt.localhost.example.com/notes.html",
            "agt://localhost/notes.html",
        ] {
            for served in [false, true] {
                assert!(
                    !own_page_on(Page::Notes, other, served),
                    "{other} ({served})"
                );
            }
        }
        // This build's own value.
        let mine = page_url(Page::Notes, cfg!(windows));
        assert!(own_page(Page::Notes, &mine));
        assert_eq!(mine == "about:blank", cfg!(not(windows)));
        // What the IPC handler checks: the sender's URI as wry passes it.
        let req = |uri: &str, body: &str| {
            wry::http::Request::builder()
                .uri(uri)
                .body(body.to_owned())
                .unwrap()
        };
        let req_from_page = req(&mine, r#"{"type":"ready"}"#);
        assert_eq!(
            from_page::<FromNotes>(Page::Notes, &req_from_page),
            Some(FromNotes::Ready)
        );
        let other = req("https://example.com/", r#"{"type":"start"}"#);
        assert_eq!(from_page::<FromNotes>(Page::Notes, &other), None);
        // Both pages' URLs are well under http::Uri's limit, unlike a data: URL of the page.
        for served in [false, true] {
            for page in [Page::Notes, Page::Pill] {
                let url = page_url(page, served);
                assert!(url.parse::<wry::http::Uri>().is_ok(), "{url}");
            }
        }
    }

    #[test]
    fn custom_protocol_serves_only_the_page() {
        for page in [Page::Notes, Page::Pill] {
            let ok = serve(page, &format!("/{}", page.file()));
            assert_eq!(ok.status(), 200);
            assert_eq!(ok.body().as_ref(), page.html().as_bytes());
            assert_eq!(
                ok.headers()["Content-Type"],
                "text/html; charset=utf-8",
                "{page:?}"
            );
        }
        for path in [
            "/",
            "/pill.html",
            "/favicon.ico",
            "/notes.html/x",
            "notes.html",
            "",
        ] {
            let other = serve(Page::Notes, path);
            assert_eq!(other.status(), 404, "{path}");
            assert!(other.body().is_empty(), "{path}");
        }
    }

    #[test]
    fn system_value_maps() {
        use SystemValue as V;
        let unsupported = SystemAudio::Unsupported("old macOS".into());
        for permission in [Some(true), Some(false), None] {
            assert_eq!(system_value(&unsupported, permission), V::Unsupported);
        }
        assert_eq!(system_value(&SystemAudio::Ok, Some(true)), V::Ok);
        assert_eq!(system_value(&SystemAudio::Ok, None), V::Ok);
        // The tap runs but delivers zeros without the permission.
        assert_eq!(system_value(&SystemAudio::Ok, Some(false)), V::NoPermission);
        assert_eq!(
            system_value(&SystemAudio::Silent, Some(false)),
            V::NoPermission
        );
        // Not known: most likely the permission, so the page offers the settings.
        assert_eq!(system_value(&SystemAudio::Silent, None), V::NoPermission);
        // Granted: no false alarm about the permission.
        assert_eq!(system_value(&SystemAudio::Silent, Some(true)), V::Silent);
    }

    #[test]
    fn live_carries_the_unsupported_reason() {
        let why = SystemAudio::Unsupported(" system audio needs macOS 14.2 or later ".into());
        let live = Live::new(true, false, 5, &why, Some(true));
        assert_eq!(live.system, SystemValue::Unsupported);
        assert_eq!(
            out(&live),
            json!({"recording":true,"paused":false,"elapsed_ms":5,"system":"unsupported",
                "system_reason":"system audio needs macOS 14.2 or later"})
        );
        for system in [
            SystemAudio::Ok,
            SystemAudio::Silent,
            SystemAudio::Unsupported("  ".into()),
        ] {
            let live = Live::new(true, false, 5, &system, Some(true));
            assert_eq!(live.system_reason, None, "{system:?}");
            assert!(out(&live).get("system_reason").is_none());
        }
        let silent = Live::new(true, true, 9, &SystemAudio::Silent, Some(true));
        assert_eq!(out(&silent)["system"], "silent");
    }

    #[test]
    fn levels_normalize_from_decibels() {
        assert_eq!(level(0.0), 0.0);
        assert_eq!(level(-0.5), 0.0);
        assert_eq!(level(f32::NAN), 0.0);
        assert_eq!(level(0.001), 0.0, "-60 dBFS is the floor");
        assert_eq!(level(0.0001), 0.0);
        assert!(
            (level(10f32.powf(-37.5 / 20.0)) - 0.5).abs() < 1e-4,
            "-37.5 dBFS is half"
        );
        assert!(
            (level(10f32.powf(-15.0 / 20.0)) - 1.0).abs() < 1e-4,
            "-15 dBFS is full"
        );
        assert_eq!(level(1.0), 1.0, "clamped");
    }

    /// The main display's visible frame in a y-down layout (Windows; macOS is y-up, see below).
    const SCREEN: Rect = Rect {
        x: 0.0,
        y: 25.0,
        w: 1440.0,
        h: 875.0,
    };

    #[test]
    fn pill_geometry() {
        let center = default_center(SCREEN, false);
        assert_eq!(center, 25.0 + 0.4 * 875.0);
        let c = pill_rect(Shape::Collapsed, SCREEN, center);
        assert_eq!((c.w, c.h), (44.0, 120.0));
        assert_eq!(c.x + c.w, 1448.0, "its right 8 px are off-screen");
        assert_eq!(c.y + c.h / 2.0, center);
        let e = pill_rect(Shape::Expanded, SCREEN, center);
        assert_eq!((e.w, e.h), (52.0, 212.0));
        assert_eq!(e.x + e.w, 1432.0, "fully on screen, 8 px from the edge");
        assert_eq!(e.y + e.h / 2.0, center, "grows around the same middle");
        let p = pill_rect(Shape::Prompt, SCREEN, center);
        assert_eq!((p.w, p.h, p.x + p.w), (300.0, 88.0, 1432.0));
        // Kept on screen at either end.
        assert_eq!(pill_rect(Shape::Expanded, SCREEN, 0.0).y, 25.0);
        let low = pill_rect(Shape::Expanded, SCREEN, 5000.0);
        assert_eq!(low.y + low.h, 900.0);
        // macOS: y goes up, so 40% from the top is 60% from the bottom.
        let mac = Rect { y: 0.0, ..SCREEN };
        assert_eq!(default_center(mac, true), 0.6 * 875.0);
        // A screen to the left of the primary one has negative x.
        let left = Rect {
            x: -1920.0,
            ..SCREEN
        };
        assert_eq!(pill_rect(Shape::Collapsed, left, center).x, -516.0);
    }

    #[test]
    fn pill_hover() {
        let center = default_center(SCREEN, false);
        let zones = Zones::new(SCREEN, center);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut h = Hover::default();
        // Next to the pill, not on it.
        assert!(!h.update(
            at(0),
            Some(Point {
                x: 1400.0,
                y: center
            }),
            &zones
        ));
        assert!(!h.expanded());
        // On the 36 visible pixels: expands.
        assert!(h.update(
            at(66),
            Some(Point {
                x: 1420.0,
                y: center
            }),
            &zones
        ));
        assert!(h.expanded());
        // Parked in the rightmost pixel (between the expanded pill and the edge): stays.
        assert!(!h.update(
            at(1000),
            Some(Point {
                x: 1439.0,
                y: center
            }),
            &zones
        ));
        assert!(h.expanded());
        // Leaves: collapses 600 ms later, not before.
        let away = Some(Point {
            x: 900.0,
            y: center,
        });
        assert!(!h.update(at(2000), away, &zones));
        assert!(!h.update(at(2590), away, &zones));
        assert!(h.expanded());
        assert!(h.update(at(2600), away, &zones));
        assert!(!h.expanded());
        // Leaving and coming back in time keeps it open.
        h.update(
            at(3000),
            Some(Point {
                x: 1420.0,
                y: center,
            }),
            &zones,
        );
        h.update(at(3100), away, &zones);
        h.update(
            at(3500),
            Some(Point {
                x: 1390.0,
                y: center + 100.0,
            }),
            &zones,
        );
        assert!(!h.update(
            at(4500),
            Some(Point {
                x: 1390.0,
                y: center + 100.0
            }),
            &zones
        ));
        assert!(h.expanded(), "inside the expanded pill");
        // The page may expand it, never collapse it.
        let mut h = Hover::default();
        assert!(h.page(true));
        assert!(h.expanded());
        assert!(!h.page(false));
        assert!(h.expanded());
        // Unknown cursor: counts as away.
        assert!(!h.update(at(20), None, &zones));
        assert!(h.update(at(700), None, &zones));
        assert!(!h.expanded());
    }
}
