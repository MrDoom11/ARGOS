use std::ptr::{NonNull, null, null_mut};
use std::slice;

use windows_sys::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush,
    HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

const RGB_MASK: u32 = 0x00ff_ffff;

#[derive(Clone, Copy)]
pub(super) struct Background {
    pub(super) marker: COLORREF,
    pub(super) color: COLORREF,
    pub(super) opacity: u8,
}

impl Background {
    pub(super) fn new(
        color: COLORREF,
        opacity: u8,
        excluded: impl Fn(COLORREF) -> bool,
    ) -> Option<Self> {
        let color = color & RGB_MASK;
        // A small change reduces the fringe where GDI antialiases text into the marker.
        for delta in 1..=u8::MAX {
            for shift in [16, 8, 0] {
                let channel = (color >> shift) as u8;
                for nearby in [channel.checked_add(delta), channel.checked_sub(delta)]
                    .into_iter()
                    .flatten()
                {
                    let marker = (color & !(0xff << shift)) | (u32::from(nearby) << shift);
                    if !excluded(marker) {
                        return Some(Self {
                            marker,
                            color,
                            opacity,
                        });
                    }
                }
            }
        }
        (1..=4096)
            .map(|offset| (color + offset) & RGB_MASK)
            .find(|&marker| !excluded(marker))
            .map(|marker| Self {
                marker,
                color,
                opacity,
            })
    }
}

#[derive(Default)]
pub(super) struct Surface {
    dib: Option<Dib>,
}

impl Surface {
    pub(super) fn target(&mut self, reference: HDC, width: i32, height: i32) -> Option<HDC> {
        if let Some(dib) = &self.dib
            && dib.size == (width, height)
        {
            return Some(dib.dc);
        }
        let dib = Dib::new(reference, width, height)?;
        let dc = dib.dc;
        self.dib = Some(dib);
        Some(dc)
    }

    pub(super) fn finish<const N: usize>(
        &mut self,
        backgrounds: [Background; N],
        opaque: Option<RECT>,
    ) {
        if let Some(dib) = &mut self.dib {
            // GDI batches writes to DIB sections. Synchronize before accessing the pixel memory.
            unsafe { GdiFlush() };
            let pixels = unsafe { slice::from_raw_parts_mut(dib.bits.as_ptr(), dib.len) };
            finish_pixels(pixels, backgrounds, dib.size, opaque);
        }
    }

    pub(super) fn present(
        &self,
        hwnd: HWND,
        position: Option<&POINT>,
        size: SIZE,
        source: POINT,
    ) -> bool {
        let Some(dib) = &self.dib else {
            return false;
        };
        if size.cx <= 0
            || size.cy <= 0
            || source.x < 0
            || source.y < 0
            || size.cx > dib.size.0 - source.x
            || size.cy > dib.size.1 - source.y
        {
            return false;
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                null_mut(),
                position.map_or(null(), |point| point),
                &size,
                dib.dc,
                &source,
                0,
                &blend,
                ULW_ALPHA,
            ) != 0
        }
    }
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: NonNull<u32>,
    len: usize,
    size: (i32, i32),
}

