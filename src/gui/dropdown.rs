use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMNCRP_DISABLED, DWMNCRP_USEWINDOWSTYLE, DWMWA_NCRENDERING_POLICY,
    DWMWA_TRANSITIONS_FORCEDISABLED, DwmFlush, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::{BackBuffer, OwnedWindow, area, client, height, monitor_info, scale, wide, width};
use super::{appearance, surface::Surface};

const TRIGGER_HEIGHT: i32 = 5;
const SLIDE_DURATION: Duration = Duration::from_millis(160);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const PREVIEW_CLASS: windows_sys::core::PCWSTR = windows_sys::w!("ArgosDropdownPreview");

#[derive(Clone)]
pub(super) struct Display {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) bounds: RECT,
    pub(super) work: RECT,
}

impl Display {
    pub(super) fn trigger(&self, alignment: u8, width_percent: u8) -> RECT {
        let screen_width = width(self.bounds);
        let percent = i32::from(width_percent.clamp(1, 100));
        let trigger_width = ((screen_width * percent + 50) / 100).max(1);
        let offset = ((screen_width - trigger_width) * i32::from(alignment.min(100)) + 50) / 100;
        let left = self.bounds.left + offset;
        area(
            left,
            self.bounds.top,
            left + trigger_width,
            (self.bounds.top + TRIGGER_HEIGHT).min(self.bounds.bottom),
        )
    }

    pub(super) fn panel(&self, desired_width: i32, desired_height: i32, trigger: RECT) -> RECT {
        let w = desired_width.clamp(1, width(self.work));
        let h = desired_height.clamp(1, height(self.work));
        let centered_left = trigger.left + (width(trigger) - w) / 2;
        let left = centered_left.clamp(self.work.left, self.work.right - w);
        area(left, self.work.top, left + w, self.work.top + h)
    }
}

pub(super) fn contains(rect: RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

pub(super) fn displays() -> Vec<Display> {
    let mut displays: Vec<(u32, Display)> = Vec::new();
    // SAFETY: a enumeração é síncrona e o vetor permanece válido até o retorno.
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(collect_display),
            (&mut displays as *mut Vec<(u32, Display)>) as LPARAM,
        );
    }
    displays.sort_by(|(a_num, a), (b_num, b)| a_num.cmp(b_num).then_with(|| a.id.cmp(&b.id)));
    displays.into_iter().map(|(_, display)| display).collect()
}

unsafe extern "system" fn collect_display(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) } == 0 {
        return 1;
    }
    let bounds = info.monitorInfo.rcMonitor;
    let work = info.monitorInfo.rcWork;
    // A enumeração também pode retornar superfícies de espelhamento sem área de trabalho.
    if width(bounds) <= 0
        || height(bounds) <= 0
        || width(work) <= 0
        || height(work) <= 0
        || unsafe { MonitorFromRect(&work, MONITOR_DEFAULTTONULL) } != monitor
    {
        return 1;
    }
    let source = display_text(&info.szDevice);
    let number = source
        .strip_prefix(r"\\.\DISPLAY")
        .and_then(|suffix| suffix.parse::<u32>().ok());
    let title = match number {
        Some(number) => format!("Monitor {number}"),
        None => format!("Monitor ({source})"),
    };
    let (id, name) = monitor_identity(&info.szDevice, source);
    let resolution = format!("{} × {}", width(bounds), height(bounds));
    let label = if name.is_empty() {
        format!("{title} · {resolution}")
    } else {
        format!("{title} · {name} · {resolution}")
    };
    let display = Display {
        id,
        label,
        bounds,
        work,
    };
    // SAFETY: o contexto aponta para o vetor vivo e exclusivo de displays().
    let displays = unsafe { &mut *(data as *mut Vec<(u32, Display)>) };
    displays.push((number.unwrap_or(u32::MAX), display));
    1
}

fn monitor_identity(gdi_name: &[u16], fallback_id: String) -> (String, String) {
    let mut name = String::new();
    for index in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if unsafe {
            EnumDisplayDevicesW(
                gdi_name.as_ptr(),
                index,
                &mut device,
                EDD_GET_DEVICE_INTERFACE_NAME,
            )
        } == 0
        {
            break;
        }
        let device_name = display_text(&device.DeviceString);
        if device.StateFlags & DISPLAY_DEVICE_ACTIVE != 0 {
            let id = display_text(&device.DeviceID);
            if !id.is_empty() {
                return (id, device_name);
            }
        }
        if name.is_empty() {
            name = device_name;
        }
    }
    (fallback_id, name)
}

