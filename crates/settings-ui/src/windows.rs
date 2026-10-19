//! Windows: a plain Win32 window that looks like Windows 11's own Settings. A list of pages on
//! the left, the page on the right: its title, then a "card" per row (its title and note on the
//! left, its control on the right), in a pane that scrolls if the screen is short. Common
//! controls v6 (the executable's manifest must ask for them), drawn here: the page list, the
//! switches, buttons and pop-ups (shapes with GDI+, anti-aliased; text with GDI, so it's
//! ClearType), in the system's light or dark colors and accent, or its high-contrast colors,
//! which then replace every color here. Per-monitor DPI aware: sizes are in DIPs, scaled by
//! the window's DPI.
//!
//! Each control's id is its row's id + [`FIRST_ID`]. Changes are sent to the model as they
//! happen (a pop-up's once chosen). If the model's pages then differ, the page is rebuilt on a
//! zero-length timer, whose message comes after any input still queued (so a click that moved
//! focus out of an edit field, which reported its text, still reaches the control it was on).

use crate::{Control, Error, Event, Icon, Model, Page, Row, Stat};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWMWA_CAPTION_COLOR, DWMWA_COLOR_DEFAULT, DWMWA_USE_IMMERSIVE_DARK_MODE,
            DwmSetWindowAttribute,
        },
        Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, COLOR_BTNFACE, COLOR_BTNTEXT,
            COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT,
            CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection,
            CreateFontIndirectW, CreateSolidBrush, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT,
            DT_CENTER, DT_EDITCONTROL, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
            DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteDC, DeleteObject, DrawTextW, EndPaint,
            EnumFontFamiliesExW, FW_NORMAL, FW_SEMIBOLD, FillRect, FrameRect, GetDC,
            GetMonitorInfoW, GetSysColor, GetWindowDC, HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect,
            LOGFONTW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MapWindowPoints, MonitorFromWindow,
            PAINTSTRUCT, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RedrawWindow,
            ReleaseDC, SRCCOPY, SelectObject, SetBkColor, SetBkMode, SetTextColor, TEXTMETRICW,
            TRANSPARENT,
        },
        GdiPlus::{
            CombineModeReplace, FillModeAlternate, FlushIntentionSync, GdipAddPathArc,
            GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1,
            GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
            GdipDeletePen, GdipDrawPath, GdipFillEllipse, GdipFillPath, GdipFillRectangle,
            GdipFlush, GdipResetClip, GdipSetClipRect, GdipSetPixelOffsetMode,
            GdipSetSmoothingMode, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpBrush,
            GpGraphics, GpPath, PixelOffsetModeHalf, SmoothingModeAntiAlias, UnitPixel,
        },
    },
    System::{
        DataExchange::COPYDATASTRUCT,
        LibraryLoader::GetModuleHandleW,
        Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RegGetValueW},
    },
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        Controls::{
            BCM_SETNOTE, BS_COMMANDLINK, BS_DEFCOMMANDLINK, BST_CHECKED, CDDS_PREPAINT,
            CDIS_DISABLED, CDIS_HOT, CDIS_SELECTED, CDRF_SKIPDEFAULT, COMBOBOXINFO, DRAWITEMSTRUCT,
            EM_GETLINECOUNT, EM_SETSEL, GetComboBoxInfo, ICC_STANDARD_CLASSES,
            INITCOMMONCONTROLSEX, InitCommonControlsEx, MEASUREITEMSTRUCT, NM_CUSTOMDRAW,
            NMCUSTOMDRAW, NMHDR, ODS_COMBOBOXEDIT, ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED,
            ODT_COMBOBOX, ODT_LISTBOX, SetScrollInfo, SetWindowTheme, ShowScrollBar, WM_MOUSELEAVE,
        },
        HiDpi::{
            AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            GetSystemMetricsForDpi, SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
        },
        Input::KeyboardAndMouse::{
            EnableWindow, GetFocus, GetKeyState, IsWindowEnabled, SetFocus, TME_LEAVE,
            TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_LBUTTON, VK_SHIFT, VK_TAB,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
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

// Sizes in DIPs, after Windows 11's Settings.
const NAV_WIDTH: i32 = 200;
/// The page list's left edge, its items' height and their highlight's inset.
const NAV_LEFT: i32 = 4;
const NAV_ITEM: i32 = 38;
const NAV_INSET: i32 = 4;
const MARGIN: i32 = 28;
/// Above the page's title, and from it to the first section.
const TOP: i32 = 16;
const TITLE_GAP: i32 = 20;
const SECTION_GAP: i32 = 24;
const HEADER_GAP: i32 = 8;
const CARD_WIDTH: i32 = 600;
const CARD_GAP: i32 = 4;
const CARD_PADDING: i32 = 16;
const CARD_PADDING_Y: i32 = 14;
const CARD_MIN: i32 = 60;
const RADIUS: i32 = 4;
/// Between a card's text and its control; between a title and its note; above what's under
/// them (a long text's field).
const CONTROL_GAP: i32 = 16;
const NOTE_GAP: i32 = 2;
const UNDER_GAP: i32 = 10;
/// Around a control, inside its window: room for its focus ring.
const FOCUS: i32 = 3;
const CONTROL_HEIGHT: i32 = 32;
const TEXT_WIDTH: i32 = 260;
const FIELD_PADDING: i32 = 10;
const LONG_TEXT_HEIGHT: i32 = 88;
const CHOICE_MIN: i32 = 120;
const CHOICE_MAX: i32 = 320;
/// A pop-up's text inset, and the room for its chevron (a card's too).
const CHOICE_TEXT: i32 = 11;
const CHEVRON: i32 = 32;
const DROP_ITEM: i32 = 32;
/// An open pop-up's text inset (its highlight's and selection bar's included).
const DROP_TEXT: i32 = 16;
const DROP_HEIGHT: i32 = 300;
const BUTTON_MIN: i32 = 120;
const BUTTON_PADDING: i32 = 12;
/// A value wider than this goes under its title.
const VALUE_WIDTH: i32 = 260;
const SWITCH_WIDTH: i32 = 40;
const SWITCH_HEIGHT: i32 = 20;
/// Between a switch's "On"/"Off" and the switch.
const SWITCH_GAP: i32 = 12;
const SCROLLBAR: i32 = 17;
/// A table's columns after the first, and between them.
const TABLE_COLUMN_WIDTH: i32 = 70;
const TABLE_GAP: i32 = 8;
const TABLE_LINE: i32 = 32;
const TABLE_PADDING: i32 = 4;

/// `WM_COPYDATA` from [`activate`]: the page to show (its title, UTF-16).
const SHOW_PAGE: usize = 0x4147_5450;

const EN_KILLFOCUS: u32 = 0x0200;
// Static control styles (in windows-sys' System_SystemServices, not worth the feature).
const SS_LEFT: u32 = 0;
const SS_CENTER: u32 = 1;
const SS_RIGHT: u32 = 2;
const SS_NOPREFIX: u32 = 0x80;
const SS_EDITCONTROL: u32 = 0x2000;
const EM_SETCUEBANNER: u32 = 0x1501;

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const ACCENT: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent";
const DWM: &str = r"Software\Microsoft\Windows\DWM";

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
    /// The page's controls (texts too), destroyed on each rebuild.
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
    /// The page changed size while the window was minimized: [`place`] it once restored.
    place_owed: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    // Kept apart from [`STATE`]: controls ask for these while it's borrowed (being created),
    // and they're all drawing needs.
    static FONTS: Cell<[HFONT; FONT_COUNT]> = const { Cell::new([null_mut(); FONT_COUNT]) };
    /// The DPI the fonts are for.
    static DPI: Cell<u32> = const { Cell::new(96) };
    static THEME: Cell<Option<Theme>> = const { Cell::new(None) };
    /// Whether Segoe Fluent Icons (Windows 11) is installed, else MDL2 Assets is used.
    static FLUENT: Cell<bool> = const { Cell::new(false) };
    /// How the page's texts and buttons are drawn.
    static LOOKS: RefCell<HashMap<HWND, Look>> = RefCell::new(HashMap::new());
    static DECOR: RefCell<Decor> = RefCell::new(Decor::default());
    /// The page list's item under the mouse (-1: none).
    static NAV_HOT: Cell<i32> = const { Cell::new(-1) };
    /// The pop-up under the mouse.
    static HOT: Cell<HWND> = const { Cell::new(null_mut()) };
    static ICON: Cell<Option<Icon>> = const { Cell::new(None) };
    /// The window's small and big icons.
    static ICONS: Cell<[HICON; 2]> = const { Cell::new([null_mut(); 2]) };
}

#[derive(Clone, Copy)]
enum Font {
    Body,
    /// Notes, captions.
    Caption,
    /// Section titles, table headers.
    Strong,
    /// The page's title.
    Title,
    /// Figures.
    Big,
    /// The page list's icons (Segoe Fluent Icons).
    Icon,
    /// Chevrons, in the same.
    Glyph,
}
const FONT_COUNT: usize = 7;

impl Font {
    fn handle(self) -> HFONT {
        FONTS.get()[self as usize]
    }
}

/// The color of a text.
#[derive(Clone, Copy, PartialEq)]
enum Ink {
    Text,
    Secondary,
    Disabled,
}

/// How a control of the page is drawn.
enum Look {
    /// A STATIC: its color, and whether it's on a card (else on the window).
    Text(Ink, bool),
    /// A checkbox drawn as a switch.
    Switch,
    /// A push button, on a card.
    Button,
    /// A button that's a whole card: its note under its title.
    Card(String),
}

/// What the pane paints itself, in pixels from the page's top.
#[derive(Default)]
struct Decor {
    cards: Vec<RECT>,
    /// A table's lines between rows.
    rules: Vec<RECT>,
    /// Edit fields' boxes (the borderless edit control inside) and their control ids.
    fields: Vec<(RECT, i32)>,
}

/// 0xRRGGBB as a `COLORREF` (0x00BBGGRR), and back.
const fn hex(c: u32) -> u32 {
    (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF)
}

/// A GDI+ color, 0xAARRGGBB: laid over what's under it.
#[derive(Clone, Copy, PartialEq)]
struct Argb(u32);

