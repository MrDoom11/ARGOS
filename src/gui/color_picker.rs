use std::cell::Cell;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, DC_BRUSH, DC_PEN, DIB_RGB_COLORS, FillRect,
    FrameRect, GetStockObject, InvalidateRect, RestoreDC, RoundRect, SRCCOPY, SaveDC, SelectObject,
    SetDCBrushColor, SetDCPenColor, StretchDIBits, WHITE_BRUSH,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW,
};
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, EM_SETSEL, ODS_FOCUS};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use super::appearance::{MenuColors, Theme};
use super::chart::rgb;
use super::dialog::{DIALOG_USER, DialogStyle, Template};
use super::{area, client, height, message_point, scale, wide, width};

const HEX: i32 = 100;
const RED: i32 = 101;
const GREEN: i32 = 102;
const BLUE: i32 = 103;
const PREVIEW: i32 = 104;
const HELP: i32 = 105;
const SATURATION: i32 = 106;
const HUE: i32 = 107;
const SWATCHES: std::ops::Range<i32> = 110..126;
const SS_OWNERDRAW: u32 = 0x0d;
const SS_NOTIFY: u32 = 0x0100;

pub(super) fn choose(
    owner: HWND,
    current: COLORREF,
    custom_colors: &mut [COLORREF; 16],
    theme: Theme,
) -> Option<COLORREF> {
    let Some(style) = DialogStyle::new(theme) else {
        let mut dialog = CHOOSECOLORW {
            lStructSize: size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: owner,
            rgbResult: current,
            lpCustColors: custom_colors.as_mut_ptr(),
            Flags: CC_RGBINIT | CC_FULLOPEN,
            ..Default::default()
        };
        return (unsafe { ChooseColorW(&mut dialog) } != 0).then_some(dialog.rgbResult);
    };

    let mut template = Template::new("Cor do gráfico", [232, 206]);
    template.add_control(0x0082, u16::MAX, 0, [10, 8, 212, 10], "Paleta");
    for (index, (id, &color)) in SWATCHES.zip(custom_colors.iter()).enumerate() {
        let x = 10 + (index as u16 % 8) * 27;
        let y = 21 + (index as u16 / 8) * 17;
        template.add_control(
            0x0080,
            id as u16,
            WS_TABSTOP | BS_OWNERDRAW as u32,
            [x, y, 23, 14],
            &format_hex(color),
        );
    }
    for (id, bounds, label) in [
        (SATURATION, [10, 58, 192, 73], "Saturação e brilho"),
        (HUE, [208, 58, 14, 73], "Matiz"),
    ] {
        template.add_control(0x0082, id as u16, SS_OWNERDRAW | SS_NOTIFY, bounds, label);
    }
    for (id, x, width, label, numeric) in [
        (HEX, 10, 58, "&Hexadecimal", false),
        (RED, 76, 35, "&Vermelho", true),
        (GREEN, 116, 35, "V&erde", true),
        (BLUE, 156, 33, "A&zul", true),
    ] {
        template.add_control(0x0082, u16::MAX, 0, [x, 139, width, 10], label);
        template.add_control(
            0x0081,
            id as u16,
            WS_TABSTOP
                | WS_BORDER
                | ES_AUTOHSCROLL as u32
                | if numeric { ES_NUMBER as u32 } else { 0 },
            [x, 151, width, 14],
            "",
        );
    }
    template.add_control(
        0x0082,
        PREVIEW as u16,
        SS_OWNERDRAW,
        [198, 150, 24, 16],
        "Prévia da cor",
    );
    template.add_control(0x0082, HELP as u16, 0, [10, 171, 212, 12], "");
    for (id, x, label) in [(IDOK, 111, "Aplicar"), (IDCANCEL, 168, "Cancelar")] {
        template.add_control(
            0x0080,
            id as u16,
            WS_TABSTOP | BS_OWNERDRAW as u32,
            [x, 183, 54, 14],
            label,
        );
    }

    let state = Picker {
        palette: *custom_colors,
        selected: Cell::new(Some(current)),
        hsv: Cell::new(Hsv::from_color(current, 0.0)),
        updating: Cell::new(false),
        style,
        colors: theme.menu_colors(),
    };
    let result = template.show(owner, Some(dialog_proc), (&raw const state) as LPARAM);
    let color = u32::try_from(result).ok()?.checked_sub(1)?;
    if !custom_colors.contains(&color) {
        custom_colors.rotate_right(1);
        custom_colors[0] = color;
    }
    Some(color)
}

