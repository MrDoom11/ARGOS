use std::num::NonZeroU16;

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DC_BRUSH, DC_PEN, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, FillRect, GetStockObject, HBRUSH, HDC, RestoreDC, RoundRect, SaveDC,
    SelectObject, SetBkColor, SetBkMode, SetDCBrushColor, SetDCPenColor, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_SETSEL, ODS_DISABLED, ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED, ODT_BUTTON,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_RETURN};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use super::appearance::{MenuColors, Theme, high_contrast};
use super::dialog::Template;

const CEILING: i32 = 100;
const HELP: i32 = 101;
const BUTTON_SUBCLASS: usize = 1;
const DIALOG_USER: i32 = (2 * size_of::<isize>()) as i32;

pub(super) fn choose(owner: HWND, current: NonZeroU16, theme: Theme) -> Option<NonZeroU16> {
    let style = (!high_contrast())
        .then(|| DialogStyle::new(theme.menu_colors()))
        .flatten();
    let (apply_style, cancel_style) = if style.is_some() {
        (BS_OWNERDRAW, BS_OWNERDRAW)
    } else {
        (BS_DEFPUSHBUTTON, BS_PUSHBUTTON)
    };
    let mut template = Template::new("Teto do gráfico de FPS", [224, 86]);
    template.add_control(
        0x0082,
        u16::MAX,
        0,
        [10, 10, 204, 10],
        "&Teto da escala (FPS)",
    );
    template.add_control(
        0x0081, // EDIT
        CEILING as u16,
        WS_TABSTOP | WS_BORDER | (ES_NUMBER | ES_AUTOHSCROLL) as u32,
        [10, 24, 68, 14],
        "",
    );
    template.add_control(
        0x0082, // STATIC
        HELP as u16,
        0,
        [10, 43, 204, 12],
        "Informe um valor de 1 a 65535 FPS.",
    );
    template.add_control(
        0x0080, // BUTTON
        IDOK as u16,
        WS_TABSTOP | apply_style as u32,
        [103, 62, 54, 14],
        "Aplicar",
    );
    template.add_control(
        0x0080,
        IDCANCEL as u16,
        WS_TABSTOP | cancel_style as u32,
        [160, 62, 54, 14],
        "Cancelar",
    );
    let state = DialogState {
        current,
        theme,
        style,
    };
    // O diálogo modal termina antes de o estado e seus pincéis saírem de escopo.
    let result = template.show(owner, Some(dialog_proc), (&raw const state) as LPARAM);
    u16::try_from(result).ok().and_then(NonZeroU16::new)
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    unsafe {
        match (msg, i32::from(wp as u16)) {
            (WM_INITDIALOG, _) => {
                let state = &*(lp as *const DialogState);
                SetWindowLongPtrW(hwnd, DIALOG_USER, lp);
                if let Some(style) = &state.style {
                    style.apply(hwnd, state.theme);
                }
                SetDlgItemInt(hwnd, CEILING, u32::from(state.current.get()), 0);
                select_ceiling(hwnd);
                0
            }
            (WM_COMMAND, IDOK) => {
                let mut text = [0; 7];
                let len = GetDlgItemTextW(hwnd, CEILING, text.as_mut_ptr(), text.len() as i32);
                // Um sexto caractere distingue um valor válido de texto truncado.
                let value = (len <= 5)
                    .then(|| String::from_utf16_lossy(&text[..len as usize]))
                    .and_then(|text| text.parse::<NonZeroU16>().ok());
                if let Some(value) = value {
                    EndDialog(hwnd, value.get() as isize);
                } else {
                    SetDlgItemTextW(
                        hwnd,
                        HELP,
                        w!("Valor inválido. Digite um número de 1 a 65535."),
                    );
                    select_ceiling(hwnd);
                }
                1
            }
            (WM_CLOSE, _) | (WM_COMMAND, IDCANCEL) => {
                EndDialog(hwnd, 0);
                1
            }
            _ => (GetWindowLongPtrW(hwnd, DIALOG_USER) as *const DialogState)
                .as_ref()
                .and_then(|state| state.style.as_ref())
                .map_or(0, |style| style.paint(msg, wp, lp)),
        }
    }
}

struct DialogState {
    current: NonZeroU16,
    theme: Theme,
    style: Option<DialogStyle>,
}

struct DialogStyle {
    colors: MenuColors,
    background: HBRUSH,
    input: HBRUSH,
}

impl DialogStyle {
    fn new(colors: MenuColors) -> Option<Self> {
        let style = Self {
            background: unsafe { CreateSolidBrush(colors.background) },
            input: unsafe { CreateSolidBrush(colors.selected) },
            colors,
        };
        (!style.background.is_null() && !style.input.is_null()).then_some(style)
    }

