use std::cell::RefCell;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    HBRUSH, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{MSAA_MENU_SIG, MSAAMENUINFO};
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODT_MENU};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_END, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::{
    appearance::{Theme, high_contrast},
    menu_style::{RowKind, Style},
    wide,
};

pub(super) const APPLY: u32 = WM_APP + 1;
pub(super) const DISMISS: u32 = WM_APP + 4;
pub(super) const FRAME: u32 = WM_APP + 5;

// Valores de ícones cabem em u16.
const TEXT_BLOCK: usize = u16::MAX as usize + 1;
const CENTERED_ITEM: usize = TEXT_BLOCK + 1;
const SLIDER_FIRST: usize = CENTERED_ITEM + 1;
const SWATCH_FIRST: usize = 0x0100_0000;
const TEXT_BLOCK_SEPARATOR: &str = " · ";

#[repr(u16)]
#[derive(Clone, Copy)]
pub(super) enum Icon {
    Sampling = 0xe917,
    History = 0xe81c,
    Cpu = 0xeea1,
    Memory = 0xeea0,
    Gpu = 0xe770,
    Sensors = 0xe9d2,
    Fps = 0xe916,
    Application = 0xe8f9,
    Network = 0xe774,
    Adapter = 0xeda3,
    Ping = 0xe9d9,
    Storage = 0xeda2,
    Grid = 0xe8a9,
    Appearance = 0xe771,
    Opacity = 0xef1f,
    Dropdown = 0xe70d,
    Monitor = 0xe7f9,
    Width = 0xe7ea,
    Page = 0xe8a5,
    About = 0xea1f,
    Update = 0xe72c,
    Error = 0xe783,
    Move = 0xe7c2,
    Layers = 0xe81e,
    Split = 0xe89a,
    Color = 0xe790,
    Undo = 0xe7a7,
    Pin = 0xe718,
    Pinned = 0xe842,
    Power = 0xe7e8,
    Minimize = 0xe921,
    Maximize = 0xe922,
    Restore = 0xe923,
    Close = 0xe8bb,
}

pub(super) struct Menu(pub(super) HMENU);

impl Menu {
    pub(super) fn new() -> Option<Self> {
        let menu = unsafe { CreatePopupMenu() };
        (!menu.is_null()).then_some(Self(menu))
    }

    pub(super) fn item(&self, id: usize, label: &str, checked: bool) {
        self.append(
            MF_STRING | if checked { MF_CHECKED } else { 0 },
            id,
            label,
            0,
        );
    }

    pub(super) fn item_icon(&self, id: usize, label: &str, icon: Icon) {
        self.item_icon_checked(id, label, icon, false);
    }

    pub(super) fn item_icon_checked(&self, id: usize, label: &str, icon: Icon, checked: bool) {
        self.append(
            MF_STRING | if checked { MF_CHECKED } else { 0 },
            id,
            label,
            icon as usize,
        );
    }

    pub(super) fn item_swatch(&self, id: usize, label: &str, color: COLORREF) {
        self.append(MF_STRING, id, label, SWATCH_FIRST + color as usize);
    }

    pub(super) fn label(&self, label: &str) {
        self.append(MF_STRING | MF_GRAYED, 0, label, 0);
    }

    pub(super) fn label_icon(&self, label: &str, icon: Icon) {
        self.append(MF_STRING | MF_GRAYED, 0, label, icon as usize);
    }

    pub(super) fn text_block(&self, lines: &[&str]) {
        // O menu nativo de alto contraste mantém uma legenda de uma linha.
        let label = lines.join(TEXT_BLOCK_SEPARATOR);
        self.append(MF_STRING | MF_GRAYED, 0, &label, TEXT_BLOCK);
    }

    pub(super) fn centered_item(&self, id: usize, label: &str) {
        self.append(MF_STRING, id, label, CENTERED_ITEM);
    }

    pub(super) fn slider(&self, id: usize, value: u8) {
        // Reserve os 101 IDs seguintes para as escolhas de 0% a 100%.
        self.append(
            MF_STRING,
            id,
            &format!("{value}%"),
            SLIDER_FIRST + usize::from(value),
        );
    }

    pub(super) fn separator(&self) {
        unsafe { AppendMenuW(self.0, MF_SEPARATOR, 0, null()) };
    }