struct Picker {
    palette: [COLORREF; 16],
    selected: Cell<Option<COLORREF>>,
    hsv: Cell<Hsv>,
    updating: Cell<bool>,
    style: DialogStyle,
    colors: MenuColors,
}

impl Picker {
    fn select(&self, hwnd: HWND, color: Option<COLORREF>, source: i32) {
        self.selected.set(color);
        self.updating.set(true);
        unsafe {
            if let Some(color) = color {
                if !matches!(source, SATURATION | HUE) {
                    self.hsv.set(Hsv::from_color(color, self.hsv.get().hue));
                }
                if source != HEX {
                    SetDlgItemTextW(hwnd, HEX, wide(&format_hex(color)).as_ptr());
                }
                if !matches!(source, RED | GREEN | BLUE) {
                    let [red, green, blue, _] = color.to_le_bytes();
                    SetDlgItemInt(hwnd, RED, u32::from(red), 0);
                    SetDlgItemInt(hwnd, GREEN, u32::from(green), 0);
                    SetDlgItemInt(hwnd, BLUE, u32::from(blue), 0);
                }
                SetDlgItemTextW(hwnd, HELP, w!(""));
            }
            for id in [PREVIEW, SATURATION, HUE] {
                InvalidateRect(GetDlgItem(hwnd, id), std::ptr::null(), 0);
            }
        }
        self.updating.set(false);
    }

    fn select_spectrum(&self, control: HWND, id: i32, lp: LPARAM) {
        let bounds = client(control);
        let point = message_point(lp);
        let fraction =
            |offset, span: i32| (f64::from(offset) / f64::from((span - 1).max(1))).clamp(0.0, 1.0);
        let y = fraction(point.y, height(bounds));
        let mut hsv = self.hsv.get();
        if id == HUE {
            hsv.hue = y;
        } else {
            hsv.saturation = fraction(point.x, width(bounds));
            hsv.value = 1.0 - y;
        }
        self.hsv.set(hsv);
        self.select(unsafe { GetParent(control) }, Some(hsv.color()), id);
    }

    fn swatch(&self, id: i32) -> Option<COLORREF> {
        SWATCHES
            .contains(&id)
            .then(|| self.palette[(id - SWATCHES.start) as usize])
    }

