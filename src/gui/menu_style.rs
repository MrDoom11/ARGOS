use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{COLORREF, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateFontIndirectW, CreateFontW,
    CreatePen, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CALCRECT, DT_CENTER,
    DT_END_ELLIPSIS, DT_HIDEPREFIX, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject,
    DrawTextW, FW_NORMAL, FillRect, GGI_MARK_NONEXISTING_GLYPHS, GetGlyphIndicesW, GetStockObject,
    GetTextFaceW, HBRUSH, HDC, HFONT, HGDIOBJ, IntersectClipRect, NULL_PEN, OUT_DEFAULT_PRECIS,
    PS_SOLID, Polyline, RestoreDC, RoundRect, SaveDC, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_DISABLED, ODS_FOCUS, ODS_GRAYED,
    ODS_NOACCEL, ODS_NOFOCUSRECT, ODS_SELECTED,
};
use windows_sys::Win32::UI::HiDpi::{GetSystemMetricsForDpi, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    NONCLIENTMETRICSW, SM_CXMENUCHECK, SPI_GETNONCLIENTMETRICS,
};
use windows_sys::core::w;

use super::appearance::Theme;
use super::chart::rgb;

const LABEL_INSET: i32 = 32;

pub(super) struct Style {
    // The font is selected into this DC, which is dropped first.
    measure_dc: MeasureDc,
    font: Object,
    icon_fonts: [Option<Object>; 2],
    dark: Palette,
    light: Palette,
    dpi: u32,
    theme: Theme,
    right_gutter: i32,
}

impl Style {
    pub(super) fn new(theme: Theme, dpi: u32) -> Option<Self> {
        let dpi = dpi.max(96);
        let measure_dc = MeasureDc::new()?;
        let font = Object::new(menu_font(dpi))?;
        let icon_fonts = ["Segoe Fluent Icons", "Segoe MDL2 Assets"]
            .map(|face| installed_icon_font(measure_dc.0, dpi, face));
        let dark = Palette::new(Theme::Dark, dpi)?;
        let light = Palette::new(Theme::Light, dpi)?;
        if unsafe { SelectObject(measure_dc.0, font.0) }.is_null() {
            return None;
        }
        let right_gutter = unsafe { GetSystemMetricsForDpi(SM_CXMENUCHECK, dpi) } + px(dpi, 8);
        Some(Self {
            measure_dc,
            font,
            icon_fonts,
            dark,
            light,
            dpi,
            theme,
            right_gutter: right_gutter.max(px(dpi, 24)),
        })
    }

    pub(super) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub(super) fn background(&self) -> HBRUSH {
        self.palette().background.0
    }

    pub(super) fn border_color(&self) -> COLORREF {
        self.palette().border
    }

    pub(super) fn scroll_background(&self) -> Option<HBRUSH> {
        self.palette().arrow_badge.as_ref().map(|brush| brush.0)
    }

    pub(super) fn measure(&self, item: &mut MEASUREITEMSTRUCT, label: &[u16], separator: bool) {
        if separator {
            item.itemWidth = 0;
            item.itemHeight = px(self.dpi, 8) as u32;
            return;
        }

        let mut bounds = RECT::default();
        let count = text_len(label);
        if count != 0 {
            unsafe {
                DrawTextW(
                    self.measure_dc.0,
                    label.as_ptr(),
                    count,
                    &mut bounds,
                    DT_CALCRECT | DT_SINGLELINE | DT_HIDEPREFIX,
                );
            }
        }
        let width = (bounds.right - bounds.left).clamp(0, px(self.dpi, 480));
        item.itemWidth = (width + px(self.dpi, LABEL_INSET) + self.right_gutter) as u32;
        let height = bounds.bottom - bounds.top;
        item.itemHeight = (height + px(self.dpi, 10)).max(px(self.dpi, 28)) as u32;
    }

    pub(super) fn draw(
        &self,
        item: &DRAWITEMSTRUCT,
        label: &[u16],
        separator: bool,
        submenu: bool,
        glyph: Option<u16>,
    ) {
        let dc = item.hDC;
        let bounds = item.rcItem;
        if bounds.left >= bounds.right || bounds.top >= bounds.bottom {
            return;
        }
        let state = unsafe { SaveDC(dc) };
        if state == 0 {
            return;
        }

        let palette = self.palette();
        unsafe {
            IntersectClipRect(dc, bounds.left, bounds.top, bounds.right, bounds.bottom);
            FillRect(dc, &bounds, palette.background.0);
        }
        if separator {
            let middle = bounds.top + (bounds.bottom - bounds.top) / 2;
            let line = RECT {
                left: bounds.left + px(self.dpi, 11),
                top: middle,
                right: bounds.right - px(self.dpi, 11),
                bottom: middle + px(self.dpi, 1),
            };
            unsafe { FillRect(dc, &line, palette.separator.0) };
        } else {
            self.draw_row(item, palette, label, submenu, glyph);
        }
        unsafe { RestoreDC(dc, state) };
    }

