//! Windows: a plain Win32 window. A list of pages on the left (like Windows' own Settings), the
//! page on the right: right-aligned labels and their controls (common controls v6, so the
//! executable's manifest must ask for them), in a pane that scrolls if the screen is short.
//! Per-monitor DPI aware: sizes are in DIPs, scaled by the window's DPI.
//!
//! Each control's id is its row's id + [`FIRST_ID`]. Changes are sent to the model as they
//! happen (a pop-up's once chosen). If the model's pages then differ, the page is rebuilt on a
//! zero-length timer, whose message comes after any input still queued (so a click that moved
//! focus out of an edit field, which reported its text, still reaches the control it was on).

use crate::{Control, Error, Event, Model, Page, Row, Stat};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        COLOR_GRAYTEXT, COLOR_WINDOW, CreateFontIndirectW, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE,
        DT_WORDBREAK, DeleteObject, DrawTextW, FW_SEMIBOLD, GetDC, GetMonitorInfoW, GetSysColor,
        GetSysColorBrush, HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, MONITOR_DEFAULTTONEAREST,
        MONITORINFO, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
        TRANSPARENT,
    },
    System::{DataExchange::COPYDATASTRUCT, LibraryLoader::GetModuleHandleW},
    UI::{
        Controls::{
            BST_CHECKED, EM_SETSEL, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
            InitCommonControlsEx, SetScrollInfo,
        },
        HiDpi::{
            AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
        },
        Input::KeyboardAndMouse::{
            EnableWindow, GetFocus, GetKeyState, SetFocus, VK_CONTROL, VK_LBUTTON, VK_SHIFT, VK_TAB,
        },
        WindowsAndMessaging::*,
    },
};

const CLASS: &str = "AGT.Settings";
const PANE_CLASS: &str = "AGT.SettingsPane";

/// The page list's control id; rows start at [`FIRST_ID`] (clear of IDOK and IDCANCEL).
const LIST_ID: i32 = 10;
const FIRST_ID: i32 = 100;

const REBUILD_TIMER: usize = 1;
const POLL_TIMER: usize = 2;
const POLL_MS: u32 = 2000;

// Sizes in DIPs.
const LIST_WIDTH: i32 = 150;
const MARGIN: i32 = 20;
const LABEL_WIDTH: i32 = 170;
const GAP: i32 = 8;
const CONTROL_WIDTH: i32 = 320;
const SCROLLBAR: i32 = 17;
const ROW_SPACING: i32 = 8;
const SECTION_SPACING: i32 = 14;
const LONG_TEXT_HEIGHT: i32 = 72;
const LIST_ITEM_HEIGHT: i32 = 28;
/// A full-width row (a table, figures): both columns and the gap between them.
const FULL_WIDTH: i32 = LABEL_WIDTH + GAP + CONTROL_WIDTH;
/// A table's columns after the first.
const TABLE_COLUMN_WIDTH: i32 = 70;

/// `WM_COPYDATA` from [`activate`]: the page to show (its title, UTF-16).
const SHOW_PAGE: usize = 0x4147_5450;

const EN_KILLFOCUS: u32 = 0x0200;
// Static control styles (in windows-sys' System_SystemServices, not worth the feature).
const SS_LEFT: u32 = 0;
const SS_CENTER: u32 = 1;
const SS_RIGHT: u32 = 2;
const SS_NOPREFIX: u32 = 0x80;
const EM_SETCUEBANNER: u32 = 0x1501;

struct State {
    model: Box<dyn Model>,
    pages: Vec<Page>,
    /// The page shown, and the title of the one the user chose: while the file has an
    /// error that page is missing (the error page shows), and it's shown again once fixed.
    current: usize,
    page: String,
    window: HWND,
    pane: HWND,
    list: HWND,
    dpi: u32,
    /// The page's controls (labels and notes too), destroyed on each rebuild.
    controls: Vec<HWND>,
    /// Row ids of the edit fields and the text they show: a change is only reported when the
    /// text differs.
    shown_text: HashMap<usize, String>,
    /// Pixels the pane is scrolled down by, and the page's height.
    scroll: i32,
    content_height: i32,
    /// The file changed while an edit field had focus: rebuild once it loses it.
    stale: bool,
    /// The id of the control focused when the window was last deactivated (0: none).
    last_focus: i32,
    /// While the user moves the window: whether its DPI (so its size) changed on the way.
    moving: Option<bool>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Kept apart from [`STATE`]: controls ask for them while it's borrowed (being created).
    /// The dialog font, its semibold, and a large semibold for figures.
    static FONTS: Cell<(HFONT, HFONT, HFONT)> =
        const { Cell::new((null_mut(), null_mut(), null_mut())) };
    static NOTES: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    static BACKGROUND: Cell<HBRUSH> = const { Cell::new(null_mut()) };
}

