use std::cell::RefCell;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_RETURN, VK_SPACE};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::wide;

pub(super) const APPLY: u32 = WM_APP + 1;

pub(super) struct Menu(pub(super) HMENU);

impl Menu {
    pub(super) fn new() -> Option<Self> {
        let menu = unsafe { CreatePopupMenu() };
        (!menu.is_null()).then_some(Self(menu))
    }

    pub(super) fn item(&self, id: usize, label: &str, checked: bool) {
        let label = wide(label);
        unsafe {
            AppendMenuW(
                self.0,
                MF_STRING | if checked { MF_CHECKED } else { 0 },
                id,
                label.as_ptr(),
            )
        };
    }

    pub(super) fn label(&self, label: &str) {
        let label = wide(label);
        unsafe { AppendMenuW(self.0, MF_STRING | MF_GRAYED, 0, label.as_ptr()) };
    }

    pub(super) fn separator(&self) {
        unsafe { AppendMenuW(self.0, MF_SEPARATOR, 0, null()) };
    }

    pub(super) fn group(&self, label: &str, items: impl FnOnce(&Self)) {
        let Some(submenu) = Self::new() else { return };
        items(&submenu);
        let label = wide(label);
        // Uma inclusão bem-sucedida transfere o submenu para o menu pai.
        if unsafe { AppendMenuW(self.0, MF_POPUP, submenu.0 as usize, label.as_ptr()) } != 0 {
            std::mem::forget(submenu);
        }
    }

    pub(super) fn track(&self, owner: HWND, point: POINT, keep_open: fn(usize) -> bool) -> usize {
        let hook = unsafe {
            SetWindowsHookExW(WH_MSGFILTER, Some(filter), null_mut(), GetCurrentThreadId())
        };
        if !hook.is_null() {
            TRACKING.with_borrow_mut(|slot| {
                *slot = Some(Tracking {
                    owner,
                    menus: vec![self.0],
                    keep_open,
                    selected: None,
                    pressed: None,
                });
            });
        }
        let choice = unsafe {
            TrackPopupMenu(
                self.0,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                owner,
                null(),
            )
        } as usize;
        if !hook.is_null() {
            unsafe { UnhookWindowsHookEx(hook) };
            TRACKING.with_borrow_mut(|slot| *slot = None);
        }
        choice
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        unsafe { DestroyMenu(self.0) };
    }
}

struct Tracking {
    owner: HWND,
    menus: Vec<HMENU>,
    keep_open: fn(usize) -> bool,
    selected: Option<usize>,
    pressed: Option<(u32, usize)>,
}

thread_local! {
    static TRACKING: RefCell<Option<Tracking>> = const { RefCell::new(None) };
}

pub(super) fn notify(owner: HWND, msg: u32, wp: WPARAM, lp: LPARAM) {
    TRACKING.with_borrow_mut(|slot| {
        let Some(tracking) = slot.as_mut().filter(|tracking| tracking.owner == owner) else {
            return;
        };
        let menu = if msg == WM_INITMENUPOPUP || msg == WM_UNINITMENUPOPUP {
            wp as HMENU
        } else {
            lp as HMENU
        };
        match msg {
            WM_INITMENUPOPUP if !tracking.menus.contains(&menu) => tracking.menus.push(menu),
            WM_UNINITMENUPOPUP => tracking.menus.retain(|&open| open != menu),
            WM_MENUSELECT => {
                let flags = (wp >> 16) as u32;
                tracking.selected = (tracking.menus.contains(&menu)
                    && flags & (MF_POPUP | MF_SEPARATOR | MF_DISABLED | MF_GRAYED) == 0)
                    .then_some(wp as u16 as usize);
            }
            _ => {}
        }
    });
}

unsafe extern "system" fn filter(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == MSGF_MENU as i32
        && let Some(msg) = unsafe { (lp as *const MSG).as_ref() }
        && let Some((owner, choice)) = TRACKING.with_borrow_mut(|slot| {
            let tracking = slot.as_mut()?;
            Some((tracking.owner, tracking.input(msg)?))
        })
    {
        if let Some(choice) = choice {
            unsafe { SendMessageW(owner, APPLY, choice, 0) };
        }
        return 1;
    }
    unsafe { CallNextHookEx(null_mut(), code, wp, lp) }
}