fn display_text(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

pub(super) struct Preview {
    _marker: OwnedWindow,
    _label: Option<OwnedWindow>,
}

impl Preview {
    pub(super) fn show(rect: RECT, label: &str) -> Option<Self> {
        let marker = preview_window(rect, "")?;
        let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
        let label = monitor_info(monitor).and_then(|monitor| {
            let work = monitor.rcWork;
            let dpi = unsafe { GetDpiForWindow(marker.0) }.max(96);
            let w = scale(420, dpi).min(width(work));
            let h = scale(28, dpi);
            let left = (rect.left + (width(rect) - w) / 2).clamp(work.left, work.right - w);
            let top = rect.bottom.max(work.top) + scale(6, dpi);
            preview_window(area(left, top, left + w, (top + h).min(work.bottom)), label)
        });
        Some(Self {
            _marker: marker,
            _label: label,
        })
    }
}

fn preview_window(rect: RECT, label: &str) -> Option<OwnedWindow> {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    if width(rect) <= 0 || height(rect) <= 0 {
        return None;
    }
    let instance = unsafe { GetModuleHandleW(null()) };
    let registered = REGISTERED.get_or_init(|| unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(preview_proc),
            hInstance: instance,
            lpszClassName: PREVIEW_CLASS,
            ..Default::default()
        }) != 0
    });
    if !registered {
        return None;
    }
    let label = wide(label);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PREVIEW_CLASS,
            label.as_ptr(),
            WS_POPUP,
            rect.left,
            rect.top,
            width(rect),
            height(rect),
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if hwnd.is_null() {
        return None;
    }
    let preview = OwnedWindow(hwnd);
    let shown = unsafe {
        SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA) != 0
            && SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            ) != 0
    };
    shown.then_some(preview)
}

unsafe extern "system" fn preview_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != WM_PAINT {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let mut paint = PAINTSTRUCT::default();
    let mut bounds = client(hwnd);
    let mut label = [0_u16; 256];
    // SAFETY: os buffers permanecem válidos durante as chamadas síncronas de desenho.
    unsafe {
        let dc = BeginPaint(hwnd, &mut paint);
        if !dc.is_null() {
            FillRect(dc, &bounds, GetSysColorBrush(COLOR_HIGHLIGHT));
            let length = GetWindowTextW(hwnd, label.as_mut_ptr(), label.len() as i32);
            if length > 0 {
                let old_font = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(dc, GetSysColor(COLOR_HIGHLIGHTTEXT));
                bounds.left += 6;
                bounds.right -= 6;
                DrawTextW(
                    dc,
                    label.as_ptr(),
                    length,
                    &mut bounds,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
                if !old_font.is_null() {
                    SelectObject(dc, old_font);
                }
            }
        }
        EndPaint(hwnd, &paint);
    }
    0
}

pub(super) fn set_visible(hwnd: HWND, visible: bool) -> bool {
    if (unsafe { IsWindowVisible(hwnd) } != 0) == visible {
        return true;
    }
    let visibility = if visible {
        SWP_SHOWWINDOW
    } else {
        SWP_HIDEWINDOW
    };
    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | visibility,
        ) != 0
    }
}

pub(super) fn set_dropdown_effects(hwnd: HWND, dropdown: bool) {
    let disable_transitions = i32::from(dropdown);
    let policy = if dropdown {
        DWMNCRP_DISABLED
    } else {
        DWMNCRP_USEWINDOWSTYLE
    };
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED as u32,
            (&raw const disable_transitions).cast(),
            size_of::<i32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY as u32,
            (&raw const policy).cast(),
            size_of::<i32>() as u32,
        );
    }
}