impl Argb {
    /// `COLORREF` `c`, opaque.
    const fn solid(c: u32) -> Self {
        Argb(0xFF00_0000 | hex(c))
    }

    /// The `COLORREF` it makes over `under` (for controls that paint with GDI brushes).
    fn over(self, under: u32) -> u32 {
        let a = (self.0 >> 24) as f32 / 255.0;
        let rgb = hex(self.0 & 0xFF_FFFF);
        (0..3).fold(0, |c, i| {
            let (top, bottom) = ((rgb >> (8 * i)) & 0xFF, (under >> (8 * i)) & 0xFF);
            let mixed = (top as f32 * a + bottom as f32 * (1.0 - a)).round() as u32;
            c | mixed << (8 * i)
        })
    }
}

/// The window's colors: `COLORREF`s, but those GDI+ lays over what's under them.
#[derive(Clone, Copy, PartialEq)]
struct Palette {
    dark: bool,
    /// High contrast: the system's colors.
    contrast: bool,
    window: u32,
    card: u32,
    card_border: u32,
    /// A card that's a button.
    card_hover: u32,
    card_pressed: u32,
    text: u32,
    secondary: u32,
    disabled: u32,
    /// The page list's hovered and selected item.
    nav: u32,
    /// Text on `nav` and `flyout_item` (high contrast only: `text` otherwise).
    selected_text: u32,
    accent: u32,
    /// The knob of a switch that's on.
    on_accent: u32,
    /// Buttons and pop-ups.
    control: Argb,
    control_hover: Argb,
    control_pressed: Argb,
    control_text: u32,
    stroke: Argb,
    /// Darker along the bottom edge (light), lighter along the top (dark).
    stroke_bottom: Argb,
    /// Edit fields: solid, as the edit control paints its own background with it.
    field: u32,
    field_focus: u32,
    field_bottom: Argb,
    /// An open pop-up's list, and its hovered item.
    flyout: u32,
    flyout_item: u32,
    flyout_border: u32,
    /// Inside a switch that's off (its outline and knob: `secondary`).
    switch_off: Argb,
}

impl Palette {
    /// The system's: light or dark, its accent, or high contrast. For testing,
    /// `AGT_SETTINGS_THEME=light|dark|contrast` (the high-contrast code path, with whatever
    /// the system's colors are) overrides it.
    fn current() -> Self {
        let forced = std::env::var("AGT_SETTINGS_THEME")
            .ok()
            .filter(|t| ["light", "dark", "contrast"].contains(&t.as_str()));
        let contrast = match forced.as_deref() {
            Some(t) => t == "contrast",
            None => high_contrast(),
        };
        if contrast {
            return Self::system();
        }
        let dark = match forced.as_deref() {
            Some(t) => t == "dark",
            None => registry_dword(PERSONALIZE, "AppsUseLightTheme") == Some(0),
        };
        // Its 8 RGBA entries: Light3, Light2, Light1, Accent, Dark1, Dark2, Dark3, unused.
        // Windows' own apps take Light2 on dark, Dark1 on light.
        let mut entries = [0u8; 32];
        let accent = (registry(ACCENT, "AccentPalette", RRF_RT_REG_BINARY, &mut entries)
            == Some(32))
        .then(|| {
            let e = &entries[if dark { 4 } else { 16 }..];
            e[0] as u32 | (e[1] as u32) << 8 | (e[2] as u32) << 16
        });
        if dark {
            Self::dark(accent.unwrap_or(hex(0x60CDFF)))
        } else {
            Self::light(accent.unwrap_or(hex(0x005FB8)))
        }
    }

    // Fluent's colors (WinUI's, solid where they're laid over Mica).
    fn light(accent: u32) -> Self {
        let card = hex(0xFBFBFB);
        let text = hex(0x1B1B1B);
        Self {
            dark: false,
            contrast: false,
            window: hex(0xF3F3F3),
            card,
            card_border: hex(0xE5E5E5),
            card_hover: hex(0xF6F6F6),
            card_pressed: hex(0xF3F3F3),
            text,
            secondary: hex(0x5F5F5F),
            disabled: hex(0xA0A0A0),
            nav: hex(0xEAEAEA),
            selected_text: text,
            accent,
            on_accent: hex(0xFFFFFF),
            control: Argb(0xB3FF_FFFF),
            control_hover: Argb(0x80F9_F9F9),
            control_pressed: Argb(0x4DF9_F9F9),
            control_text: text,
            stroke: Argb(0x1700_0000),
            stroke_bottom: Argb(0x2900_0000),
            field: Argb(0xB3FF_FFFF).over(card),
            field_focus: hex(0xFFFFFF),
            field_bottom: Argb(0x7200_0000),
            flyout: hex(0xF9F9F9),
            flyout_item: hex(0xEDEDED),
            flyout_border: hex(0xD5D5D5),
            switch_off: Argb(0x0600_0000),
        }
    }

    fn dark(accent: u32) -> Self {
        let card = hex(0x2B2B2B);
        let text = hex(0xFFFFFF);
        Self {
            dark: true,
            contrast: false,
            window: hex(0x202020),
            card,
            card_border: hex(0x1D1D1D),
            card_hover: hex(0x323232),
            card_pressed: hex(0x272727),
            text,
            secondary: hex(0xC5C5C5),
            disabled: hex(0x787878),
            nav: hex(0x2D2D2D),
            selected_text: text,
            accent,
            on_accent: hex(0x000000),
            control: Argb(0x0FFF_FFFF),
            control_hover: Argb(0x15FF_FFFF),
            control_pressed: Argb(0x08FF_FFFF),
            control_text: text,
            stroke: Argb(0x12FF_FFFF),
            stroke_bottom: Argb(0x18FF_FFFF),
            field: Argb(0x0FFF_FFFF).over(card),
            field_focus: hex(0x1F1F1F),
            field_bottom: Argb(0x8BFF_FFFF),
            flyout: hex(0x2C2C2C),
            flyout_item: hex(0x383838),
            flyout_border: hex(0x454545),
            switch_off: Argb(0x1900_0000),
        }
    }

    /// High contrast: nothing but the system's colors.
    fn system() -> Self {
        let sys = |i| unsafe { GetSysColor(i) };
        let (window, text, gray) = (
            sys(COLOR_WINDOW),
            sys(COLOR_WINDOWTEXT),
            sys(COLOR_GRAYTEXT),
        );
        let (highlight, highlight_text) = (sys(COLOR_HIGHLIGHT), sys(COLOR_HIGHLIGHTTEXT));
        let (face, face_text) = (sys(COLOR_BTNFACE), sys(COLOR_BTNTEXT));
        Self {
            dark: false,
            contrast: true,
            window,
            card: window,
            card_border: text,
            card_hover: window,
            card_pressed: window,
            text,
            secondary: text,
            disabled: gray,
            nav: highlight,
            selected_text: highlight_text,
            accent: highlight,
            on_accent: highlight_text,
            control: Argb::solid(face),
            control_hover: Argb::solid(face),
            control_pressed: Argb::solid(face),
            control_text: face_text,
            stroke: Argb::solid(face_text),
            stroke_bottom: Argb::solid(face_text),
            field: window,
            field_focus: window,
            field_bottom: Argb::solid(text),
            flyout: window,
            flyout_item: highlight,
            flyout_border: text,
            switch_off: Argb::solid(window),
        }
    }

    fn ink(&self, ink: Ink) -> u32 {
        match ink {
            Ink::Text => self.text,
            Ink::Secondary => self.secondary,
            Ink::Disabled => self.disabled,
        }
    }
}

/// The palette and the brushes made of it.
#[derive(Clone, Copy)]
struct Theme {
    colors: Palette,
    window: HBRUSH,
    card: HBRUSH,
    field: HBRUSH,
    field_focus: HBRUSH,
    flyout: HBRUSH,
}

impl Theme {
    fn load() -> Self {
        let colors = Palette::current();
        let brush = |c| unsafe { CreateSolidBrush(c) };
        Self {
            window: brush(colors.window),
            card: brush(colors.card),
            field: brush(colors.field),
            field_focus: brush(colors.field_focus),
            flyout: brush(colors.flyout),
            colors,
        }
    }

    fn free(self) {
        for b in [
            self.window,
            self.card,
            self.field,
            self.field_focus,
            self.flyout,
        ] {
            unsafe { DeleteObject(b as HGDIOBJ) };
        }
    }
}

/// Set first thing in [`run`].
fn theme() -> Theme {
    THEME.get().expect("the theme is loaded before any window")
}

fn colors() -> Palette {
    theme().colors
}

fn high_contrast() -> bool {
    unsafe {
        let mut hc: HIGHCONTRASTW = zeroed();
        hc.cbSize = size_of::<HIGHCONTRASTW>() as u32;
        SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, &mut hc as *mut _ as _, 0) != 0
            && hc.dwFlags & HCF_HIGHCONTRASTON != 0
    }
}

/// Reads value `name` of `HKEY_CURRENT_USER\key` (of type `flags`) into `out`: its size.
fn registry(key: &str, name: &str, flags: u32, out: &mut [u8]) -> Option<usize> {
    let mut size = out.len() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(key).as_ptr(),
            wide(name).as_ptr(),
            flags,
            null_mut(),
            out.as_mut_ptr() as _,
            &mut size,
        )
    };
    (result == ERROR_SUCCESS).then_some(size as usize)
}