#[derive(Clone, Copy)]
enum Font {
    Normal,
    Bold,
    Big,
}

impl Font {
    fn handle(self) -> HFONT {
        let (font, bold, big) = FONTS.get();
        match self {
            Font::Normal => font,
            Font::Bold => bold,
            Font::Big => big,
        }
    }
}

/// Runs `f` on the state unless it's borrowed (a message sent while handling another): `None`.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut s = s.try_borrow_mut().ok()?;
        s.as_mut().map(f)
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn dip(px: i32, dpi: u32) -> i32 {
    (px as i64 * dpi as i64 / 96) as i32
}

pub fn run(title: &str, mut model: Box<dyn Model>, page: Option<&str>) -> Result<(), Error> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let instance = GetModuleHandleW(null());
        BACKGROUND.set(GetSysColorBrush(COLOR_WINDOW));

        let class = wide(CLASS);
        let pane_class = wide(PANE_CLASS);
        let procs: [(&Vec<u16>, WNDPROC); 2] =
            [(&class, Some(window_proc)), (&pane_class, Some(pane_proc))];
        for (name, proc_) in procs {
            let wc = WNDCLASSW {
                lpfnWndProc: proc_,
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: BACKGROUND.get(),
                lpszClassName: name.as_ptr(),
                hIcon: LoadIconW(instance, 1 as _),
                ..zeroed()
            };
            if RegisterClassW(&wc) == 0 {
                return Err("cannot register the settings window class".into());
            }
        }

        let pages = model.pages();
        if pages.is_empty() {
            return Err("no settings to show".into());
        }
        let current = crate::page_index(&pages, page).unwrap_or(0);
        let title = wide(title);
        let window = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if window.is_null() {
            return Err("cannot create the settings window".into());
        }
        let dpi = GetDpiForWindow(window).max(96);
        let list = CreateWindowExW(
            0,
            wide("LISTBOX").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32,
            0,
            0,
            10,
            10,
            window,
            LIST_ID as _,
            instance,
            null(),
        );
        let pane = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            pane_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_CLIPCHILDREN,
            0,
            0,
            10,
            10,
            window,
            null_mut(),
            instance,
            null(),
        );
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                model,
                page: pages[current].title.clone(),
                pages,
                current,
                window,
                pane,
                list,
                dpi,
                controls: Vec::new(),
                shown_text: HashMap::new(),
                scroll: 0,
                content_height: 0,
                stale: false,
                last_focus: 0,
                moving: None,
            })
        });
        set_fonts(dpi);
        rebuild(true);
        place(window, Place::Center);
        ShowWindow(window, SW_SHOW);
        SetForegroundWindow(window);
        SetTimer(window, POLL_TIMER, POLL_MS, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // The wheel over a closed pop-up scrolls the page instead of changing the choice.
            if msg.message == WM_MOUSEWHEEL
                && is_class(msg.hwnd, "ComboBox")
                && SendMessageW(msg.hwnd, CB_GETDROPPEDSTATE, 0, 0) == 0
            {
                msg.hwnd = pane;
            }
            // A multi-line field wants every key, Tab too: move on from it as from any other
            // (Ctrl+Tab still types a tab).
            if msg.message == WM_KEYDOWN
                && msg.wParam == VK_TAB as WPARAM
                && GetKeyState(VK_CONTROL as i32) >= 0
                && is_class(msg.hwnd, "Edit")
                && GetWindowLongW(msg.hwnd, GWL_STYLE) as u32 & ES_MULTILINE as u32 != 0
            {
                let back = GetKeyState(VK_SHIFT as i32) < 0;
                let next = GetNextDlgTabItem(window, msg.hwnd, back as i32);
                if !next.is_null() {
                    SetFocus(next);
                    if is_class(next, "Edit") {
                        SendMessageW(next, EM_SETSEL, 0, -1);
                    }
                }
                continue;
            }
            // Tab between controls, Enter as IDOK.
            if IsDialogMessageW(window, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let (font, bold, big) = FONTS.get();
        DeleteObject(font as HGDIOBJ);
        DeleteObject(bold as HGDIOBJ);
        DeleteObject(big as HGDIOBJ);
    }
    Ok(())
}

pub fn activate(_pid: u32, page: Option<&str>) -> bool {
    unsafe {
        let window = FindWindowW(wide(CLASS).as_ptr(), null());
        if window.is_null() {
            return false;
        }
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window);
        if let Some(page) = page {
            let title: Vec<u16> = page.encode_utf16().collect();
            let data = COPYDATASTRUCT {
                dwData: SHOW_PAGE,
                cbData: (title.len() * 2) as u32,
                lpData: title.as_ptr() as _,
            };
            let mut result = 0;
            SendMessageTimeoutW(
                window,
                WM_COPYDATA,
                0,
                &data as *const _ as LPARAM,
                SMTO_ABORTIFHUNG,
                2000,
                &mut result,
            );
        }
        true
    }
}