    pub(super) fn group_icon(&self, label: &str, icon: Icon, items: impl FnOnce(&Self)) {
        let Some(submenu) = Self::new() else { return };
        items(&submenu);
        // Uma inclusão bem-sucedida transfere o submenu para o menu pai.
        if self.append(MF_POPUP, submenu.0 as usize, label, icon as usize) {
            std::mem::forget(submenu);
        }
    }

    fn append(&self, flags: u32, id: usize, label: &str, data: usize) -> bool {
        let label = wide(label);
        if unsafe { AppendMenuW(self.0, flags, id, label.as_ptr()) } == 0 {
            return false;
        }
        if data != 0 {
            let position = unsafe { GetMenuItemCount(self.0) } - 1;
            if let Ok(position) = u32::try_from(position) {
                let info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_DATA,
                    dwItemData: data,
                    ..Default::default()
                };
                unsafe { SetMenuItemInfoW(self.0, position, 1, &info) };
            }
        }
        true
    }

    pub(super) fn track(
        self,
        owner: HWND,
        point: POINT,
        keep_open: fn(usize) -> bool,
        theme: Theme,
        dpi: u32,
    ) -> usize {
        let menu_hook = unsafe {
            SetWindowsHookExW(WH_MSGFILTER, Some(filter), null_mut(), GetCurrentThreadId())
        };
        let skin = (!menu_hook.is_null())
            .then(|| MenuSkin::new(self.0, theme, dpi))
            .flatten();
        if skin.is_none() {
            use_native_sliders(self.0);
        }
        TRACKING.with_borrow_mut(|slot| {
            *slot = Some(Tracking {
                owner,
                root: self.0,
                menus: vec![self.0],
                keep_open,
                selected: None,
                pressed: None,
                slider_drag: None,
                owner_buttons: 0,
                dismiss_pending: false,
                skin,
            });
        });
        // Menus abertos pela faixa não ativam o ARGOS. Nesse caso, o Windows pode
        // entregar o clique externo ao jogo sem encerrar o menu.
        let mouse_hook = unsafe {
            if GetForegroundWindow() != owner {
                SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_filter), GetModuleHandleW(null()), 0)
            } else {
                null_mut()
            }
        };
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
        for hook in [mouse_hook, menu_hook] {
            if !hook.is_null() {
                unsafe { UnhookWindowsHookEx(hook) };
            }
        }
        // O Windows mantém os ponteiros dos rótulos e o pincel até destruirmos o menu.
        drop(self);
        TRACKING.with_borrow_mut(|slot| *slot = None);
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
    root: HMENU,
    menus: Vec<HMENU>,
    keep_open: fn(usize) -> bool,
    selected: Option<usize>,
    pressed: Option<(u32, usize)>,
    slider_drag: Option<SliderDrag>,
    owner_buttons: u8,
    dismiss_pending: bool,
    skin: Option<MenuSkin>,
}

#[derive(Clone, Copy)]
struct SliderDrag {
    row: usize,
    popup: HWND,
    bounds: RECT,
    release: u32,
    initial: u8,
}

enum MenuInput {
    Consume,
    Apply(usize),
    Redraw(HWND),
}

struct MenuSkin {
    theme: Theme,
    style: Style,
    rows: Box<[StyledRow]>,
}

#[repr(C)]
struct StyledRow {
    // MSAA lê este cabeçalho a partir de dwItemData.
    accessible: MSAAMENUINFO,
    label: Vec<u16>,
    menu: HMENU,
    position: u32,
    item_type: u32,
    id: usize,
    state: u32,
    submenu: bool,
    original_data: usize,
}

impl StyledRow {
    fn set_label(&mut self, label: Vec<u16>) {
        self.label = label;
        self.accessible = MSAAMENUINFO {
            dwMSAASignature: MSAA_MENU_SIG as u32,
            cchWText: self.label.len().saturating_sub(1) as u32,
            pszWText: if self.label.is_empty() {
                null_mut()
            } else {
                self.label.as_mut_ptr()
            },
        };
    }

    fn separator(&self) -> bool {
        self.item_type & MFT_SEPARATOR != 0
    }