fn registry_dword(key: &str, name: &str) -> Option<u32> {
    let mut value = [0u8; 4];
    registry(key, name, RRF_RT_REG_DWORD, &mut value).map(|_| u32::from_le_bytes(value))
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

/// `dip` at the fonts' DPI, for drawing.
fn d(px: i32) -> i32 {
    dip(px, DPI.get())
}

fn scale() -> f32 {
    DPI.get() as f32 / 96.0
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

fn inset(r: &RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right - dx,
        bottom: r.bottom - dy,
    }
}

/// `r` moved up by `dy`.
fn up(r: &RECT, dy: i32) -> RECT {
    RECT {
        top: r.top - dy,
        bottom: r.bottom - dy,
        ..*r
    }
}

pub fn run(
    title: &str,
    mut model: Box<dyn Model>,
    page: Option<&str>,
    icon: Option<Icon>,
) -> Result<(), Error> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let mut gdiplus = 0;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..zeroed()
        };
        if GdiplusStartup(&mut gdiplus, &input, null_mut()) != 0 {
            return Err("cannot start GDI+".into());
        }
        THEME.set(Some(Theme::load()));
        ICON.set(icon);
        let instance = GetModuleHandleW(null());

        let class = wide(CLASS);
        let pane_class = wide(PANE_CLASS);
        let procs: [(&Vec<u16>, WNDPROC); 2] =
            [(&class, Some(window_proc)), (&pane_class, Some(pane_proc))];
        for (name, proc_) in procs {
            // No background brush: both paint their own (in the theme's colors).
            let wc = WNDCLASSW {
                lpfnWndProc: proc_,
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
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
        set_fonts(dpi);
        set_frame(window);
        set_icons(window, dpi);
        // Owner-drawn (see `draw_nav_item`), still a list box: keys, screen readers.
        let list = CreateWindowExW(
            0,
            wide("LISTBOX").as_ptr(),
            null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | LBS_OWNERDRAWFIXED | LBS_HASSTRINGS) as u32,
            0,
            0,
            10,
            10,
            window,
            LIST_ID as _,
            instance,
            null(),
        );
        SetWindowSubclass(list, Some(nav_proc), 0, 0);
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
        set_scrollbar_theme(pane);
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
                place_owed: false,
            })
        });
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
                    reveal(next);
                }
                continue;
            }
            // Tab between controls (scrolled into view), Enter as IDOK.
            if IsDialogMessageW(window, &msg) != 0 {
                if msg.message == WM_KEYDOWN && msg.wParam == VK_TAB as WPARAM {
                    reveal(GetFocus());
                }
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for font in FONTS.get() {
            DeleteObject(font as HGDIOBJ);
        }
        for icon in ICONS.get() {
            if !icon.is_null() {
                DestroyIcon(icon);
            }
        }
        if let Some(theme) = THEME.take() {
            theme.free();
        }
        GdiplusShutdown(gdiplus);
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

/// Whether a font family is installed.
fn installed(face: &str) -> bool {
    unsafe extern "system" fn found(
        _: *const LOGFONTW,
        _: *const TEXTMETRICW,
        _: u32,
        seen: LPARAM,
    ) -> i32 {
        unsafe { *(seen as *mut bool) = true };
        0
    }
    unsafe {
        let mut lf: LOGFONTW = zeroed();
        lf.lfCharSet = DEFAULT_CHARSET;
        for (d, s) in lf.lfFaceName[..31].iter_mut().zip(face.encode_utf16()) {
            *d = s;
        }
        let mut seen = false;
        let dc = GetDC(null_mut());
        EnumFontFamiliesExW(dc, &lf, Some(found), &mut seen as *mut bool as LPARAM, 0);
        ReleaseDC(null_mut(), dc);
        seen
    }
}

/// The fonts at `dpi`: Segoe UI Variable (Windows 11) if it's there, else the dialog font's,
/// in Fluent's sizes; Segoe Fluent Icons (else MDL2 Assets) for icons.
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
        let text = installed("Segoe UI Variable Text").then_some("Segoe UI Variable Text");
        let display = installed("Segoe UI Variable Display")
            .then_some("Segoe UI Variable Display")
            .or(text);
        let fluent = installed("Segoe Fluent Icons");
        FLUENT.set(fluent);
        let icons = Some(if fluent {
            "Segoe Fluent Icons"
        } else {
            "Segoe MDL2 Assets"
        });
        let font = |face: Option<&str>, px: i32, weight: u32| {
            let mut lf = metrics.lfMessageFont;
            if let Some(face) = face {
                lf.lfFaceName = [0; 32];
                for (d, s) in lf.lfFaceName[..31].iter_mut().zip(face.encode_utf16()) {
                    *d = s;
                }
            }
            lf.lfHeight = -dip(px, dpi);
            lf.lfWeight = weight as i32;
            lf.lfItalic = 0;
            lf.lfCharSet = DEFAULT_CHARSET;
            CreateFontIndirectW(&lf)
        };
        let fonts = [
            font(text, 14, FW_NORMAL),
            font(text, 12, FW_NORMAL),
            font(text, 14, FW_SEMIBOLD),
            font(display, 28, FW_SEMIBOLD),
            font(display, 20, FW_SEMIBOLD),
            font(icons, 16, FW_NORMAL),
            font(icons, 12, FW_NORMAL),
        ];
        DPI.set(dpi);
        for old in FONTS.replace(fonts) {
            if !old.is_null() {
                DeleteObject(old as HGDIOBJ);
            }
        }
    }
}

/// The title bar dark or light like the window, in the window's color (as Settings' is),
/// unless title bars show the accent color.
fn set_frame(window: HWND) {
    let c = colors();
    let dark = c.dark as i32;
    let caption = if c.contrast || registry_dword(DWM, "ColorPrevalence") == Some(1) {
        DWMWA_COLOR_DEFAULT
    } else {
        c.window
    };
    unsafe {
        DwmSetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark as *const i32 as _,
            4,
        );
        DwmSetWindowAttribute(
            window,
            DWMWA_CAPTION_COLOR as u32,
            &caption as *const u32 as _,
            4,
        );
    }
}

/// Dark scroll bars on `hwnd` (the pane, a field, a pop-up's list) in the dark theme.
fn set_scrollbar_theme(hwnd: HWND) {
    let name = wide("DarkMode_Explorer");
    let name = if colors().dark { name.as_ptr() } else { null() };
    unsafe { SetWindowTheme(hwnd, name, null()) };
}

/// The theme changed (dark, light, accent, high contrast): everything redrawn in it.
fn retheme(window: HWND) {
    let new = Theme::load();
    if THEME.get().is_some_and(|t| t.colors == new.colors) {
        new.free();
        return;
    }
    if let Some(old) = THEME.replace(Some(new)) {
        old.free();
    }
    set_frame(window);
    let Some((pane, controls)) = with(|s| (s.pane, s.controls.clone())) else {
        return;
    };
    set_scrollbar_theme(pane);
    for c in controls {
        if is_class(c, "Edit") {
            set_scrollbar_theme(c);
        } else if is_class(c, "ComboBox") {
            set_scrollbar_theme(drop_list(c));
        }
    }
    unsafe {
        // The title bar follows once the frame is redrawn.
        SetWindowPos(
            window,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        RedrawWindow(
            window,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN,
        );
    }
}

/// The window's icons at `dpi`, drawn by the app's [`Icon`].
fn set_icons(window: HWND, dpi: u32) {
    let Some(draw) = ICON.get() else { return };
    unsafe {
        let mut icons = [null_mut(); 2];
        for (i, (kind, metric)) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)]
            .into_iter()
            .enumerate()
        {
            icons[i] = icon(draw, GetSystemMetricsForDpi(metric, dpi));
            SendMessageW(window, WM_SETICON, kind as WPARAM, icons[i] as LPARAM);
        }
        for old in ICONS.replace(icons) {
            if !old.is_null() {
                DestroyIcon(old);
            }
        }
    }
}

/// `draw`'s icon, `size` pixels square.
unsafe fn icon(draw: Icon, size: i32) -> HICON {
    let n = size.max(1) as usize;
    let rgba = draw(n as u32);
    if rgba.len() != n * n * 4 {
        return null_mut();
    }
    unsafe {
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: n as i32,
            // Top-down, as `rgba`.
            biHeight: -(n as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };
        let mut bits = null_mut();
        let color = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() {
            return null_mut();
        }
        let out = std::slice::from_raw_parts_mut(bits as *mut u8, n * n * 4);
        for (o, [r, g, b, a]) in out
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(rgba.as_chunks().0)
        {
            *o = [*b, *g, *r, *a];
        }
        // Unused (the alpha channel says what's transparent), but required.
        let mask_bits = vec![0u8; n.div_ceil(16) * 2 * n];
        let mask = CreateBitmap(n as i32, n as i32, 1, 1, mask_bits.as_ptr() as _);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        DeleteObject(color as HGDIOBJ);
        DeleteObject(mask as HGDIOBJ);
        icon
    }
}

/// A Segoe Fluent Icons (or MDL2 Assets) look-alike of SF Symbol `symbol`.
fn glyph(symbol: &str) -> Option<char> {
    Some(match symbol {
        "gearshape" => '\u{E713}',
        "waveform" => '\u{E720}',
        // Sparkles are only in Fluent Icons; a pen otherwise.
        "wand.and.stars" if FLUENT.get() => '\u{F4A5}',
        "wand.and.stars" => '\u{E70F}',
        "clock.arrow.circlepath" => '\u{E81C}',
        "arrow.down.circle" => '\u{E896}',
        "chart.bar.xaxis" => '\u{E9D2}',
        "exclamationmark.triangle" => '\u{E7BA}',
        _ => return None,
    })
}

const CHEVRON_DOWN: &str = "\u{E70D}";
const CHEVRON_RIGHT: &str = "\u{E76C}";

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
    // Minimized: its rectangle is parked off-screen. Done once it's restored.
    if unsafe { IsIconic(window) } != 0 {
        with(|s| s.place_owed = true);
        return;
    }
    let Some((dpi, list, pane, page_height, pages, scroll)) = with(|s| {
        let height = s
            .pages
            .get(s.current)
            .map_or(0, |p| layout(p, s.dpi).height);
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

        let pane_width = dip(MARGIN + CARD_WIDTH + MARGIN + SCROLLBAR, dpi);
        let nav = dip(NAV_WIDTH, dpi);
        let width = nav + pane_width;
        // The first item level with the page's title.
        let item = dip(NAV_ITEM, dpi);
        let list_top = (dip(TOP, dpi) + (line_height(Font::Title) - item) / 2).max(0);
        let list_height = pages as i32 * item;
        let height = page_height
            .max(list_top + list_height + dip(MARGIN, dpi))
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
        let left = dip(NAV_LEFT, dpi);
        MoveWindow(
            list,
            left,
            list_top,
            nav - left,
            list_height.min(height - list_top),
            1,
        );
        MoveWindow(pane, nav, 0, pane_width, height, 1);
    }
    // A taller pane may no longer scroll that far.
    scroll_to(scroll);
    update_scrollbar();
}

