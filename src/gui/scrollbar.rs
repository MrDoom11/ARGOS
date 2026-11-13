use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, DC_BRUSH, DC_PEN, EndPaint, GetStockObject, HDC, InvalidateRect,
    NULL_PEN, PAINTSTRUCT, PATCOPY, PatBlt, Polyline, RestoreDC, RoundRect, SaveDC, SelectObject,
    SetDCBrushColor, SetDCPenColor,
};
use windows_sys::Win32::UI::Controls::{STATE_SYSTEM_PRESSED, STATE_SYSTEM_UNAVAILABLE};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use super::appearance::{MenuColors, Theme, high_contrast};
use super::{OwnedWindow, scale};

const SUBCLASS: usize = 1;

pub(super) fn create(parent: HWND) -> HWND {
    unsafe {
        CreateWindowExW(
            0,
            w!("SCROLLBAR"),
            null(),
            WS_CHILD | SBS_VERT as u32,
            0,
            0,
            0,
            0,
            parent,
            null_mut(),
            null_mut(),
            null(),
        )
    }
}

// A janela em camadas ignora cliques inclusive nos filhos. A barra interativa
// fica numa janela própria, limitada ao retângulo da barra e vinculada ao painel.
pub(super) struct Overlay {
    window: OwnedWindow,
    pub(super) control: HWND,
}

impl Overlay {
    pub(super) fn new(owner: HWND) -> Option<Self> {
        let window = OwnedWindow(unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                null(),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                0,
                0,
                owner,
                null_mut(),
                null_mut(),
                null(),
            )
        });
        if window.0.is_null()
            || unsafe { SetWindowSubclass(window.0, Some(overlay_proc), SUBCLASS, owner as usize) }
                == 0
        {
            return None;
        }
        let control = create(window.0);
        (!control.is_null()).then_some(Self { window, control })
    }

    pub(super) fn hwnd(&self) -> HWND {
        self.window.0
    }
}

pub(super) fn sync_overlay(hwnd: HWND, owner: HWND, position: Option<[i32; 4]>) {
    unsafe {
        let visible = IsWindowVisible(hwnd) != 0;
        let Some([x, y, width, height]) = position.filter(|_| {
            IsWindowVisible(owner) != 0 && IsIconic(owner) == 0 && IsWindowEnabled(owner) != 0
        }) else {
            if visible {
                ShowWindow(hwnd, SW_HIDE);
            }
            return;
        };
        let mut origin = POINT { x, y };
        if ClientToScreen(owner, &mut origin) == 0 {
            return;
        }
        let mut current = RECT::default();
        if !visible
            || GetWindowRect(hwnd, &mut current) == 0
            || [current.left, current.top, current.right, current.bottom]
                != [origin.x, origin.y, origin.x + width, origin.y + height]
        {
            SetWindowPos(
                hwnd,
                null_mut(),
                origin.x,
                origin.y,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            );
        }
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    owner: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
            WM_VSCROLL | WM_MOUSEWHEEL => return SendMessageW(owner as HWND, msg, wp, lp),
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(overlay_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

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