    fn kind(&self) -> RowKind {
        if self.separator() {
            return RowKind::Separator;
        }
        match self.original_data {
            TEXT_BLOCK => RowKind::TextBlock,
            CENTERED_ITEM => RowKind::CenteredItem,
            data if (SWATCH_FIRST..2 * SWATCH_FIRST).contains(&data) => {
                RowKind::Swatch((data - SWATCH_FIRST) as COLORREF)
            }
            data => slider_value(data).map_or(RowKind::Item, RowKind::Slider),
        }
    }

    fn set_slider(&mut self, value: u8) {
        self.original_data = SLIDER_FIRST + usize::from(value);
        self.set_label(wide(&format!("{value}%")));
    }

    fn configure(&self, styled: bool) -> bool {
        let info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_DATA,
            fType: if styled {
                (self.item_type & !MFT_SEPARATOR) | MFT_OWNERDRAW
            } else {
                self.item_type
            },
            fState: self.state
                | if styled && self.separator() {
                    MFS_DISABLED
                } else {
                    0
                },
            dwItemData: if styled {
                std::ptr::from_ref(self).addr()
            } else {
                self.original_data
            },
            ..Default::default()
        };
        unsafe { SetMenuItemInfoW(self.menu, self.position, 1, &info) != 0 }
    }
}

impl MenuSkin {
    fn new(root: HMENU, theme: Theme, dpi: u32) -> Option<Self> {
        if high_contrast() {
            return None;
        }
        let style = Style::new(theme, dpi)?;
        let mut rows = Vec::new();
        collect_rows(root, &mut rows)?;
        let rows = rows.into_boxed_slice();
        if !set_background(root, style.background(), MNS_NOCHECK) {
            return None;
        }
        for (index, row) in rows.iter().enumerate() {
            if !row.configure(true) {
                for row in &rows[..index] {
                    row.configure(false);
                }
                set_background(root, null_mut(), 0);
                return None;
            }
        }
        Some(Self { theme, style, rows })
    }

    fn row_index(&self, data: usize) -> Option<usize> {
        let offset = data.checked_sub(self.rows.as_ptr().addr())?;
        let size = size_of::<StyledRow>();
        let index = offset / size;
        (offset.is_multiple_of(size) && index < self.rows.len()).then_some(index)
    }
}

fn collect_rows(menu: HMENU, rows: &mut Vec<StyledRow>) -> Option<()> {
    let count = u32::try_from(unsafe { GetMenuItemCount(menu) }).ok()?;
    for position in 0..count {
        let item = item_info(menu, position, true)?;
        let mut label = item_label(menu, position, MF_BYPOSITION).unwrap_or_default();
        let mut row = StyledRow {
            accessible: MSAAMENUINFO::default(),
            label: Vec::new(),
            menu,
            position,
            item_type: item.fType,
            id: item.wID as usize,
            state: item.fState,
            submenu: !item.hSubMenu.is_null(),
            original_data: item.dwItemData,
        };
        if row.kind() == RowKind::TextBlock
            && let Some(text) = label.strip_suffix(&[0])
        {
            let text = String::from_utf16_lossy(text).replace(TEXT_BLOCK_SEPARATOR, "\n\n");
            label = wide(&text);
        }
        row.set_label(label);
        rows.push(row);
        if !item.hSubMenu.is_null() {
            collect_rows(item.hSubMenu, rows)?;
        }
    }
    Some(())
}

fn set_background(menu: HMENU, brush: HBRUSH, style: u32) -> bool {
    let info = MENUINFO {
        cbSize: size_of::<MENUINFO>() as u32,
        fMask: MIM_BACKGROUND | MIM_STYLE | MIM_APPLYTOSUBMENUS,
        dwStyle: style,
        hbrBack: brush,
        ..Default::default()
    };
    unsafe { SetMenuInfo(menu, &info) != 0 }
}

fn slider_value(data: usize) -> Option<u8> {
    (SLIDER_FIRST..=SLIDER_FIRST + 100)
        .contains(&data)
        .then(|| (data - SLIDER_FIRST) as u8)
}