    fn draw_row(
        &self,
        item: &DRAWITEMSTRUCT,
        palette: &Palette,
        label: &[u16],
        submenu: bool,
        glyph: Option<u16>,
    ) {
        let dc = item.hDC;
        let state = item.itemState;
        let bounds = item.rcItem;
        let dip = |value| px(self.dpi, value);
        let focused = state & (ODS_FOCUS | ODS_NOFOCUSRECT) == ODS_FOCUS;
        if state & ODS_SELECTED != 0 || focused {
            unsafe {
                SelectObject(dc, GetStockObject(NULL_PEN));
                SelectObject(dc, palette.selected.0);
                RoundRect(
                    dc,
                    bounds.left + dip(4),
                    bounds.top + dip(1),
                    bounds.right - dip(4),
                    bounds.bottom - dip(1),
                    dip(8),
                    dip(8),
                );
            }
        }

        if submenu && let Some(badge) = &palette.arrow_badge {
            let middle = bounds.top + (bounds.bottom - bounds.top) / 2;
            // Windows pinta a seta depois desta chamada e pode usar preto no tema escuro.
            unsafe {
                SelectObject(dc, GetStockObject(NULL_PEN));
                SelectObject(dc, badge.0);
                RoundRect(
                    dc,
                    bounds.right - dip(16),
                    middle - dip(6),
                    bounds.right - dip(2),
                    middle + dip(6),
                    dip(6),
                    dip(6),
                );
            }
        }

        let disabled = state & (ODS_DISABLED | ODS_GRAYED) != 0;
        let text_color = if disabled {
            palette.disabled
        } else {
            palette.text
        };
        if state & ODS_CHECKED != 0 {
            let x = bounds.left + dip(11);
            let y = bounds.top + (bounds.bottom - bounds.top) / 2;
            let points = [
                POINT { x, y },
                POINT {
                    x: x + dip(3),
                    y: y + dip(3),
                },
                POINT {
                    x: x + dip(9),
                    y: y - dip(4),
                },
            ];
            let pen = if disabled {
                &palette.disabled_check
            } else {
                &palette.check
            };
            unsafe {
                SelectObject(dc, pen.0);
                Polyline(dc, points.as_ptr(), points.len() as i32);
            }
        } else if let Some(glyph) = glyph {
            let icon_bounds = RECT {
                left: bounds.left + dip(7),
                right: bounds.left + dip(24),
                ..bounds
            };
            self.draw_icon(
                dc,
                icon_bounds,
                glyph,
                if disabled {
                    palette.disabled
                } else {
                    palette.icon
                },
            );
        }

        let count = text_len(label);
        if count == 0 {
            return;
        }
        let mut text = RECT {
            left: bounds.left + dip(LABEL_INSET),
            // Windows paints the submenu arrow after WM_DRAWITEM.
            right: bounds.right - if submenu { self.right_gutter } else { dip(12) },
            ..bounds
        };
        let flags = DT_SINGLELINE
            | DT_VCENTER
            | DT_END_ELLIPSIS
            | if state & ODS_NOACCEL != 0 {
                DT_HIDEPREFIX
            } else {
                0
            };
        unsafe {
            SelectObject(dc, self.font.0);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, text_color);
            DrawTextW(dc, label.as_ptr(), count, &mut text, flags);
        }
    }

    fn draw_icon(&self, dc: HDC, mut bounds: RECT, glyph: u16, color: COLORREF) {
        for font in self.icon_fonts.iter().flatten() {
            if unsafe { SelectObject(dc, font.0) }.is_null() {
                continue;
            }
            let mut index = 0;
            let mapped =
                unsafe { GetGlyphIndicesW(dc, &glyph, 1, &mut index, GGI_MARK_NONEXISTING_GLYPHS) };
            if mapped != 1 || index == 0 || index == u16::MAX {
                continue;
            }
            unsafe {
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(dc, color);
                DrawTextW(
                    dc,
                    &glyph,
                    1,
                    &mut bounds,
                    DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                );
            }
            return;
        }
    }

    fn palette(&self) -> &Palette {
        match self.theme {
            Theme::Dark => &self.dark,
            Theme::Light => &self.light,
        }
    }
}