    fn paint_color(&self, item: &DRAWITEMSTRUCT, color: Option<COLORREF>) {
        let dc = item.hDC;
        let bounds = item.rcItem;
        let border = if item.itemState & ODS_FOCUS != 0 {
            self.colors.accent
        } else {
            self.colors.border
        };
        unsafe {
            let saved = SaveDC(dc);
            if saved == 0 {
                return;
            }
            let brush = GetStockObject(DC_BRUSH);
            SelectObject(dc, brush);
            SelectObject(dc, GetStockObject(DC_PEN));
            SetDCBrushColor(dc, self.colors.background);
            FillRect(dc, &bounds, brush);
            SetDCBrushColor(dc, color.unwrap_or(self.colors.background));
            SetDCPenColor(dc, border);
            let corner = scale(5, GetDpiForWindow(item.hwndItem).max(96));
            RoundRect(
                dc,
                bounds.left,
                bounds.top,
                bounds.right,
                bounds.bottom,
                corner,
                corner,
            );
            RestoreDC(dc, saved);
        }
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    unsafe {
        if msg == WM_INITDIALOG {
            let state = &*(lp as *const Picker);
            SetWindowLongPtrW(hwnd, DIALOG_USER, lp);
            let buttons: Vec<_> = [IDOK, IDCANCEL].into_iter().chain(SWATCHES).collect();
            state.style.apply(hwnd, IDOK, &buttons);
            for id in [SATURATION, HUE] {
                SetWindowSubclass(GetDlgItem(hwnd, id), Some(spectrum_proc), 1, lp as usize);
            }
            state.select(hwnd, state.selected.get(), 0);
            let input = GetDlgItem(hwnd, HEX);
            SetFocus(input);
            SendMessageW(input, EM_SETSEL, 0, -1);
            return 0;
        }
        let Some(state) = (GetWindowLongPtrW(hwnd, DIALOG_USER) as *const Picker).as_ref() else {
            return 0;
        };
        match msg {
            WM_COMMAND => {
                let id = i32::from(wp as u16);
                let notification = (wp >> 16) as u16;
                match id {
                    IDOK => {
                        if let Some(color) = state.selected.get() {
                            EndDialog(hwnd, color as isize + 1);
                        } else {
                            SetDlgItemTextW(
                                hwnd,
                                HELP,
                                w!("Use #RRGGBB ou valores RGB de 0 a 255."),
                            );
                        }
                    }
                    IDCANCEL => {
                        EndDialog(hwnd, 0);
                    }
                    HEX | RED | GREEN | BLUE if notification == EN_CHANGE as u16 => {
                        if !state.updating.get() {
                            let color = if id == HEX {
                                parse_hex(&input_text(hwnd, HEX))
                            } else {
                                read_channels(hwnd)
                            };
                            state.select(hwnd, color, id);
                        }
                    }
                    id if notification == BN_CLICKED as u16 && SWATCHES.contains(&id) => {
                        state.select(hwnd, state.swatch(id), id);
                    }
                    _ => return 0,
                }
                1
            }
            WM_CLOSE => {
                EndDialog(hwnd, 0);
                1
            }
            WM_DRAWITEM => {
                if let Some(item) = (lp as *const DRAWITEMSTRUCT).as_ref() {
                    let id = item.CtlID as i32;
                    if matches!(id, SATURATION | HUE) {
                        paint_spectrum(item, id == HUE, state.hsv.get());
                        return 1;
                    }
                    if id == PREVIEW || SWATCHES.contains(&id) {
                        let color = if id == PREVIEW {
                            state.selected.get()
                        } else {
                            state.swatch(id)
                        };
                        state.paint_color(item, color);
                        return 1;
                    }
                }
                state.style.paint(msg, wp, lp, Some(HELP))
            }
            _ => state.style.paint(msg, wp, lp, Some(HELP)),
        }
    }
}

unsafe extern "system" fn spectrum_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    subclass: usize,
    state: usize,
) -> isize {
    unsafe {
        let state = &*(state as *const Picker);
        match msg {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                SetCapture(hwnd);
                state.select_spectrum(hwnd, GetDlgCtrlID(hwnd), lp);
                return 0;
            }
            WM_MOUSEMOVE | WM_LBUTTONUP if GetCapture() == hwnd => {
                state.select_spectrum(hwnd, GetDlgCtrlID(hwnd), lp);
                if msg == WM_LBUTTONUP {
                    ReleaseCapture();
                }
                return 0;
            }
            WM_SETCURSOR => {
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_CROSS));
                return 1;
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(spectrum_proc), subclass);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