fn use_native_sliders(menu: HMENU) {
    for position in (0..unsafe { GetMenuItemCount(menu) }).rev() {
        let position = position as u32;
        let Some(item) = item_info(menu, position, true) else {
            continue;
        };
        if !item.hSubMenu.is_null() {
            use_native_sliders(item.hSubMenu);
        } else if let Some(current) = slider_value(item.dwItemData)
            && unsafe { DeleteMenu(menu, position, MF_BYPOSITION) } != 0
        {
            let values = (0..=100)
                .rev()
                .filter(|value| value % 10 == 0 || *value == current);
            for (offset, value) in values.enumerate() {
                let label = wide(&format!("{value}%"));
                let flags =
                    MF_BYPOSITION | MF_STRING | if value == current { MF_CHECKED } else { 0 };
                unsafe {
                    InsertMenuW(
                        menu,
                        position + offset as u32,
                        flags,
                        item.wID as usize + 1 + usize::from(value),
                        label.as_ptr(),
                    )
                };
            }
        }
    }
}

thread_local! {
    static TRACKING: RefCell<Option<Tracking>> = const { RefCell::new(None) };
}

pub(super) fn measure(owner: HWND, item: &mut MEASUREITEMSTRUCT) -> bool {
    if item.CtlType != ODT_MENU {
        return false;
    }
    with_style_row(owner, item.itemData, |style, row| {
        style.measure(item, &row.label, row.kind());
    })
}

pub(super) fn draw(owner: HWND, item: &DRAWITEMSTRUCT) -> bool {
    if item.CtlType != ODT_MENU {
        return false;
    }
    with_style_row(owner, item.itemData, |style, row| {
        let glyph = u16::try_from(row.original_data)
            .ok()
            .filter(|glyph| *glyph != 0);
        style.draw(item, &row.label, row.kind(), row.submenu, glyph);
    })
}

fn with_style_row(owner: HWND, data: usize, draw: impl FnOnce(&Style, &StyledRow)) -> bool {
    TRACKING.with(|slot| {
        let Ok(slot) = slot.try_borrow() else {
            return false;
        };
        let Some(skin) = slot
            .as_ref()
            .filter(|tracking| tracking.owner == owner)
            .and_then(|tracking| tracking.skin.as_ref())
        else {
            return false;
        };
        let Some(index) = skin.row_index(data) else {
            return false;
        };
        draw(&skin.style, &skin.rows[index]);
        true
    })
}

pub(super) fn select_character(owner: HWND, menu: HMENU, key: u16) -> Option<LRESULT> {
    let key = char::from_u32(u32::from(key))?;
    TRACKING.with_borrow(|slot| {
        let tracking = slot.as_ref().filter(|tracking| tracking.owner == owner)?;
        let skin = tracking.skin.as_ref()?;
        let mut first = None;
        let mut after_selected = None;
        let mut selected = false;
        for row in skin
            .rows
            .iter()
            .filter(|row| row.menu == menu && !row.separator())
        {
            let Some(info) = item_info(menu, row.position, true) else {
                continue;
            };
            if info.fState & MFS_DISABLED == 0
                && row
                    .label
                    .first()
                    .and_then(|&first| char::from_u32(u32::from(first)))
                    .is_some_and(|first| first.to_lowercase().eq(key.to_lowercase()))
            {
                first.get_or_insert(row.position);
                if selected {
                    after_selected.get_or_insert(row.position);
                }
            }
            selected |= info.fState & MFS_HILITE != 0;
        }
        let position = after_selected.or(first)?;
        Some(((MNC_SELECT << 16) | position) as LRESULT)
    })
}

pub(super) fn notify(owner: HWND, msg: u32, wp: WPARAM, lp: LPARAM) {
    let root = TRACKING.with_borrow_mut(|slot| {
        let tracking = slot.as_mut().filter(|tracking| tracking.owner == owner)?;
        let menu = if msg == WM_INITMENUPOPUP || msg == WM_UNINITMENUPOPUP {
            wp as HMENU
        } else {
            lp as HMENU
        };
        match msg {
            WM_INITMENUPOPUP if !tracking.menus.contains(&menu) => tracking.menus.push(menu),
            WM_UNINITMENUPOPUP => {
                tracking.menus.retain(|&open| open != menu);
                if let (Some(drag), Some(skin)) = (tracking.slider_drag, &mut tracking.skin)
                    && skin.rows[drag.row].menu == menu
                {
                    skin.rows[drag.row].set_slider(drag.initial);
                    tracking.slider_drag = None;
                }
            }
            WM_MENUSELECT => {
                let flags = (wp >> 16) as u32;
                tracking.selected = (tracking.menus.contains(&menu)
                    && flags & (MF_POPUP | MF_SEPARATOR | MF_DISABLED | MF_GRAYED) == 0)
                    .then_some(wp as u16 as usize);
            }
            _ => {}
        }
        (msg == WM_INITMENUPOPUP && tracking.skin.is_some()).then_some(tracking.root)
    });
    if let Some(root) = root {
        // A janela do menu pode ainda não existir durante WM_INITMENUPOPUP.
        unsafe { PostMessageW(owner, FRAME, root as usize, 0) };
    }
}

