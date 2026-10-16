//! Windows: a plain Win32 window. A list of pages on the left (like Windows' own Settings), the
//! page on the right: right-aligned labels and their controls (common controls v6, so the
//! executable's manifest must ask for them), in a pane that scrolls if the screen is short.
//! Per-monitor DPI aware: sizes are in DIPs, scaled by the window's DPI.
//!
//! Each control's id is its row's id + [`FIRST_ID`]. Changes are sent to the model as they
//! happen; the page is then rebuilt from the model on a zero-length timer, whose message comes
//! after any input still queued (so a click that moved focus out of an edit field, which
//! reported its text, still reaches the control it was on).

use crate::{Control, Error, Event, Model, Page, Row};
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
        MONITORINFO, MonitorFromPoint, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
        TRANSPARENT,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{
            BST_CHECKED, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
            SetScrollInfo,
        },
        HiDpi::{
            AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
        },
        Input::KeyboardAndMouse::{EnableWindow, GetFocus, GetKeyState, SetFocus, VK_LBUTTON},
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

const EN_KILLFOCUS: u32 = 0x0200;
// Static control styles (in windows-sys' System_SystemServices, not worth the feature).
const SS_LEFT: u32 = 0;
const SS_RIGHT: u32 = 2;
const SS_NOPREFIX: u32 = 0x80;
const EM_SETCUEBANNER: u32 = 0x1501;

struct State {
    model: Box<dyn Model>,
    pages: Vec<Page>,
    current: usize,
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
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Kept apart from [`STATE`]: controls ask for them while it's borrowed (being created).
    static FONTS: Cell<(HFONT, HFONT)> = const { Cell::new((null_mut(), null_mut())) };
    static NOTES: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    static BACKGROUND: Cell<HBRUSH> = const { Cell::new(null_mut()) };
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

pub fn run(title: &str, mut model: Box<dyn Model>) -> Result<(), Error> {
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
                pages,
                current: 0,
                window,
                pane,
                list,
                dpi,
                controls: Vec::new(),
                shown_text: HashMap::new(),
                scroll: 0,
                content_height: 0,
                stale: false,
            })
        });
        set_fonts(dpi);
        rebuild();
        place(window, true);
        ShowWindow(window, SW_SHOW);
        SetForegroundWindow(window);
        SetTimer(window, POLL_TIMER, POLL_MS, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // Tab between controls, Enter as IDOK.
            if IsDialogMessageW(window, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let (font, bold) = FONTS.get();
        DeleteObject(font as HGDIOBJ);
        DeleteObject(bold as HGDIOBJ);
    }
    Ok(())
}

pub fn activate(_pid: u32) -> bool {
    unsafe {
        let window = FindWindowW(wide(CLASS).as_ptr(), null());
        if window.is_null() {
            return false;
        }
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window);
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
        let bold = CreateFontIndirectW(&bold);
        let (old, old_bold) = FONTS.replace((font, bold));
        if !old.is_null() {
            DeleteObject(old as HGDIOBJ);
            DeleteObject(old_bold as HGDIOBJ);
        }
    }
}

