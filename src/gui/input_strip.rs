use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, ClientToScreen, EndPaint, FillRect, GetStockObject, PAINTSTRUCT,
    ScreenToClient,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use super::{OwnedWindow, height, message_point, packed_point, width};

pub(super) const HEIGHT: i32 = 10;
const SUBCLASS: usize = 1;

pub(super) struct Strip {
    window: OwnedWindow,
}

impl Strip {
    pub(super) fn new(owner: HWND) -> Option<Self> {
        let window = OwnedWindow(unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                null(),
                WS_POPUP,
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
            || unsafe {
                SetWindowSubclass(window.0, Some(strip_proc), SUBCLASS, owner as usize) == 0
                    || SetLayeredWindowAttributes(window.0, 0, 1, LWA_ALPHA) == 0
            }
        {
            return None;
        }
        Some(Self { window })
    }

    pub(super) fn hwnd(&self) -> HWND {
        self.window.0
    }
}

pub(super) fn sync(hwnd: HWND, owner: HWND, enabled: bool) {
    if hwnd.is_null() {
        return;
    }
    let mut bounds = RECT::default();
    let visible = unsafe { IsWindowVisible(hwnd) != 0 };
    if !enabled
        || unsafe {
            IsWindowVisible(owner) == 0
                || IsIconic(owner) != 0
                || IsWindowEnabled(owner) == 0
                || GetWindowRect(owner, &mut bounds) == 0
        }
        || width(bounds) <= 0
        || height(bounds) <= 0
    {
        if visible {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
        return;
    }

    let strip_height = height(bounds).min(HEIGHT);
    let mut current = RECT::default();
    let positioned = unsafe { GetWindowRect(hwnd, &mut current) != 0 }
        && (current.left, current.top, current.right, height(current))
            == (bounds.left, bounds.top, bounds.right, strip_height);
    if visible && positioned {
        return;
    }
    let mut flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER;
    if positioned {
        flags |= SWP_NOMOVE | SWP_NOSIZE;
    }
    if !visible {
        flags |= SWP_SHOWWINDOW;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            bounds.left,
            bounds.top,
            width(bounds),
            strip_height,
            flags,
        )
    };
}

unsafe extern "system" fn strip_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    owner: usize,
) -> LRESULT {
    let owner = owner as HWND;
    unsafe {
        match msg {
            WM_NCHITTEST => HTCLIENT as LRESULT,
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_ERASEBKGND => 1,
            WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU
            | WM_MOUSEWHEEL
                if IsWindowEnabled(owner) == 0 =>
            {
                0
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                if !dc.is_null() {
                    FillRect(dc, &paint.rcPaint, GetStockObject(BLACK_BRUSH));
                }
                EndPaint(hwnd, &paint);
                0
            }
            WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_LBUTTONUP => {
                let mut point = message_point(lp);
                if ClientToScreen(hwnd, &mut point) != 0 && ScreenToClient(owner, &mut point) != 0 {
                    SendMessageW(owner, msg, wp, packed_point(point))
                } else {
                    0
                }
            }
            WM_RBUTTONUP => {
                let mut point = message_point(lp);
                if ClientToScreen(hwnd, &mut point) != 0 {
                    SendMessageW(owner, WM_CONTEXTMENU, owner as WPARAM, packed_point(point))
                } else {
                    0
                }
            }
            WM_CONTEXTMENU => SendMessageW(owner, msg, owner as WPARAM, lp),
            WM_MOUSEWHEEL => SendMessageW(owner, msg, wp, lp),
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(strip_proc), id);
                DefSubclassProc(hwnd, msg, wp, lp)
            }
            _ => DefSubclassProc(hwnd, msg, wp, lp),
        }
    }
}