pub(super) fn frame(owner: HWND, root: usize) {
    let active = TRACKING.with_borrow(|slot| {
        slot.as_ref().is_some_and(|tracking| {
            tracking.owner == owner && tracking.root as usize == root && tracking.skin.is_some()
        })
    });
    if active {
        unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(redraw_popup), 1) };
    }
}

unsafe extern "system" fn filter(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == MSGF_MENU as i32
        && let Some(msg) = unsafe { (lp as *const MSG).as_ref() }
        && let Some((owner, input)) = TRACKING.with_borrow_mut(|slot| {
            let tracking = slot.as_mut()?;
            Some((tracking.owner, tracking.input(msg)?))
        })
    {
        match input {
            MenuInput::Consume => {}
            MenuInput::Apply(choice) => unsafe {
                SendMessageW(owner, APPLY, choice, 0);
            },
            MenuInput::Redraw(popup) => unsafe {
                RedrawWindow(popup, null(), null_mut(), RDW_INVALIDATE | RDW_UPDATENOW);
            },
        }
        return 1;
    }
    unsafe { CallNextHookEx(null_mut(), code, wp, lp) }
}

pub(super) fn dismiss(owner: HWND, root: usize) {
    let active = TRACKING.with_borrow(|slot| {
        slot.as_ref().is_some_and(|tracking| {
            tracking.owner == owner && tracking.root as usize == root && tracking.dismiss_pending
        })
    });
    if active {
        unsafe { EndMenu() };
    }
}

unsafe extern "system" fn mouse_filter(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && let Some((button, down)) = mouse_button(wp as u32)
        && let Some(mouse) = unsafe { (lp as *const MSLLHOOKSTRUCT).as_ref() }
        && outside_click(mouse.pt, button, down)
    {
        return 1;
    }
    unsafe { CallNextHookEx(null_mut(), code, wp, lp) }
}

fn mouse_button(message: u32) -> Option<(u8, bool)> {
    match message {
        WM_LBUTTONDOWN => Some((1, true)),
        WM_LBUTTONUP => Some((1, false)),
        WM_RBUTTONDOWN => Some((2, true)),
        WM_RBUTTONUP => Some((2, false)),
        WM_MBUTTONDOWN => Some((4, true)),
        WM_MBUTTONUP => Some((4, false)),
        WM_XBUTTONDOWN => Some((8, true)),
        WM_XBUTTONUP => Some((8, false)),
        _ => None,
    }
}

fn outside_click(point: POINT, button: u8, down: bool) -> bool {
    let (popup, root_owner) = if down {
        let target = unsafe { WindowFromPoint(point) };
        // Só consulte o menu de janelas deste thread dentro do hook global.
        if unsafe { GetWindowThreadProcessId(target, null_mut()) == GetCurrentThreadId() } {
            (popup_menu(target), unsafe {
                GetAncestor(target, GA_ROOTOWNER)
            })
        } else {
            (None, null_mut())
        }
    } else {
        (None, null_mut())
    };
    TRACKING.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return false;
        };
        let Some(tracking) = slot.as_mut() else {
            return false;
        };
        let (close, consume) = tracking.outside_button(popup, root_owner, button, down);
        if close && !tracking.dismiss_pending {
            tracking.dismiss_pending =
                unsafe { PostMessageW(tracking.owner, DISMISS, tracking.root as usize, 0) != 0 };
        }
        consume
    })
}

impl Tracking {
    fn outside_button(
        &mut self,
        popup: Option<HMENU>,
        root_owner: HWND,
        button: u8,
        down: bool,
    ) -> (bool, bool) {
        if !down {
            let held = self.owner_buttons & button != 0;
            self.owner_buttons &= !button;
            return (held, held);
        }
        if popup.is_some_and(|menu| self.menus.contains(&menu)) {
            return (false, false);
        }
        if root_owner == self.owner {
            // Consuma o par na faixa. A soltura não pode abrir o menu novamente.
            self.owner_buttons |= button;
            return (false, true);
        }
        (true, false)
    }