/// A page's layout: its controls and texts, what the pane paints, in pixels from the page's
/// top, and its height.
struct Layout {
    items: Vec<(Placed, RECT)>,
    decor: Decor,
    height: i32,
}

enum Placed {
    /// A STATIC: `align` an `SS_*` style.
    Text {
        text: String,
        font: Font,
        ink: Ink,
        on_card: bool,
        align: u32,
    },
    /// The row's control. An edit field: its box (see [`Decor::fields`]); a button that's a
    /// whole card: the card, and room for its focus ring.
    Row(Row),
}

impl Layout {
    fn text(&mut self, text: &str, font: Font, ink: Ink, on_card: bool, align: u32, r: RECT) {
        if text.is_empty() {
            return;
        }
        let text = text.to_owned();
        self.items.push((
            Placed::Text {
                text,
                font,
                ink,
                on_card,
                align,
            },
            r,
        ));
    }
}

fn layout(page: &Page, dpi: u32) -> Layout {
    let d = |v| dip(v, dpi);
    let (x, width) = (d(MARGIN), d(CARD_WIDTH));
    let mut out = Layout {
        items: Vec::new(),
        decor: Decor::default(),
        height: 0,
    };
    let mut y = d(TOP);
    let h = line_height(Font::Title);
    out.text(
        &page.title,
        Font::Title,
        Ink::Text,
        false,
        SS_LEFT,
        rect(x, y, width, h),
    );
    y += h + d(TITLE_GAP);
    for (i, section) in page.sections.iter().enumerate() {
        if i > 0 {
            y += d(SECTION_GAP);
        }
        if !section.title.is_empty() {
            let h = text_height(Font::Strong, &section.title, width);
            let r = rect(x, y, width, h);
            out.text(&section.title, Font::Strong, Ink::Text, false, SS_LEFT, r);
            y += h + d(HEADER_GAP);
        }
        for (j, row) in section.rows.iter().enumerate() {
            if j > 0 {
                y += d(CARD_GAP);
            }
            y += card(row, x, y, width, dpi, &mut out);
        }
    }
    out.height = y + d(MARGIN);
    out
}

/// Lays out `row` as a card at `(x, y)`, `width` wide: its height.
fn card(row: &Row, x: i32, y: i32, width: i32, dpi: u32, out: &mut Layout) -> i32 {
    let d = |v| dip(v, dpi);
    let (pad, pad_y, focus) = (d(CARD_PADDING), d(CARD_PADDING_Y), d(FOCUS));
    let inner = width - 2 * pad;
    let ink = |ink| if row.enabled { ink } else { Ink::Disabled };
    match &row.control {
        // Clickable, like Settings' links to other pages.
        Control::Button(title) if row.label.is_empty() => {
            let h = button_card_height(title, &row.note, width, dpi);
            let r = rect(x - focus, y - focus, width + 2 * focus, h + 2 * focus);
            out.items.push((Placed::Row(row.clone()), r));
            return h;
        }
        Control::Stats(stats) => {
            let n = stats.len().max(1) as i32;
            let column = inner / n;
            let (value_h, caption_h) = (line_height(Font::Big), line_height(Font::Caption));
            for (i, Stat { value, caption }) in stats.iter().enumerate() {
                let left = x + pad + i as i32 * column;
                let r = rect(left, y + pad_y, column, value_h);
                out.text(value, Font::Big, Ink::Text, true, SS_CENTER, r);
                let r = rect(left, r.bottom + d(NOTE_GAP), column, caption_h);
                out.text(caption, Font::Caption, Ink::Secondary, true, SS_CENTER, r);
            }
            let h = 2 * pad_y + value_h + d(NOTE_GAP) + caption_h;
            out.decor.cards.push(rect(x, y, width, h));
            return h;
        }
        Control::Table { columns, rows } => {
            let rest = columns.len().saturating_sub(1) as i32;
            let column = d(TABLE_COLUMN_WIDTH);
            let first = inner - rest * (column + d(TABLE_GAP));
            let (line, text_h) = (d(TABLE_LINE), line_height(Font::Body));
            let lines = std::iter::once((columns, true)).chain(rows.iter().map(|r| (r, false)));
            for (i, (texts, header)) in lines.enumerate() {
                let top = y + d(TABLE_PADDING) + i as i32 * line;
                if i > 0 {
                    out.decor
                        .rules
                        .push(rect(x + pad, top, inner, hairline() as i32));
                }
                let (font, color) = if header {
                    (Font::Strong, Ink::Secondary)
                } else {
                    (Font::Body, Ink::Text)
                };
                let ty = top + (line - text_h) / 2;
                for c in 0..columns.len() {
                    let text = texts.get(c).map_or("", String::as_str);
                    let r = if c == 0 {
                        rect(x + pad, ty, first, text_h)
                    } else {
                        let left = first + d(TABLE_GAP) + (c as i32 - 1) * (column + d(TABLE_GAP));
                        rect(x + pad + left, ty, column, text_h)
                    };
                    let align = if c == 0 { SS_LEFT } else { SS_RIGHT };
                    out.text(text, font, color, true, align, r);
                }
            }
            let h = 2 * d(TABLE_PADDING) + (rows.len() as i32 + 1) * line;
            out.decor.cards.push(rect(x, y, width, h));
            return h;
        }
        _ => {}
    }
    // The title: the label, a switch's own text, or a value without a label.
    let (title, title_ink) = match &row.control {
        Control::Toggle { text, .. } if row.label.is_empty() => (text.as_str(), ink(Ink::Text)),
        // Read-only text: nothing to grey out.
        Control::Value(text) if row.label.is_empty() => (text.as_str(), Ink::Text),
        _ => (row.label.as_str(), ink(Ink::Text)),
    };
    let control = d(CONTROL_HEIGHT);
    let (with_ring, line) = (control + 2 * focus, line_height(Font::Body));
    // The control on the right: its window's size, and the margin in it (its focus ring's).
    let right = match &row.control {
        Control::Toggle { .. } => Some((switch_width() + 2 * focus, with_ring, focus)),
        Control::Choice { options, .. } => {
            let widest = options
                .iter()
                .map(|o| text_width(Font::Body, o))
                .max()
                .unwrap_or(0);
            let w = (widest + d(CHOICE_TEXT + CHEVRON)).clamp(d(CHOICE_MIN), d(CHOICE_MAX));
            Some((w + 2 * focus, with_ring, focus))
        }
        // Its focus shows on its bottom line.
        Control::Text { .. } => Some((d(TEXT_WIDTH), control, 0)),
        Control::Button(title) => {
            let w = (text_width(Font::Body, title) + d(2 * BUTTON_PADDING)).max(d(BUTTON_MIN));
            Some((w + 2 * focus, with_ring, focus))
        }
        Control::Value(text) if !row.label.is_empty() => {
            let w = text_width(Font::Body, text);
            (w <= d(VALUE_WIDTH)).then_some((w, line, 0))
        }
        _ => None,
    };
    let text_w = right.map_or(inner, |(w, _, m)| inner - (w - 2 * m) - d(CONTROL_GAP));
    let title_h = text_height(Font::Body, title, text_w);
    let note_h = text_height(Font::Caption, &row.note, text_w);
    let gap = if title_h > 0 && note_h > 0 {
        d(NOTE_GAP)
    } else {
        0
    };
    let block = title_h + gap + note_h;
    // Across the card, under its text: a long text's field, a long value.
    let long_value = match &row.control {
        Control::Value(text) if !row.label.is_empty() && right.is_none() => Some(text),
        _ => None,
    };
    let under = match (&row.control, long_value) {
        (Control::LongText { .. }, _) => d(UNDER_GAP) + d(LONG_TEXT_HEIGHT),
        (_, Some(text)) => d(NOTE_GAP) + text_height(Font::Body, text, inner),
        _ => 0,
    };
    let visual = right.map_or(0, |(_, h, m)| h - 2 * m);
    let h = (block.max(visual) + under + 2 * pad_y).max(d(CARD_MIN));
    // Centered, unless something's under the text.
    let top = if under > 0 {
        y + pad_y
    } else {
        y + (h - block) / 2
    };
    // The title right before its control: a screen reader names the control after it.
    let r = rect(x + pad, top, text_w, title_h);
    out.text(title, Font::Body, title_ink, true, SS_LEFT, r);
    if let Some((w, ch, m)) = right {
        let r = rect(x + width - pad + m - w, y + (h - ch) / 2, w, ch);
        match &row.control {
            Control::Value(text) => out.text(text, Font::Body, Ink::Secondary, true, SS_RIGHT, r),
            _ => {
                if matches!(row.control, Control::Text { .. }) {
                    out.decor.fields.push((r, FIRST_ID + row.id as i32));
                }
                out.items.push((Placed::Row(row.clone()), r));
            }
        }
    }
    if let Control::LongText { .. } = row.control {
        let r = rect(
            x + pad,
            top + block + d(UNDER_GAP),
            inner,
            d(LONG_TEXT_HEIGHT),
        );
        out.decor.fields.push((r, FIRST_ID + row.id as i32));
        out.items.push((Placed::Row(row.clone()), r));
    }
    // After the control, so it's not what names it.
    let r = rect(x + pad, top + title_h + gap, text_w, note_h);
    out.text(
        &row.note,
        Font::Caption,
        ink(Ink::Secondary),
        true,
        SS_LEFT,
        r,
    );
    if let Some(text) = long_value {
        let r = rect(
            x + pad,
            top + block + d(NOTE_GAP),
            inner,
            under - d(NOTE_GAP),
        );
        out.text(text, Font::Body, Ink::Secondary, true, SS_LEFT, r);
    }
    out.decor.cards.push(rect(x, y, width, h));
    h
}

/// The width of a card that's a button, left of its chevron, for its text.
fn button_card_text_width(width: i32, dpi: u32) -> i32 {
    width - dip(2 * CARD_PADDING + CHEVRON, dpi)
}

