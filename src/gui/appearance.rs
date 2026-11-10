use windows_sys::Win32::Foundation::{COLORREF, HWND};
use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, SPI_GETHIGHCONTRAST, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, SystemParametersInfoW,
    WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

use super::chart::rgb;

pub(super) const LAYERED_STYLES: u32 = WS_EX_LAYERED | WS_EX_TRANSPARENT;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Appearance {
    pub(super) theme: Theme,
    pub(super) opacity: u8,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            opacity: 100,
        }
    }
}

impl Appearance {
    pub(super) fn translucent(self) -> bool {
        self.opacity < 100
    }

    pub(super) fn layered_palette(self) -> Palette {
        let mut palette = self.theme.palette();
        if self.theme == Theme::Light && self.translucent() {
            palette.text_outline = Some(rgb(248, 250, 252));
        }
        palette
    }
}

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) background: COLORREF,
    pub(super) plot: COLORREF,
    pub(super) border: COLORREF,
    pub(super) text: COLORREF,
    pub(super) muted: COLORREF,
    pub(super) grid: COLORREF,
    pub(super) text_outline: Option<COLORREF>,
}

#[derive(Clone, Copy)]
pub(super) struct MenuColors {
    pub(super) background: COLORREF,
    pub(super) selected: COLORREF,
    pub(super) separator: COLORREF,
    pub(super) border: COLORREF,
    pub(super) accent: COLORREF,
    pub(super) text: COLORREF,
    pub(super) disabled: COLORREF,
    pub(super) icon: COLORREF,
}

impl Theme {
    pub(super) fn menu_colors(self) -> MenuColors {
        match self {
            Self::Dark => MenuColors {
                background: rgb(32, 32, 32),
                selected: rgb(47, 47, 47),
                separator: rgb(63, 63, 63),
                border: rgb(74, 74, 74),
                accent: rgb(96, 205, 255),
                text: rgb(244, 244, 244),
                disabled: rgb(160, 160, 160),
                icon: rgb(213, 213, 213),
            },
            Self::Light => MenuColors {
                background: rgb(249, 249, 249),
                selected: rgb(236, 236, 236),
                separator: rgb(221, 221, 221),
                border: rgb(210, 210, 210),
                accent: rgb(0, 103, 192),
                text: rgb(26, 26, 26),
                disabled: rgb(103, 103, 103),
                icon: rgb(62, 62, 62),
            },
        }
    }

    pub(super) fn palette(self) -> Palette {
        match self {
            Self::Dark => Palette {
                background: rgb(0, 0, 0),
                plot: rgb(0, 0, 0),
                border: rgb(72, 79, 89),
                text: rgb(236, 240, 246),
                muted: rgb(181, 190, 203),
                grid: rgb(52, 55, 61),
                text_outline: None,
            },
            Self::Light => Palette {
                background: rgb(255, 255, 255),
                plot: rgb(204, 208, 213),
                border: rgb(103, 111, 120),
                text: rgb(18, 23, 29),
                muted: rgb(53, 62, 72),
                grid: rgb(171, 178, 186),
                text_outline: None,
            },
        }
    }
}

pub(super) fn high_contrast() -> bool {
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            (&raw mut contrast).cast(),
            0,
        )
    };
    read != 0 && contrast.dwFlags & HCF_HIGHCONTRASTON != 0
}

pub(super) fn is_layered(hwnd: HWND) -> bool {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_LAYERED != 0 }
}

pub(super) fn is_click_through(hwnd: HWND) -> bool {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & LAYERED_STYLES == LAYERED_STYLES }
}

pub(super) fn set_layered(hwnd: HWND, layered: bool) -> bool {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let next = (style & !LAYERED_STYLES) | if layered { LAYERED_STYLES } else { 0 };
    if next != style {
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next as isize);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    }
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & LAYERED_STYLES == next & LAYERED_STYLES }
}