/// The dialog font (Segoe UI) at `dpi`, and its semibold for section titles.
fn set_fonts(dpi: u32) {
    unsafe {
        let mut metrics: NONCLIENTMETRICSW = zeroed();
        metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            &mut metrics as *mut _ as _,
            0,
            dpi,
        );
        let font = CreateFontIndirectW(&metrics.lfMessageFont);
        let mut bold = metrics.lfMessageFont;
        bold.lfWeight = FW_SEMIBOLD as i32;
        let mut big = bold;
        big.lfHeight *= 2;
        let (bold, big) = (CreateFontIndirectW(&bold), CreateFontIndirectW(&big));
        let old = FONTS.replace((font, bold, big));
        if !old.0.is_null() {
            DeleteObject(old.0 as HGDIOBJ);
            DeleteObject(old.1 as HGDIOBJ);
            DeleteObject(old.2 as HGDIOBJ);
        }
    }
}

/// Where [`place`] puts the window.
#[derive(Clone, Copy)]
enum Place {
    /// Centered on its monitor.
    Center,
    /// Where it is, moved up if it would run off the bottom of the screen.
    Fit,
    /// Where it is (the user is dragging it).
    Keep,
}

/// Sizes the window for the current page (up to most of the screen, at least the page
/// list; another page resizes it, like the macOS window), the list and the pane in it.
fn place(window: HWND, at: Place) {
    let Some((dpi, list, pane, page_height, pages, scroll)) = with(|s| {
        let height = s.pages.get(s.current).map_or(0, |p| layout(p, s.dpi).1);
        (s.dpi, s.list, s.pane, height, s.pages.len(), s.scroll)
    }) else {
        return;
    };
    unsafe {
        let mut rect: RECT = zeroed();
        GetWindowRect(window, &mut rect);
        let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let work = info.rcWork;

        let pane_width = dip(
            MARGIN + LABEL_WIDTH + GAP + CONTROL_WIDTH + MARGIN + SCROLLBAR,
            dpi,
        );
        let width = dip(LIST_WIDTH, dpi) + pane_width;
        let gutter = dip(8, dpi);
        let list_height = pages as i32 * dip(LIST_ITEM_HEIGHT, dpi) + 2 * gutter;
        let height = page_height
            .max(list_height)
            .min((work.bottom - work.top) * 85 / 100);
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        let style = GetWindowLongW(window, GWL_STYLE) as u32;
        let ex_style = GetWindowLongW(window, GWL_EXSTYLE) as u32;
        AdjustWindowRectExForDpi(&mut frame, style, 0, ex_style, dpi);
        let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
        let (x, y) = match at {
            Place::Center => (
                work.left + (work.right - work.left - w) / 2,
                work.top + (work.bottom - work.top - h) / 2,
            ),
            Place::Fit => (rect.left, rect.top.min(work.bottom - h).max(work.top)),
            Place::Keep => (rect.left, rect.top),
        };
        SetWindowPos(
            window,
            null_mut(),
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        MoveWindow(
            list,
            gutter,
            gutter,
            dip(LIST_WIDTH, dpi) - 2 * gutter,
            height - 2 * gutter,
            1,
        );
        MoveWindow(pane, dip(LIST_WIDTH, dpi), 0, pane_width, height, 1);
    }
    // A taller pane may no longer scroll that far.
    scroll_to(scroll);
    update_scrollbar();
}

/// What goes where on a page, in pixels from the page's top.
enum Placed {
    Header(String),
    Label(String, bool),
    Row(Row),
    Note(String),
    /// Read-only text of a full-width row: `align` an `SS_*` style, `gray` as a note.
    Cell {
        text: String,
        align: u32,
        font: Font,
        gray: bool,
    },
}

/// Each element's rectangle, and the page's height.
fn layout(page: &Page, dpi: u32) -> (Vec<(Placed, RECT)>, i32) {
    let d = |v| dip(v, dpi);
    let label_x = d(MARGIN);
    let control_x = d(MARGIN + LABEL_WIDTH + GAP);
    let control_width = d(CONTROL_WIDTH);
    let mut out = Vec::new();
    let mut y = d(MARGIN);
    let rect = |x, y, w, h| RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    for (i, section) in page.sections.iter().enumerate() {
        if i > 0 {
            y += d(SECTION_SPACING);
        }
        if !section.title.is_empty() {
            let h = d(20);
            out.push((
                Placed::Header(section.title.clone()),
                rect(label_x, y, d(LABEL_WIDTH + GAP + CONTROL_WIDTH), h),
            ));
            y += h + d(ROW_SPACING) / 2;
        }
        for row in &section.rows {
            if let Some(h) = full_width(&row.control, label_x, y, dpi, &mut out) {
                y += h + d(ROW_SPACING);
                continue;
            }
            let h = match &row.control {
                Control::Toggle { .. } => d(20),
                Control::Choice { .. } | Control::Text { .. } => d(24),
                Control::LongText { .. } => d(LONG_TEXT_HEIGHT),
                Control::Button(_) => d(26),
                Control::Value(text) => measure(text, control_width, false).max(d(20)),
                Control::Stats(_) | Control::Table { .. } => unreachable!("laid out above"),
            };
            if !row.label.is_empty() {
                // Centered on a one-line control, level with the first line otherwise.
                let label_h = d(16);
                let top = if h <= d(26) {
                    y + (h - label_h) / 2
                } else {
                    y + d(3)
                };
                out.push((
                    Placed::Label(format!("{}:", row.label), row.enabled),
                    rect(label_x, top, d(LABEL_WIDTH), label_h + d(4)),
                ));
            }
            let width = match &row.control {
                Control::Button(title) => (measure(title, control_width, true) + d(32)).max(d(100)),
                Control::Choice { options, .. } => {
                    let widest = options
                        .iter()
                        .map(|o| measure(o, control_width, true))
                        .max()
                        .unwrap_or(0);
                    (widest + d(40)).clamp(d(100), control_width)
                }
                _ => control_width,
            };
            out.push((Placed::Row(row.clone()), rect(control_x, y, width, h)));
            y += h;
            if !row.note.is_empty() {
                y += d(3);
                let nh = measure(&row.note, control_width, false);
                out.push((
                    Placed::Note(row.note.clone()),
                    rect(control_x, y, control_width, nh),
                ));
                y += nh;
            }
            y += d(ROW_SPACING);
        }
    }
    (out, y + d(MARGIN))
}

/// Lays out a control that spans both columns ([`Control::Stats`], [`Control::Table`]) at
/// `(x, y)`: its height, or `None` for other controls.
fn full_width(
    control: &Control,
    x: i32,
    y: i32,
    dpi: u32,
    out: &mut Vec<(Placed, RECT)>,
) -> Option<i32> {
    let d = |v| dip(v, dpi);
    let mut cell = |text: &str, align, font, gray, left, top, width, height| {
        out.push((
            Placed::Cell {
                text: text.to_owned(),
                align,
                font,
                gray,
            },
            RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            },
        ));
    };
    match control {
        Control::Stats(stats) => {
            let n = stats.len().max(1) as i32;
            let width = (d(FULL_WIDTH) - (n - 1) * d(GAP)) / n;
            let (value_h, caption_h) = (d(34), d(18));
            for (i, Stat { value, caption }) in stats.iter().enumerate() {
                let left = x + i as i32 * (width + d(GAP));
                cell(value, SS_CENTER, Font::Big, false, left, y, width, value_h);
                cell(
                    caption,
                    SS_CENTER,
                    Font::Normal,
                    true,
                    left,
                    y + value_h,
                    width,
                    caption_h,
                );
            }
            Some(value_h + caption_h)
        }
        Control::Table { columns, rows } => {
            let rest = columns.len().saturating_sub(1) as i32;
            let column = d(TABLE_COLUMN_WIDTH);
            let first = d(FULL_WIDTH) - rest * (column + d(GAP));
            let line = d(20);
            let lines = std::iter::once((columns, true)).chain(rows.iter().map(|r| (r, false)));
            for (i, (texts, header)) in lines.enumerate() {
                let top = y + i as i32 * line;
                for c in 0..columns.len() {
                    let text = texts.get(c).map_or("", String::as_str);
                    let font = if header { Font::Bold } else { Font::Normal };
                    if c == 0 {
                        cell(text, SS_LEFT, font, header, x, top, first, line);
                    } else {
                        let left = x + first + d(GAP) + (c as i32 - 1) * (column + d(GAP));
                        cell(text, SS_RIGHT, font, header, left, top, column, line);
                    }
                }
            }
            Some((rows.len() as i32 + 1) * line)
        }
        _ => None,
    }
}