fn button_card_height(title: &str, note: &str, width: i32, dpi: u32) -> i32 {
    let w = button_card_text_width(width, dpi);
    let note_h = if note.is_empty() {
        0
    } else {
        dip(NOTE_GAP, dpi) + text_height(Font::Caption, note, w)
    };
    let h = text_height(Font::Body, title, w) + note_h + 2 * dip(CARD_PADDING_Y, dpi);
    h.max(dip(CARD_MIN, dpi))
}

/// A switch's window: "Off" (the wider), the gap, the switch.
fn switch_width() -> i32 {
    let text = text_width(Font::Body, "On").max(text_width(Font::Body, "Off"));
    text + d(SWITCH_GAP + SWITCH_WIDTH)
}

/// `text`'s size in `font`: wrapped at `width` (as a STATIC with `SS_EDITCONTROL` does), or on
/// one line for `width` 0.
fn measure(font: Font, text: &str, width: i32) -> (i32, i32) {
    if text.is_empty() {
        return (0, 0);
    }
    unsafe {
        let dc: HDC = GetDC(null_mut());
        let old = SelectObject(dc, font.handle() as HGDIOBJ);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: 0,
        };
        let mut text: Vec<u16> = text.encode_utf16().collect();
        let wrap = if width > 0 {
            DT_WORDBREAK | DT_EDITCONTROL
        } else {
            DT_SINGLELINE
        };
        let flags = DT_CALCRECT | DT_NOPREFIX | wrap;
        DrawTextW(dc, text.as_mut_ptr(), text.len() as i32, &mut rect, flags);
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        (rect.right, rect.bottom)
    }
}

fn text_height(font: Font, text: &str, width: i32) -> i32 {
    measure(font, text, width).1
}

fn text_width(font: Font, text: &str) -> i32 {
    measure(font, text, 0).0
}

fn line_height(font: Font) -> i32 {
    measure(font, "Ag", 0).1
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
        let titles: Vec<(String, &str)> = s
            .pages
            .iter()
            .map(|p| (p.title.clone(), p.symbol))
            .collect();
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
        SendMessageW(list, WM_SETREDRAW, 0, 0);
        SendMessageW(list, WM_SETFONT, Font::Body.handle() as WPARAM, 0);
        let item_height = dip(NAV_ITEM, dpi);
        SendMessageW(list, LB_SETITEMHEIGHT, 0, item_height as LPARAM);
        SendMessageW(list, LB_RESETCONTENT, 0, 0);
        for (t, symbol) in &titles {
            let i = SendMessageW(list, LB_ADDSTRING, 0, wide(t).as_ptr() as LPARAM);
            let glyph = glyph(symbol).map_or(0, |c| c as LPARAM);
            SendMessageW(list, LB_SETITEMDATA, i as WPARAM, glyph);
        }
        SendMessageW(list, LB_SETCURSEL, current, 0);
        SendMessageW(list, WM_SETREDRAW, 1, 0);
        InvalidateRect(list, null(), 1);

        SendMessageW(pane, WM_SETREDRAW, 0, 0);
        let old = with(|s| std::mem::take(&mut s.controls)).unwrap_or_default();
        for c in old {
            DestroyWindow(c);
        }
        LOOKS.with(|l| l.borrow_mut().clear());
        let layout = layout(&page, dpi);
        let height = layout.height;
        DECOR.set(layout.decor);
        // A page that got shorter (rows removed) may no longer scroll that far.
        let mut client: RECT = zeroed();
        GetClientRect(pane, &mut client);
        let scroll = scroll.min((height - client.bottom).max(0));
        with(|s| s.scroll = scroll);
        let mut controls = Vec::new();
        let mut shown_text = HashMap::new();
        for (item, rect) in layout.items {
            let hwnd = create(pane, &item, &up(&rect, scroll), &mut shown_text);
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
        RedrawWindow(
            pane,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN,
        );
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
    shown_text: &mut HashMap<usize, String>,
) -> HWND {
    let (class, text, style, id, enabled, font, look) = match item {
        Placed::Text {
            text,
            font,
            ink,
            on_card,
            align,
        } => {
            // Wrapped as `measure` does.
            let wrap = if *align == SS_LEFT { SS_EDITCONTROL } else { 0 };
            let look = Look::Text(*ink, *on_card);
            let style = align | wrap | SS_NOPREFIX;
            ("STATIC", text.clone(), style, 0, true, *font, Some(look))
        }
        Placed::Row(row) => {
            let id = FIRST_ID + row.id as i32;
            let (class, text, style, look) = match &row.control {
                // Its text for screen readers; it shows "On" or "Off".
                Control::Toggle { text, .. } => (
                    "BUTTON",
                    text.clone(),
                    BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
                    Some(Look::Switch),
                ),
                Control::Choice { .. } => (
                    "COMBOBOX",
                    String::new(),
                    (CBS_DROPDOWNLIST | CBS_OWNERDRAWFIXED | CBS_HASSTRINGS) as u32
                        | WS_VSCROLL
                        | WS_TABSTOP,
                    None,
                ),
                Control::Text { value, secret, .. } => {
                    shown_text.insert(row.id, value.clone());
                    let password = if *secret { ES_PASSWORD } else { 0 };
                    let style = (ES_AUTOHSCROLL | password) as u32 | WS_TABSTOP;
                    ("EDIT", value.clone(), style, None)
                }
                Control::LongText { value } => {
                    shown_text.insert(row.id, value.clone());
                    (
                        "EDIT",
                        value.replace('\n', "\r\n"),
                        (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32
                            | WS_VSCROLL
                            | WS_TABSTOP,
                        None,
                    )
                }
                // Laid out as `Placed::Text`s.
                Control::Value(_) | Control::Stats(_) | Control::Table { .. } => {
                    return null_mut();
                }
                Control::Button(title) => {
                    let look = if row.label.is_empty() {
                        Look::Card(row.note.clone())
                    } else {
                        Look::Button
                    };
                    // A command link: its note (`BCM_SETNOTE`) is its description for
                    // screen readers. Drawn here all the same.
                    let kind = if row.label.is_empty() {
                        BS_COMMANDLINK
                    } else {
                        BS_PUSHBUTTON
                    };
                    let style = kind as u32 | WS_TABSTOP;
                    ("BUTTON", title.clone(), style, Some(look))
                }
            };
            (class, text, style, id, row.enabled, Font::Body, look)
        }
    };
    let (mut x, mut y, mut w, mut h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
    let multiline = style & ES_MULTILINE as u32 != 0;
    match class {
        // Borderless, inside the box the pane paints; one line centered in it.
        "EDIT" if multiline => {
            (x, y, w, h) = (
                x + d(FIELD_PADDING),
                y + d(6),
                w - d(FIELD_PADDING + 2),
                h - d(12),
            );
        }
        "EDIT" => {
            let line = line_height(Font::Body);
            (x, y, w, h) = (
                x + d(FIELD_PADDING),
                y + (h - line) / 2,
                w - d(2 * FIELD_PADDING),
                line,
            );
        }
        // Its height includes its drop-down list.
        "COMBOBOX" => h = d(DROP_HEIGHT),
        _ => {}
    }
    unsafe {
        let hwnd = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(&text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            pane,
            id as isize as _,
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return hwnd;
        }
        if let Some(look) = look {
            LOOKS.with(|l| l.borrow_mut().insert(hwnd, look));
        }
        SendMessageW(hwnd, WM_SETFONT, font.handle() as WPARAM, 0);
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
                    // Its closed height is its selection field's (owner-drawn: set) and borders.
                    let mut closed: RECT = zeroed();
                    GetWindowRect(hwnd, &mut closed);
                    let field = SendMessageW(hwnd, CB_GETITEMHEIGHT, usize::MAX, 0) as i32;
                    let want = field + (r.bottom - r.top) - (closed.bottom - closed.top);
                    SendMessageW(hwnd, CB_SETITEMHEIGHT, usize::MAX, want as LPARAM);
                    // The open list shows whole options (a long microphone name), up to a
                    // card's width.
                    let widest = options.iter().map(|o| text_width(Font::Body, o)).max();
                    let scrollbar = GetSystemMetricsForDpi(SM_CXVSCROLL, DPI.get());
                    let list = widest.unwrap_or(0) + d(DROP_TEXT + 8) + scrollbar;
                    let list = list.clamp(r.right - r.left, d(CARD_WIDTH));
                    SendMessageW(hwnd, CB_SETDROPPEDWIDTH, list as WPARAM, 0);
                    SetWindowSubclass(hwnd, Some(combo_proc), 0, 0);
                    let list = drop_list(hwnd);
                    SetWindowSubclass(list, Some(drop_list_proc), 0, 0);
                    set_scrollbar_theme(list);
                }
                Control::Text { placeholder, .. } if !placeholder.is_empty() => {
                    SendMessageW(
                        hwnd,
                        EM_SETCUEBANNER,
                        0,
                        wide(placeholder).as_ptr() as LPARAM,
                    );
                }
                Control::LongText { .. } => {
                    set_scrollbar_theme(hwnd);
                    fit_scrollbar(hwnd);
                }
                Control::Button(_) if row.label.is_empty() && !row.note.is_empty() => {
                    SendMessageW(hwnd, BCM_SETNOTE, 0, wide(&row.note).as_ptr() as LPARAM);
                }
                _ => {}
            }
        }
        if !enabled {
            EnableWindow(hwnd, 0);
        }
        hwnd
    }
}

/// A pop-up's drop-down list.
fn drop_list(combo: HWND) -> HWND {
    unsafe {
        let mut info: COMBOBOXINFO = zeroed();
        info.cbSize = size_of::<COMBOBOXINFO>() as u32;
        GetComboBoxInfo(combo, &mut info);
        info.hwndList
    }
}