    fn input(&mut self, msg: &MSG) -> Option<MenuInput> {
        if let Some(input) = self.slider_input(msg) {
            return Some(input);
        }
        match msg.message {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                let choice = self.command_at(msg.pt).filter(|&id| (self.keep_open)(id))?;
                let release = if matches!(msg.message, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
                    WM_LBUTTONUP
                } else {
                    WM_RBUTTONUP
                };
                self.pressed = Some((release, choice));
                Some(MenuInput::Consume)
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                let (_, choice) = self
                    .pressed
                    .filter(|&(release, _)| release == msg.message)?;
                self.pressed = None;
                Some(if self.command_at(msg.pt) == Some(choice) {
                    MenuInput::Apply(choice)
                } else {
                    MenuInput::Consume
                })
            }
            WM_KEYDOWN if matches!(msg.wParam as u16, VK_RETURN | VK_SPACE) => {
                let choice = self.selected.filter(|&id| (self.keep_open)(id))?;
                Some(if msg.lParam & (1 << 30) == 0 {
                    MenuInput::Apply(choice)
                } else {
                    MenuInput::Consume
                })
            }
            _ => None,
        }
    }

    fn slider_input(&mut self, msg: &MSG) -> Option<MenuInput> {
        let skin = self.skin.as_mut()?;
        let (index, value, commit, popup) = match msg.message {
            WM_MOUSEMOVE | WM_LBUTTONUP | WM_RBUTTONUP => {
                let drag = self.slider_drag?;
                let commit = msg.message == drag.release;
                if commit {
                    self.slider_drag = None;
                } else if msg.message != WM_MOUSEMOVE {
                    return None;
                }
                (
                    drag.row,
                    skin.style.slider_value(drag.bounds, msg.pt.x),
                    commit,
                    drag.popup,
                )
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                let popup = unsafe { WindowFromPoint(msg.pt) };
                let menu = popup_menu(popup)?;
                if !self.menus.contains(&menu) {
                    return None;
                }
                let position =
                    u32::try_from(unsafe { MenuItemFromPoint(null_mut(), menu, msg.pt) }).ok()?;
                let index = skin.rows.iter().position(|row| {
                    row.menu == menu
                        && row.position == position
                        && matches!(row.kind(), RowKind::Slider(_))
                })?;
                let mut bounds = RECT::default();
                if unsafe { GetMenuItemRect(null_mut(), menu, position, &mut bounds) } == 0 {
                    return None;
                }
                let release = if matches!(msg.message, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
                    WM_LBUTTONUP
                } else {
                    WM_RBUTTONUP
                };
                let row = &skin.rows[index];
                let initial = slider_value(row.original_data)?;
                self.slider_drag = Some(SliderDrag {
                    row: index,
                    popup,
                    bounds,
                    release,
                    initial,
                });
                self.selected = Some(row.id);
                (
                    index,
                    skin.style.slider_value(bounds, msg.pt.x),
                    false,
                    popup,
                )
            }
            WM_KEYDOWN if self.slider_drag.is_none() => {
                let selected = self.selected?;
                let (index, current) = skin.rows.iter().enumerate().find_map(|(index, row)| {
                    let RowKind::Slider(current) = row.kind() else {
                        return None;
                    };
                    (row.id == selected && self.menus.contains(&row.menu))
                        .then_some((index, current))
                })?;
                let value = match msg.wParam as u16 {
                    VK_LEFT => current.saturating_sub(1),
                    VK_RIGHT => (current + 1).min(100),
                    VK_HOME => 0,
                    VK_END => 100,
                    VK_RETURN | VK_SPACE => return Some(MenuInput::Consume),
                    _ => return None,
                };
                if value == current {
                    return Some(MenuInput::Consume);
                }
                (index, value, true, null_mut())
            }
            _ => return None,
        };

        let row = &mut skin.rows[index];
        let changed = row.kind() != RowKind::Slider(value);
        if changed {
            row.set_slider(value);
        }
        Some(if commit {
            MenuInput::Apply(row.id + 1 + usize::from(value))
        } else if changed {
            MenuInput::Redraw(popup)
        } else {
            MenuInput::Consume
        })
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
        fMask: MIIM_ID | MIIM_STATE | MIIM_FTYPE | MIIM_SUBMENU | MIIM_DATA,
        ..Default::default()
    };
    (unsafe { GetMenuItemInfoW(menu, item, i32::from(by_position), &mut info) } != 0)
        .then_some(info)
}