/// `text`'s height wrapped at `width` (`single`: its width on one line), in the dialog font.
fn measure(text: &str, width: i32, single: bool) -> i32 {
    unsafe {
        let dc: HDC = GetDC(null_mut());
        let old = SelectObject(dc, Font::Normal.handle() as HGDIOBJ);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: 0,
        };
        let mut text: Vec<u16> = text.encode_utf16().collect();
        let flags = DT_CALCRECT | DT_NOPREFIX | if single { DT_SINGLELINE } else { DT_WORDBREAK };
        DrawTextW(dc, text.as_mut_ptr(), text.len() as i32, &mut rect, flags);
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        if single { rect.right } else { rect.bottom }
    }
}

/// Shows the current page afresh from the model, keeping the focused control focused. Unless
/// `force`d (another page, another DPI), only if the pages changed. Whether it did.
fn rebuild(force: bool) -> bool {
    let focused = unsafe { GetFocus() };
    let Some(Some((pane, focus_id, page, dpi, scroll, titles, current, list))) = with(|s| {
        // `Some(None)`: on the page, but another page shows now.
        let mut focus_id = s
            .controls
            .contains(&focused)
            .then(|| Some(unsafe { GetDlgCtrlID(focused) }));
        s.stale = false;
        let pages = s.model.pages();
        if !force && pages == s.pages {
            return None;
        }
        s.pages = pages;
        let current = s.pages.iter().position(|p| p.title == s.page).unwrap_or(0);
        if current != s.current {
            s.current = current;
            s.scroll = 0;
            focus_id = focus_id.map(|_| None);
        }
        let titles: Vec<String> = s.pages.iter().map(|p| p.title.clone()).collect();
        Some((
            s.pane,
            focus_id,
            s.pages.get(s.current).cloned(),
            s.dpi,
            s.scroll,
            titles,
            s.current,
            s.list,
        ))
    }) else {
        return false;
    };
    let Some(page) = page else { return false };
    unsafe {
        SendMessageW(list, WM_SETFONT, Font::Normal.handle() as WPARAM, 0);
        let item_height = dip(LIST_ITEM_HEIGHT, dpi);
        SendMessageW(list, LB_SETITEMHEIGHT, 0, item_height as LPARAM);
        SendMessageW(list, LB_RESETCONTENT, 0, 0);
        for t in &titles {
            SendMessageW(list, LB_ADDSTRING, 0, wide(t).as_ptr() as LPARAM);
        }
        SendMessageW(list, LB_SETCURSEL, current, 0);

        SendMessageW(pane, WM_SETREDRAW, 0, 0);
        let old = with(|s| std::mem::take(&mut s.controls)).unwrap_or_default();
        for c in old {
            DestroyWindow(c);
        }
        NOTES.with(|n| n.borrow_mut().clear());
        let (placed, height) = layout(&page, dpi);
        // A page that got shorter (rows removed) may no longer scroll that far.
        let mut client: RECT = zeroed();
        GetClientRect(pane, &mut client);
        let scroll = scroll.min((height - client.bottom).max(0));
        with(|s| s.scroll = scroll);
        let mut controls = Vec::new();
        let mut shown_text = HashMap::new();
        for (item, mut rect) in placed {
            rect.top -= scroll;
            rect.bottom -= scroll;
            let hwnd = create(pane, &item, &rect, dpi, &mut shown_text);
            if !hwnd.is_null() {
                controls.push(hwnd);
            }
        }
        with(|s| {
            s.controls = controls;
            s.shown_text = shown_text;
            s.content_height = height;
        });
        SendMessageW(pane, WM_SETREDRAW, 1, 0);
        InvalidateRect(pane, null(), 1);
        if let Some(id) = focus_id {
            // Gone: the page list, rather than nothing.
            let c = id.map_or(null_mut(), |id| GetDlgItem(pane, id));
            SetFocus(if c.is_null() { list } else { c });
        }
    }
    update_scrollbar();
    true
}

