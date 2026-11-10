use std::mem::size_of;
use std::ptr::null;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, DC_BRUSH, DC_PEN, EndPaint, GetStockObject, HDC, InvalidateRect, NULL_PEN,
    PAINTSTRUCT, PATCOPY, PatBlt, Polyline, RestoreDC, RoundRect, SaveDC, SelectObject,
    SetDCBrushColor, SetDCPenColor,
};
use windows_sys::Win32::UI::Controls::{STATE_SYSTEM_PRESSED, STATE_SYSTEM_UNAVAILABLE};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetScrollBarInfo, OBJID_CLIENT, SCROLLBARINFO, SM_CYVSCROLL, WM_CAPTURECHANGED,
    WM_ENABLE, WM_ERASEBKGND, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_NCDESTROY, WM_PAINT, WM_PRINTCLIENT, WM_SETFOCUS,
};

use super::appearance::{MenuColors, Theme, high_contrast};
use super::scale;

const SUBCLASS: usize = 1;

pub(super) fn set_theme(hwnd: HWND, theme: Theme) {
    if !hwnd.is_null() {
        unsafe {
            SetWindowSubclass(
                hwnd,
                Some(scrollbar_proc),
                SUBCLASS,
                usize::from(theme == Theme::Light),
            );
        }
    }
}

unsafe extern "system" fn scrollbar_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    theme: usize,
) -> LRESULT {
    unsafe {
        if msg == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(scrollbar_proc), id);
        } else if matches!(msg, WM_PAINT | WM_PRINTCLIENT | WM_ERASEBKGND)
            && !high_contrast()
            && let Some(geometry) = Geometry::read(hwnd)
        {
            let theme = if theme == 0 {
                Theme::Dark
            } else {
                Theme::Light
            };
            let colors = theme.menu_colors();
            let focused = GetFocus() == hwnd;
            match msg {
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut paint);
                    geometry.paint(dc, colors, focused);
                    EndPaint(hwnd, &paint);
                    return 0;
                }
                WM_PRINTCLIENT => {
                    if geometry.paint(wp as HDC, colors, focused) {
                        return 0;
                    }
                }
                WM_ERASEBKGND => return 1,
                _ => {}
            }
        }

        let result = DefSubclassProc(hwnd, msg, wp, lp);
        if matches!(
            msg,
            WM_SETFOCUS
                | WM_KILLFOCUS
                | WM_ENABLE
                | WM_LBUTTONDOWN
                | WM_LBUTTONDBLCLK
                | WM_LBUTTONUP
                | WM_CAPTURECHANGED
        ) {
            InvalidateRect(hwnd, null(), 0);
        }
        result
    }
}

struct Geometry {
    bounds: RECT,
    dpi: u32,
    info: SCROLLBARINFO,
}

impl Geometry {
    fn read(hwnd: HWND) -> Option<Self> {
        let mut bounds = RECT::default();
        let mut info = SCROLLBARINFO {
            cbSize: size_of::<SCROLLBARINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetClientRect(hwnd, &mut bounds) == 0 }
            || bounds.right <= bounds.left
            || bounds.bottom <= bounds.top
            || unsafe { GetScrollBarInfo(hwnd, OBJID_CLIENT, &mut info) == 0 }
        {
            return None;
        }
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        Some(Self { bounds, dpi, info })
    }

    fn paint(&self, dc: HDC, colors: MenuColors, focused: bool) -> bool {
        if dc.is_null() {
            return false;
        }
        let saved = unsafe { SaveDC(dc) };
        if saved == 0 {
            return false;
        }

        let bounds = self.bounds;
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        let arrow_height =
            unsafe { GetSystemMetricsForDpi(SM_CYVSCROLL, self.dpi) }.clamp(0, height / 2);
        let dip = |value| scale(value, self.dpi);
        let inset = dip(4).min((width - 1) / 2);
        let thumb_top = bounds.top + self.info.xyThumbTop.clamp(0, height);
        let thumb_bottom = bounds.top + self.info.xyThumbBottom.clamp(0, height);
        let thumb_color = if focused || self.info.rgstate[3] & STATE_SYSTEM_PRESSED != 0 {
            colors.accent
        } else {
            colors.disabled
        };

        unsafe {
            SelectObject(dc, GetStockObject(DC_BRUSH));
            SetDCBrushColor(dc, colors.background);
            PatBlt(dc, bounds.left, bounds.top, width, height, PATCOPY);
            if thumb_bottom > thumb_top {
                SelectObject(dc, GetStockObject(NULL_PEN));
                SetDCBrushColor(dc, thumb_color);
                RoundRect(
                    dc,
                    bounds.left + inset,
                    thumb_top,
                    bounds.right - inset,
                    thumb_bottom,
                    dip(6),
                    dip(6),
                );
            }
        }

        if arrow_height >= dip(6) && width >= dip(7) {
            let center = bounds.left + width / 2;
            let half_width = dip(3);
            let rise = dip(2);
            unsafe { SelectObject(dc, GetStockObject(DC_PEN)) };
            for (y, direction, state) in [
                (bounds.top + arrow_height / 2, -1, self.info.rgstate[1]),
                (
                    bounds.bottom - (arrow_height + 1) / 2,
                    1,
                    self.info.rgstate[5],
                ),
            ] {
                let color = if state & STATE_SYSTEM_UNAVAILABLE != 0 {
                    colors.disabled
                } else if state & STATE_SYSTEM_PRESSED != 0 {
                    colors.accent
                } else {
                    colors.icon
                };
                let points = [
                    POINT {
                        x: center - half_width,
                        y: y - direction * rise,
                    },
                    POINT {
                        x: center,
                        y: y + direction * rise,
                    },
                    POINT {
                        x: center + half_width,
                        y: y - direction * rise,
                    },
                ];
                unsafe {
                    SetDCPenColor(dc, color);
                    Polyline(dc, points.as_ptr(), points.len() as i32);
                }
            }
        }
        unsafe { RestoreDC(dc, saved) };
        true
    }
}
