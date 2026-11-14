use std::net::{IpAddr, SocketAddrV6};

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::game_network::custom_ping::MAX_TARGETS;

use super::{dialog::Template, wide};

const EDIT: i32 = 100;
const HELP: i32 = 101;
const DIALOG_USER: i32 = (2 * size_of::<isize>()) as i32;
const MAX_TEXT_UNITS: usize = 2048;

pub(super) fn edit(owner: HWND, targets: &[String]) -> Option<Vec<String>> {
    let mut template = Template::new("Endereços do Ping", [247, 125]);
    template.add_control(
        0x0082, // STATIC
        u16::MAX,
        0,
        [10, 9, 227, 18],
        &format!("Digite um IP ou nome de host por linha (até {MAX_TARGETS})."),
    );
    template.add_control(
        0x0081, // EDIT
        EDIT as u16,
        WS_TABSTOP
            | WS_BORDER
            | WS_VSCROLL
            | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32,
        [10, 25, 227, 54],
        "",
    );
    template.add_control(
        0x0082, // STATIC
        HELP as u16,
        0,
        [10, 84, 227, 18],
        "",
    );
    template.add_control(
        0x0080, // BUTTON
        IDOK as u16,
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        [121, 102, 55, 14],
        "Aplicar",
    );
    template.add_control(
        0x0080, // BUTTON
        IDCANCEL as u16,
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        [182, 102, 55, 14],
        "Cancelar",
    );
    let mut state = EditState {
        initial_text: wide(&targets.join("\r\n")),
        result: None,
    };
    let result = template.show(owner, Some(dialog_proc), (&raw mut state) as LPARAM);
    state.result.filter(|_| result == IDOK as isize)
}

struct EditState {
    initial_text: Vec<u16>,
    result: Option<Vec<String>>,
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    // O diálogo modal mantém o estado em um endereço válido até retornar.
    unsafe {
        match (msg, i32::from(wp as u16)) {
            (WM_INITDIALOG, _) => {
                let state = &*(lp as *const EditState);
                SetWindowLongPtrW(hwnd, DIALOG_USER, lp);
                SendDlgItemMessageW(hwnd, EDIT, EM_SETLIMITTEXT, MAX_TEXT_UNITS, 0);
                SetDlgItemTextW(hwnd, EDIT, state.initial_text.as_ptr());
                1
            }
            (WM_COMMAND, IDOK) => {
                let mut text = [0; MAX_TEXT_UNITS + 1];
                let len = GetDlgItemTextW(hwnd, EDIT, text.as_mut_ptr(), text.len() as i32);
                let result = if len as usize >= MAX_TEXT_UNITS {
                    Err("A lista de endereços está longa demais.".to_owned())
                } else {
                    parse_targets(&String::from_utf16_lossy(&text[..len as usize]))
                };
                match result {
                    Ok(targets) => {
                        let state = GetWindowLongPtrW(hwnd, DIALOG_USER) as *mut EditState;
                        (*state).result = Some(targets);
                        EndDialog(hwnd, IDOK as isize);
                    }
                    Err(error) => {
                        SetDlgItemTextW(hwnd, HELP, wide(&error).as_ptr());
                        SetFocus(GetDlgItem(hwnd, EDIT));
                    }
                }
                1
            }
            (WM_CLOSE, _) | (WM_COMMAND, IDCANCEL) => {
                EndDialog(hwnd, IDCANCEL as isize);
                1
            }
            _ => 0,
        }
    }
}

fn parse_targets(text: &str) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let target = normalize_target(line).ok_or_else(|| {
            format!(
                "Digite um IP ou nome de host válido na linha {}.",
                index + 1
            )
        })?;
        if !targets.contains(&target) {
            if targets.len() >= MAX_TARGETS {
                return Err(format!("Informe até {MAX_TARGETS} endereços diferentes."));
            }
            targets.push(target);
        }
    }
    Ok(targets)
}

pub(super) fn normalize_target(target: &str) -> Option<String> {
    let target = target.trim();
    if let Ok(address) = target.parse::<IpAddr>() {
        return Some(address.to_string());
    }
    if target.contains('%') {
        let address = format!("[{target}]:0").parse::<SocketAddrV6>().ok()?;
        return (address.scope_id() != 0)
            .then(|| format!("{}%{}", address.ip(), address.scope_id()));
    }
    let host = target.strip_suffix('.').unwrap_or(target);
    if host.len() > 253
        || host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    host.split('.')
        .all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .then(|| target.to_ascii_lowercase())
}