impl Dib {
    fn new(reference: HDC, width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let len = (width as usize).checked_mul(height as usize)?;
        if len > isize::MAX as usize / size_of::<u32>() {
            return None;
        }
        let dc = unsafe { CreateCompatibleDC(reference) };
        if dc.is_null() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        let bitmap =
            unsafe { CreateDIBSection(reference, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        let Some(bits) = NonNull::new(bits.cast::<u32>()).filter(|_| !bitmap.is_null()) else {
            unsafe {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                DeleteDC(dc);
            }
            return None;
        };
        // Initialize the allocation before selecting it into a DC for drawing.
        unsafe { bits.as_ptr().write_bytes(0, len) };
        let original = unsafe { SelectObject(dc, bitmap) };
        if original.is_null() {
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
            return None;
        }
        Some(Self {
            dc,
            bitmap,
            original,
            bits,
            len,
            size: (width, height),
        })
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.original);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

fn finish_pixels<const N: usize>(
    pixels: &mut [u32],
    backgrounds: [Background; N],
    size: (i32, i32),
    opaque: Option<RECT>,
) {
    let backgrounds = backgrounds.map(|bg| {
        let alpha = u32::from(bg.opacity);
        let channel = |shift: u32| (((bg.color >> shift) & 0xff) * alpha + 127) / 255;
        // COLORREF packs red first, while the little-endian DIB stores blue first.
        let marker = ((bg.marker & 0xff) << 16) | (bg.marker & 0xff00) | ((bg.marker >> 16) & 0xff);
        let color = (alpha << 24) | (channel(0) << 16) | (channel(8) << 8) | channel(16);
        (marker, color)
    });
    let finish = |pixels: &mut [u32]| {
        for pixel in pixels {
            let rgb = *pixel & RGB_MASK;
            *pixel =
                backgrounds
                    .iter()
                    .rev()
                    .fold(0xff00_0000 | rgb, |result, &(marker, color)| {
                        if rgb == marker { color } else { result }
                    });
        }
    };
    let Some(rect) = opaque else {
        finish(pixels);
        return;
    };
    let [left, right] = [rect.left, rect.right].map(|x| x.clamp(0, size.0) as usize);
    let [top, bottom] = [rect.top, rect.bottom].map(|y| y.clamp(0, size.1) as usize);
    if left >= right || top >= bottom {
        finish(pixels);
        return;
    }
    let width = size.0 as usize;
    let (above, rest) = pixels.split_at_mut(top * width);
    let (middle, below) = rest.split_at_mut((bottom - top) * width);
    finish(above);
    finish(below);
    for row in middle.chunks_exact_mut(width) {
        let (left_pixels, rest) = row.split_at_mut(left);
        let (opaque_pixels, right_pixels) = rest.split_at_mut(right - left);
        finish(left_pixels);
        for pixel in opaque_pixels {
            *pixel |= 0xff00_0000;
        }
        finish(right_pixels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_nearby_markers_outside_the_foreground_palette() {
        let color = 0x0032_64c8;
        let excluded = |value| value == 0x0033_64c8;
        let first = Background::new(color, 128, excluded).unwrap();
        let second =
            Background::new(color, 128, |value| excluded(value) || value == first.marker).unwrap();
        assert_eq!(first.marker, 0x0031_64c8);
        assert_ne!(second.marker, color);
        assert_ne!(second.marker, first.marker);
        assert!(!excluded(second.marker));
    }

    #[test]
    fn premultiplies_background_and_preserves_foreground_matching_its_real_color() {
        let backgrounds = [
            Background::new(0x0032_64c8, 128, |_| false).unwrap(),
            Background::new(0x00e0_3210, 64, |_| false).unwrap(),
        ];
        let mut pixels = [0x99c8_6433, 0x0010_32e1, 0x12c8_6432, 0x0032_64c8];
        finish_pixels(&mut pixels, backgrounds, (2, 2), None);
        assert_eq!(pixels, [0x8064_3219, 0x4004_0d38, 0xffc8_6432, 0xff32_64c8]);
    }

    #[test]
    fn zero_opacity_clears_both_black_backgrounds_and_preserves_black_ink() {
        let first = Background::new(0, 0, |_| false).unwrap();
        let second = Background::new(0, 0, |color| color == first.marker).unwrap();
        let mut pixels = [1, 0x100, 0, 0x1234_5678];
        finish_pixels(&mut pixels, [first, second], (2, 2), None);
        assert_eq!(pixels, [0, 0, 0xff00_0000, 0xff34_5678]);
    }

    #[test]
    fn third_marker_fades_the_grid_independently_of_clear_black_and_opaque_ink() {
        let first = Background::new(0, 0, |_| false).unwrap();
        let second = Background::new(0, 0, |color| color == first.marker).unwrap();
        let grid = Background::new(0x00e6_e6e6, 64, |color| {
            color == first.marker || color == second.marker
        })
        .unwrap();
        let mut pixels = [1, 0x100, 0x00e6_e6e7, 0x00e6_e6e6, 0, 0x0048_d7b9];
        finish_pixels(&mut pixels, [first, second, grid], (3, 2), None);
        assert_eq!(
            pixels,
            [0, 0, 0x403a_3a3a, 0xffe6_e6e6, 0xff00_0000, 0xff48_d7b9]
        );
    }

    #[test]
    fn keeps_native_scrollbar_colors_opaque_even_if_they_match_a_marker() {
        let backgrounds = [
            Background::new(0, 100, |_| false).unwrap(),
            Background::new(0x0020_2020, 50, |_| false).unwrap(),
        ];
        let mut pixels = [1, 1, 1, 1];
        let opaque = RECT {
            left: 1,
            top: 0,
            right: 3,
            bottom: 1,
        };
        finish_pixels(&mut pixels, backgrounds, (2, 2), Some(opaque));
        assert_eq!(pixels, [0x6400_0000, 0xff00_0001, 0x6400_0000, 0x6400_0000]);
    }
}