/// One control of the page, as a child of `pane`.
unsafe fn create(
    pane: HWND,
    item: &Placed,
    r: &RECT,
    dpi: u32,
    shown_text: &mut HashMap<usize, String>,
) -> HWND {
    let (font, bold) = (Font::Normal.handle(), Font::Bold.handle());
    let (class, text, style, ex_style, id, enabled, font) = match item {
        Placed::Header(t) => ("STATIC", t.clone(), SS_LEFT | SS_NOPREFIX, 0, 0, true, bold),
        Placed::Label(t, enabled) => (
            "STATIC",
            t.clone(),
            SS_RIGHT | SS_NOPREFIX,
            0,
            0,
            *enabled,
            font,
        ),
        Placed::Note(t) => ("STATIC", t.clone(), SS_LEFT | SS_NOPREFIX, 0, 0, true, font),
        Placed::Cell {
            text, align, font, ..
        } => (
            "STATIC",
            text.clone(),
            *align | SS_NOPREFIX,
            0,
            0,
            true,
            font.handle(),
        ),
        Placed::Row(row) => {
            let id = FIRST_ID + row.id as i32;
            let (class, text, style, ex) = match &row.control {
                Control::Toggle { text, .. } => (
                    "BUTTON",
                    text.clone(),
                    BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
                    0,
                ),
                Control::Choice { .. } => (
                    "COMBOBOX",
                    String::new(),
                    CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP,
                    0,
                ),
                Control::Text { value, secret, .. } => {
                    shown_text.insert(row.id, value.clone());
                    let password = if *secret { ES_PASSWORD } else { 0 };
                    (
                        "EDIT",
                        value.clone(),
                        (ES_AUTOHSCROLL | password) as u32 | WS_TABSTOP,
                        WS_EX_CLIENTEDGE,
                    )
                }
                Control::LongText { value, .. } => {
                    shown_text.insert(row.id, value.clone());
                    (
                        "EDIT",
                        value.replace('\n', "\r\n"),
                        (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32
                            | WS_VSCROLL
                            | WS_TABSTOP,
                        WS_EX_CLIENTEDGE,
                    )
                }
                Control::Value(text) => ("STATIC", text.clone(), SS_LEFT | SS_NOPREFIX, 0),
                // Laid out as `Placed::Cell`s.
                Control::Stats(_) | Control::Table { .. } => return null_mut(),
                Control::Button(title) => (
                    "BUTTON",
                    title.clone(),
                    BS_PUSHBUTTON as u32 | WS_TABSTOP,
                    0,
                ),
            };
            (class, text, style, ex, id, row.enabled, font)
        }
    };
    // A combo box's height includes its drop-down list.
    let height = if class == "COMBOBOX" {
        dip(300, dpi)
    } else {
        r.bottom - r.top
    };
    unsafe {
        let hwnd = CreateWindowExW(
            ex_style,
            wide(class).as_ptr(),
            wide(&text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            r.left,
            r.top,
            r.right - r.left,
            height,
            pane,
            id as isize as _,
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return hwnd;
        }
        SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 0);
        if let Placed::Row(row) = item {
            match &row.control {
                Control::Toggle { on, .. } => {
                    SendMessageW(hwnd, BM_SETCHECK, *on as WPARAM, 0);
                }
                Control::Choice { options, selected } => {
                    for o in options {
                        SendMessageW(hwnd, CB_ADDSTRING, 0, wide(o).as_ptr() as LPARAM);
                    }
                    SendMessageW(hwnd, CB_SETCURSEL, *selected, 0);
                }
                Control::Text { placeholder, .. } if !placeholder.is_empty() => {
                    SendMessageW(
                        hwnd,
                        EM_SETCUEBANNER,
                        0,
                        wide(placeholder).as_ptr() as LPARAM,
                    );
                }
                _ => {}
            }
        }
        if matches!(item, Placed::Note(_) | Placed::Cell { gray: true, .. }) {
            NOTES.with(|n| n.borrow_mut().push(hwnd));
        }
        if !enabled {
            EnableWindow(hwnd, 0);
        }
        hwnd
    }
}

fn update_scrollbar() {
    let Some((pane, scroll, height)) = with(|s| (s.pane, s.scroll, s.content_height)) else {
        return;
    };
    unsafe {
        let mut client: RECT = zeroed();
        GetClientRect(pane, &mut client);
        let info = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: height,
            nPage: (client.bottom + 1) as u32,
            nPos: scroll,
            nTrackPos: 0,
        };
        SetScrollInfo(pane, SB_VERT, &info, 1);
    }
}

