use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DC_BRUSH, DC_PEN, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, FillRect, GetDC, GetStockObject, GetTextMetricsW, HBRUSH, HDC,
    ReleaseDC, RestoreDC, RoundRect, SaveDC, SelectObject, SetBkColor, SetBkMode, SetDCBrushColor,
    SetDCPenColor, SetTextColor, TEXTMETRICW, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_GETLINECOUNT, ODS_DISABLED, ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED,
    ODT_BUTTON, ShowScrollBar,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_RETURN};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::appearance::{MenuColors, Theme, high_contrast};
use super::{client, current_monitor, height, scale, wide, width};

pub(super) const DIALOG_USER: i32 = (2 * size_of::<isize>()) as i32;
const BUTTON_SUBCLASS: usize = 1;
const NOTICE_TEXT: i32 = 100;

pub(super) struct Template(Vec<u16>);

impl Template {
    pub(super) fn new(title: &str, [width, height]: [u16; 2]) -> Self {
        let style =
            WS_POPUP | WS_CAPTION | WS_SYSMENU | (DS_CENTER | DS_MODALFRAME | DS_SETFONT) as u32;
        // DLGTEMPLATE seguido de menu, classe, título, tamanho e nome da fonte.
        let mut words = vec![
            style as u16,
            (style >> 16) as u16,
            0,
            0,
            0,
            0,
            0,
            width,
            height,
            0,
            0,
        ];
        words.extend(title.encode_utf16().chain([0]));
        words.push(9);
        words.extend("Segoe UI".encode_utf16().chain([0]));
        Self(words)
    }

    pub(super) fn add_control(
        &mut self,
        class: u16,
        id: u16,
        style: u32,
        rect: [u16; 4],
        title: &str,
    ) {
        let words = &mut self.0;
        words[4] += 1;
        if !words.len().is_multiple_of(2) {
            words.push(0);
        }
        let style = style | WS_CHILD | WS_VISIBLE;
        words.extend([style as u16, (style >> 16) as u16, 0, 0]);
        words.extend(rect);
        words.extend([id, 0xffff, class]);
        words.extend(title.encode_utf16().chain([0, 0]));
    }

    fn request_foreground(&mut self) {
        self.0[0] |= DS_SETFOREGROUND as u16;
    }

    pub(super) fn show(self, owner: HWND, proc: DLGPROC, initial: LPARAM) -> isize {
        // Um Vec<u32> mantém o modelo alinhado em DWORD durante o diálogo modal.
        let template: Vec<u32> = self
            .0
            .chunks(2)
            .map(|pair| u32::from(pair[0]) | (u32::from(*pair.get(1).unwrap_or(&0)) << 16))
            .collect();
        unsafe {
            DialogBoxIndirectParamW(
                GetModuleHandleW(null()),
                template.as_ptr().cast(),
                owner,
                proc,
                initial,
            )
        }
    }
}

pub(super) fn notice(owner: HWND, theme: Theme, title: &str, message: &str) -> bool {
    let style = DialogStyle::new(theme);
    let template = notice_template(title, message, style.is_some());
    match template.show(owner, Some(notice_proc), (&raw const style) as LPARAM) {
        1 => true,
        -1 => {
            let title = wide(title);
            let message = wide(message);
            unsafe {
                MessageBoxW(
                    owner,
                    message.as_ptr(),
                    title.as_ptr(),
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
                ) != 0
            }
        }
        _ => false,
    }
}

fn notice_template(title: &str, message: &str, themed: bool) -> Template {
    let button = if themed {
        BS_OWNERDRAW
    } else {
        BS_DEFPUSHBUTTON
    };
    let text = message.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut template = Template::new(title, [306, 62]);
    template.request_foreground();
    template.add_control(
        0x0081, // EDIT
        NOTICE_TEXT as u16,
        WS_TABSTOP | WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
        [12, 9, 282, 23],
        &text,
    );
    template.add_control(
        0x0080, // BUTTON
        IDOK as u16,
        WS_TABSTOP | button as u32,
        [232, 40, 62, 16],
        "OK",
    );
    template
}

