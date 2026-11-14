use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETFOCUS,
    NIM_SETVERSION, NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, GetCursorPos, HICON, MSGFLT_ALLOW, RegisterWindowMessageW, WM_APP,
    WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP,
};
use windows_sys::w;

pub(super) const CALLBACK: u32 = WM_APP + 6;
const ICON_ID: u32 = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

#[derive(Clone, Copy)]
enum Protocol {
    Modern,
    Legacy,
}

pub(super) enum Event {
    Show,
    Menu(POINT),
}

pub(super) struct Tray {
    data: NOTIFYICONDATAW,
    taskbar_created: u32,
    protocol: Option<Protocol>,
}

impl Tray {
    pub(super) fn new(hwnd: HWND, icon: HICON) -> Self {
        let mut tooltip = [0; 128];
        for (slot, unit) in tooltip.iter_mut().zip("ARGOS".encode_utf16()) {
            *slot = unit;
        }
        Self {
            data: NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: ICON_ID,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
                uCallbackMessage: CALLBACK,
                hIcon: icon,
                szTip: tooltip,
                ..Default::default()
            },
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            protocol: None,
        }
    }

    pub(super) fn register(&mut self) -> bool {
        if self.data.hIcon.is_null() {
            return false;
        }
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &self.data) } != 0;
        // O Explorer também pode anunciar TaskbarCreated sem descartar o ícone existente.
        if !added && unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.data) } == 0 {
            self.protocol = None;
            return false;
        }
        self.data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        self.protocol = Some(
            if unsafe { Shell_NotifyIconW(NIM_SETVERSION, &self.data) } != 0 {
                Protocol::Modern
            } else if added {
                Protocol::Legacy
            } else {
                self.protocol.unwrap_or(Protocol::Legacy)
            },
        );
        true
    }

    pub(super) fn allow_elevated_notifications(&self) -> bool {
        // In an elevated panel these messages can only reveal the window or
        // re-register its notification icon.
        unsafe {
            if self.taskbar_created != 0 {
                ChangeWindowMessageFilterEx(
                    self.data.hWnd,
                    self.taskbar_created,
                    MSGFLT_ALLOW,
                    null_mut(),
                );
            }
            ChangeWindowMessageFilterEx(self.data.hWnd, CALLBACK, MSGFLT_ALLOW, null_mut()) != 0
        }
    }

    pub(super) fn available(&self) -> bool {
        self.protocol.is_some()
    }

    pub(super) fn is_taskbar_created(&self, message: u32) -> bool {
        self.taskbar_created != 0 && message == self.taskbar_created
    }

    pub(super) fn event(&self, wp: WPARAM, lp: LPARAM) -> Option<Event> {
        let protocol = self.protocol?;
        match (protocol, notification(protocol, wp, lp)?) {
            (Protocol::Modern, NIN_SELECT | NIN_KEYSELECT) | (_, WM_LBUTTONUP) => Some(Event::Show),
            (Protocol::Modern, WM_CONTEXTMENU) | (Protocol::Legacy, WM_RBUTTONUP) => {
                Some(Event::Menu(self.menu_point()))
            }
            _ => None,
        }
    }

    pub(super) fn return_focus(&self) {
        if self.available() {
            unsafe { Shell_NotifyIconW(NIM_SETFOCUS, &self.data) };
        }
    }

    fn menu_point(&self) -> POINT {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.data.hWnd,
            uID: ICON_ID,
            ..Default::default()
        };
        let mut rect = RECT::default();
        if unsafe { Shell_NotifyIconGetRect(&identifier, &mut rect) } >= 0 {
            return POINT {
                x: rect.left + (rect.right - rect.left) / 2,
                y: rect.top,
            };
        }
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor) };
        cursor
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if self.available() {
            unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data) };
        }
    }
}

fn notification(protocol: Protocol, wp: WPARAM, lp: LPARAM) -> Option<u32> {
    match protocol {
        Protocol::Modern if (lp as u32 >> 16) == ICON_ID => Some(u32::from(lp as u16)),
        Protocol::Legacy if wp == ICON_ID as WPARAM => Some(lp as u32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_callbacks_match_icon_id_in_both_protocols() {
        let modern = |id: u32, event: u32| ((id << 16) | event) as LPARAM;
        assert_eq!(
            notification(Protocol::Modern, 0, modern(ICON_ID, NIN_KEYSELECT)),
            Some(NIN_KEYSELECT),
        );
        assert_eq!(
            notification(Protocol::Modern, 0, modern(ICON_ID + 1, WM_CONTEXTMENU)),
            None,
        );
        assert_eq!(
            notification(Protocol::Legacy, ICON_ID as WPARAM, WM_LBUTTONUP as LPARAM),
            Some(WM_LBUTTONUP),
        );
        assert_eq!(
            notification(
                Protocol::Legacy,
                (ICON_ID + 1) as WPARAM,
                WM_RBUTTONUP as LPARAM
            ),
            None,
        );
    }
}