/// Scrolls the pane to `pos` (clamped), moving its controls.
fn scroll_to(pos: i32) {
    let Some((pane, old, height)) = with(|s| (s.pane, s.scroll, s.content_height)) else {
        return;
    };
    unsafe {
        let mut client: RECT = zeroed();
        GetClientRect(pane, &mut client);
        let pos = pos.clamp(0, (height - client.bottom).max(0));
        if pos == old {
            return;
        }
        with(|s| s.scroll = pos);
        ScrollWindowEx(
            pane,
            0,
            old - pos,
            null(),
            null(),
            null_mut(),
            null_mut(),
            SW_SCROLLCHILDREN | SW_INVALIDATE | SW_ERASE,
        );
    }
    update_scrollbar();
}

/// Shows page `index` (the user chose it), from its top.
fn show(index: usize) {
    let Some(Some(window)) = with(|s| {
        s.page = s.pages.get(index)?.title.clone();
        s.current = index;
        s.scroll = 0;
        Some(s.window)
    }) else {
        return;
    };
    rebuild(true);
    place(window, Place::Fit);
}

/// Rebuilds once queued input has been handled (see the module docs).
fn schedule_rebuild() {
    if let Some(window) = with(|s| s.window) {
        unsafe { SetTimer(window, REBUILD_TIMER, 0, None) };
    }
}

fn send(event: Event) {
    let Some(result) = with(|s| {
        crate::note(&mut s.pages, &event);
        s.model.handle(event)
    }) else {
        return;
    };
    if let Err(message) = result {
        let window = with(|s| s.window).unwrap_or(null_mut());
        unsafe {
            MessageBoxW(
                window,
                wide(&message).as_ptr(),
                wide("The setting wasn't changed").as_ptr(),
                MB_ICONWARNING | MB_OK,
            );
        }
    }
    schedule_rebuild();
}