impl Tracking {
    // Some(None) consome a mensagem sem executar uma opção.
    fn input(&mut self, msg: &MSG) -> Option<Option<usize>> {
        match msg.message {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                let choice = self.command_at(msg.pt).filter(|&id| (self.keep_open)(id))?;
                let release = if matches!(msg.message, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
                    WM_LBUTTONUP
                } else {
                    WM_RBUTTONUP
                };
                self.pressed = Some((release, choice));
                Some(None)
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                let (_, choice) = self
                    .pressed
                    .filter(|&(release, _)| release == msg.message)?;
                self.pressed = None;
                Some((self.command_at(msg.pt) == Some(choice)).then_some(choice))
            }
            WM_KEYDOWN if matches!(msg.wParam as u16, VK_RETURN | VK_SPACE) => {
                let choice = self.selected.filter(|&id| (self.keep_open)(id))?;
                Some((msg.lParam & (1 << 30) == 0).then_some(choice))
            }
            _ => None,
        }
    }

    fn command_at(&self, point: POINT) -> Option<usize> {
        let menu = popup_menu(unsafe { WindowFromPoint(point) })?;
        if !self.menus.contains(&menu) {
            return None;
        }
        let index = unsafe { MenuItemFromPoint(null_mut(), menu, point) };
        let info = item_info(menu, u32::try_from(index).ok()?, true)?;
        (info.hSubMenu.is_null()
            && info.fType & MFT_SEPARATOR == 0
            && info.fState & MFS_DISABLED == 0)
            .then_some(info.wID as usize)
    }
}

fn item_info(menu: HMENU, item: u32, by_position: bool) -> Option<MENUITEMINFOW> {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_ID | MIIM_STATE | MIIM_FTYPE | MIIM_SUBMENU,
        ..Default::default()
    };
    (unsafe { GetMenuItemInfoW(menu, item, i32::from(by_position), &mut info) } != 0)
        .then_some(info)
}

pub(super) fn sync(root: HMENU, updated: &Menu) {
    update_items(root, updated.0);
    unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(redraw_popup), 0) };
}

fn update_items(menu: HMENU, updated: HMENU) {
    for index in 0..unsafe { GetMenuItemCount(menu) } {
        let index = index as u32;
        let Some(mut item) = item_info(menu, index, true) else {
            continue;
        };
        if !item.hSubMenu.is_null() {
            update_items(item.hSubMenu, updated);
        } else if item.wID != 0 && item.fType & MFT_SEPARATOR == 0 {
            let mask = MFS_CHECKED | MFS_DISABLED;
            let desired = item_info(updated, item.wID, false);
            let state = desired.map_or(MFS_DISABLED, |info| info.fState & mask);
            let mut label = desired.and_then(|info| item_label(updated, info.wID, MF_BYCOMMAND));
            item.fMask = 0;
            if item.fState & mask != state {
                item.fMask |= MIIM_STATE;
                item.fState = (item.fState & !mask) | state;
            }
            if let Some(label) = label.as_mut()
                && item_label(menu, index, MF_BYPOSITION).as_ref() != Some(label)
            {
                item.fMask |= MIIM_STRING;
                item.dwTypeData = label.as_mut_ptr();
            }
            if item.fMask != 0 {
                unsafe { SetMenuItemInfoW(menu, index, 1, &item) };
            }
        }
    }
}

fn item_label(menu: HMENU, item: u32, flags: MENU_ITEM_FLAGS) -> Option<Vec<u16>> {
    let length = unsafe { GetMenuStringW(menu, item, null_mut(), 0, flags) };
    if length <= 0 {
        return None;
    }
    let mut label = vec![0; length as usize + 1];
    (unsafe { GetMenuStringW(menu, item, label.as_mut_ptr(), length + 1, flags) } == length)
        .then_some(label)
}

fn popup_menu(hwnd: HWND) -> Option<HMENU> {
    let mut info = MENUBARINFO {
        cbSize: size_of::<MENUBARINFO>() as u32,
        ..Default::default()
    };
    (unsafe { GetMenuBarInfo(hwnd, OBJID_CLIENT, 0, &mut info) } != 0).then_some(info.hMenu)
}

unsafe extern "system" fn redraw_popup(hwnd: HWND, _: LPARAM) -> i32 {
    if unsafe { IsWindowVisible(hwnd) != 0 }
        && let Some(menu) = popup_menu(hwnd)
        && TRACKING.with_borrow(|slot| {
            slot.as_ref()
                .is_some_and(|tracking| tracking.menus.contains(&menu))
        })
    {
        unsafe { RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_UPDATENOW) };
    }
    1
}