/// Shows a multi-line field's scroll bar only while its text doesn't fit.
fn fit_scrollbar(edit: HWND) {
    unsafe {
        let lines = SendMessageW(edit, EM_GETLINECOUNT, 0, 0) as i32;
        let mut client: RECT = zeroed();
        GetClientRect(edit, &mut client);
        let fits = lines * line_height(Font::Body) <= client.bottom;
        ShowScrollBar(edit, SB_VERT, (!fits) as i32);
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

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

fn edit_text(hwnd: HWND) -> String {
    window_text(hwnd).replace("\r\n", "\n")
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

/// Whether `hwnd` is a push button (or a command link): Enter clicks it.
fn pushed(hwnd: HWND) -> bool {
    let kind = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } & BS_TYPEMASK;
    is_class(hwnd, "Button")
        && [
            BS_PUSHBUTTON,
            BS_DEFPUSHBUTTON,
            BS_COMMANDLINK,
            BS_DEFCOMMANDLINK,
        ]
        .contains(&kind)
}

/// Scrolls the pane so `control` shows, with its whole card if that fits: the keyboard
/// moved to it.
fn reveal(control: HWND) {
    let Some((pane, scroll)) = with(|s| (s.pane, s.scroll)) else {
        return;
    };
    unsafe {
        if GetParent(control) != pane {
            return;
        }
        let mut r: RECT = zeroed();
        GetWindowRect(control, &mut r);
        MapWindowPoints(null_mut(), pane, &mut r as *mut RECT as *mut POINT, 2);
        let r = up(&r, -scroll);
        let mut client: RECT = zeroed();
        GetClientRect(pane, &mut client);
        let card = DECOR.with(|d| {
            let d = d.borrow();
            let around = |c: &&RECT| c.top <= r.top && r.bottom <= c.bottom;
            d.cards.iter().find(around).copied()
        });
        let shown = card
            .filter(|c| c.bottom - c.top <= client.bottom)
            .unwrap_or(r);
        let margin = d(CARD_PADDING);
        if shown.top - margin < scroll {
            scroll_to(shown.top - margin);
        } else if shown.bottom + margin > scroll + client.bottom {
            scroll_to(shown.bottom + margin - client.bottom);
        }
    }
}

/// Whether focus rectangles show (the keyboard was used), as for dialogs.
fn cues(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0 }
}

/// A canvas: shapes with GDI+ (anti-aliased), text with GDI (ClearType), in the coordinates
/// of the DC [`buffered`] draws for.
struct Canvas {
    dc: HDC,
    g: *mut GpGraphics,
    origin: (i32, i32),
}

impl Canvas {
    fn local(&self, r: &RECT) -> (f32, f32, f32, f32) {
        let (x, y) = (
            (r.left - self.origin.0) as f32,
            (r.top - self.origin.1) as f32,
        );
        (x, y, (r.right - r.left) as f32, (r.bottom - r.top) as f32)
    }

    fn fill(&self, path: *mut GpPath, color: Argb) {
        unsafe {
            let mut brush = null_mut();
            GdipCreateSolidFill(color.0, &mut brush);
            GdipFillPath(self.g, brush as *mut GpBrush, path);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    fn rect(&self, r: &RECT, color: Argb) {
        let (x, y, w, h) = self.local(r);
        unsafe {
            let mut brush = null_mut();
            GdipCreateSolidFill(color.0, &mut brush);
            GdipFillRectangle(self.g, brush as *mut GpBrush, x, y, w, h);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    /// A rounded rectangle filling `r`, outlined (1 px) inside it.
    fn round(&self, r: &RECT, radius: f32, fill: Option<Argb>, stroke: Option<Argb>) {
        if let Some(color) = fill {
            let (x, y, w, h) = self.local(r);
            let path = rounded(x, y, w, h, radius);
            self.fill(path, color);
            unsafe { GdipDeletePath(path) };
        }
        if let Some(color) = stroke {
            self.ring(r, radius, hairline(), color);
        }
    }

    /// The bottom `height` pixels of a rounded rectangle filling `r`: its bottom edge.
    fn bottom(&self, r: &RECT, radius: f32, height: f32, color: Argb) {
        let (x, y, w, h) = self.local(r);
        unsafe {
            GdipSetClipRect(self.g, x, y + h - height, w, height, CombineModeReplace);
            let path = rounded(x, y, w, h, radius);
            self.fill(path, color);
            GdipDeletePath(path);
            GdipResetClip(self.g);
        }
    }

    /// An outline `width` pixels wide inside `r`.
    fn ring(&self, r: &RECT, radius: f32, width: f32, color: Argb) {
        let (x, y, w, h) = self.local(r);
        let i = width / 2.0;
        unsafe {
            let path = rounded(x + i, y + i, w - width, h - width, (radius - i).max(0.5));
            let mut pen = null_mut();
            GdipCreatePen1(color.0, width, UnitPixel, &mut pen);
            GdipDrawPath(self.g, pen, path);
            GdipDeletePen(pen);
            GdipDeletePath(path);
        }
    }

    fn circle(&self, cx: f32, cy: f32, radius: f32, color: Argb) {
        let (cx, cy) = (cx - self.origin.0 as f32, cy - self.origin.1 as f32);
        unsafe {
            let mut brush = null_mut();
            GdipCreateSolidFill(color.0, &mut brush);
            let d = 2.0 * radius;
            GdipFillEllipse(
                self.g,
                brush as *mut GpBrush,
                cx - radius,
                cy - radius,
                d,
                d,
            );
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    fn text(&self, text: &str, r: &RECT, font: Font, color: u32, flags: u32) {
        // (DrawTextW reads past an empty one.)
        if text.is_empty() {
            return;
        }
        let (x, y, w, h) = self.local(r);
        let mut r = rect(x as i32, y as i32, w as i32, h as i32);
        let mut text: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            // What GDI+ drew first.
            GdipFlush(self.g, FlushIntentionSync);
            let old = SelectObject(self.dc, font.handle() as HGDIOBJ);
            SetBkMode(self.dc, TRANSPARENT as i32);
            SetTextColor(self.dc, color);
            let flags = flags | DT_NOPREFIX;
            DrawTextW(self.dc, text.as_mut_ptr(), text.len() as i32, &mut r, flags);
            SelectObject(self.dc, old);
        }
    }
}

/// A rounded rectangle's path (deleted by the caller).
fn rounded(x: f32, y: f32, w: f32, h: f32, radius: f32) -> *mut GpPath {
    let d = (2.0 * radius).min(w).min(h).max(0.1);
    unsafe {
        let mut path = null_mut();
        GdipCreatePath(FillModeAlternate, &mut path);
        GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
        GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
        GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
        GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
        GdipClosePathFigure(path);
        path
    }
}

/// Draws `r` of `dc` off-screen with `draw`, then copies it there in one go: no flicker.
fn buffered(dc: HDC, r: &RECT, draw: impl FnOnce(&Canvas)) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    unsafe {
        // In the screen's format (`dc` may be a memory DC).
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        ReleaseDC(null_mut(), screen);
        let old = SelectObject(mem, bitmap as HGDIOBJ);
        let mut g = null_mut();
        GdipCreateFromHDC(mem, &mut g);
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        // Pixel centers at .5: a rectangle on whole pixels has crisp edges.
        GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
        draw(&Canvas {
            dc: mem,
            g,
            origin: (r.left, r.top),
        });
        GdipDeleteGraphics(g);
        BitBlt(dc, r.left, r.top, w, h, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(mem);
    }
}

/// A line's width: 1 DIP, in whole pixels.
fn hairline() -> f32 {
    scale().round().max(1.0)
}

fn radius() -> f32 {
    RADIUS as f32 * scale()
}

/// A button's or pop-up's box: Fluent's, a little darker (light) or lighter (dark) on one
/// edge.
fn control_box(c: &Canvas, r: &RECT, fill: Argb) {
    let p = colors();
    c.round(r, radius(), Some(fill), Some(p.stroke));
    c.bottom(r, radius(), hairline(), p.stroke_bottom);
}

/// A focus ring around `r` (a control's window, the box inside it by [`FOCUS`]).
fn focus_ring(c: &Canvas, r: &RECT, radius: f32) {
    let s = scale();
    c.ring(
        r,
        radius + FOCUS as f32 * s,
        2.0 * hairline(),
        Argb::solid(colors().text),
    );
}

/// The accent's short bar at the left of a selected item `r`.
fn selection_bar(c: &Canvas, r: &RECT) {
    let p = colors();
    let mid = (r.top + r.bottom) / 2;
    let bar = rect(r.left, mid - d(8), d(3), d(16));
    let color = if p.contrast {
        p.selected_text
    } else {
        p.accent
    };
    c.round(&bar, 1.5 * scale(), Some(Argb::solid(color)), None);
}

/// A button's or pop-up's fill in its state.
fn control_fill(enabled: bool, pressed: bool, hot: bool) -> Argb {
    let p = colors();
    match () {
        _ if !enabled => p.control,
        _ if pressed => p.control_pressed,
        _ if hot => p.control_hover,
        _ => p.control,
    }
}

/// The page list's item.
fn draw_nav_item(item: &DRAWITEMSTRUCT) {
    let p = colors();
    let r = item.rcItem;
    buffered(item.hDC, &r, |c| {
        c.rect(&r, Argb::solid(p.window));
        if item.itemID == u32::MAX {
            return;
        }
        let selected = item.itemState & ODS_SELECTED != 0;
        let hot = NAV_HOT.get() == item.itemID as i32;
        let pill = inset(&r, d(NAV_INSET), d(1));
        if selected || (hot && !p.contrast) {
            c.round(&pill, radius(), Some(Argb::solid(p.nav)), None);
        }
        let ink = if selected { p.selected_text } else { p.text };
        if selected {
            selection_bar(c, &pill);
        }
        if let Some(g) = char::from_u32(item.itemData as u32).filter(|&g| g != '\0') {
            let icon = rect(pill.left + d(12), r.top, d(16), r.bottom - r.top);
            let flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
            c.text(&g.to_string(), &icon, Font::Icon, ink, flags);
        }
        let title = unsafe {
            let len = SendMessageW(item.hwndItem, LB_GETTEXTLEN, item.itemID as WPARAM, 0);
            let mut buf = vec![0u16; len.max(0) as usize + 1];
            let n = SendMessageW(
                item.hwndItem,
                LB_GETTEXT,
                item.itemID as WPARAM,
                buf.as_mut_ptr() as LPARAM,
            );
            String::from_utf16_lossy(&buf[..n.max(0) as usize])
        };
        let text = RECT {
            left: pill.left + d(12 + 16 + 12),
            right: pill.right - d(8),
            ..r
        };
        let flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
        c.text(&title, &text, Font::Body, ink, flags);
        if item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0 {
            c.ring(&pill, radius(), 2.0 * hairline(), Argb::solid(p.text));
        }
    });
}

/// A pop-up, closed: Fluent's box, the choice, a chevron.
fn paint_combo(combo: HWND, dc: HDC) {
    let p = colors();
    unsafe {
        let mut r: RECT = zeroed();
        GetClientRect(combo, &mut r);
        let enabled = IsWindowEnabled(combo) != 0;
        let dropped = SendMessageW(combo, CB_GETDROPPEDSTATE, 0, 0) != 0;
        let hot = HOT.get() == combo;
        let focused = GetFocus() == combo && cues(combo);
        let index = SendMessageW(combo, CB_GETCURSEL, 0, 0);
        let text = usize::try_from(index).map_or(String::new(), |i| option(combo, i));
        buffered(dc, &r, |c| {
            c.rect(&r, Argb::solid(p.card));
            let b = inset(&r, d(FOCUS), d(FOCUS));
            control_box(c, &b, control_fill(enabled, dropped, hot));
            let ink = if enabled { p.control_text } else { p.disabled };
            let label = RECT {
                left: b.left + d(CHOICE_TEXT),
                right: b.right - d(CHEVRON),
                ..b
            };
            let flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
            c.text(&text, &label, Font::Body, ink, flags);
            let chevron = RECT {
                left: b.right - d(CHEVRON),
                ..b
            };
            let ink = if enabled { p.secondary } else { p.disabled };
            let flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
            c.text(CHEVRON_DOWN, &chevron, Font::Glyph, ink, flags);
            if focused {
                focus_ring(c, &r, radius());
            }
        });
    }
}

/// A pop-up's option `index`.
fn option(combo: HWND, index: usize) -> String {
    unsafe {
        let len = SendMessageW(combo, CB_GETLBTEXTLEN, index, 0);
        let mut buf = vec![0u16; len.max(0) as usize + 1];
        let n = SendMessageW(combo, CB_GETLBTEXT, index, buf.as_mut_ptr() as LPARAM);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// An item of an open pop-up's list.
fn draw_drop_item(item: &DRAWITEMSTRUCT) {
    let p = colors();
    let r = item.rcItem;
    buffered(item.hDC, &r, |c| {
        c.rect(&r, Argb::solid(p.flyout));
        if item.itemID == u32::MAX {
            return;
        }
        let selected = item.itemState & ODS_SELECTED != 0;
        let b = inset(&r, d(4), d(2));
        if selected {
            c.round(&b, radius(), Some(Argb::solid(p.flyout_item)), None);
            selection_bar(c, &b);
        }
        let text = option(item.hwndItem, item.itemID as usize);
        let ink = if selected { p.selected_text } else { p.text };
        let label = RECT {
            left: r.left + d(DROP_TEXT),
            right: b.right - d(8),
            ..r
        };
        let flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
        c.text(&text, &label, Font::Body, ink, flags);
    });
}

/// A button of the page (custom draw): a switch, a push button, a card. Whether it was one.
fn draw_button(cd: &NMCUSTOMDRAW) -> bool {
    let hwnd = cd.hdr.hwndFrom;
    let Some(look) = LOOKS.with(|l| {
        let l = l.try_borrow().ok()?;
        match l.get(&hwnd)? {
            Look::Switch => Some(Look::Switch),
            Look::Button => Some(Look::Button),
            Look::Card(note) => Some(Look::Card(note.clone())),
            Look::Text(..) => None,
        }
    }) else {
        return false;
    };
    let p = colors();
    let s = scale();
    let state = cd.uItemState;
    let (hot, pressed) = (state & CDIS_HOT != 0, state & CDIS_SELECTED != 0);
    let enabled = state & CDIS_DISABLED == 0;
    let focused = unsafe { GetFocus() } == hwnd && cues(hwnd);
    let r = cd.rc;
    let b = inset(&r, d(FOCUS), d(FOCUS));
    buffered(cd.hdc, &r, |c| match &look {
        Look::Switch => {
            c.rect(&r, Argb::solid(p.card));
            let on = unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) } == BST_CHECKED as isize;
            let (w, h) = (d(SWITCH_WIDTH), d(SWITCH_HEIGHT));
            let track = rect(b.right - w, b.top + (b.bottom - b.top - h) / 2, w, h);
            let state = RECT {
                right: track.left - d(SWITCH_GAP),
                ..r
            };
            let ink = if enabled { p.text } else { p.disabled };
            let flags = DT_RIGHT | DT_VCENTER | DT_SINGLELINE;
            c.text(
                if on { "On" } else { "Off" },
                &state,
                Font::Body,
                ink,
                flags,
            );
            let round = h as f32 / 2.0;
            let knob = if enabled && (hot || pressed) {
                7.0
            } else {
                6.0
            } * s;
            let cy = (track.top + track.bottom) as f32 / 2.0;
            if on {
                let fill = if enabled { p.accent } else { p.disabled };
                c.round(&track, round, Some(Argb::solid(fill)), None);
                let cx = track.right as f32 - round;
                let knob_color = if enabled { p.on_accent } else { p.card };
                c.circle(cx, cy, knob, Argb::solid(knob_color));
            } else {
                let ink = Argb::solid(if enabled { p.secondary } else { p.disabled });
                c.round(&track, round, Some(p.switch_off), Some(ink));
                c.circle(track.left as f32 + round, cy, knob, ink);
            }
            if focused {
                let ring = inset(&track, -d(FOCUS), -d(FOCUS));
                c.ring(
                    &ring,
                    round + FOCUS as f32 * s,
                    2.0 * s,
                    Argb::solid(p.text),
                );
            }
        }
        Look::Button => {
            c.rect(&r, Argb::solid(p.card));
            control_box(c, &b, control_fill(enabled, pressed, hot));
            let ink = match () {
                _ if !enabled => p.disabled,
                _ if pressed => p.secondary,
                _ => p.control_text,
            };
            let flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS;
            c.text(&window_text(hwnd), &b, Font::Body, ink, flags);
            if focused {
                focus_ring(c, &r, radius());
            }
        }
        Look::Card(note) => {
            c.rect(&r, Argb::solid(p.window));
            let fill = match () {
                _ if !enabled => p.card,
                _ if pressed => p.card_pressed,
                _ if hot => p.card_hover,
                _ => p.card,
            };
            c.round(
                &b,
                radius(),
                Some(Argb::solid(fill)),
                Some(Argb::solid(p.card_border)),
            );
            let dpi = DPI.get();
            let pad = d(CARD_PADDING);
            let width = button_card_text_width(b.right - b.left, dpi);
            let title = window_text(hwnd);
            let title_h = text_height(Font::Body, &title, width);
            let note_h = if note.is_empty() {
                0
            } else {
                d(NOTE_GAP) + text_height(Font::Caption, note, width)
            };
            let top = b.top + (b.bottom - b.top - title_h - note_h) / 2;
            let (ink, secondary) = if enabled {
                (p.text, p.secondary)
            } else {
                (p.disabled, p.disabled)
            };
            let t = rect(b.left + pad, top, width, title_h);
            c.text(&title, &t, Font::Body, ink, DT_LEFT | DT_WORDBREAK);
            let n = rect(t.left, t.bottom + d(NOTE_GAP), width, note_h);
            c.text(
                note,
                &n,
                Font::Caption,
                secondary,
                DT_LEFT | DT_WORDBREAK | DT_EDITCONTROL,
            );
            let chevron = RECT {
                left: b.right - pad - d(CHEVRON) / 2,
                right: b.right - pad,
                ..b
            };
            let flags = DT_RIGHT | DT_VCENTER | DT_SINGLELINE;
            c.text(CHEVRON_RIGHT, &chevron, Font::Glyph, secondary, flags);
            if focused {
                focus_ring(c, &r, radius());
            }
        }
        Look::Text(..) => {}
    });
    true
}

/// Paints the pane: the cards, the tables' lines, the edit fields' boxes.
fn paint_pane(pane: HWND, dc: HDC, dirty: &RECT) {
    let p = colors();
    let scroll = with(|s| s.scroll).unwrap_or(0);
    let focused = unsafe {
        let f = GetFocus();
        if IsChild(pane, f) != 0 {
            GetDlgCtrlID(f)
        } else {
            0
        }
    };
    buffered(dc, dirty, |c| {
        c.rect(dirty, Argb::solid(p.window));
        let visible = |r: &RECT| r.bottom > dirty.top && r.top < dirty.bottom;
        DECOR.with(|decor| {
            let decor = decor.borrow();
            for card in decor.cards.iter().map(|r| up(r, scroll)).filter(visible) {
                let border = Some(Argb::solid(p.card_border));
                c.round(&card, radius(), Some(Argb::solid(p.card)), border);
            }
            for rule in decor.rules.iter().map(|r| up(r, scroll)).filter(visible) {
                c.rect(&rule, Argb::solid(p.card_border));
            }
            for (field, id) in &decor.fields {
                let field = up(field, scroll);
                if !visible(&field) {
                    continue;
                }
                let focus = *id == focused;
                let fill = Argb::solid(if focus { p.field_focus } else { p.field });
                c.round(&field, radius(), Some(fill), Some(p.stroke));
                if focus {
                    c.bottom(&field, radius(), 2.0 * hairline(), Argb::solid(p.accent));
                } else {
                    c.bottom(&field, radius(), hairline(), p.field_bottom);
                }
            }
        });
    });
}

/// An edit field gained or lost the focus: its box shows it.
fn refresh_field(pane: HWND, edit: HWND) {
    let id = unsafe { GetDlgCtrlID(edit) };
    let scroll = with(|s| s.scroll).unwrap_or(0);
    let field = DECOR.with(|d| {
        d.borrow()
            .fields
            .iter()
            .find(|(_, i)| *i == id)
            .map(|(r, _)| up(r, scroll))
    });
    unsafe {
        InvalidateRect(edit, null(), 1);
        if let Some(field) = field {
            InvalidateRect(pane, &field, 0);
        }
    }
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
                } else if pushed(GetFocus()) && (id == IDOK || id == GetDlgCtrlID(GetFocus())) {
                    // Enter on a button clicks it, as Space does.
                    SendMessageW(GetFocus(), BM_CLICK, 0, 0);
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
            WM_MEASUREITEM => {
                let item = &mut *(lp as *mut MEASUREITEMSTRUCT);
                if item.CtlType == ODT_LISTBOX {
                    item.itemHeight = d(NAV_ITEM) as u32;
                }
                1
            }
            WM_DRAWITEM => {
                let item = &*(lp as *const DRAWITEMSTRUCT);
                if item.CtlType == ODT_LISTBOX {
                    draw_nav_item(item);
                }
                1
            }
            WM_ACTIVATE => {
                // Like a dialog: the focus goes back to the control that had it (at first, the
                // page list), not to the window.
                if wp & 0xFFFF == WA_INACTIVE as usize {
                    let focused = GetFocus();
                    if IsChild(hwnd, focused) != 0 {
                        with(|s| s.last_focus = GetDlgCtrlID(focused));
                    }
                } else if IsIconic(hwnd) == 0 {
                    let Some((list, pane, id)) = with(|s| (s.list, s.pane, s.last_focus)) else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
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
                } else {
                    // Re-placed if rows were shown or hidden (e.g. the advanced settings) or
                    // another page shows, not when only figures changed: a window the user put
                    // somewhere stays there.
                    let size = || with(|s| (s.current, s.pages.len(), s.content_height));
                    let before = size();
                    if rebuild(false) && size() != before {
                        place(hwnd, Place::Fit);
                    }
                }
                0
            }
            WM_SIZE => {
                if wp == SIZE_RESTORED as WPARAM
                    && with(|s| std::mem::take(&mut s.place_owed)).unwrap_or(false)
                {
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
                set_icons(hwnd, dpi);
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
            // Light or dark, the accent, high contrast.
            WM_SETTINGCHANGE => {
                let area = (lp != 0).then(|| {
                    let p = lp as *const u16;
                    let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
                });
                if area.as_deref() == Some("ImmersiveColorSet")
                    || wp == SPI_SETHIGHCONTRAST as WPARAM
                {
                    retheme(hwnd);
                }
                0
            }
            WM_SYSCOLORCHANGE | WM_THEMECHANGED | WM_DWMCOLORIZATIONCOLORCHANGED => {
                retheme(hwnd);
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
                // The page shown already (e.g. the error page for "Settings" while the file
                // is broken): the one the user chose is kept for when it's back.
                let index =
                    with(|s| crate::page_index(&s.pages, Some(&title)).filter(|&i| i != s.current));
                if let Some(index) = index.flatten() {
                    show(index);
                }
                if let Some(list) = with(|s| s.list) {
                    SetFocus(list);
                }
                1
            }
            WM_ERASEBKGND => {
                let mut client: RECT = zeroed();
                GetClientRect(hwnd, &mut client);
                FillRect(wp as HDC, &client, theme().window);
                1
            }
            WM_CTLCOLORLISTBOX => theme().window as LRESULT,
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
                // How it looks.
                if is_class(control, "ComboBox") {
                    if matches!(code, CBN_DROPDOWN | CBN_CLOSEUP | CBN_SELCHANGE) {
                        InvalidateRect(control, null(), 0);
                    }
                } else if is_class(control, "Edit") {
                    match code {
                        EN_SETFOCUS | EN_KILLFOCUS => refresh_field(hwnd, control),
                        EN_CHANGE
                            if GetWindowLongW(control, GWL_STYLE) as u32 & ES_MULTILINE as u32
                                != 0 =>
                        {
                            fit_scrollbar(control)
                        }
                        _ => {}
                    }
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
            WM_NOTIFY => {
                let header = &*(lp as *const NMHDR);
                if header.code == NM_CUSTOMDRAW {
                    let cd = &*(lp as *const NMCUSTOMDRAW);
                    if cd.dwDrawStage == CDDS_PREPAINT && draw_button(cd) {
                        return CDRF_SKIPDEFAULT as LRESULT;
                    }
                }
                0
            }
            WM_MEASUREITEM => {
                let item = &mut *(lp as *mut MEASUREITEMSTRUCT);
                // The selection field's height (`u32::MAX`) is set once created (see `create`).
                if item.CtlType == ODT_COMBOBOX {
                    item.itemHeight = d(DROP_ITEM) as u32;
                }
                1
            }
            WM_DRAWITEM => {
                let item = &*(lp as *const DRAWITEMSTRUCT);
                if item.CtlType == ODT_COMBOBOX {
                    // The closed pop-up draws its choice outside `WM_PAINT` too: all of it,
                    // as there.
                    if item.itemState & ODS_COMBOBOXEDIT != 0 {
                        let dc = GetDC(item.hwndItem);
                        paint_combo(item.hwndItem, dc);
                        ReleaseDC(item.hwndItem, dc);
                    } else {
                        draw_drop_item(item);
                    }
                }
                1
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                paint_pane(hwnd, dc, &ps.rcPaint);
                EndPaint(hwnd, &ps);
                0
            }
            WM_ERASEBKGND => 1,
            // A click on an edit field's box, off the edit control in it.
            WM_LBUTTONDOWN => {
                let (x, y) = (
                    (lp & 0xFFFF) as i16 as i32,
                    ((lp >> 16) & 0xFFFF) as i16 as i32,
                );
                let scroll = with(|s| s.scroll).unwrap_or(0);
                let hit = DECOR.with(|d| {
                    d.borrow()
                        .fields
                        .iter()
                        .find(|(r, _)| {
                            let r = up(r, scroll);
                            x >= r.left && x < r.right && y >= r.top && y < r.bottom
                        })
                        .map(|(_, id)| *id)
                });
                if let Some(id) = hit {
                    SetFocus(GetDlgItem(hwnd, id));
                }
                0
            }
            WM_CTLCOLOREDIT => {
                let (dc, t) = (wp as HDC, theme());
                let focused = GetFocus() == lp as HWND;
                SetTextColor(dc, t.colors.text);
                if focused {
                    SetBkColor(dc, t.colors.field_focus);
                    t.field_focus as LRESULT
                } else {
                    SetBkColor(dc, t.colors.field);
                    t.field as LRESULT
                }
            }
            WM_CTLCOLORSTATIC => {
                let (dc, t, control) = (wp as HDC, theme(), lp as HWND);
                // A disabled edit field.
                if is_class(control, "Edit") {
                    SetTextColor(dc, t.colors.disabled);
                    SetBkColor(dc, t.colors.field);
                    return t.field as LRESULT;
                }
                let look = LOOKS.with(|l| match l.try_borrow().ok()?.get(&control)? {
                    Look::Text(ink, on_card) => Some((*ink, *on_card)),
                    _ => None,
                });
                let (ink, on_card) = look.unwrap_or((Ink::Text, true));
                SetTextColor(dc, t.colors.ink(ink));
                SetBkMode(dc, TRANSPARENT as i32);
                (if on_card { t.card } else { t.window }) as LRESULT
            }
            WM_CTLCOLORBTN => theme().card as LRESULT,
            WM_CTLCOLORLISTBOX => {
                let (dc, t) = (wp as HDC, theme());
                SetTextColor(dc, t.colors.text);
                SetBkColor(dc, t.colors.flyout);
                t.flyout as LRESULT
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

/// Asks for `WM_MOUSELEAVE` once the mouse leaves `hwnd`.
fn track_leave(hwnd: HWND) {
    let mut track = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    unsafe { TrackMouseEvent(&mut track) };
}

/// The page list: the item under the mouse is highlighted.
unsafe extern "system" fn nav_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        let hover = |index: i32| {
            let old = NAV_HOT.replace(index);
            if old == index {
                return;
            }
            for i in [old, index].into_iter().filter(|&i| i >= 0) {
                let mut r: RECT = zeroed();
                if SendMessageW(
                    hwnd,
                    LB_GETITEMRECT,
                    i as WPARAM,
                    &mut r as *mut _ as LPARAM,
                ) != LB_ERR as isize
                {
                    InvalidateRect(hwnd, &r, 0);
                }
            }
        };
        match msg {
            WM_MOUSEMOVE => {
                let hit = SendMessageW(hwnd, LB_ITEMFROMPOINT, 0, lp) as u32;
                // The high word: outside the items.
                hover(if hit >> 16 != 0 {
                    -1
                } else {
                    (hit & 0xFFFF) as i32
                });
                track_leave(hwnd);
            }
            WM_MOUSELEAVE => hover(-1),
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(nav_proc), 0);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// A pop-up: painted here (see [`paint_combo`]), lit under the mouse.
unsafe extern "system" fn combo_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                paint_combo(hwnd, dc);
                EndPaint(hwnd, &ps);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            WM_MOUSEMOVE if HOT.get() != hwnd => {
                HOT.set(hwnd);
                track_leave(hwnd);
                InvalidateRect(hwnd, null(), 0);
            }
            WM_MOUSELEAVE => {
                if HOT.get() == hwnd {
                    HOT.set(null_mut());
                }
                InvalidateRect(hwnd, null(), 0);
            }
            WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE | WM_UPDATEUISTATE | CB_SETCURSEL => {
                let result = DefSubclassProc(hwnd, msg, wp, lp);
                InvalidateRect(hwnd, null(), 0);
                return result;
            }
            WM_NCDESTROY => {
                if HOT.get() == hwnd {
                    HOT.set(null_mut());
                }
                RemoveWindowSubclass(hwnd, Some(combo_proc), 0);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// An open pop-up's list: its border in the theme's color.
unsafe extern "system" fn drop_list_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_NCPAINT => {
                let result = DefSubclassProc(hwnd, msg, wp, lp);
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                let frame = rect(0, 0, r.right - r.left, r.bottom - r.top);
                let dc = GetWindowDC(hwnd);
                let brush = CreateSolidBrush(colors().flyout_border);
                for i in 0..hairline() as i32 {
                    FrameRect(dc, &inset(&frame, i, i), brush);
                }
                DeleteObject(brush as HGDIOBJ);
                ReleaseDC(hwnd, dc);
                return result;
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(drop_list_proc), 0);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
