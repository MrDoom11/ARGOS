use std::num::NonZeroU16;

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::EM_SETSEL;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use super::appearance::Theme;
use super::dialog::{DIALOG_USER, DialogStyle, Template};

const CEILING: i32 = 100;
const HELP: i32 = 101;

pub(super) fn choose(owner: HWND, current: NonZeroU16, theme: Theme) -> Option<NonZeroU16> {
    let style = DialogStyle::new(theme);
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
    let state = DialogState { current, style };
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
                    style.apply(hwnd, IDOK, &[IDOK, IDCANCEL]);
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
                .map_or(0, |style| style.paint(msg, wp, lp, Some(HELP))),
        }
    }
}

struct DialogState {
    current: NonZeroU16,
    style: Option<DialogStyle>,
}

fn select_ceiling(hwnd: HWND) {
    unsafe {
        let input = GetDlgItem(hwnd, CEILING);
        SetFocus(input);
        SendMessageW(input, EM_SETSEL, 0, -1);
    }
}