unsafe extern "system" fn notice_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    unsafe {
        match (msg, i32::from(wp as u16)) {
            (WM_INITDIALOG, _) => {
                let style = &*(lp as *const Option<DialogStyle>);
                SetWindowLongPtrW(hwnd, DIALOG_USER, lp);
                if let Some(style) = style {
                    style.apply(hwnd, IDOK, &[IDOK]);
                }
                size_notice(hwnd);
                SetFocus(GetDlgItem(hwnd, IDOK));
                0
            }
            (WM_CLOSE, _) | (WM_COMMAND, IDOK | IDCANCEL) => {
                EndDialog(hwnd, 1);
                1
            }
            _ => (GetWindowLongPtrW(hwnd, DIALOG_USER) as *const Option<DialogStyle>)
                .as_ref()
                .and_then(Option::as_ref)
                .map_or(0, |style| style.paint(msg, wp, lp, None)),
        }
    }
}

fn size_notice(hwnd: HWND) {
    unsafe {
        let text = GetDlgItem(hwnd, NOTICE_TEXT);
        let button = GetDlgItem(hwnd, IDOK);
        let Some(monitor) = current_monitor(hwnd) else {
            return;
        };
        let dc = GetDC(text);
        if dc.is_null() {
            return;
        }
        let font = SendMessageW(text, WM_GETFONT, 0, 0);
        let previous = (font != 0).then(|| SelectObject(dc, font as _));
        let mut metrics = TEXTMETRICW::default();
        let measured = GetTextMetricsW(dc, &mut metrics) != 0;
        if let Some(previous) = previous.filter(|previous| !previous.is_null()) {
            SelectObject(dc, previous);
        }
        ReleaseDC(text, dc);
        if !measured {
            return;
        }

        let mut spacing = RECT {
            right: 12,
            bottom: 9,
            ..Default::default()
        };
        if MapDialogRect(hwnd, &mut spacing) == 0 {
            return;
        }
        let work = monitor.rcWork;
        // Preserve o espaço do botão ao limitar o texto à área útil do monitor.
        let mut outer = RECT::default();
        if GetWindowRect(hwnd, &mut outer) == 0 {
            return;
        }
        let inner = client(hwnd);
        let button_size = client(button);
        let outer_width = width(outer).min(width(work));
        let client_width = outer_width - (width(outer) - width(inner));
        let frame_height = height(outer) - height(inner);
        let text_width = (client_width - 2 * spacing.right).max(1);
        let max_height = (height(work) - frame_height - height(button_size) - 3 * spacing.bottom)
            .max(metrics.tmHeight);
        ShowScrollBar(text, SB_VERT, 0);
        MoveWindow(
            text,
            spacing.right,
            spacing.bottom,
            text_width,
            max_height,
            0,
        );
        let lines = SendMessageW(text, EM_GETLINECOUNT, 0, 0) as i32;
        let desired_height = lines.saturating_mul(metrics.tmHeight) + 2;
        let text_height = desired_height.min(max_height);
        if desired_height > max_height {
            ShowScrollBar(text, SB_VERT, 1);
        }
        MoveWindow(
            text,
            spacing.right,
            spacing.bottom,
            text_width,
            text_height,
            0,
        );
        MoveWindow(
            button,
            client_width - spacing.right - width(button_size),
            text_height + 2 * spacing.bottom,
            width(button_size),
            height(button_size),
            0,
        );
        let outer_height = text_height + height(button_size) + 3 * spacing.bottom + frame_height;
        SetWindowPos(
            hwnd,
            null_mut(),
            work.left + (width(work) - outer_width) / 2,
            work.top + (height(work) - outer_height) / 2,
            outer_width,
            outer_height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

pub(super) struct DialogStyle {
    colors: MenuColors,
    theme: Theme,
    background: HBRUSH,
    input: HBRUSH,
}

impl DialogStyle {
    pub(super) fn new(theme: Theme) -> Option<Self> {
        if high_contrast() {
            return None;
        }
        let colors = theme.menu_colors();
        let style = Self {
            background: unsafe { CreateSolidBrush(colors.background) },
            input: unsafe { CreateSolidBrush(colors.selected) },
            colors,
            theme,
        };
        (!style.background.is_null() && !style.input.is_null()).then_some(style)
    }

    pub(super) fn apply(&self, hwnd: HWND, default_button: i32, buttons: &[i32]) {
        let dark = i32::from(self.theme == Theme::Dark);
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
            for &id in buttons {
                SetWindowSubclass(GetDlgItem(hwnd, id), Some(button_proc), BUTTON_SUBCLASS, 0);
            }
            SendMessageW(hwnd, DM_SETDEFID, default_button as WPARAM, 0);
        }
    }

    pub(super) fn paint(
        &self,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
        muted_label: Option<i32>,
    ) -> isize {
        unsafe {
            match msg {
                WM_CTLCOLORDLG => self.background as isize,
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                    let (background, brush) = if msg == WM_CTLCOLOREDIT {
                        (self.colors.selected, self.input)
                    } else {
                        (self.colors.background, self.background)
                    };
                    let color = if Some(GetDlgCtrlID(lp as HWND)) == muted_label {
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
                    if item.CtlType != ODT_BUTTON {
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
            let corner = scale(6, GetDpiForWindow(item.hwndItem).max(96));
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::{GetBkColor, GetTextColor};

    #[test]
    fn notice_grows_with_its_text_and_limits_height_to_the_monitor() {
        let short = notice_height("Uma linha.", false, Theme::Dark);
        let multiline = notice_height(
            "Primeira linha.\nSegunda linha.\nTerceira linha.",
            false,
            Theme::Light,
        );
        let overflow = notice_height(&"Linha adicional.\n".repeat(500), true, Theme::Dark);
        assert!(short < multiline && multiline < overflow);
    }

    fn notice_height(message: &str, scrolls: bool, theme: Theme) -> i32 {
        let mut check = NoticeCheck {
            style: DialogStyle::new(theme),
            theme,
            scrolls,
            height: 0,
        };
        let template = notice_template("Teste de aviso", message, check.style.is_some());
        let dismissed = template.show(null_mut(), Some(check_proc), (&raw mut check) as LPARAM);
        assert_eq!(dismissed, 1);
        check.height
    }

    struct NoticeCheck {
        style: Option<DialogStyle>,
        theme: Theme,
        scrolls: bool,
        height: i32,
    }

    unsafe extern "system" fn check_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
        unsafe {
            if msg != WM_INITDIALOG {
                return notice_proc(hwnd, msg, wp, lp);
            }
            let check = lp as *mut NoticeCheck;
            notice_proc(hwnd, msg, wp, (&raw const (*check).style) as LPARAM);
            let text = GetDlgItem(hwnd, NOTICE_TEXT);
            let has_scrollbar = GetWindowLongPtrW(text, GWL_STYLE) as u32 & WS_VSCROLL != 0;
            let mut bounds = RECT::default();
            let fits = current_monitor(hwnd).is_some_and(|monitor| {
                GetWindowRect(hwnd, &mut bounds) != 0 && height(bounds) <= height(monitor.rcWork)
            });
            let colors_match = (*check).style.is_none() || {
                let dc = GetDC(text);
                let colors = (*check).theme.menu_colors();
                let brush = SendMessageW(hwnd, WM_CTLCOLORSTATIC, dc as WPARAM, text as LPARAM);
                let matches = brush != 0
                    && GetBkColor(dc) == colors.background
                    && GetTextColor(dc) == colors.text;
                ReleaseDC(text, dc);
                matches
            };
            if !fits || !colors_match || has_scrollbar != (*check).scrolls {
                EndDialog(hwnd, 0);
                return 0;
            }
            (*check).height = height(client(text));
            notice_proc(hwnd, WM_COMMAND, IDOK as WPARAM, 0);
            0
        }
    }
}