pub(super) fn sync(root: HMENU, updated: &Menu, theme: Theme) {
    let native = TRACKING.with_borrow(|slot| {
        slot.as_ref()
            .is_some_and(|tracking| tracking.skin.is_none())
    });
    if native {
        use_native_sliders(updated.0);
    }
    let background = TRACKING.with_borrow_mut(|slot| {
        let skin = slot.as_mut()?.skin.as_mut()?;
        if skin.theme == theme {
            return None;
        }
        skin.theme = theme;
        skin.style.set_theme(theme);
        Some(skin.style.background())
    });
    if let Some(background) = background {
        set_background(root, background, MNS_NOCHECK);
    }
    update_items(root, updated.0);
    unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(redraw_popup),
            LPARAM::from(background.is_some()),
        )
    };
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
                && sync_row(
                    item.dwItemData,
                    label,
                    desired.map_or(0, |info| info.dwItemData),
                )
                .unwrap_or_else(|| item_label(menu, index, MF_BYPOSITION).as_ref() != Some(label))
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

fn sync_row(data: usize, label: &[u16], original_data: usize) -> Option<bool> {
    TRACKING.with_borrow_mut(|slot| {
        let skin = slot.as_mut()?.skin.as_mut()?;
        let index = skin.row_index(data)?;
        let row = &mut skin.rows[index];
        row.original_data = original_data;
        let changed = row.label != label;
        if changed {
            row.set_label(label.to_vec());
        }
        Some(changed)
    })
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

unsafe extern "system" fn redraw_popup(hwnd: HWND, update_frame: LPARAM) -> i32 {
    if unsafe { IsWindowVisible(hwnd) == 0 } {
        return 1;
    }
    let Some(menu) = popup_menu(hwnd) else {
        return 1;
    };
    let tracked = TRACKING.with_borrow(|slot| {
        let tracking = slot
            .as_ref()
            .filter(|tracking| tracking.menus.contains(&menu))?;
        Some(
            tracking
                .skin
                .as_ref()
                .map(|skin| (skin.style.border_color(), skin.style.scroll_background())),
        )
    });
    let Some(style) = tracked else { return 1 };
    if update_frame != 0
        && let Some((border, scroll_background)) = style
    {
        let corners = DWMWCP_ROUNDSMALL;
        // Em versões sem estes atributos, o Windows mantém sua moldura padrão.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                (&raw const corners).cast(),
                size_of_val(&corners) as u32,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR as u32,
                (&raw const border).cast(),
                size_of_val(&border) as u32,
            );
        }
        if let Some(brush) = scroll_background
            && has_scroll_caps(hwnd)
        {
            let info = MENUINFO {
                cbSize: size_of::<MENUINFO>() as u32,
                fMask: MIM_BACKGROUND,
                hbrBack: brush,
                ..Default::default()
            };
            // O Windows desenha as setas de rolagem fora das linhas temáticas.
            unsafe { SetMenuInfo(menu, &info) };
        }
    }
    let flags = RDW_INVALIDATE | RDW_UPDATENOW | if update_frame != 0 { RDW_FRAME } else { 0 };
    unsafe { RedrawWindow(hwnd, null(), null_mut(), flags) };
    1
}

