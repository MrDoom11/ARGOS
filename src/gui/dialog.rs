use std::ptr::null;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

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