pub(super) fn slide(
    hwnd: HWND,
    visible: bool,
    paint: impl FnOnce(HDC),
    surface: Option<Surface>,
) -> bool {
    if (unsafe { IsWindowVisible(hwnd) } != 0) == visible {
        return true;
    }
    let layered = appearance::is_layered(hwnd);
    let frame = if animations_enabled() && (!layered || surface.is_some()) {
        SlideFrame::capture(hwnd, paint, surface)
    } else {
        if visible && let Some(surface) = &surface {
            let bounds = client(hwnd);
            surface.present(
                hwnd,
                None,
                SIZE {
                    cx: width(bounds),
                    cy: height(bounds),
                },
                POINT::default(),
            );
        }
        None
    };
    if layered && let Some(frame) = frame {
        if frame.slide_translucent(hwnd, visible) {
            return true;
        }
        // A falha da animação pode ser recuperada pela apresentação direta.
        drop(frame);
        return show_panel(hwnd, visible, false);
    }
    let frame = frame.filter(|frame| frame.show(visible));
    if let Some(frame) = &frame {
        if !visible {
            // A imagem cobre o painel antes de ocultarmos a janela real.
            unsafe { DwmFlush() };
            if !set_visible(hwnd, false) {
                return false;
            }
        }
        frame.animate(visible);
    }

    show_panel(hwnd, visible, frame.is_some())
}

fn show_panel(hwnd: HWND, visible: bool, animated: bool) -> bool {
    if !set_visible(hwnd, visible) {
        return false;
    }
    if visible {
        // A última imagem continua cobrindo o painel até ele terminar de pintar.
        unsafe {
            RedrawWindow(
                hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
            );
            if animated {
                GdiFlush();
                DwmFlush();
            }
        }
    }
    true
}

struct SlideFrame {
    window: OwnedWindow,
    image: SlideImage,
    position: POINT,
    size: SIZE,
}

enum SlideImage {
    Opaque(BackBuffer),
    Translucent(Surface),
}

impl SlideFrame {
    fn capture(hwnd: HWND, paint: impl FnOnce(HDC), surface: Option<Surface>) -> Option<Self> {
        let mut rect = RECT::default();
        let bounds = client(hwnd);
        let size = SIZE {
            cx: width(bounds),
            cy: height(bounds),
        };
        if size.cx <= 0 || size.cy <= 0 || unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            return None;
        }
        let image = if let Some(surface) = surface {
            SlideImage::Translucent(surface)
        } else {
            let mut buffer = BackBuffer::default();
            let screen = unsafe { GetDC(null_mut()) };
            if screen.is_null() {
                return None;
            }
            let dc = buffer.target(screen, size.cx, size.cy);
            unsafe { ReleaseDC(null_mut(), screen) };
            if dc == screen {
                return None;
            }
            paint(dc);
            unsafe { GdiFlush() };
            SlideImage::Opaque(buffer)
        };
        let position = POINT {
            x: rect.left,
            y: rect.top,
        };
        let window = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                windows_sys::w!("STATIC"),
                null(),
                WS_POPUP,
                position.x,
                position.y,
                size.cx,
                size.cy,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        if window.is_null() {
            return None;
        }
        let window = OwnedWindow(window);
        set_dropdown_effects(window.0, true);
        Some(Self {
            window,
            image,
            position,
            size,
        })
    }

    fn slide_translucent(&self, panel: HWND, opening: bool) -> bool {
        if opening {
            if !self.show(true) {
                return false;
            }
        } else if !self.present(self.size.cy) || !swap_visibility(self.window.0, panel) {
            return false;
        }
        self.animate(opening);
        if !opening {
            return true;
        }
        let SlideImage::Translucent(surface) = &self.image else {
            return false;
        };
        // As duas imagens têm alfa. A troca em lote evita compor uma sobre a outra.
        surface.present(panel, None, self.size, POINT::default())
            && swap_visibility(panel, self.window.0)
    }

    fn show(&self, opening: bool) -> bool {
        let rows = if opening { 1 } else { self.size.cy };
        self.present(rows)
            && unsafe {
                SetWindowPos(
                    self.window.0,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                ) != 0
            }
    }

    fn animate(&self, opening: bool) {
        let start = Instant::now();
        let mut next_frame = FRAME_INTERVAL;
        loop {
            let delay = next_frame.saturating_sub(start.elapsed());
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let elapsed = start.elapsed();
            let progress = (elapsed.as_secs_f64() / SLIDE_DURATION.as_secs_f64()).min(1.0);
            let eased = progress * progress * (3.0 - 2.0 * progress);
            let fraction = if opening { eased } else { 1.0 - eased };
            let rows = (f64::from(self.size.cy) * fraction).round().max(1.0) as i32;
            if !self.present(rows) || elapsed >= SLIDE_DURATION {
                break;
            }
            next_frame = (next_frame + FRAME_INTERVAL).min(SLIDE_DURATION);
        }
    }

    fn present(&self, rows: i32) -> bool {
        let size = SIZE {
            cx: self.size.cx,
            cy: rows,
        };
        // Revele a parte inferior da imagem primeiro, com o topo da janela fixo.
        let source = POINT {
            x: 0,
            y: self.size.cy - rows,
        };
        let SlideImage::Opaque(buffer) = &self.image else {
            let SlideImage::Translucent(surface) = &self.image else {
                unreachable!()
            };
            return surface.present(self.window.0, Some(&self.position), size, source);
        };
        unsafe {
            UpdateLayeredWindow(
                self.window.0,
                null_mut(),
                &self.position,
                &size,
                buffer.dc,
                &source,
                0,
                null(),
                ULW_OPAQUE,
            ) != 0
        }
    }
}