    fn apply(&self, hwnd: HWND, theme: Theme) {
        let dark = i32::from(theme == Theme::Dark);
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                (&raw const dark).cast(),
                size_of_val(&dark) as u32,
            );
            for (attribute, color) in [
                (DWMWA_CAPTION_COLOR, self.colors.background),
                (DWMWA_TEXT_COLOR, self.colors.text),
                (DWMWA_BORDER_COLOR, self.colors.border),
            ] {
                DwmSetWindowAttribute(
                    hwnd,
                    attribute as u32,
                    (&raw const color).cast(),
                    size_of_val(&color) as u32,
                );
            }
            for id in [IDOK, IDCANCEL] {
                SetWindowSubclass(GetDlgItem(hwnd, id), Some(button_proc), BUTTON_SUBCLASS, 0);
            }
            SendMessageW(hwnd, DM_SETDEFID, IDOK as WPARAM, 0);
        }
    }

    fn paint(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
        unsafe {
            match msg {
                WM_CTLCOLORDLG => self.background as isize,
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                    let (background, brush) = if msg == WM_CTLCOLOREDIT {
                        (self.colors.selected, self.input)
                    } else {
                        (self.colors.background, self.background)
                    };
                    let color = if GetDlgCtrlID(lp as HWND) == HELP {
                        self.colors.disabled
                    } else {
                        self.colors.text
                    };
                    let dc = wp as HDC;
                    SetBkColor(dc, background);
                    SetTextColor(dc, color);
                    brush as isize
                }
                WM_DRAWITEM => {
                    let Some(item) = (lp as *const DRAWITEMSTRUCT).as_ref() else {
                        return 0;
                    };
                    if item.CtlType != ODT_BUTTON || !matches!(item.CtlID as i32, IDOK | IDCANCEL) {
                        return 0;
                    }
                    self.draw_button(item);
                    1
                }
                _ => 0,
            }
        }
    }

    fn draw_button(&self, item: &DRAWITEMSTRUCT) {
        let dc = item.hDC;
        let state = item.itemState;
        let focused = state & (ODS_FOCUS | ODS_NOFOCUSRECT) == ODS_FOCUS;
        let border = if focused {
            self.colors.accent
        } else {
            self.colors.border
        };
        let background = if state & ODS_SELECTED != 0 {
            self.colors.separator
        } else {
            self.colors.selected
        };
        let foreground = if state & ODS_DISABLED != 0 {
            self.colors.disabled
        } else if item.CtlID as i32 == IDOK {
            self.colors.accent
        } else {
            self.colors.text
        };
        let mut bounds = item.rcItem;
        let mut label = [0; 32];
        unsafe {
            let saved = SaveDC(dc);
            if saved == 0 {
                return;
            }
            FillRect(dc, &bounds, self.background);
            SelectObject(dc, GetStockObject(DC_BRUSH));
            SelectObject(dc, GetStockObject(DC_PEN));
            SetDCBrushColor(dc, background);
            SetDCPenColor(dc, border);
            let corner = super::scale(6, GetDpiForWindow(item.hwndItem).max(96));
            RoundRect(
                dc,
                bounds.left,
                bounds.top,
                bounds.right,
                bounds.bottom,
                corner,
                corner,
            );
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, foreground);
            let font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0);
            if font != 0 {
                SelectObject(dc, font as _);
            }
            let len = GetWindowTextW(item.hwndItem, label.as_mut_ptr(), label.len() as i32);
            DrawTextW(
                dc,
                label.as_ptr(),
                len,
                &mut bounds,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            RestoreDC(dc, saved);
        }
    }
}

impl Drop for DialogStyle {
    fn drop(&mut self) {
        for brush in [self.background, self.input] {
            if !brush.is_null() {
                unsafe { DeleteObject(brush) };
            }
        }
    }
}

unsafe extern "system" fn button_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    _: usize,
) -> isize {
    unsafe {
        match msg {
            // Um botão desenhado pelo diálogo recebe Enter quando tem foco.
            WM_GETDLGCODE
                if wp as u16 == VK_RETURN
                    && (lp as *const MSG)
                        .as_ref()
                        .is_some_and(|message| message.message == WM_KEYDOWN) =>
            {
                return DefSubclassProc(hwnd, msg, wp, lp) | DLGC_WANTMESSAGE as isize;
            }
            WM_KEYDOWN if wp as u16 == VK_RETURN => {
                SendMessageW(hwnd, BM_CLICK, 0, 0);
                return 0;
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(button_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

fn select_ceiling(hwnd: HWND) {
    unsafe {
        let input = GetDlgItem(hwnd, CEILING);
        SetFocus(input);
        SendMessageW(input, EM_SETSEL, 0, -1);
    }
}
