use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use super::board::DropdownHotkey;
use super::dialog::Template;

const INSTRUCTIONS: i32 = 100;
const SUBCLASS: usize = 1;

pub(super) fn choose(owner: HWND) -> Option<DropdownHotkey> {
    let result = template().show(owner, Some(dialog_proc), 0);
    let encoded = u32::try_from(result).ok()?;
    DropdownHotkey::new(encoded >> 16, encoded & 0xffff)
}

pub(super) fn label(hotkey: DropdownHotkey) -> String {
    let mut parts = Vec::with_capacity(5);
    for (flag, name) in [
        (MOD_CONTROL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
        (MOD_WIN, "Win"),
    ] {
        if hotkey.modifiers & flag != 0 {
            parts.push(name);
        }
    }
    let key = key_name(hotkey.key);
    parts.push(&key);
    parts.join("+")
}

fn key_name(key: u32) -> String {
    let scan = unsafe { MapVirtualKeyW(key, MAPVK_VK_TO_VSC_EX) };
    let mut key_info = (scan & 0xff) << 16;
    if scan & 0xff00 == 0xe000 {
        key_info |= 1 << 24;
    }
    let mut name = [0_u16; 64];
    let len = unsafe { GetKeyNameTextW(key_info as i32, name.as_mut_ptr(), name.len() as i32) };
    if len > 0 {
        return String::from_utf16_lossy(&name[..len as usize]);
    }
    format!("Tecla {key}")
}

fn pressed_modifiers() -> u32 {
    let mut modifiers = 0;
    for (key, flag) in [
        (VK_CONTROL, MOD_CONTROL),
        (VK_MENU, MOD_ALT),
        (VK_SHIFT, MOD_SHIFT),
        (VK_LWIN, MOD_WIN),
        (VK_RWIN, MOD_WIN),
    ] {
        if unsafe { GetKeyState(i32::from(key)) } < 0 {
            modifiers |= flag;
        }
    }
    modifiers
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, _: LPARAM) -> isize {
    // SAFETY: os HWNDs são do diálogo e de seus controles na mesma thread.
    unsafe {
        match msg {
            WM_INITDIALOG => {
                let button = GetDlgItem(hwnd, IDCANCEL);
                if button.is_null()
                    || SetWindowSubclass(button, Some(capture_proc), SUBCLASS, 0) == 0
                {
                    EndDialog(hwnd, 0);
                }
                1
            }
            WM_CLOSE => {
                EndDialog(hwnd, 0);
                1
            }
            WM_COMMAND if wp as u16 == IDCANCEL as u16 => {
                EndDialog(hwnd, 0);
                1
            }
            _ => 0,
        }
    }
}

unsafe extern "system" fn capture_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    // SAFETY: WM_GETDLGCODE fornece um MSG válido ou NULL durante o despacho.
    unsafe {
        match msg {
            WM_GETDLGCODE => {
                if let Some(message) = (lp as *const MSG).as_ref()
                    && matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN)
                    && pressed_modifiers() != 0
                {
                    return DLGC_WANTALLKEYS as LRESULT;
                }
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let modifiers = pressed_modifiers();
                if modifiers != 0 {
                    if matches!(
                        wp as u16,
                        VK_CONTROL
                            | VK_SHIFT
                            | VK_MENU
                            | VK_LWIN
                            | VK_RWIN
                            | VK_LCONTROL
                            | VK_RCONTROL
                            | VK_LSHIFT
                            | VK_RSHIFT
                            | VK_LMENU
                            | VK_RMENU
                    ) {
                        return 0;
                    }
                    let dialog = GetParent(hwnd);
                    if let Some(hotkey) = DropdownHotkey::new(modifiers, wp as u32) {
                        EndDialog(dialog, ((hotkey.modifiers << 16) | hotkey.key) as isize);
                    } else {
                        SetDlgItemTextW(
                            dialog,
                            INSTRUCTIONS,
                            w!(
                                "Essa combinação não está disponível.\r\nTente outra ou pressione Esc para cancelar."
                            ),
                        );
                    }
                    return 0;
                }
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(capture_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

fn template() -> Template {
    let mut template = Template::new("Atalho global do dropdown", [239, 57]);
    template.add_control(
        0x0082, // STATIC
        INSTRUCTIONS as u16,
        0,
        [10, 9, 220, 22],
        "Pressione Ctrl, Alt, Shift ou Win junto com outra tecla.\r\nPressione Esc para cancelar.",
    );
    template.add_control(
        0x0080, // BUTTON
        IDCANCEL as u16,
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        [174, 34, 55, 14],
        "Cancelar",
    );
    template
}