fn swap_visibility(show: HWND, hide: HWND) -> bool {
    // Ambos são popups de nível superior no mesmo thread.
    unsafe {
        let batch = BeginDeferWindowPos(2);
        if batch.is_null() {
            return false;
        }
        let common = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        let batch = DeferWindowPos(
            batch,
            show,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            common | SWP_SHOWWINDOW,
        );
        if batch.is_null() {
            return false;
        }
        let batch = DeferWindowPos(
            batch,
            hide,
            null_mut(),
            0,
            0,
            0,
            0,
            common | SWP_NOZORDER | SWP_HIDEWINDOW,
        );
        !batch.is_null() && EndDeferWindowPos(batch) != 0
    }
}

fn animations_enabled() -> bool {
    let mut enabled = 0_i32;
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        ) != 0
            && enabled != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> Display {
        Display {
            id: String::new(),
            label: String::new(),
            bounds: area(-1920, -1080, 0, 0),
            work: area(-1920, -1040, 0, 0),
        }
    }

    fn assert_rect(rect: RECT, expected: [i32; 4]) {
        assert_eq!([rect.left, rect.top, rect.right, rect.bottom], expected);
    }

    #[test]
    fn trigger_positions_use_available_space_on_the_physical_top_edge() {
        let display = display();
        assert_rect(display.trigger(0, 25), [-1920, -1080, -1440, -1075]);
        assert_rect(display.trigger(25, 25), [-1560, -1080, -1080, -1075]);
        assert_rect(display.trigger(50, 25), [-1200, -1080, -720, -1075]);
        assert_rect(display.trigger(100, 25), [-480, -1080, 0, -1075]);
        assert_rect(display.trigger(25, 100), [-1920, -1080, 0, -1075]);
    }

    #[test]
    fn panel_preserves_size_when_it_fits_and_leaves_the_taskbar_clear() {
        let mut display = display();
        display.work.left += 180;
        let center = display.trigger(50, 30);
        assert_rect(display.panel(1000, 400, center), [-1460, -1040, -460, -640]);
        assert_rect(display.panel(1920, 1080, center), [-1740, -1040, 0, 0]);
        let right = display.trigger(100, 20);
        assert_rect(display.panel(300, 400, right), [-342, -1040, -42, -640]);
    }

    #[test]
    fn hit_test_excludes_the_right_and_bottom_edges() {
        let rect = display().trigger(100, 25);
        assert!(contains(rect, POINT { x: -480, y: -1080 }));
        assert!(!contains(rect, POINT { x: 0, y: -1080 }));
        assert!(!contains(rect, POINT { x: -1, y: -1075 }));
    }

    #[test]
    fn slide_frame_crops_the_bitmap_at_the_fixed_top_edge() {
        let bounds = area(24, 48, 104, 108);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                windows_sys::w!("STATIC"),
                null(),
                WS_POPUP,
                bounds.left,
                bounds.top,
                width(bounds),
                height(bounds),
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!hwnd.is_null());
        let panel = OwnedWindow(hwnd);
        let frame = SlideFrame::capture(
            panel.0,
            |dc| unsafe {
                FillRect(dc, &client(panel.0), GetStockObject(WHITE_BRUSH));
            },
            None,
        )
        .expect("quadro da animação");

        for rows in [1, 27, height(bounds)] {
            assert!(frame.present(rows));
            let mut cropped = RECT::default();
            assert_ne!(unsafe { GetWindowRect(frame.window.0, &mut cropped) }, 0);
            assert_rect(
                cropped,
                [bounds.left, bounds.top, bounds.right, bounds.top + rows],
            );
            assert_eq!(unsafe { IsWindowVisible(frame.window.0) }, 0);
        }
    }
}
