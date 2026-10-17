//! Windows: a layered popup with per-pixel alpha (`UpdateLayeredWindow`).
//!
//! Extended styles: `WS_EX_LAYERED | WS_EX_TRANSPARENT` (click-through: the mouse goes to
//! whatever is below), `WS_EX_NOACTIVATE` (clicking or showing it never activates it),
//! `WS_EX_TOOLWINDOW` (not in the taskbar or Alt+Tab), `WS_EX_TOPMOST`. It is shown with
//! `SWP_NOACTIVATE`, so the foreground window and keyboard focus never change.
//!
//! The window is owned by the thread that creates it (the UI thread), whose message loop
//! (winit's) dispatches its few messages to `DefWindowProcW`.

use crate::{Error, NoticeSurface, Surface, canvas::Canvas};
use std::{mem::zeroed, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, POINT, RECT, SIZE},
    Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush,
        GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
        MONITORINFO, MonitorFromPoint, MonitorFromWindow, SelectObject,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow,
            HWND_TOPMOST, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SWP_SHOWWINDOW, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WNDCLASSW,
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
            WS_POPUP,
        },
    },
};

pub struct Window {
    hwnd: HWND,
    /// Memory DC with `bitmap` selected; `UpdateLayeredWindow` reads from it.
    dc: HDC,
    bitmap: HBITMAP,
    /// The DC's original bitmap, put back before deleting ours.
    old_bitmap: HGDIOBJ,
    /// `bitmap`'s pixels (top-down, premultiplied BGRA) and size.
    bits: *mut u32,
    bitmap_size: (usize, usize),
    /// Work area (screen minus taskbar) of the monitor chosen by `place`, physical pixels.
    work: RECT,
    margin: i32,
    top: bool,
    visible: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Window {
    pub fn new() -> Result<Self, Error> {
        unsafe {
            let instance = GetModuleHandleW(null_mut());
            let class = wide("AGT.Overlay");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..zeroed()
            };
            // Fails harmlessly if already registered (a second overlay in the same process).
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                class.as_ptr(),
                wide("AGT").as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            );
            if hwnd.is_null() {
                return Err(format!("CreateWindowExW failed (error {})", GetLastError()).into());
            }
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                DestroyWindow(hwnd);
                return Err("CreateCompatibleDC failed".into());
            }
            Ok(Self {
                hwnd,
                dc,
                bitmap: null_mut(),
                old_bitmap: null_mut(),
                bits: null_mut(),
                bitmap_size: (0, 0),
                work: zeroed(),
                margin: 0,
                top: false,
                visible: false,
            })
        }
    }

    /// (Re)creates the DIB section if the canvas size changed.
    fn ensure_bitmap(&mut self, w: usize, h: usize) -> bool {
        if self.bitmap_size == (w, h) && !self.bitmap.is_null() {
            return true;
        }
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..zeroed()
                },
                ..zeroed()
            };
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                tracing::warn!(
                    "overlay: CreateDIBSection failed (error {})",
                    GetLastError()
                );
                return false;
            }
            let prev = SelectObject(self.dc, bitmap);
            if self.bitmap.is_null() {
                self.old_bitmap = prev;
            } else {
                DeleteObject(self.bitmap);
            }
            self.bitmap = bitmap;
            self.bits = bits.cast();
            self.bitmap_size = (w, h);
        }
        true
    }
}

impl Surface for Window {
    /// The monitor of the foreground window (where the text will be pasted), or the one under
    /// the cursor if there is none.
    fn place(&mut self, _width: f32, _height: f32, margin: f32, top: bool) -> f32 {
        unsafe {
            let fg = GetForegroundWindow();
            let monitor = if fg.is_null() {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY)
            } else {
                MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
            };
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..zeroed()
            };
            GetMonitorInfoW(monitor, &mut info);
            let (mut dpi_x, mut dpi_y) = (96, 96);
            // Physical pixels: winit makes the process per-monitor DPI aware.
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) != 0 {
                dpi_x = 96;
            }
            let scale = dpi_x as f32 / 96.0;
            self.work = info.rcWork;
            self.margin = (margin * scale).round() as i32;
            self.top = top;
            scale
        }
    }

    fn present(&mut self, canvas: &Canvas) {
        let (w, h) = (canvas.width(), canvas.height());
        if !self.ensure_bitmap(w, h) {
            return;
        }
        unsafe {
            // GDI may still be using the bitmap; finish before writing to it.
            GdiFlush();
            std::ptr::copy_nonoverlapping(canvas.pixels().as_ptr(), self.bits, w * h);
            let (w, h) = (w as i32, h as i32);
            let work = self.work;
            let pos = POINT {
                x: work.left + (work.right - work.left - w) / 2,
                y: if self.top {
                    work.top + self.margin
                } else {
                    work.bottom - self.margin - h
                },
            };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if UpdateLayeredWindow(
                self.hwnd,
                null_mut(),
                &pos,
                &size,
                self.dc,
                &src,
                0,
                &blend,
                ULW_ALPHA,
            ) == 0
            {
                tracing::warn!(
                    "overlay: UpdateLayeredWindow failed (error {})",
                    GetLastError()
                );
                return;
            }
            if !self.visible {
                // Shows it on top of other topmost windows without activating it.
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                self.visible = true;
            }
        }
    }

    fn hide(&mut self) {
        if self.visible {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.visible = false;
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            if !self.bitmap.is_null() {
                SelectObject(self.dc, self.old_bitmap);
                DeleteObject(self.bitmap);
            }
            DeleteDC(self.dc);
            DestroyWindow(self.hwnd);
        }
    }
}

/// The "not in a text box" notice isn't built on Windows yet (nothing asks for it: the focus
/// check there always answers "can't tell").
pub struct Notice;

impl Notice {
    pub fn new(_on_copy: Box<dyn Fn()>) -> Result<Self, Error> {
        Err("not on Windows yet".into())
    }
}

impl NoticeSurface for Notice {
    fn show(&mut self, _: f32, _: bool) -> bool {
        false
    }
    fn copied(&mut self) {}
    fn hide(&mut self) {}
}