fn has_scroll_caps(hwnd: HWND) -> bool {
    let mut window = RECT::default();
    let mut client = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut window) == 0 || GetClientRect(hwnd, &mut client) == 0 } {
        return false;
    }
    let horizontal_frame = super::width(window) - super::width(client);
    let vertical_frame = super::height(window) - super::height(client);
    vertical_frame > (horizontal_frame * 2).max(12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_previews_drag_and_applies_the_final_value() {
        let menu = Menu::new().unwrap();
        let id = 800;
        menu.slider(id, 50);
        let mut rows = Vec::new();
        collect_rows(menu.0, &mut rows).unwrap();
        let theme = Theme::Dark;
        let mut tracking = Tracking {
            owner: null_mut(),
            root: menu.0,
            menus: vec![menu.0],
            keep_open: |_| false,
            selected: Some(id),
            pressed: None,
            slider_drag: Some(SliderDrag {
                row: 0,
                popup: null_mut(),
                bounds: RECT {
                    left: 0,
                    right: 200,
                    top: 0,
                    bottom: 56,
                },
                release: WM_LBUTTONUP,
                initial: 50,
            }),
            owner_buttons: 0,
            dismiss_pending: false,
            skin: Some(MenuSkin {
                theme,
                style: Style::new(theme, 96).unwrap(),
                rows: rows.into_boxed_slice(),
            }),
        };
        let mut message = MSG {
            message: WM_MOUSEMOVE,
            pt: POINT { x: 200, y: 50 },
            ..Default::default()
        };
        assert!(matches!(
            tracking.input(&message),
            Some(MenuInput::Redraw(_))
        ));
        assert_eq!(tracking.skin.as_ref().unwrap().rows[0].label, wide("100%"));

        message.message = WM_LBUTTONUP;
        message.pt.x = 100;
        assert!(
            matches!(tracking.input(&message), Some(MenuInput::Apply(choice)) if choice == id + 1 + 50)
        );
        assert!(tracking.slider_drag.is_none());

        message.message = WM_KEYDOWN;
        message.wParam = usize::from(VK_RIGHT);
        assert!(
            matches!(tracking.input(&message), Some(MenuInput::Apply(choice)) if choice == id + 1 + 51)
        );
        assert_eq!(tracking.skin.as_ref().unwrap().rows[0].label, wide("51%"));
    }

    #[test]
    fn informational_text_is_one_disabled_item_with_room_for_each_line() {
        let menu = Menu::new().unwrap();
        let description = "Monitor de hardware para Windows";
        menu.text_block(&["ARGOS 0.3.0", description, "Licença MIT"]);
        menu.centered_item(338, "Guia do painel");

        let native = item_info(menu.0, 0, true).unwrap();
        assert_eq!(native.wID, 0);
        assert_ne!(native.fState & MFS_DISABLED, 0);
        assert_eq!(
            item_label(menu.0, 0, MF_BYPOSITION).unwrap(),
            wide("ARGOS 0.3.0 · Monitor de hardware para Windows · Licença MIT"),
        );

        let mut rows = Vec::new();
        collect_rows(menu.0, &mut rows).unwrap();
        assert_eq!(rows.len(), 2);
        let row = &rows[0];
        assert!(row.kind() == RowKind::TextBlock);
        assert_eq!(
            row.label,
            wide("ARGOS 0.3.0\n\nMonitor de hardware para Windows\n\nLicença MIT"),
        );
        let command = item_info(menu.0, 1, true).unwrap();
        assert!(rows[1].kind() == RowKind::CenteredItem);
        assert_eq!(command.wID, 338);
        assert_eq!(command.fState & MFS_DISABLED, 0);

        let style = Style::new(Theme::Dark, 96).unwrap();
        let mut block = MEASUREITEMSTRUCT::default();
        let mut single_line = MEASUREITEMSTRUCT::default();
        style.measure(&mut block, &row.label, RowKind::TextBlock);
        style.measure(&mut single_line, &wide(description), RowKind::TextBlock);
        assert_eq!(block.itemWidth, single_line.itemWidth);
        assert!(block.itemHeight > single_line.itemHeight);
    }

    #[test]
    fn outside_presses_dismiss_without_eating_another_apps_click() {
        let owner = 1_usize as HWND;
        let root = 2_usize as HMENU;
        let submenu = 3_usize as HMENU;
        let mut tracking = Tracking {
            owner,
            root,
            menus: vec![root, submenu],
            keep_open: |_| false,
            selected: None,
            pressed: None,
            slider_drag: None,
            owner_buttons: 0,
            dismiss_pending: false,
            skin: None,
        };
        for menu in [root, submenu] {
            assert_eq!(
                tracking.outside_button(Some(menu), owner, 1, true),
                (false, false)
            );
        }
        assert_eq!(
            tracking.outside_button(None, null_mut(), 1, true),
            (true, false)
        );
        assert_eq!(tracking.outside_button(None, owner, 2, true), (false, true));
        assert_eq!(
            tracking.outside_button(None, null_mut(), 1, false),
            (false, false)
        );
        assert_eq!(
            tracking.outside_button(None, null_mut(), 2, false),
            (true, true)
        );
        assert_eq!(
            tracking.outside_button(None, null_mut(), 2, false),
            (false, false)
        );
    }
}