/// Sizes the window for the tallest page (up to most of the screen), the list and the pane
/// in it. `center`: also centers it on its monitor.
fn place(window: HWND, center: bool) {
    let Some((dpi, list, pane, heights)) = with(|s| {
        let heights: Vec<i32> = s.pages.iter().map(|p| layout(p, s.dpi).1).collect();
        (s.dpi, s.list, s.pane, heights)
    }) else {
        return;
    };
    unsafe {
        let mut rect: RECT = zeroed();
        GetWindowRect(window, &mut rect);
        let monitor = MonitorFromPoint(
            windows_sys::Win32::Foundation::POINT {
                x: rect.left,
                y: rect.top,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let work = info.rcWork;

        let pane_width = dip(
            MARGIN + LABEL_WIDTH + GAP + CONTROL_WIDTH + MARGIN + SCROLLBAR,
            dpi,
        );
        let width = dip(LIST_WIDTH, dpi) + pane_width;
        let tallest = heights.into_iter().max().unwrap_or(0);
        let height = tallest.min((work.bottom - work.top) * 85 / 100);
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
        let (x, y) = if center {
            (
                work.left + (work.right - work.left - w) / 2,
                work.top + (work.bottom - work.top - h) / 2,
            )
        } else {
            (rect.left, rect.top)
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
        let gutter = dip(8, dpi);
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
    update_scrollbar();
}

/// What goes where on a page, in pixels from the page's top.
enum Placed {
    Header(String),
    Label(String, bool),
    Row(Row),
    Note(String),
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
            let h = match &row.control {
                Control::Toggle { .. } => d(20),
                Control::Choice { .. } | Control::Text { .. } => d(24),
                Control::LongText { .. } => d(LONG_TEXT_HEIGHT),
                Control::Button(_) => d(26),
                Control::Value(text) => measure(text, control_width, false).max(d(20)),
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

/// `text`'s height wrapped at `width` (`single`: its width on one line), in the dialog font.
fn measure(text: &str, width: i32, single: bool) -> i32 {
    unsafe {
        let dc: HDC = GetDC(null_mut());
        let old = SelectObject(dc, FONTS.get().0 as HGDIOBJ);
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

/// Shows the current page afresh from the model, keeping the focused control focused.
fn rebuild() {
    let focused = unsafe { GetFocus() };
    let Some((pane, focus_id, page, dpi, scroll, titles, current, list)) = with(|s| {
        let focus_id = s
            .controls
            .contains(&focused)
            .then(|| unsafe { GetDlgCtrlID(focused) });
        s.stale = false;
        s.pages = s.model.pages();
        s.current = s.current.min(s.pages.len().saturating_sub(1));
        let titles: Vec<String> = s.pages.iter().map(|p| p.title.clone()).collect();
        (
            s.pane,
            focus_id,
            s.pages.get(s.current).cloned(),
            s.dpi,
            s.scroll,
            titles,
            s.current,
            s.list,
        )
    }) else {
        return;
    };
    let Some(page) = page else { return };
    unsafe {
        SendMessageW(list, WM_SETFONT, FONTS.get().0 as WPARAM, 0);
        SendMessageW(list, LB_SETITEMHEIGHT, 0, dip(28, dpi) as LPARAM);
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
            let c = GetDlgItem(pane, id);
            if !c.is_null() {
                SetFocus(c);
            }
        }
    }
    update_scrollbar();
}

/// One control of the page, as a child of `pane`.
unsafe fn create(
    pane: HWND,
    item: &Placed,
    r: &RECT,
    dpi: u32,
    shown_text: &mut HashMap<usize, String>,
) -> HWND {
    let (font, bold) = FONTS.get();
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
        if let Placed::Note(_) = item {
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

/// Rebuilds once queued input has been handled (see the module docs).
fn schedule_rebuild() {
    if let Some(window) = with(|s| s.window) {
        unsafe { SetTimer(window, REBUILD_TIMER, 0, None) };
    }
}

fn send(event: Event) {
    let Some(result) = with(|s| s.model.handle(event)) else {
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

fn is_edit(hwnd: HWND) -> bool {
    let mut class = [0u16; 16];
    let n = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    String::from_utf16_lossy(&class[..n.max(0) as usize]).eq_ignore_ascii_case("edit")
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
                        with(|s| {
                            s.current = index as usize;
                            s.scroll = 0;
                        });
                        rebuild();
                    }
                } else if id == IDOK {
                    // Enter in a one-line field: report it now.
                    let focused = GetFocus();
                    if is_edit(focused) {
                        let row = GetDlgCtrlID(focused) - FIRST_ID;
                        if row >= 0 {
                            commit_text(row as usize, focused);
                        }
                    }
                }
                0
            }
            WM_TIMER if wp == REBUILD_TIMER => {
                KillTimer(hwnd, REBUILD_TIMER);
                // Not under a held mouse button: the click it belongs to isn't over yet.
                if GetKeyState(VK_LBUTTON as i32) < 0 {
                    SetTimer(hwnd, REBUILD_TIMER, 50, None);
                } else {
                    rebuild();
                }
                0
            }
            WM_TIMER if wp == POLL_TIMER => {
                let changed = with(|s| s.model.changed_elsewhere()).unwrap_or(false);
                if changed || with(|s| s.stale).unwrap_or(false) {
                    if is_edit(GetFocus()) {
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
                rebuild();
                place(hwnd, false);
                0
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
                    (Some(Control::Choice { .. }), CBN_SELCHANGE) => {
                        let index = SendMessageW(control, CB_GETCURSEL, 0, 0);
                        if index >= 0 {
                            send(Event::Choose(row, index as usize));
                        }
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