fn paint_spectrum(item: &DRAWITEMSTRUCT, hue_strip: bool, hsv: Hsv) {
    let bounds = item.rcItem;
    let [columns, rows] = if hue_strip { [1, 128] } else { [128, 80] };
    let mut pixels = Vec::with_capacity((columns * rows) as usize);
    let full_color = Hsv {
        saturation: 1.0,
        value: 1.0,
        ..hsv
    }
    .color();
    let [red, green, blue, _] = full_color.to_le_bytes();
    for row in 0..rows {
        let y = f64::from(row) / f64::from(rows - 1);
        if hue_strip {
            let sample = Hsv {
                hue: y,
                saturation: 1.0,
                value: 1.0,
            }
            .color();
            let [red, green, blue, _] = sample.to_le_bytes();
            pixels.push([blue, green, red, 0]);
            continue;
        }
        for column in 0..columns {
            let saturation = f64::from(column) / f64::from(columns - 1);
            let tint = |channel: u8| {
                ((1.0 - y) * (255.0 + saturation * (f64::from(channel) - 255.0))).round() as u8
            };
            pixels.push([tint(blue), tint(green), tint(red), 0]);
        }
    }
    let bitmap = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: columns,
            biHeight: -rows,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let dc = item.hDC;
    let offset = |fraction: f64, span: i32| (fraction * f64::from(span - 1)).round() as i32;
    let y = bounds.top
        + offset(
            if hue_strip { hsv.hue } else { 1.0 - hsv.value },
            height(bounds),
        );
    let x = bounds.left + offset(hsv.saturation, width(bounds));
    let radius = scale(4, unsafe { GetDpiForWindow(item.hwndItem) }.max(96)).max(2);
    let (left, right) = if hue_strip {
        (bounds.left, bounds.right)
    } else {
        (x - radius, x + radius + 1)
    };
    unsafe {
        StretchDIBits(
            dc,
            bounds.left,
            bounds.top,
            width(bounds),
            height(bounds),
            0,
            0,
            columns,
            rows,
            pixels.as_ptr().cast(),
            &bitmap,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        FrameRect(
            dc,
            &area(left, y - radius, right, y + radius + 1),
            GetStockObject(BLACK_BRUSH),
        );
        FrameRect(
            dc,
            &area(left + 1, y - radius + 1, right - 1, y + radius),
            GetStockObject(WHITE_BRUSH),
        );
    }
}

#[derive(Clone, Copy)]
struct Hsv {
    hue: f64,
    saturation: f64,
    value: f64,
}

impl Hsv {
    fn from_color(color: COLORREF, previous_hue: f64) -> Self {
        let [red, green, blue, _] = color
            .to_le_bytes()
            .map(|channel| f64::from(channel) / 255.0);
        let value = red.max(green).max(blue);
        let range = value - red.min(green).min(blue);
        let hue = if range == 0.0 {
            previous_hue
        } else if value == red {
            ((green - blue) / range).rem_euclid(6.0) / 6.0
        } else if value == green {
            ((blue - red) / range + 2.0) / 6.0
        } else {
            ((red - green) / range + 4.0) / 6.0
        };
        Self {
            hue,
            saturation: if value == 0.0 { 0.0 } else { range / value },
            value,
        }
    }

    fn color(self) -> COLORREF {
        let channel = |offset: f64| {
            let angle = (offset + self.hue * 6.0).rem_euclid(6.0);
            let intensity =
                self.value * (1.0 - self.saturation * angle.min(4.0 - angle).clamp(0.0, 1.0));
            (intensity * 255.0).round() as u8
        };
        rgb(channel(5.0), channel(3.0), channel(1.0))
    }
}

fn input_text(hwnd: HWND, id: i32) -> String {
    let mut text = [0; 16];
    let length = unsafe { GetDlgItemTextW(hwnd, id, text.as_mut_ptr(), text.len() as i32) };
    if length as usize >= text.len() - 1 {
        return String::new();
    }
    String::from_utf16_lossy(&text[..length as usize])
}

fn read_channels(hwnd: HWND) -> Option<COLORREF> {
    let channel = |id| {
        let text = input_text(hwnd, id);
        if text.len() > 3 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    Some(rgb(channel(RED)?, channel(GREEN)?, channel(BLUE)?))
}

fn parse_hex(text: &str) -> Option<COLORREF> {
    let text = text.trim();
    let hex = text.strip_prefix('#').unwrap_or(text);
    let mut channels = [0; 3];
    hex::decode_to_slice(hex, &mut channels).ok()?;
    let [red, green, blue] = channels;
    Some(rgb(red, green, blue))
}

fn format_hex(color: COLORREF) -> String {
    let [red, green, blue, _] = color.to_le_bytes();
    format!("#{red:02X}{green:02X}{blue:02X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_hex_in_red_green_blue_order_and_rejects_partial_input() {
        let color = rgb(0x24, 0x99, 0xf1);
        assert_eq!(format_hex(color), "#2499F1");
        assert_eq!(parse_hex(" #2499f1 "), Some(color));
        assert_eq!(parse_hex("2499f1"), Some(color));
        assert_eq!(parse_hex("#000000"), Some(0));
        for text in ["#", "#2499", "#2499f10", "0x2499f1", "#2499g1"] {
            assert_eq!(parse_hex(text), None);
        }
    }

    #[test]
    fn spectrum_preserves_channel_order_and_retains_hue_on_grayscale() {
        for color in [
            rgb(255, 0, 0),
            rgb(0, 255, 0),
            rgb(0, 0, 255),
            rgb(36, 153, 241),
            0,
        ] {
            assert_eq!(Hsv::from_color(color, 0.0).color(), color);
        }
        let hsv = Hsv::from_color(rgb(127, 127, 127), 0.5);
        assert_eq!(hsv.hue, 0.5);
        assert_eq!(
            Hsv {
                saturation: 1.0,
                value: 1.0,
                ..hsv
            }
            .color(),
            rgb(0, 255, 255)
        );
    }
}