struct Palette {
    background: Object,
    selected: Object,
    separator: Object,
    check: Object,
    disabled_check: Object,
    arrow_badge: Option<Object>,
    border: COLORREF,
    text: COLORREF,
    disabled: COLORREF,
    icon: COLORREF,
}

impl Palette {
    fn new(theme: Theme, dpi: u32) -> Option<Self> {
        let (background, selected, separator, border, accent, text, disabled, icon) = match theme {
            Theme::Dark => (
                rgb(32, 32, 32),
                rgb(47, 47, 47),
                rgb(63, 63, 63),
                rgb(74, 74, 74),
                rgb(96, 205, 255),
                rgb(244, 244, 244),
                rgb(160, 160, 160),
                rgb(213, 213, 213),
            ),
            Theme::Light => (
                rgb(249, 249, 249),
                rgb(236, 236, 236),
                rgb(221, 221, 221),
                rgb(210, 210, 210),
                rgb(0, 103, 192),
                rgb(26, 26, 26),
                rgb(103, 103, 103),
                rgb(62, 62, 62),
            ),
        };
        Some(Self {
            background: Object::new(unsafe { CreateSolidBrush(background) })?,
            selected: Object::new(unsafe { CreateSolidBrush(selected) })?,
            separator: Object::new(unsafe { CreateSolidBrush(separator) })?,
            check: Object::new(unsafe { CreatePen(PS_SOLID, px(dpi, 2), accent) })?,
            disabled_check: Object::new(unsafe { CreatePen(PS_SOLID, px(dpi, 2), disabled) })?,
            arrow_badge: if theme == Theme::Dark {
                Some(Object::new(unsafe {
                    CreateSolidBrush(rgb(132, 132, 132))
                })?)
            } else {
                None
            },
            border,
            text,
            disabled,
            icon,
        })
    }
}

fn menu_font(dpi: u32) -> HFONT {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    if unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&mut metrics as *mut NONCLIENTMETRICSW).cast(),
            0,
            dpi,
        )
    } != 0
    {
        let font = unsafe { CreateFontIndirectW(&metrics.lfMenuFont) };
        if !font.is_null() {
            return font;
        }
    }
    font_by_name(px(dpi, 13), w!("Segoe UI"))
}

fn installed_icon_font(dc: HDC, dpi: u32, face: &str) -> Option<Object> {
    let wide: Vec<_> = face.encode_utf16().chain([0]).collect();
    let font = Object::new(font_by_name(px(dpi, 16), wide.as_ptr()))?;
    let old = unsafe { SelectObject(dc, font.0) };
    if old.is_null() {
        return None;
    }
    let mut actual = [0; 32];
    let read = unsafe { GetTextFaceW(dc, actual.len() as i32, actual.as_mut_ptr()) };
    unsafe { SelectObject(dc, old) };
    // GDI may return a substitute font even when the requested family is absent.
    let actual = &actual[..text_len(&actual) as usize];
    let matches = read > 0 && String::from_utf16_lossy(actual).eq_ignore_ascii_case(face);
    matches.then_some(font)
}

fn font_by_name(height: i32, face: *const u16) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET),
            u32::from(OUT_DEFAULT_PRECIS),
            u32::from(CLIP_DEFAULT_PRECIS),
            u32::from(ANTIALIASED_QUALITY),
            u32::from(DEFAULT_PITCH),
            face,
        )
    }
}

fn text_len(label: &[u16]) -> i32 {
    let length = label
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(label.len());
    length.min(i32::MAX as usize) as i32
}

fn px(dpi: u32, dips: i32) -> i32 {
    (dips * dpi as i32 + 48) / 96
}

struct MeasureDc(HDC);

impl MeasureDc {
    fn new() -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        (!dc.is_null()).then(|| Self(dc))
    }
}

impl Drop for MeasureDc {
    fn drop(&mut self) {
        unsafe { DeleteDC(self.0) };
    }
}

struct Object(HGDIOBJ);

impl Object {
    fn new(handle: HGDIOBJ) -> Option<Self> {
        (!handle.is_null()).then(|| Self(handle))
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        unsafe { DeleteObject(self.0) };
    }
}
