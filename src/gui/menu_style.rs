use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{COLORREF, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateFontIndirectW, CreateFontW,
    CreatePen, CreateSolidBrush, DC_BRUSH, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CALCRECT, DT_CENTER,
    DT_END_ELLIPSIS, DT_HIDEPREFIX, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteDC,
    DeleteObject, DrawTextW, FW_NORMAL, FillRect, GGI_MARK_NONEXISTING_GLYPHS, GetGlyphIndicesW,
    GetStockObject, GetTextFaceW, HBRUSH, HDC, HFONT, HGDIOBJ, IntersectClipRect, NULL_PEN,
    OUT_DEFAULT_PRECIS, PS_SOLID, Polyline, RestoreDC, RoundRect, SaveDC, SelectObject, SetBkMode,
    SetDCBrushColor, SetTextColor, TRANSPARENT,
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

use super::appearance::{MenuColors, Theme};
use super::chart::rgb;

const LABEL_INSET: i32 = 32;
const TEXT_BLOCK_INSET: i32 = 12;
const VERTICAL_PADDING: i32 = 5;
const TEXT_BLOCK_WIDTH: i32 = 270;
const SLIDER_WIDTH: i32 = 200;
const SLIDER_HEIGHT: i32 = 56;
const SLIDER_INSET: i32 = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RowKind {
    Item,
    Separator,
    TextBlock,
    CenteredItem,
    Slider(u8),
    Swatch(COLORREF),
}

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

    pub(super) fn measure(&self, item: &mut MEASUREITEMSTRUCT, label: &[u16], kind: RowKind) {
        if matches!(kind, RowKind::Slider(_)) {
            item.itemWidth = px(self.dpi, SLIDER_WIDTH) as u32;
            item.itemHeight = px(self.dpi, SLIDER_HEIGHT) as u32;
            return;
        }
        if kind == RowKind::Separator {
            item.itemWidth = 0;
            item.itemHeight = px(self.dpi, 8) as u32;
            return;
        }

        let mut bounds = RECT::default();
        let count = text_len(label);
        if count != 0 {
            let flags = match kind {
                RowKind::TextBlock => {
                    bounds.right = px(self.dpi, TEXT_BLOCK_WIDTH);
                    DT_WORDBREAK | DT_NOPREFIX
                }
                _ => DT_SINGLELINE | DT_HIDEPREFIX,
            };
            unsafe {
                DrawTextW(
                    self.measure_dc.0,
                    label.as_ptr(),
                    count,
                    &mut bounds,
                    DT_CALCRECT | flags,
                );
            }
        }
        let width = (bounds.right - bounds.left).clamp(0, px(self.dpi, 480));
        let horizontal_padding = if matches!(kind, RowKind::TextBlock | RowKind::CenteredItem) {
            2 * px(self.dpi, TEXT_BLOCK_INSET)
        } else {
            px(self.dpi, LABEL_INSET) + self.right_gutter
        };
        item.itemWidth = (width + horizontal_padding) as u32;
        let height = bounds.bottom - bounds.top;
        item.itemHeight =
            (height + px(self.dpi, 2 * VERTICAL_PADDING)).max(px(self.dpi, 28)) as u32;
    }

    pub(super) fn draw(
        &self,
        item: &DRAWITEMSTRUCT,
        label: &[u16],
        kind: RowKind,
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
        if kind == RowKind::Separator {
            let middle = bounds.top + (bounds.bottom - bounds.top) / 2;
            let line = RECT {
                left: bounds.left + px(self.dpi, 11),
                top: middle,
                right: bounds.right - px(self.dpi, 11),
                bottom: middle + px(self.dpi, 1),
            };
            unsafe { FillRect(dc, &line, palette.separator.0) };
        } else if let RowKind::Slider(value) = kind {
            self.draw_slider(item, palette, label, value);
        } else if kind == RowKind::TextBlock {
            self.draw_text_block(item, palette, label);
        } else {
            self.draw_row(item, palette, label, submenu, glyph, kind);
        }
        unsafe { RestoreDC(dc, state) };
    }

    fn slider_track(&self, bounds: RECT) -> (i32, i32) {
        let inset = px(self.dpi, SLIDER_INSET).min((bounds.right - bounds.left) / 2);
        (bounds.left + inset, bounds.right - inset)
    }

    pub(super) fn slider_value(&self, bounds: RECT, x: i32) -> u8 {
        let (left, right) = self.slider_track(bounds);
        let length = right - left;
        if length == 0 {
            return 0;
        }
        let offset = x.clamp(left, right) - left;
        ((offset * 100 + length / 2) / length) as u8
    }

    fn draw_slider(&self, item: &DRAWITEMSTRUCT, palette: &Palette, label: &[u16], value: u8) {
        self.draw_selection(item, palette);
        let dc = item.hDC;
        let bounds = item.rcItem;
        let dip = |value| px(self.dpi, value);
        let (left, right) = self.slider_track(bounds);
        let thumb = left + ((right - left) * i32::from(value.min(100)) + 50) / 100;
        let y = bounds.bottom - dip(17);
        let track_radius = dip(2);
        let thumb_radius = dip(7);
        let mut text = RECT {
            top: bounds.top + dip(3),
            bottom: y - thumb_radius,
            ..bounds
        };
        unsafe {
            SelectObject(dc, self.font.0);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, palette.text);
            DrawTextW(
                dc,
                label.as_ptr(),
                text_len(label),
                &mut text,
                DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
            );

            SelectObject(dc, GetStockObject(NULL_PEN));
            SelectObject(dc, GetStockObject(DC_BRUSH));
            SetDCBrushColor(dc, palette.disabled);
            RoundRect(
                dc,
                left,
                y - track_radius,
                right,
                y + track_radius,
                dip(4),
                dip(4),
            );
            SetDCBrushColor(dc, palette.accent);
            if thumb > left {
                RoundRect(
                    dc,
                    left,
                    y - track_radius,
                    thumb,
                    y + track_radius,
                    dip(4),
                    dip(4),
                );
            }
            RoundRect(
                dc,
                thumb - thumb_radius,
                y - thumb_radius,
                thumb + thumb_radius,
                y + thumb_radius,
                dip(14),
                dip(14),
            );
        }
    }

    fn draw_text_block(&self, item: &DRAWITEMSTRUCT, palette: &Palette, label: &[u16]) {
        let dc = item.hDC;
        let bounds = item.rcItem;
        let inset = px(self.dpi, TEXT_BLOCK_INSET);
        let extra =
            (bounds.right - bounds.left - 2 * inset - px(self.dpi, TEXT_BLOCK_WIDTH)).max(0);
        let mut text = RECT {
            left: bounds.left + inset + extra / 2,
            right: bounds.right - inset - (extra - extra / 2),
            top: bounds.top + px(self.dpi, VERTICAL_PADDING),
            bottom: bounds.bottom,
        };
        unsafe {
            SelectObject(dc, self.font.0);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, palette.text);
            DrawTextW(
                dc,
                label.as_ptr(),
                text_len(label),
                &mut text,
                DT_WORDBREAK | DT_CENTER | DT_NOPREFIX,
            );
        }
    }

    fn draw_selection(&self, item: &DRAWITEMSTRUCT, palette: &Palette) {
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
    }

    fn draw_row(
        &self,
        item: &DRAWITEMSTRUCT,
        palette: &Palette,
        label: &[u16],
        submenu: bool,
        glyph: Option<u16>,
        kind: RowKind,
    ) {
        self.draw_selection(item, palette);
        let dc = item.hDC;
        let state = item.itemState;
        let bounds = item.rcItem;
        let dip = |value| px(self.dpi, value);
        let centered = kind == RowKind::CenteredItem;

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
        if let RowKind::Swatch(color) = kind {
            self.draw_swatch(dc, bounds, color, palette.border);
        } else if state & ODS_CHECKED != 0 && glyph.is_none() {
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
            left: bounds.left
                + dip(if centered {
                    TEXT_BLOCK_INSET
                } else {
                    LABEL_INSET
                }),
            // Windows paints the submenu arrow after WM_DRAWITEM.
            right: bounds.right
                - if submenu {
                    self.right_gutter
                } else {
                    dip(TEXT_BLOCK_INSET)
                },
            ..bounds
        };
        let flags = DT_SINGLELINE
            | DT_VCENTER
            | DT_END_ELLIPSIS
            | if centered { DT_CENTER } else { 0 }
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

    fn draw_swatch(&self, dc: HDC, bounds: RECT, color: COLORREF, border: COLORREF) {
        let dip = |value| px(self.dpi, value);
        let left = bounds.left + dip(8);
        let right = left + dip(16);
        let height = dip(12);
        let top = bounds.top + (bounds.bottom - bounds.top - height) / 2;
        let bottom = top + height;
        let inset = dip(1);
        unsafe {
            SelectObject(dc, GetStockObject(NULL_PEN));
            SelectObject(dc, GetStockObject(DC_BRUSH));
            SetDCBrushColor(dc, border);
            RoundRect(dc, left, top, right, bottom, dip(4), dip(4));
            SetDCBrushColor(dc, color);
            RoundRect(
                dc,
                left + inset,
                top + inset,
                right - inset,
                bottom - inset,
                dip(2),
                dip(2),
            );
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
    accent: COLORREF,
    border: COLORREF,
    text: COLORREF,
    disabled: COLORREF,
    icon: COLORREF,
}

impl Palette {
    fn new(theme: Theme, dpi: u32) -> Option<Self> {
        let MenuColors {
            background,
            selected,
            separator,
            border,
            accent,
            text,
            disabled,
            icon,
        } = theme.menu_colors();
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
            accent,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_uses_the_drawn_track_endpoints() {
        for dpi in [96, 144] {
            let style = Style::new(Theme::Dark, dpi).unwrap();
            let bounds = RECT {
                left: -240,
                right: -240 + px(dpi, SLIDER_WIDTH),
                ..Default::default()
            };
            let (left, right) = style.slider_track(bounds);
            assert_eq!(style.slider_value(bounds, left - 1), 0);
            assert_eq!(style.slider_value(bounds, left), 0);
            assert_eq!(style.slider_value(bounds, (left + right) / 2), 50);
            assert_eq!(style.slider_value(bounds, right), 100);
            assert_eq!(style.slider_value(bounds, right + 1), 100);
        }
    }
}