fn edit_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize]).replace("\r\n", "\n")
    }
}

/// Reports an edit field's text if it changed.
fn commit_text(id: usize, hwnd: HWND) {
    let text = edit_text(hwnd);
    let changed = with(|s| match s.shown_text.get_mut(&id) {
        Some(shown) if *shown != text => {
            *shown = text.clone();
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if changed {
        send(Event::Text(id, text));
    } else if with(|s| s.stale).unwrap_or(false) {
        schedule_rebuild();
    }
}

/// A pop-up's choice not yet reported (arrowed to while it's closed): its row and option.
fn unsent_choice(hwnd: HWND) -> Option<(usize, usize)> {
    let row = usize::try_from(unsafe { GetDlgCtrlID(hwnd) } - FIRST_ID).ok()?;
    let selected = with(|s| {
        let page = s.pages.get(s.current)?;
        let row = page
            .sections
            .iter()
            .flat_map(|sec| &sec.rows)
            .find(|r| r.id == row)?;
        match row.control {
            Control::Choice { selected, .. } => Some(selected),
            _ => None,
        }
    })??;
    let index = usize::try_from(unsafe { SendMessageW(hwnd, CB_GETCURSEL, 0, 0) }).ok()?;
    (index != selected).then_some((row, index))
}

/// Reports a pop-up's choice if it changed.
fn commit_choice(hwnd: HWND) {
    if let Some((row, index)) = unsent_choice(hwnd) {
        send(Event::Choose(row, index));
    } else if with(|s| s.stale).unwrap_or(false) {
        schedule_rebuild();
    }
}

/// The focused control is being edited or has its list open (or a choice not yet made): a
/// rebuild would undo that.
fn busy() -> bool {
    unsafe {
        let focused = GetFocus();
        is_class(focused, "Edit")
            || SendMessageW(focused, CB_GETDROPPEDSTATE, 0, 0) != 0
            || unsent_choice(focused).is_some()
    }
}

fn is_class(hwnd: HWND, name: &str) -> bool {
    let mut class = [0u16; 16];
    let n = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    String::from_utf16_lossy(&class[..n.max(0) as usize]).eq_ignore_ascii_case(name)
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wp & 0xFFFF) as i32;
                let code = ((wp >> 16) & 0xFFFF) as u32;
                if id == LIST_ID && code == LBN_SELCHANGE {
                    let list = lp as HWND;
                    let index = SendMessageW(list, LB_GETCURSEL, 0, 0);
                    if index >= 0 {
                        show(index as usize);
                    }
                } else if id == IDOK {
                    // Enter in a one-line field, or on a pop-up: report it now.
                    let focused = GetFocus();
                    if is_class(focused, "Edit") {
                        let row = GetDlgCtrlID(focused) - FIRST_ID;
                        if row >= 0 {
                            commit_text(row as usize, focused);
                        }
                    } else if is_class(focused, "ComboBox") {
                        commit_choice(focused);
                    }
                }
                0
            }
            WM_ACTIVATE => {
                // Like a dialog: the focus goes back to the control that had it (at first, the
                // page list), not to the window.
                if wp & 0xFFFF == WA_INACTIVE as usize {
                    let focused = GetFocus();
                    if IsChild(hwnd, focused) != 0 {
                        with(|s| s.last_focus = GetDlgCtrlID(focused));
                    }
                } else if IsIconic(hwnd) == 0
                    && let Some((list, pane, id)) = with(|s| (s.list, s.pane, s.last_focus))
                {
                    let c = if id >= FIRST_ID {
                        GetDlgItem(pane, id)
                    } else {
                        null_mut()
                    };
                    SetFocus(if c.is_null() { list } else { c });
                }
                0
            }
            WM_TIMER if wp == REBUILD_TIMER => {
                KillTimer(hwnd, REBUILD_TIMER);
                // Not under a held mouse button (the click it belongs to isn't over yet), nor
                // under an open list.
                let dropped = SendMessageW(GetFocus(), CB_GETDROPPEDSTATE, 0, 0) != 0;
                if GetKeyState(VK_LBUTTON as i32) < 0 || dropped {
                    SetTimer(hwnd, REBUILD_TIMER, 50, None);
                } else if rebuild(false) {
                    // Rows shown or hidden (e.g. the advanced settings), or another page.
                    place(hwnd, Place::Fit);
                }
                0
            }
            WM_TIMER if wp == POLL_TIMER => {
                let changed = with(|s| s.model.changed_elsewhere()).unwrap_or(false);
                if changed || with(|s| s.stale).unwrap_or(false) {
                    if busy() {
                        with(|s| s.stale = true);
                    } else {
                        schedule_rebuild();
                    }
                }
                0
            }
            WM_DPICHANGED => {
                let dpi = (wp & 0xFFFF) as u32;
                with(|s| s.dpi = dpi);
                set_fonts(dpi);
                let suggested = &*(lp as *const RECT);
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                rebuild(true);
                // Being dragged: where the user puts it, fitted once let go.
                let dragged = with(|s| {
                    let dragged = s.moving.is_some();
                    if dragged {
                        s.moving = Some(true);
                    }
                    dragged
                })
                .unwrap_or(false);
                place(hwnd, if dragged { Place::Keep } else { Place::Fit });
                0
            }
            WM_ENTERSIZEMOVE => {
                with(|s| s.moving = Some(false));
                0
            }
            WM_EXITSIZEMOVE => {
                if with(|s| s.moving.take()).flatten() == Some(true) {
                    place(hwnd, Place::Fit);
                }
                0
            }
            WM_COPYDATA => {
                let data = &*(lp as *const COPYDATASTRUCT);
                if data.dwData != SHOW_PAGE || data.lpData.is_null() {
                    return 0;
                }
                let title = String::from_utf16_lossy(std::slice::from_raw_parts(
                    data.lpData as *const u16,
                    data.cbData as usize / 2,
                ));
                // Moving the focus off an edit field reports its last change.
                SetFocus(hwnd);
                let index = with(|s| crate::page_index(&s.pages, Some(&title))).flatten();
                if let Some(index) = index {
                    show(index);
                }
                if let Some(list) = with(|s| s.list) {
                    SetFocus(list);
                }
                1
            }
            WM_CTLCOLORLISTBOX => BACKGROUND.get() as LRESULT,
            WM_CLOSE => {
                // Moving the focus off an edit field reports its last change.
                SetFocus(hwnd);
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn pane_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let row = (wp & 0xFFFF) as i32 - FIRST_ID;
                let code = ((wp >> 16) & 0xFFFF) as u32;
                let control = lp as HWND;
                if row < 0 || control.is_null() {
                    return 0;
                }
                let row = row as usize;
                let kind = with(|s| {
                    s.pages
                        .get(s.current)
                        .and_then(|p| {
                            p.sections
                                .iter()
                                .flat_map(|sec| &sec.rows)
                                .find(|r| r.id == row)
                        })
                        .map(|r| r.control.clone())
                })
                .flatten();
                match (kind, code) {
                    (Some(Control::Toggle { .. }), BN_CLICKED) => {
                        let on = SendMessageW(control, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
                        send(Event::Toggle(row, on));
                    }
                    (Some(Control::Button(_)), BN_CLICKED) => send(Event::Click(row)),
                    // Once the choice is made (the list closes on it, or the focus moves on
                    // after arrowing through it), not on every step through the list: a closed
                    // one reports `CBN_SELENDOK` for each arrow key too.
                    (Some(Control::Choice { .. }), CBN_SELENDOK)
                        if SendMessageW(control, CB_GETDROPPEDSTATE, 0, 0) == 0 => {}
                    (Some(Control::Choice { .. }), CBN_SELENDOK | CBN_KILLFOCUS) => {
                        commit_choice(control);
                    }
                    (Some(Control::Text { .. } | Control::LongText { .. }), EN_KILLFOCUS) => {
                        commit_text(row, control);
                    }
                    _ => {}
                }
                0
            }
            WM_CTLCOLORSTATIC => {
                let dc = wp as HDC;
                let note = NOTES.with(|n| n.borrow().contains(&(lp as HWND)));
                if note {
                    SetTextColor(dc, GetSysColor(COLOR_GRAYTEXT));
                }
                SetBkMode(dc, TRANSPARENT as i32);
                BACKGROUND.get() as LRESULT
            }
            WM_VSCROLL => {
                let Some((pos, height)) = with(|s| (s.scroll, s.content_height)) else {
                    return 0;
                };
                let mut client: RECT = zeroed();
                GetClientRect(hwnd, &mut client);
                let dpi = with(|s| s.dpi).unwrap_or(96);
                let line = dip(30, dpi);
                let new = match (wp & 0xFFFF) as i32 {
                    SB_LINEUP => pos - line,
                    SB_LINEDOWN => pos + line,
                    SB_PAGEUP => pos - client.bottom,
                    SB_PAGEDOWN => pos + client.bottom,
                    SB_THUMBTRACK | SB_THUMBPOSITION => ((wp >> 16) & 0xFFFF) as i32,
                    SB_TOP => 0,
                    SB_BOTTOM => height,
                    _ => pos,
                };
                scroll_to(new);
                0
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp >> 16) & 0xFFFF) as u16 as i16 as i32;
                let dpi = with(|s| s.dpi).unwrap_or(96);
                let pos = with(|s| s.scroll).unwrap_or(0);
                scroll_to(pos - delta * dip(60, dpi) / WHEEL_DELTA as i32);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
