use std::cell::RefCell;
use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::SetScrollInfo;
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, IsWindowEnabled, ReleaseCapture, SetCapture, VK_END, VK_ESCAPE, VK_F2, VK_F3,
    VK_F4, VK_HOME, VK_NEXT, VK_PRIOR,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use crate::cli::GpuIndex;
use crate::frames::{FrameTarget, Process, SENSOR_COUNT, visible_processes};
use crate::gpu::Selection;
use crate::instance::{self, WINDOW_CLASS, WINDOW_TITLE};
use crate::update::{self, CheckNotice, Source, Status, Updater};
use crate::watch::{
    CPU_POWER_INDEX, CPU_TEMPERATURE_INDEX, CPU_USAGE_INDEX, CUSTOM_PING_INDEX, FRAME_SENSOR_START,
    GAME_LATENCY_INDEX, GUI_INTERVALS_MS, Monitor, Snapshot,
};

mod appearance;
mod autostart;
mod board;
mod chart;
mod color_picker;
mod dialog;
mod dropdown;
mod dropdown_mode;
mod fps_scale;
mod hotkey;
mod input_strip;
mod menu;
mod menu_style;
mod ping_targets;
mod scrollbar;
mod startup;
mod surface;
mod tray;

use appearance::{Palette, Theme};
use board::{Board, MAX_COLUMNS, MAX_ROWS, MAX_SLOTS};
use menu::{Icon, Menu};
use surface::{Background, Surface};

const ARGOS_ICON: *const u16 = std::ptr::without_provenance(1);
const TIMER: usize = 1;
const TRAY_RETRY_TIMER: usize = 3;
const PERIOD_FIRST: usize = 201;
const RESET_SENSORS: usize = 205;
const COLUMNS_FIRST: usize = 301;
const ROWS_FIRST: usize = 311;
const SPLIT_PANEL: usize = 321;
const RESET_COLORS: usize = 322;
const MOVE_WINDOW: usize = 331;
const MINIMIZE_WINDOW: usize = 333;
const MAXIMIZE_WINDOW: usize = 334;
const CLOSE_WINDOW: usize = 335;
const TRAY_SHOW: usize = 336;
const ABOUT_RELEASES: usize = 337;
const ABOUT_GUIDE: usize = 338;
const ABOUT_TWITCH: usize = 339;
const START_WITH_WINDOWS: usize = 340;
const ABOUT_GUIDE_URL: &str = "https://github.com/MrDoom11/ARGOS#janela-de-monitoramento";
const ABOUT_TWITCH_URL: &str = "https://www.twitch.tv/mr_doom11";
const UPDATE_CHECK: usize = 341;
const UPDATE_RESTART: usize = 342;
const UPDATE_SCHEDULE: usize = 343;
const UPDATE_CANCEL: usize = 344;
const UPDATE_ERROR: usize = 345;
const UPDATE_CHECK_DETAILS: usize = 346;
const EDIT_PING_TARGETS: usize = 347;
const UPDATE_NOTICE_DURATION: Duration = Duration::from_secs(8);
const DROPDOWN_TOGGLE: usize = 350;
const DROPDOWN_PREVIEW: usize = 351;
const DROPDOWN_PIN: usize = 352;
const DROPDOWN_HOTKEY: usize = 353;
const DROPDOWN_CLEAR_HOTKEY: usize = 354;
const DROPDOWN_ALIGNMENT_FIRST: usize = 360;
const DROPDOWN_WIDTH_FIRST: usize = 370;
const THEME_DARK: usize = 380;
const THEME_LIGHT: usize = 381;
const FPS_CEILING_EDIT: usize = 382;
const DROPDOWN_ALIGNMENTS: [(u8, &str); 5] = [
    (0, "Esquerda"),
    (25, "Entre esquerda e centro"),
    (50, "Centro"),
    (75, "Entre centro e direita"),
    (100, "Direita"),
];
const DROPDOWN_WIDTHS: [u8; 6] = [10, 20, 30, 50, 75, 100];
const REFRESH_FIRST: usize = 401;
const PAGE_FIRST: usize = 501;
const DROPDOWN_MONITOR_FIRST: usize = 600;
const DROPDOWN_MONITOR_LIMIT: usize = 64;
const GPU_AUTO: usize = 700;
const GPU_FIRST: usize = 701;
const GPU_LIMIT: usize = 64;
const OPACITY_SLIDER: usize = 800;
const OPACITY_FIRST: usize = OPACITY_SLIDER + 1;
const SENSOR_FIRST: usize = 1000;
const MERGE_FIRST: usize = 10000;
const COLOR_FIRST: usize = 10100;
const MOVE_PANEL_FIRST: usize = 20000;
const FRAME_TARGET_FOREGROUND: usize = 20100;
const FRAME_TARGET_OFF: usize = 20101;
const FRAME_TARGET_FIRST: usize = 20102;
const FRAME_SENSORS: std::ops::Range<usize> = FRAME_SENSOR_START..FRAME_SENSOR_START + SENSOR_COUNT;
const PERIODS: [(u64, &str); 4] = [(30, "30 s"), (60, "1 min"), (120, "2 min"), (300, "5 min")];
const SERIES: [COLORREF; 16] = [
    chart::rgb(72, 215, 185),
    chart::rgb(255, 182, 103),
    chart::rgb(108, 181, 254),
    chart::rgb(185, 157, 255),
    chart::rgb(240, 137, 198),
    chart::rgb(148, 213, 127),
    chart::rgb(247, 220, 104),
    chart::rgb(251, 124, 108),
    chart::rgb(78, 205, 239),
    chart::rgb(154, 104, 243),
    chart::rgb(190, 233, 95),
    chart::rgb(245, 232, 198),
    chart::rgb(135, 163, 238),
    chart::rgb(245, 116, 252),
    chart::rgb(38, 179, 104),
    chart::rgb(255, 162, 178),
];

pub(crate) fn run(gpu: Option<GpuIndex>, source: Source) -> Result<(), String> {
    if gpu.is_none() && update::launch_cached(source) {
        return Ok(());
    }
    let Some(mut panel_instance) = instance::enter()? else {
        return Ok(());
    };
    let saved_board = Board::load();
    let restored = saved_board.is_some();
    let startup_notice = startup::prepare(panel_instance.previous_version(), restored);
    let board = saved_board.unwrap_or_else(|| Board::new(board::INITIAL_SENSORS));
    let saved_bounds = board.window_bounds;
    let refresh_ms = board.refresh_ms;
    let selections = gpu.map_or_else(
        || {
            board
                .gpu_ids
                .iter()
                .cloned()
                .map(Selection::Device)
                .collect()
        },
        |index| vec![Selection::Index(index)],
    );
    let monitor = Monitor::start(selections, refresh_ms, board.cpu_metrics())?;
    monitor.set_game_latency_enabled(board.contains(GAME_LATENCY_INDEX));
    monitor.set_custom_ping_targets(board.active_ping_targets());
    let _dpi_context = DpiContext(unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    });
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let state = Box::new(RefCell::new(Window::new(monitor, board, dpi, source)));
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: unsafe { LoadIconW(instance, ARGOS_ICON) },
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(window_error("registrar a janela"));
    }

    let work_area = preferred_work_area();
    let start = initial_bounds(work_area, dpi);
    // SAFETY: o estado em Box permanece no mesmo endereço até a destruição de hwnd.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            WINDOW_CLASS,
            WINDOW_TITLE,
            WS_POPUP
                | WS_THICKFRAME
                | WS_SYSMENU
                | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX
                | WS_CLIPCHILDREN,
            start.left,
            start.top,
            width(start),
            height(start),
            null_mut(),
            null_mut(),
            instance,
            (&*state as *const RefCell<Window>).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(window_error("criar a janela"));
    }
    let hwnd_guard = OwnedWindow(hwnd);
    state.borrow_mut().tray = Some(tray::Tray::new(hwnd, class.hIcon));
    refresh_tray(hwnd, &state);
    let placed = saved_bounds.is_some_and(|bounds| restore_window_bounds(hwnd, bounds));
    let window_dpi = unsafe { GetDpiForWindow(hwnd) };
    if window_dpi != 0 && window_dpi != dpi {
        state.borrow_mut().set_dpi(window_dpi);
        if !placed {
            position_window(hwnd, initial_bounds(work_area, window_dpi));
        }
    }
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            (&DWMWA_COLOR_NONE as *const u32).cast(),
            size_of::<u32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&DWMWCP_DONOTROUND as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        if SetTimer(hwnd, TIMER, refresh_ms, None) == 0 {
            return Err(window_error("agendar a atualização da janela"));
        }
    }
    sync_appearance(hwnd, &state);
    if startup_notice.is_none() {
        dropdown_mode::sync(hwnd, &state);
    }
    if !state.borrow().dropdown.active() {
        show_normal(hwnd);
    }
    panel_instance.ready();
    let _ = state.borrow_mut().updater.check(false);
    if let Some(notice) = startup_notice {
        let theme = state.borrow().board.appearance.theme;
        notice.show(hwnd, theme);
        dropdown_mode::sync(hwnd, &state);
    }

    let mut message = MSG::default();
    loop {
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(window_error("processar mensagens da janela")),
            0 => break,
            _ => {}
        }
        if message.message == WM_KEYDOWN
            && unsafe { GetAncestor(message.hwnd, GA_ROOT) } == hwnd
            && keyboard(hwnd, &state, message.wParam as u16)
        {
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    drop(hwnd_guard);
    Ok(())
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: os ponteiros das mensagens e o estado são válidos durante este despacho.
    unsafe {
        if msg == WM_NCCALCSIZE {
            // Toda a área da janela é cliente, mesmo com os estilos nativos de tamanho.
            return 0;
        }
        if msg == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Window>;
        let Some(state) = ptr.as_ref() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        if msg >= 0xc000
            && state.try_borrow().is_ok_and(|app| {
                app.tray
                    .as_ref()
                    .is_some_and(|tray| tray.is_taskbar_created(msg))
            })
        {
            refresh_tray(hwnd, state);
            return 0;
        }
        match msg {
            WM_CREATE => {
                let scrollbar = scrollbar::create(hwnd);
                if scrollbar.is_null() {
                    return -1;
                }
                let overlay = scrollbar::Overlay::new(hwnd);
                let mut app = state.borrow_mut();
                app.scrollbar = scrollbar;
                app.scrollbar_overlay = overlay;
                0
            }
            WM_NCHITTEST => {
                let mut bounds = RECT::default();
                let (dpi, dropdown) = state
                    .try_borrow()
                    .map_or((96, false), |app| (app.dpi, app.dropdown.active()));
                if dropdown || IsZoomed(hwnd) != 0 || GetWindowRect(hwnd, &mut bounds) == 0 {
                    HTCLIENT as LRESULT
                } else {
                    resize_hit(bounds, message_point(lp), scale(5, dpi)) as LRESULT
                }
            }
            WM_MOUSEACTIVATE if dropdown_active(state) => MA_NOACTIVATE as LRESULT,
            WM_SETCURSOR
                if u32::from(lp as u16) == HTCLIENT
                    && state.try_borrow().is_ok_and(|app| app.move_armed) =>
            {
                SetCursor(LoadCursorW(null_mut(), IDC_SIZEALL));
                1
            }
            WM_GETMINMAXINFO => {
                let (dpi, dropdown) = state
                    .try_borrow()
                    .map_or((96, false), |app| (app.dpi, app.dropdown.active()));
                let info = &mut *(lp as *mut MINMAXINFO);
                info.ptMinTrackSize = if dropdown {
                    POINT { x: 1, y: 1 }
                } else {
                    POINT {
                        x: scale(510, dpi),
                        y: scale(200, dpi),
                    }
                };
                if let Some(monitor) = current_monitor(hwnd) {
                    info.ptMaxPosition = POINT {
                        x: monitor.rcWork.left - monitor.rcMonitor.left,
                        y: monitor.rcWork.top - monitor.rcMonitor.top,
                    };
                    info.ptMaxSize = POINT {
                        x: width(monitor.rcWork),
                        y: height(monitor.rcWork),
                    };
                }
                0
            }
            WM_DPICHANGED => {
                cancel_drag(hwnd, state);
                if let Ok(mut app) = state.try_borrow_mut() {
                    app.set_dpi((wp & 0xffff) as u32);
                }
                if dropdown_active(state) {
                    PostMessageW(hwnd, dropdown_mode::RECONFIGURE, 0, 0);
                    invalidate(hwnd);
                    return 0;
                }
                if IsWindowVisible(hwnd) == 0 {
                    // Durante a abertura, a posição já vem da configuração ou do monitor inicial.
                    return 0;
                }
                let suggested = *(lp as *const RECT);
                let bounds = if IsZoomed(hwnd) != 0 {
                    current_monitor(hwnd).map_or(suggested, |monitor| monitor.rcWork)
                } else {
                    suggested
                };
                position_window(hwnd, bounds);
                invalidate(hwnd);
                0
            }
            WM_PAINT => {
                let drawable = IsIconic(hwnd) == 0 && IsWindowVisible(hwnd) != 0;
                if drawable {
                    sync_scrollbar(hwnd, state);
                }
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let mut reset_transparency = false;
                if !dc.is_null()
                    && drawable
                    && let Ok(mut app) = state.try_borrow_mut()
                {
                    if !appearance::is_layered(hwnd) {
                        app.paint_opaque(dc, client(hwnd));
                    } else if !app.present_layered(hwnd, dc, client(hwnd)) {
                        appearance::set_layered(hwnd, false);
                        app.paint_opaque(dc, client(hwnd));
                        reset_transparency = true;
                    }
                }
                EndPaint(hwnd, &paint);
                if reset_transparency {
                    sync_input_strip(hwnd, state);
                    sync_scrollbar(hwnd, state);
                }
                0
            }
            WM_ERASEBKGND => 1,
            WM_WINDOWPOSCHANGED => {
                let result = DefWindowProcW(hwnd, msg, wp, lp);
                sync_input_strip(hwnd, state);
                sync_scrollbar(hwnd, state);
                result
            }
            WM_ENABLE => {
                sync_input_strip(hwnd, state);
                sync_scrollbar_overlay(hwnd, state);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_SIZE => {
                cancel_drag(hwnd, state);
                if wp as u32 != SIZE_MINIMIZED {
                    invalidate(hwnd);
                }
                0
            }
            WM_TIMER if wp == TIMER => {
                if let Ok(mut app) = state.try_borrow_mut() {
                    if let Some(result) = app.updater.poll() {
                        app.update_notice = Some(UpdateNotice::new(result));
                    }
                    let drawable = IsIconic(hwnd) == 0 && IsWindowVisible(hwnd) != 0;
                    let can_show = drawable
                        && IsWindowEnabled(hwnd) != 0
                        && app.open_menu.is_none()
                        && GetCapture().is_null();
                    let now = Instant::now();
                    let notice_changed = app
                        .update_notice
                        .as_mut()
                        .is_some_and(|notice| notice.tick(now, can_show));
                    if drawable && (app.refresh() || app.snapshot_is_stale(now) || notice_changed) {
                        drop(app);
                        invalidate(hwnd);
                    }
                }
                0
            }
            WM_TIMER if wp == dropdown_mode::TIMER => {
                dropdown_mode::tick(hwnd, state);
                0
            }
            WM_TIMER if wp == TRAY_RETRY_TIMER => {
                refresh_tray(hwnd, state);
                0
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                if state
                    .try_borrow()
                    .is_ok_and(|app| app.board.dropdown.enabled)
                {
                    PostMessageW(hwnd, dropdown_mode::RECONFIGURE, 0, 0);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            dropdown_mode::RECONFIGURE => {
                dropdown_mode::sync(hwnd, state);
                0
            }
            instance::REVEAL_PANEL => {
                reveal_panel(hwnd, state);
                0
            }
            tray::CALLBACK => {
                handle_tray_event(hwnd, state, wp, lp);
                0
            }
            WM_HOTKEY => {
                dropdown_mode::hotkey_message(hwnd, state, wp, lp);
                0
            }
            WM_SYSCOMMAND if dropdown_active(state) => match wp as u32 & 0xfff0 {
                SC_MINIMIZE => {
                    dropdown_mode::reveal(hwnd, state, true);
                    0
                }
                SC_MOVE | SC_SIZE | SC_MAXIMIZE => 0,
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            },
            WM_SYSCOMMAND
                if wp as u32 & 0xfff0 == SC_MINIMIZE
                    && state.try_borrow().is_ok_and(|app| app.has_tray()) =>
            {
                ShowWindow(hwnd, SW_HIDE);
                0
            }
            WM_CLOSE => {
                if let Ok(mut app) = state.try_borrow_mut() {
                    if !app.dropdown.active()
                        && let Some(placement) = window_placement(hwnd)
                    {
                        let RECT {
                            left,
                            top,
                            right,
                            bottom,
                        } = placement.rcNormalPosition;
                        let bounds = Some([left, top, right, bottom]);
                        if app.board.window_bounds != bounds {
                            app.board.window_bounds = bounds;
                            app.save_failed = true;
                        }
                    }
                    if app.save_failed {
                        let _ = app.board.save();
                    }
                    if app.dropdown.save_failed {
                        let _ = app.board.save_dropdown();
                    }
                }
                if wp != instance::REPLACEMENT_CLOSE
                    && let Ok(mut app) = state.try_borrow_mut()
                    && let Err(error) = app.updater.apply(false)
                {
                    update::record_error(&error);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_MOUSEWHEEL => {
                navigate(hwnd, state, ScrollMove::Wheel((wp >> 16) as i16));
                0
            }
            WM_VSCROLL => {
                let movement = match i32::from(wp as u16) {
                    SB_LINEUP => ScrollMove::Rows(-1),
                    SB_LINEDOWN => ScrollMove::Rows(1),
                    SB_PAGEUP => ScrollMove::PreviousPage,
                    SB_PAGEDOWN => ScrollMove::NextPage,
                    SB_TOP => ScrollMove::First,
                    SB_BOTTOM => ScrollMove::Last,
                    SB_THUMBPOSITION | SB_THUMBTRACK => ScrollMove::ToRow(wp >> 16),
                    _ => return 0,
                };
                navigate(hwnd, state, movement);
                0
            }
            WM_LBUTTONDOWN => {
                let point = message_point(lp);
                if cancel_window_move(state) {
                    if !dropdown_active(state) {
                        begin_window_move(hwnd, point);
                    }
                } else {
                    start_drag(hwnd, state, point);
                }
                0
            }
            WM_MOUSEMOVE => {
                update_drag(hwnd, state, message_point(lp));
                0
            }
            WM_LBUTTONUP => {
                let mut point = message_point(lp);
                let clicked_menu = state.try_borrow().is_ok_and(|app| {
                    !app.move_armed
                        && app.drag.is_none()
                        && appearance::is_click_through(hwnd)
                        && (0..input_strip::HEIGHT).contains(&point.y)
                        && (0..width(client(hwnd))).contains(&point.x)
                });
                if clicked_menu && ClientToScreen(hwnd, &mut point) != 0 {
                    show_options(hwnd, state, point, MenuSection::All);
                } else {
                    finish_drag(hwnd, state, message_point(lp));
                }
                0
            }
            WM_CAPTURECHANGED => {
                clear_drag(hwnd, state);
                0
            }
            WM_ACTIVATE if u32::from(wp as u16) == WA_INACTIVE => {
                cancel_window_move(state);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_CANCELMODE => {
                cancel_window_move(state);
                cancel_drag(hwnd, state);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_INITMENUPOPUP | WM_UNINITMENUPOPUP | WM_MENUSELECT => {
                menu::notify(hwnd, msg, wp, lp);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_MEASUREITEM => {
                if let Some(item) = (lp as *mut MEASUREITEMSTRUCT).as_mut()
                    && menu::measure(hwnd, item)
                {
                    1
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_DRAWITEM => {
                if let Some(item) = (lp as *const DRAWITEMSTRUCT).as_ref()
                    && menu::draw(hwnd, item)
                {
                    1
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_MENUCHAR => menu::select_character(hwnd, lp as HMENU, wp as u16)
                .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp)),
            menu::APPLY => {
                apply_open_option(hwnd, state, wp);
                0
            }
            menu::DISMISS => {
                menu::dismiss(hwnd, wp);
                0
            }
            menu::FRAME => {
                menu::frame(hwnd, wp);
                0
            }
            WM_CONTEXTMENU | WM_NCRBUTTONUP => {
                cancel_window_move(state);
                cancel_drag(hwnd, state);
                // O clique na borda chega como mensagem não cliente, com coordenadas de tela.
                let point = if lp == -1 {
                    menu_center(hwnd)
                } else {
                    message_point(lp)
                };
                show_options(hwnd, state, point, MenuSection::All);
                0
            }
            WM_DESTROY => {
                cancel_drag(hwnd, state);
                KillTimer(hwnd, TIMER);
                KillTimer(hwnd, dropdown_mode::TIMER);
                KillTimer(hwnd, TRAY_RETRY_TIMER);
                let tray = state.borrow_mut().tray.take();
                drop(tray);
                dropdown_mode::unbind(hwnd, state);
                PostQuitMessage(0);
                0
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn resize_hit(bounds: RECT, point: POINT, grip: i32) -> u32 {
    let left = point.x < bounds.left + grip;
    let right = point.x >= bounds.right - grip;
    let top = point.y < bounds.top + grip;
    let bottom = point.y >= bounds.bottom - grip;
    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    }
}

fn message_point(value: LPARAM) -> POINT {
    POINT {
        x: value as i16 as i32,
        y: (value >> 16) as i16 as i32,
    }
}

fn packed_point(point: POINT) -> LPARAM {
    (u32::from(point.x as u16) | (u32::from(point.y as u16) << 16)) as LPARAM
}

fn current_monitor(hwnd: HWND) -> Option<MONITORINFO> {
    monitor_info(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
}

fn menu_center(hwnd: HWND) -> POINT {
    let bounds = client(hwnd);
    let mut point = POINT {
        x: bounds.left + width(bounds) / 2,
        y: bounds.top + height(bounds) / 2,
    };
    unsafe { ClientToScreen(hwnd, &mut point) };
    point
}

fn begin_window_move(hwnd: HWND, mut point: POINT) {
    // A movimentação nativa emite os eventos observados pelo FancyZones.
    unsafe {
        if ClientToScreen(hwnd, &mut point) != 0 {
            DefWindowProcW(
                hwnd,
                WM_NCLBUTTONDOWN,
                HTCAPTION as WPARAM,
                packed_point(point),
            );
        }
    }
}

fn cancel_window_move(state: &RefCell<Window>) -> bool {
    state
        .try_borrow_mut()
        .is_ok_and(|mut app| std::mem::take(&mut app.move_armed))
}

struct PanelDrag {
    source: usize,
    origin: POINT,
    target: Option<usize>,
    active: bool,
}

impl PanelDrag {
    fn update(&mut self, point: POINT, target: Option<usize>, threshold: u32) -> bool {
        let motion = point
            .x
            .abs_diff(self.origin.x)
            .max(point.y.abs_diff(self.origin.y));
        let active = self.active || motion >= threshold;
        let repaint = active && (!self.active || self.target != target);
        self.active = active;
        self.target = target;
        repaint
    }
}

fn start_drag(hwnd: HWND, state: &RefCell<Window>, point: POINT) {
    let started = state.try_borrow_mut().is_ok_and(|mut app| {
        let Some(source) = app.panel_at(client(hwnd), point) else {
            return false;
        };
        if app.snapshot.is_none() || !app.panel_is_occupied(source) || app.drag.is_some() {
            return false;
        }
        app.drag = Some(PanelDrag {
            source,
            origin: point,
            target: Some(source),
            active: false,
        });
        true
    });
    if started {
        // Alterar a captura pode despachar mensagens, por isso o estado já foi liberado.
        unsafe { SetCapture(hwnd) };
        if unsafe { GetCapture() } != hwnd {
            clear_drag(hwnd, state);
        }
    }
}

fn update_drag(hwnd: HWND, state: &RefCell<Window>, point: POINT) {
    let repaint = state.try_borrow_mut().is_ok_and(|mut app| {
        if app.drag.is_none() {
            return false;
        }
        let target = app.panel_at(client(hwnd), point);
        let threshold = scale(4, app.dpi) as u32;
        app.drag
            .as_mut()
            .is_some_and(|drag| drag.update(point, target, threshold))
    });
    if repaint {
        invalidate(hwnd);
    }
}

fn finish_drag(hwnd: HWND, state: &RefCell<Window>, point: POINT) {
    let drag = state.try_borrow_mut().ok().and_then(|mut app| {
        let mut drag = app.drag.take()?;
        let target = app.panel_at(client(hwnd), point);
        drag.update(point, target, scale(4, app.dpi) as u32);
        Some(drag)
    });
    let Some(drag) = drag else { return };
    release_mouse(hwnd);
    if drag.active {
        if let Some(target) = drag.target
            && target != drag.source
        {
            edit_board(hwnd, state, |app| app.place_panel(drag.source, target));
        }
        invalidate(hwnd);
    }
}

fn clear_drag(hwnd: HWND, state: &RefCell<Window>) -> bool {
    let drag = state
        .try_borrow_mut()
        .ok()
        .and_then(|mut app| app.drag.take());
    if let Some(drag) = drag {
        if drag.active {
            invalidate(hwnd);
        }
        return true;
    }
    false
}

fn cancel_drag(hwnd: HWND, state: &RefCell<Window>) -> bool {
    let cancelled = clear_drag(hwnd, state);
    if cancelled {
        release_mouse(hwnd);
    }
    cancelled
}

fn release_mouse(hwnd: HWND) {
    unsafe {
        if GetCapture() == hwnd {
            ReleaseCapture();
        }
    }
}

fn keyboard(hwnd: HWND, state: &RefCell<Window>, key: u16) -> bool {
    let section = match key {
        VK_ESCAPE => return cancel_window_move(state) || cancel_drag(hwnd, state),
        VK_F2 => MenuSection::Sensors,
        VK_F3 => MenuSection::Period,
        VK_F4 => MenuSection::Grid,
        VK_PRIOR | VK_NEXT | VK_HOME | VK_END => {
            let step = match key {
                VK_PRIOR => ScrollMove::PreviousPage,
                VK_NEXT => ScrollMove::NextPage,
                VK_HOME => ScrollMove::First,
                _ => ScrollMove::Last,
            };
            navigate(hwnd, state, step);
            return true;
        }
        _ => return false,
    };
    cancel_window_move(state);
    cancel_drag(hwnd, state);
    show_options(hwnd, state, menu_center(hwnd), section);
    true
}

enum ScrollMove {
    Rows(isize),
    PreviousPage,
    NextPage,
    First,
    Last,
    ToRow(usize),
    Wheel(i16),
}

fn navigate(hwnd: HWND, state: &RefCell<Window>, step: ScrollMove) {
    let changed = state.try_borrow_mut().is_ok_and(|mut app| {
        if app.drag.is_some() {
            return false;
        }
        let row = match step {
            ScrollMove::Rows(rows) => app.scroll_row.saturating_add_signed(rows),
            ScrollMove::PreviousPage => app.scroll_row.saturating_sub(app.board.rows),
            ScrollMove::NextPage => app.scroll_row + app.board.rows,
            ScrollMove::First => 0,
            ScrollMove::Last => app.scroll_limit(),
            ScrollMove::ToRow(row) => row,
            ScrollMove::Wheel(delta) => {
                let motion = app.wheel_remainder + i32::from(delta);
                let rows = motion / WHEEL_DELTA as i32;
                app.wheel_remainder = motion % WHEEL_DELTA as i32;
                app.scroll_row.saturating_add_signed(-rows as isize)
            }
        };
        let row = row.min(app.scroll_limit());
        let changed = row != app.scroll_row;
        app.scroll_row = row;
        changed
    });
    if changed {
        invalidate(hwnd);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScrollbarState {
    position: [i32; 4],
    total_rows: usize,
    visible_rows: usize,
    first_row: usize,
    layered: bool,
    theme: Theme,
}

fn scrollbar_bounds(bounds: RECT, dpi: u32) -> RECT {
    let grip = scale(5, dpi);
    let scrollbar_width = unsafe { GetSystemMetricsForDpi(SM_CXVSCROLL, dpi) };
    let right = bounds.right - grip;
    let bottom = bounds.bottom - grip;
    area(
        (right - scrollbar_width).max(bounds.left),
        (bounds.top + grip).min(bottom),
        right,
        bottom,
    )
}

fn sync_scrollbar(hwnd: HWND, state: &RefCell<Window>) {
    let Ok(mut app) = state.try_borrow_mut() else {
        return;
    };
    let scrollbar = app.scrollbar;
    let overlay = app
        .scrollbar_overlay
        .as_ref()
        .map(|overlay| overlay.control);
    let bounds = scrollbar_bounds(app.content_bounds(client(hwnd)), app.dpi);
    let next = (app.scroll_limit() > 0 && width(bounds) > 0 && height(bounds) > 0).then(|| {
        ScrollbarState {
            position: [bounds.left, bounds.top, width(bounds), height(bounds)],
            total_rows: app.total_rows(),
            visible_rows: app.board.rows,
            first_row: app.scroll_row,
            layered: appearance::is_click_through(hwnd),
            theme: app.board.appearance.theme,
        }
    });
    let previous = app.scrollbar_state;
    if previous == next {
        drop(app);
        sync_scrollbar_overlay(hwnd, state);
        return;
    }
    app.scrollbar_state = next;
    drop(app);

    unsafe {
        if let Some(next) = next {
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: next.total_rows as i32 - 1,
                nPage: next.visible_rows as u32,
                nPos: next.first_row as i32,
                nTrackPos: 0,
            };
            let [x, y, w, h] = next.position;
            for (control, x, y, show) in std::iter::once((scrollbar, x, y, !next.layered))
                .chain(overlay.map(|control| (control, 0, 0, true)))
            {
                if previous.is_none_or(|previous| previous.theme != next.theme) {
                    scrollbar::set_theme(control, next.theme);
                }
                SetScrollInfo(control, SB_CTL, &info, 0);
                if previous.is_none_or(|previous| {
                    previous.layered != next.layered || previous.position != next.position
                }) {
                    SetWindowPos(
                        control,
                        null_mut(),
                        x,
                        y,
                        w,
                        h,
                        SWP_NOACTIVATE
                            | SWP_NOZORDER
                            | if show { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW },
                    );
                }
                if show {
                    invalidate(control);
                }
            }
        } else {
            ShowWindow(scrollbar, SW_HIDE);
        }
    }
    sync_scrollbar_overlay(hwnd, state);
}

fn sync_scrollbar_overlay(hwnd: HWND, state: &RefCell<Window>) {
    let Some((overlay, position)) = state.try_borrow().ok().and_then(|app| {
        Some((
            app.scrollbar_overlay.as_ref()?.hwnd(),
            app.scrollbar_state
                .filter(|scrollbar| scrollbar.layered && appearance::is_click_through(hwnd))
                .map(|scrollbar| scrollbar.position),
        ))
    }) else {
        return;
    };
    scrollbar::sync_overlay(overlay, hwnd, position);
}

fn sync_input_strip(hwnd: HWND, state: &RefCell<Window>) {
    let strip = state
        .try_borrow()
        .ok()
        .and_then(|app| app.input_strip.as_ref().map(input_strip::Strip::hwnd));
    if let Some(strip) = strip {
        input_strip::sync(strip, hwnd, appearance::is_click_through(hwnd));
    }
}

#[derive(Clone, Copy)]
enum MenuSection {
    All,
    Sensors,
    Period,
    Grid,
}

struct OpenMenu {
    root: HMENU,
    section: MenuSection,
    processes: Vec<Process>,
    displays: Vec<dropdown::Display>,
    panel_sensors: Vec<usize>,
}

impl OpenMenu {
    fn panel(&self, app: &Window) -> Option<usize> {
        self.panel_sensors.iter().find_map(|sensor| {
            app.board
                .panels
                .iter()
                .position(|panel| panel.contains(sensor))
        })
    }
}

fn show_options(hwnd: HWND, state: &RefCell<Window>, screen: POINT, section: MenuSection) {
    if unsafe { IsWindowEnabled(hwnd) } == 0
        || state
            .try_borrow()
            .map_or(true, |app| app.open_menu.is_some())
    {
        return;
    }
    let (processes, displays) = if matches!(section, MenuSection::All) {
        (visible_processes(), dropdown::displays())
    } else {
        (Vec::new(), Vec::new())
    };
    let (menu, panel_sensors, theme, dpi) = {
        let Ok(app) = state.try_borrow() else { return };
        let mut point = screen;
        let panel = if matches!(section, MenuSection::All)
            && unsafe { ScreenToClient(hwnd, &mut point) } != 0
        {
            if appearance::is_click_through(hwnd) && (0..input_strip::HEIGHT).contains(&point.y) {
                point.y = input_strip::HEIGHT;
            }
            app.panel_at(client(hwnd), point)
                .filter(|&index| app.panel_is_occupied(index))
        } else {
            None
        };
        (
            options_menu(hwnd, &app, section, &processes, &displays, panel),
            panel.map_or_else(Vec::new, |index| app.board.panels[index].clone()),
            app.board.appearance.theme,
            app.dpi,
        )
    };
    let Some(menu) = menu else { return };
    let opened = Rc::new(OpenMenu {
        root: menu.0,
        section,
        processes,
        displays,
        panel_sensors,
    });
    state.borrow_mut().open_menu = Some(Rc::clone(&opened));
    // TrackPopupMenu despacha mensagens enquanto o menu está aberto.
    let choice = menu.track(hwnd, screen, keep_menu_open, theme, dpi);
    state.borrow_mut().open_menu = None;
    if choice != 0 {
        let panel = opened.panel(&state.borrow());
        apply_option(
            hwnd,
            state,
            choice,
            panel,
            &opened.processes,
            &opened.displays,
        );
    }
}

fn keep_menu_open(choice: usize) -> bool {
    matches!(
        choice,
        RESET_SENSORS | RESET_COLORS | GPU_AUTO | THEME_DARK | THEME_LIGHT
    ) || (OPACITY_FIRST..=OPACITY_FIRST + 100).contains(&choice)
        || (GPU_FIRST..GPU_FIRST + GPU_LIMIT).contains(&choice)
        || (PERIOD_FIRST..PERIOD_FIRST + PERIODS.len()).contains(&choice)
        || (COLUMNS_FIRST..COLUMNS_FIRST + MAX_COLUMNS).contains(&choice)
        || (ROWS_FIRST..ROWS_FIRST + MAX_ROWS).contains(&choice)
        || (REFRESH_FIRST..REFRESH_FIRST + GUI_INTERVALS_MS.len()).contains(&choice)
        || (PAGE_FIRST..PAGE_FIRST + MAX_SLOTS).contains(&choice)
        || (SENSOR_FIRST..MERGE_FIRST).contains(&choice)
        || choice >= FRAME_TARGET_FOREGROUND
}

fn options_menu(
    hwnd: HWND,
    app: &Window,
    section: MenuSection,
    processes: &[Process],
    displays: &[dropdown::Display],
    panel: Option<usize>,
) -> Option<Menu> {
    let menu = Menu::new()?;
    match section {
        MenuSection::Sensors => app.sensor_menu(&menu),
        MenuSection::Period => app.period_menu(&menu),
        MenuSection::Grid => app.grid_menu(&menu),
        MenuSection::All => {
            if let Some(snapshot) = &app.snapshot {
                if app.snapshot_is_stale(Instant::now()) {
                    menu.label("Coleta atrasada");
                }
                if snapshot.gpu_selections == app.monitor.gpu_selections() {
                    menu.label(&snapshot.notice);
                } else {
                    menu.label("Atualizando as GPUs monitoradas…");
                }
                menu.separator();
            }
            menu.group_icon("Intervalo de amostragem", Icon::Sampling, |items| {
                for (index, &ms) in GUI_INTERVALS_MS.iter().enumerate() {
                    items.item(
                        REFRESH_FIRST + index,
                        &format!("{ms} ms"),
                        ms == app.board.refresh_ms,
                    );
                }
            });
            menu.group_icon("Histórico", Icon::History, |items| app.period_menu(items));
            menu.group_icon("GPUs monitoradas", Icon::Gpu, |items| app.gpu_menu(items));
            menu.group_icon("Sensores", Icon::Sensors, |items| app.sensor_menu(items));
            menu.group_icon("FPS", Icon::Fps, |items| app.frame_menu(items, processes));
            menu.separator();
            menu.group_icon("Grade", Icon::Grid, |items| app.grid_menu(items));
            menu.group_icon("Aparência", Icon::Appearance, |items| {
                app.appearance_menu(hwnd, items)
            });
            menu.group_icon("Modo dropdown", Icon::Dropdown, |items| {
                dropdown_menu(items, app, displays)
            });
            match app.updater.status() {
                Status::Idle => {
                    menu.item_icon(UPDATE_CHECK, "Buscar por atualizações", Icon::Update);
                    match app.update_notice.as_ref().map(|notice| &notice.result) {
                        Some(CheckNotice::Current) => menu.label("O ARGOS está atualizado"),
                        Some(CheckNotice::Failed(_)) => menu.item_icon(
                            UPDATE_CHECK_DETAILS,
                            "Ver erro da busca por atualizações",
                            Icon::Error,
                        ),
                        _ => {}
                    }
                }
                Status::Checking => menu.label_icon("Buscando por atualizações…", Icon::Update),
                Status::Ready(version, scheduled) => {
                    menu.group_icon(
                        &format!("ARGOS {version} disponível"),
                        Icon::Update,
                        |items| {
                            items.item(UPDATE_RESTART, "Atualizar e reiniciar", false);
                            if scheduled {
                                items.item(UPDATE_CANCEL, "Cancelar atualização ao fechar", false);
                            } else {
                                items.item(UPDATE_SCHEDULE, "Atualizar ao fechar", false);
                            }
                        },
                    );
                }
            }
            if update::last_error().is_some() {
                menu.item_icon(UPDATE_ERROR, "Ver erro da última atualização", Icon::Error);
            }
            if app.page_count() > 1 {
                menu.group_icon("Página", Icon::Page, |items| {
                    for page in 0..app.page_count() {
                        items.item(
                            PAGE_FIRST + page,
                            &format!("Página {}", page + 1),
                            app.page_is_current(page),
                        );
                    }
                });
            }
            if app.save_failed || app.dropdown.save_failed {
                menu.label("Configuração disponível nesta sessão");
            }
        }
    }
    if let Some(index) = panel
        && let Some(snapshot) = &app.snapshot
    {
        menu.separator();
        app.panel_menu(&menu, snapshot, index, !appearance::is_click_through(hwnd));
    }
    if matches!(section, MenuSection::All) {
        menu.separator();
        menu.group_icon("Sobre o ARGOS", Icon::About, |items| {
            items.text_block(&[
                concat!("ARGOS ", env!("CARGO_PKG_VERSION")),
                "Acompanhe CPU, GPU, memória, FPS e outros recursos no Windows em um painel personalizável com gráficos em tempo real e histórico.",
                concat!("Licença ", env!("CARGO_PKG_LICENSE")),
            ]);
            items.separator();
            items.centered_item(ABOUT_GUIDE, "Guia do painel");
            items.centered_item(ABOUT_TWITCH, "Canal na Twitch");
            items.centered_item(ABOUT_RELEASES, "Versões no GitHub");
        });
        if let Some(directory) = app.startup_directory() {
            let enabled = autostart::enabled(directory).unwrap_or(false);
            let label = if enabled {
                "Iniciar com o Windows (ativado)"
            } else {
                "Iniciar com o Windows"
            };
            menu.item_icon_checked(START_WITH_WINDOWS, label, Icon::Power, enabled);
        } else {
            menu.label_icon("Iniciar com o Windows (requer instalação)", Icon::Power);
        }
        if app.dropdown.active() {
            let pinned = app.dropdown.pinned();
            let icon = if pinned { Icon::Pinned } else { Icon::Pin };
            menu.item_icon_checked(DROPDOWN_PIN, "Fixar aberto", icon, pinned);
        } else {
            if unsafe { IsZoomed(hwnd) } != 0 {
                menu.label_icon("Mover janela", Icon::Move);
                menu.item_icon(MAXIMIZE_WINDOW, "Restaurar", Icon::Restore);
            } else {
                menu.item_icon(MOVE_WINDOW, "Mover janela", Icon::Move);
                menu.item_icon(MAXIMIZE_WINDOW, "Maximizar", Icon::Maximize);
            }
            menu.item_icon(MINIMIZE_WINDOW, "Minimizar", Icon::Minimize);
        }
        menu.item_icon(CLOSE_WINDOW, "Fechar ARGOS", Icon::Close);
    }
    Some(menu)
}

fn apply_open_option(hwnd: HWND, state: &RefCell<Window>, choice: usize) {
    if !keep_menu_open(choice) {
        return;
    }
    let Some(opened) = state.borrow().open_menu.clone() else {
        return;
    };
    let panel = opened.panel(&state.borrow());
    apply_option(
        hwnd,
        state,
        choice,
        panel,
        &opened.processes,
        &opened.displays,
    );
    let app = state.borrow();
    let panel = opened.panel(&app);
    let updated = options_menu(
        hwnd,
        &app,
        opened.section,
        &opened.processes,
        &opened.displays,
        panel,
    );
    let theme = app.board.appearance.theme;
    drop(app);
    if let Some(updated) = updated {
        menu::sync(opened.root, &updated, theme);
    }
}
fn apply_option(
    hwnd: HWND,
    state: &RefCell<Window>,
    choice: usize,
    panel: Option<usize>,
    processes: &[Process],
    displays: &[dropdown::Display],
) {
    match choice {
        ABOUT_RELEASES => open_about_link(hwnd, state, update::RELEASES_URL),
        ABOUT_GUIDE => open_about_link(hwnd, state, ABOUT_GUIDE_URL),
        ABOUT_TWITCH => open_about_link(hwnd, state, ABOUT_TWITCH_URL),
        START_WITH_WINDOWS => toggle_autostart(hwnd, state),
        THEME_DARK | THEME_LIGHT => {
            edit_board(hwnd, state, |app| {
                let theme = if choice == THEME_DARK {
                    Theme::Dark
                } else {
                    Theme::Light
                };
                let previous = std::mem::replace(&mut app.board.appearance.theme, theme);
                previous != theme
            });
            sync_appearance(hwnd, state);
        }
        FPS_CEILING_EDIT => {
            let (current, theme) = {
                let app = state.borrow();
                (app.board.fps_ceiling, app.board.appearance.theme)
            };
            if let Some(ceiling) = fps_scale::choose(hwnd, current, theme) {
                edit_board(hwnd, state, |app| {
                    std::mem::replace(&mut app.board.fps_ceiling, ceiling) != ceiling
                });
            }
        }
        choice if (OPACITY_FIRST..=OPACITY_FIRST + 100).contains(&choice) => {
            edit_board(hwnd, state, |app| {
                let opacity = (choice - OPACITY_FIRST) as u8;
                let previous = std::mem::replace(&mut app.board.appearance.opacity, opacity);
                previous != opacity
            });
            sync_appearance(hwnd, state);
        }
        GPU_AUTO => choose_gpu(hwnd, state, None),
        choice if (GPU_FIRST..GPU_FIRST + GPU_LIMIT).contains(&choice) => {
            choose_gpu(hwnd, state, Some(choice - GPU_FIRST));
        }
        DROPDOWN_TOGGLE => {
            change_dropdown(state, |settings| settings.enabled = !settings.enabled);
            dropdown_mode::sync(hwnd, state);
        }
        DROPDOWN_PREVIEW => preview_chosen_display(hwnd, state, displays),
        DROPDOWN_PIN => dropdown_mode::pin(state),
        DROPDOWN_HOTKEY => choose_dropdown_hotkey(hwnd, state),
        DROPDOWN_CLEAR_HOTKEY => {
            dropdown_mode::unbind(hwnd, state);
            change_dropdown(state, |settings| settings.hotkey = None);
        }
        choice
            if (DROPDOWN_ALIGNMENT_FIRST..DROPDOWN_ALIGNMENT_FIRST + DROPDOWN_ALIGNMENTS.len())
                .contains(&choice) =>
        {
            change_dropdown(state, |settings| {
                settings.alignment = DROPDOWN_ALIGNMENTS[choice - DROPDOWN_ALIGNMENT_FIRST].0;
            });
            dropdown_mode::sync(hwnd, state);
            preview_chosen_display(hwnd, state, displays);
        }
        choice
            if (DROPDOWN_WIDTH_FIRST..DROPDOWN_WIDTH_FIRST + DROPDOWN_WIDTHS.len())
                .contains(&choice) =>
        {
            change_dropdown(state, |settings| {
                settings.width = DROPDOWN_WIDTHS[choice - DROPDOWN_WIDTH_FIRST];
            });
            dropdown_mode::sync(hwnd, state);
            preview_chosen_display(hwnd, state, displays);
        }
        choice
            if (DROPDOWN_MONITOR_FIRST..DROPDOWN_MONITOR_FIRST + DROPDOWN_MONITOR_LIMIT)
                .contains(&choice) =>
        {
            if let Some(display) = displays.get(choice - DROPDOWN_MONITOR_FIRST) {
                change_dropdown(state, |settings| {
                    settings.monitor = Some(display.id.clone())
                });
                dropdown_mode::sync(hwnd, state);
                dropdown_mode::preview(hwnd, state, display);
            }
        }
        MOVE_WINDOW => {
            if let Ok(mut app) = state.try_borrow_mut() {
                app.move_armed = true;
            }
        }
        EDIT_PING_TARGETS => {
            let current = state.borrow().board.ping_targets.clone();
            if let Some(targets) = ping_targets::edit(hwnd, &current) {
                edit_board(hwnd, state, |app| {
                    let changed = app.board.ping_targets != targets;
                    app.board.ping_targets = targets;
                    if app.board.ping_targets.is_empty() || app.board.contains(CUSTOM_PING_INDEX) {
                        return changed;
                    }
                    app.board.toggle(CUSTOM_PING_INDEX, app.first_slot());
                    if let Some(slot) = app
                        .board
                        .panels
                        .iter()
                        .position(|panel| panel.contains(&CUSTOM_PING_INDEX))
                    {
                        app.show_panel(slot);
                        return true;
                    }
                    changed
                });
            }
        }
        MINIMIZE_WINDOW => {
            let command = if state.borrow().has_tray() {
                SW_HIDE
            } else {
                SW_MINIMIZE
            };
            unsafe { ShowWindow(hwnd, command) };
        }
        MAXIMIZE_WINDOW => {
            unsafe {
                ShowWindow(
                    hwnd,
                    if IsZoomed(hwnd) == 0 {
                        SW_MAXIMIZE
                    } else {
                        SW_RESTORE
                    },
                )
            };
        }
        CLOSE_WINDOW => {
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        }
        UPDATE_CHECK => {
            if let Ok(mut app) = state.try_borrow_mut() {
                app.update_notice = app
                    .updater
                    .check(true)
                    .err()
                    .map(|error| UpdateNotice::new(CheckNotice::Failed(error)));
            }
            invalidate(hwnd);
        }
        UPDATE_RESTART => {
            let result = state
                .try_borrow_mut()
                .ok()
                .map(|mut app| app.updater.apply(true));
            match result {
                Some(Ok(true)) => {
                    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
                }
                Some(Err(error)) => show_notice(hwnd, state, "Atualização do ARGOS", &error),
                _ => {}
            }
        }
        UPDATE_SCHEDULE | UPDATE_CANCEL => {
            if let Ok(mut app) = state.try_borrow_mut() {
                app.updater.schedule(choice == UPDATE_SCHEDULE);
            }
        }
        UPDATE_ERROR => {
            if let Some(error) = update::last_error() {
                show_notice(hwnd, state, "Atualização do ARGOS", &error);
            }
        }
        UPDATE_CHECK_DETAILS => {
            let error = state.borrow().update_notice.as_ref().and_then(|notice| {
                if let CheckNotice::Failed(error) = &notice.result {
                    Some(error.clone())
                } else {
                    None
                }
            });
            if let Some(error) = error {
                show_notice(hwnd, state, "Atualização do ARGOS", &error);
            }
        }
        choice if (PAGE_FIRST..PAGE_FIRST + MAX_SLOTS).contains(&choice) => {
            if let Ok(mut app) = state.try_borrow_mut() {
                app.show_page(choice - PAGE_FIRST);
            }
            invalidate(hwnd);
        }
        choice if (COLOR_FIRST..MOVE_PANEL_FIRST).contains(&choice) => {
            choose_series_color(hwnd, state, choice - COLOR_FIRST);
        }
        choice if choice >= FRAME_TARGET_FOREGROUND => {
            let target = match choice {
                FRAME_TARGET_FOREGROUND => FrameTarget::Foreground,
                FRAME_TARGET_OFF => FrameTarget::Off,
                _ => {
                    let Some(process) = processes.get(choice - FRAME_TARGET_FIRST) else {
                        return;
                    };
                    FrameTarget::Process(process.pid)
                }
            };
            if let Ok(mut app) = state.try_borrow_mut() {
                app.select_frame_target(target);
            }
            invalidate(hwnd);
        }
        choice => edit_board(hwnd, state, |app| app.apply_selection(choice, panel)),
    }
}

fn choose_gpu(hwnd: HWND, state: &RefCell<Window>, index: Option<usize>) {
    let mut app = state.borrow_mut();
    let mut selections = app.monitor.gpu_selections();
    if let Some(index) = index {
        let Some(gpu) = app
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.gpu_devices.get(index))
            .cloned()
        else {
            return;
        };
        if let Some(index) = selections
            .iter()
            .position(|selection| gpu.matches_selection(selection))
        {
            selections.remove(index);
        } else {
            selections.push(gpu.selection());
            if !app.board.panels.iter().flatten().any(|&slot| {
                app.board
                    .gpu_sensor(slot)
                    .is_some_and(|(id, _)| id == gpu.id)
            }) {
                let slot = app.board.register_gpu_sensor(
                    &gpu.id,
                    gpu.persistent,
                    crate::watch::GPU_ACTIVITY_INDEX,
                );
                let first = app.first_slot();
                app.board.toggle(slot, first);
                if let Some(panel) = app
                    .board
                    .panels
                    .iter()
                    .position(|panel| panel.contains(&slot))
                {
                    app.show_panel(panel);
                }
            }
        }
    } else {
        selections.clear();
    }
    app.board.gpu_ids = selections
        .iter()
        .filter_map(|selection| match selection {
            Selection::Device(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    app.monitor.set_gpu_selections(selections);
    app.save_failed = app.board.save().is_err();
    drop(app);
    sync_scrollbar(hwnd, state);
    invalidate(hwnd);
}

fn chosen_display<'a>(
    app: &Window,
    displays: &'a [dropdown::Display],
) -> Option<&'a dropdown::Display> {
    let selected = app.board.dropdown.monitor.as_ref()?;
    displays.iter().find(|display| display.id == *selected)
}

fn dropdown_menu(menu: &Menu, app: &Window, displays: &[dropdown::Display]) {
    let settings = &app.board.dropdown;
    let selected = chosen_display(app, displays);
    if selected.is_some() || settings.enabled {
        menu.item(DROPDOWN_TOGGLE, "Ativar modo dropdown", settings.enabled);
    } else {
        menu.label("Escolha um monitor para ativar");
    }
    if settings.enabled && !app.dropdown.active() {
        menu.label("Dropdown suspenso");
    }
    menu.group("Monitor", |items| {
        for (index, display) in displays.iter().take(DROPDOWN_MONITOR_LIMIT).enumerate() {
            items.item(
                DROPDOWN_MONITOR_FIRST + index,
                &display.label.replace('&', "&&"),
                settings.monitor.as_ref() == Some(&display.id),
            );
        }
    });
    menu.group("Posição da faixa", |items| {
        for (index, (value, label)) in DROPDOWN_ALIGNMENTS.iter().enumerate() {
            items.item(
                DROPDOWN_ALIGNMENT_FIRST + index,
                label,
                settings.alignment == *value,
            );
        }
    });
    menu.group("Largura da faixa", |items| {
        for (index, value) in DROPDOWN_WIDTHS.iter().enumerate() {
            items.item(
                DROPDOWN_WIDTH_FIRST + index,
                &format!("{value}%"),
                settings.width == *value,
            );
        }
    });
    if selected.is_some() {
        menu.item(DROPDOWN_PREVIEW, "Mostrar faixa de ativação", false);
    }
    let key_label = settings.hotkey.map_or_else(
        || "Definir atalho global…".to_owned(),
        |key| {
            format!(
                "Alterar atalho global ({})…",
                hotkey::label(key).replace('&', "&&")
            )
        },
    );
    menu.item(DROPDOWN_HOTKEY, &key_label, false);
    if settings.hotkey.is_some() {
        if app.dropdown.active() && !app.dropdown.hotkey_bound() {
            menu.label("Atalho global indisponível");
        }
        menu.item(DROPDOWN_CLEAR_HOTKEY, "Remover atalho global", false);
    }
}

fn preview_chosen_display(hwnd: HWND, state: &RefCell<Window>, displays: &[dropdown::Display]) {
    let display = chosen_display(&state.borrow(), displays);
    if let Some(display) = display {
        dropdown_mode::preview(hwnd, state, display);
    }
}

fn change_dropdown(state: &RefCell<Window>, change: impl FnOnce(&mut board::DropdownSettings)) {
    let mut app = state.borrow_mut();
    change(&mut app.board.dropdown);
    app.dropdown.save_failed = app.board.save_dropdown().is_err();
}

fn choose_dropdown_hotkey(hwnd: HWND, state: &RefCell<Window>) {
    dropdown_mode::unbind(hwnd, state);
    let theme = state.borrow().board.appearance.theme;
    let chosen = hotkey::choose(hwnd, theme);
    if let Some(key) = chosen
        && dropdown_mode::bind(hwnd, state, key)
    {
        if !dropdown_active(state) {
            dropdown_mode::unbind(hwnd, state);
        }
        change_dropdown(state, |settings| settings.hotkey = Some(key));
        return;
    }
    let previous = state.borrow().board.dropdown.hotkey;
    if let Some(previous) = previous
        && dropdown_active(state)
    {
        dropdown_mode::bind(hwnd, state, previous);
    }
    if chosen.is_some() {
        show_notice(
            hwnd,
            state,
            "Atalho global do ARGOS",
            "O Windows não aceitou essa combinação. Escolha outra nas opções do dropdown.",
        );
    }
}

fn open_about_link(hwnd: HWND, state: &RefCell<Window>, url: &str) {
    let url = wide(url);
    let result = unsafe {
        let initialized = CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        ) >= 0;
        let result = ShellExecuteW(
            hwnd,
            w!("open"),
            url.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
        if initialized {
            CoUninitialize();
        }
        result
    };
    if result as isize <= 32 {
        show_notice(
            hwnd,
            state,
            "Sobre o ARGOS",
            "Não foi possível abrir o link no navegador.",
        );
    }
}

fn toggle_autostart(hwnd: HWND, state: &RefCell<Window>) {
    let result = {
        let app = state.borrow();
        let Some(directory) = app.startup_directory() else {
            return;
        };
        autostart::enabled(directory)
            .and_then(|enabled| autostart::set_enabled(directory, !enabled))
    };
    if let Err(error) = result {
        show_notice(
            hwnd,
            state,
            "Iniciar com o Windows",
            &format!("Não foi possível alterar a inicialização do ARGOS. {error}"),
        );
    }
}

fn show_notice(hwnd: HWND, state: &RefCell<Window>, title: &str, message: &str) {
    let theme = state.borrow().board.appearance.theme;
    dialog::notice(hwnd, theme, title, message);
}

fn choose_series_color(hwnd: HWND, state: &RefCell<Window>, sensor: usize) {
    let (initial, mut custom, theme) = {
        let Ok(app) = state.try_borrow() else { return };
        let Some((position, series_count)) = app.board.panels.iter().find_map(|series| {
            series
                .iter()
                .position(|&index| index == sensor)
                .map(|position| (position, series.len()))
        }) else {
            return;
        };
        (
            app.series_color(sensor, position, series_count),
            app.custom_colors,
            app.board.appearance.theme,
        )
    };
    if let Some(color) = color_picker::choose(hwnd, initial, &mut custom, theme) {
        edit_board(hwnd, state, |app| {
            app.custom_colors = custom;
            app.board.set_color(sensor, Some(color))
        });
    }
}

fn edit_board(hwnd: HWND, state: &RefCell<Window>, edit: impl FnOnce(&mut Window) -> bool) {
    let Ok(mut app) = state.try_borrow_mut() else {
        return;
    };
    let old_interval = app.board.refresh_ms;
    if !edit(&mut app) {
        return;
    }
    app.scroll_row = app.scroll_row.min(app.scroll_limit());
    let interval = app.board.refresh_ms;
    if interval != old_interval {
        app.monitor.set_interval_ms(interval);
    }
    app.monitor.set_cpu_metrics(app.board.cpu_metrics());
    app.monitor
        .set_game_latency_enabled(app.board.contains(GAME_LATENCY_INDEX));
    app.monitor
        .set_custom_ping_targets(app.board.active_ping_targets());
    app.save_failed = app.board.save().is_err();
    drop(app);
    if interval != old_interval {
        unsafe { SetTimer(hwnd, TIMER, interval, None) };
    }
    invalidate(hwnd);
}

fn sync_appearance(hwnd: HWND, state: &RefCell<Window>) {
    let translucent = state.borrow().board.appearance.translucent();
    if translucent && state.borrow().input_strip.is_none() {
        let strip = input_strip::Strip::new(hwnd);
        state.borrow_mut().input_strip = strip;
    }
    let layered = translucent && state.borrow().input_strip.is_some();
    if appearance::set_layered(hwnd, layered) && layered {
        sync_scrollbar(hwnd, state);
        let screen = unsafe { GetDC(null_mut()) };
        if !screen.is_null() {
            let presented = state
                .borrow_mut()
                .present_layered(hwnd, screen, client(hwnd));
            unsafe { ReleaseDC(null_mut(), screen) };
            if !presented {
                appearance::set_layered(hwnd, false);
            }
        } else {
            appearance::set_layered(hwnd, false);
        }
    } else {
        appearance::set_layered(hwnd, false);
        state.borrow_mut().surface = Surface::default();
    }
    sync_input_strip(hwnd, state);
    sync_scrollbar(hwnd, state);
    invalidate(hwnd);
}

enum NoticeVisibility {
    Pending,
    Showing(Instant),
    Seen,
}

struct UpdateNotice {
    result: CheckNotice,
    visibility: NoticeVisibility,
}

impl UpdateNotice {
    fn new(result: CheckNotice) -> Self {
        Self {
            result,
            visibility: NoticeVisibility::Pending,
        }
    }

    fn tick(&mut self, now: Instant, can_show: bool) -> bool {
        self.visibility = match self.visibility {
            NoticeVisibility::Pending if can_show => {
                NoticeVisibility::Showing(now + UPDATE_NOTICE_DURATION)
            }
            NoticeVisibility::Showing(until) if now >= until => NoticeVisibility::Seen,
            _ => return false,
        };
        true
    }

    fn text(&self) -> std::borrow::Cow<'static, str> {
        match &self.result {
            CheckNotice::Ready(version) => {
                format!("ARGOS {version} pronto para atualizar. Abra o menu do painel.").into()
            }
            CheckNotice::Current => "O ARGOS está atualizado.".into(),
            CheckNotice::Failed(_) => {
                "Falha na busca por atualizações. Veja os detalhes no menu.".into()
            }
        }
    }
}

struct Window {
    monitor: Monitor,
    updater: Updater,
    update_notice: Option<UpdateNotice>,
    snapshot: Option<Arc<Snapshot>>,
    board: Board,
    dropdown: dropdown_mode::Runtime,
    tray: Option<tray::Tray>,
    save_failed: bool,
    custom_colors: [COLORREF; 16],
    open_menu: Option<Rc<OpenMenu>>,
    scroll_row: usize,
    scrollbar: HWND,
    scrollbar_overlay: Option<scrollbar::Overlay>,
    scrollbar_state: Option<ScrollbarState>,
    input_strip: Option<input_strip::Strip>,
    wheel_remainder: i32,
    move_armed: bool,
    drag: Option<PanelDrag>,
    dpi: u32,
    chart_fonts: chart::ChartFonts,
    buffer: BackBuffer,
    surface: Surface,
}

impl Window {
    fn new(monitor: Monitor, board: Board, dpi: u32, source: Source) -> Self {
        Self {
            monitor,
            updater: Updater::new(source),
            update_notice: None,
            snapshot: None,
            board,
            dropdown: dropdown_mode::Runtime::default(),
            tray: None,
            save_failed: false,
            custom_colors: SERIES,
            open_menu: None,
            scroll_row: 0,
            scrollbar: null_mut(),
            scrollbar_overlay: None,
            scrollbar_state: None,
            input_strip: None,
            wheel_remainder: 0,
            move_armed: false,
            drag: None,
            dpi,
            chart_fonts: chart::ChartFonts::new(dpi),
            buffer: BackBuffer::default(),
            surface: Surface::default(),
        }
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi != 0 && dpi != self.dpi {
            self.dpi = dpi;
            self.chart_fonts = chart::ChartFonts::new(dpi);
        }
    }

    fn startup_directory(&self) -> Option<&std::path::Path> {
        self.updater
            .installed_directory()
            .filter(|directory| directory.join("argos-gui.exe").is_file())
    }

    fn has_tray(&self) -> bool {
        self.tray.as_ref().is_some_and(tray::Tray::available)
    }

    fn refresh(&mut self) -> bool {
        self.update_snapshot(self.monitor.snapshot())
    }

    fn refresh_cached(&mut self) -> bool {
        self.update_snapshot(self.monitor.latest_snapshot())
    }

    fn update_snapshot(&mut self, next: Option<Arc<Snapshot>>) -> bool {
        let Some(next) = next else {
            return false;
        };
        if self
            .snapshot
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &next))
        {
            return false;
        }
        if let Some(id) = self.board.gpu_ids.first().cloned().or_else(|| {
            let primary = next.gpus.first()?;
            next.gpu_devices
                .iter()
                .find(|gpu| gpu.index == primary.index && gpu.persistent)
                .map(|gpu| gpu.id.clone())
        }) {
            self.board.bind_legacy_gpu(&id);
        }
        for gpu in &next.gpus {
            if let Some(info) = next.gpu_devices.iter().find(|info| info.index == gpu.index) {
                for index in 0..CPU_USAGE_INDEX {
                    if gpu.has_sensor(index) {
                        self.board
                            .register_gpu_sensor(&info.id, info.persistent, index);
                    }
                }
            }
        }
        self.snapshot = Some(next);
        true
    }

    fn slots(&self) -> usize {
        self.board.columns * self.board.rows
    }

    fn page_count(&self) -> usize {
        self.board.panels.len().div_ceil(self.slots()).max(1)
    }

    fn total_rows(&self) -> usize {
        self.board.panels.len().div_ceil(self.board.columns)
    }

    fn scroll_limit(&self) -> usize {
        self.total_rows().saturating_sub(self.board.rows)
    }

    fn page_row(&self, page: usize) -> usize {
        page.saturating_mul(self.board.rows)
            .min(self.scroll_limit())
    }

    fn page_is_current(&self, page: usize) -> bool {
        self.scroll_row == self.page_row(page)
    }

    fn show_page(&mut self, page: usize) {
        self.scroll_row = self.page_row(page);
    }

    fn first_slot(&self) -> usize {
        self.scroll_row * self.board.columns
    }

    fn show_panel(&mut self, panel: usize) {
        let row = panel / self.board.columns;
        self.scroll_row = self
            .scroll_row
            .clamp(row.saturating_sub(self.board.rows - 1), row);
    }

    fn panel_is_occupied(&self, index: usize) -> bool {
        self.board
            .panels
            .get(index)
            .is_some_and(|panel| !panel.is_empty())
    }

    fn place_panel(&mut self, source: usize, target: usize) -> bool {
        if !self.board.move_panel(source, target) {
            return false;
        }
        self.show_panel(target);
        true
    }

    fn snapshot_is_stale(&self, now: Instant) -> bool {
        let limit = Duration::from_millis((u64::from(self.board.refresh_ms) * 5 / 2).max(750));
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| now.saturating_duration_since(snapshot.at) >= limit)
    }

    fn select_frame_target(&mut self, target: FrameTarget) {
        if self.monitor.frame_target() == target {
            return;
        }
        self.monitor.set_frame_target(target);
        if target != FrameTarget::Off
            && let Some(panel) = self
                .board
                .panels
                .iter()
                .position(|panel| panel.iter().any(|sensor| FRAME_SENSORS.contains(sensor)))
        {
            self.show_panel(panel);
        }
    }

    fn apply_selection(&mut self, choice: usize, panel: Option<usize>) -> bool {
        if let Some(&interval) = choice
            .checked_sub(REFRESH_FIRST)
            .and_then(|index| GUI_INTERVALS_MS.get(index))
        {
            let changed = interval != self.board.refresh_ms;
            self.board.refresh_ms = interval;
            return changed;
        }
        if (PERIOD_FIRST..PERIOD_FIRST + PERIODS.len()).contains(&choice) {
            let period = choice - PERIOD_FIRST;
            let changed = self.board.period != period;
            self.board.period = period;
            return changed;
        }
        let first = self.first_slot();
        let grid = if (COLUMNS_FIRST..COLUMNS_FIRST + MAX_COLUMNS).contains(&choice) {
            Some((&mut self.board.columns, choice - COLUMNS_FIRST + 1))
        } else if (ROWS_FIRST..ROWS_FIRST + MAX_ROWS).contains(&choice) {
            Some((&mut self.board.rows, choice - ROWS_FIRST + 1))
        } else {
            None
        };
        if let Some((dimension, count)) = grid {
            if *dimension == count {
                return false;
            }
            *dimension = count;
            self.scroll_row = first / self.board.columns;
            return true;
        }
        if let Some(panel) = panel {
            if let Some(target) = choice
                .checked_sub(MOVE_PANEL_FIRST)
                .filter(|&index| index < MAX_SLOTS)
            {
                return self.place_panel(panel, target);
            }
            if choice == SPLIT_PANEL {
                return self.board.split(panel);
            }
            if choice == RESET_COLORS {
                let Some(series) = self.board.panels.get(panel).cloned() else {
                    return false;
                };
                let mut changed = false;
                for index in series {
                    changed |= self.board.set_color(index, None);
                }
                return changed;
            }
            if let Some(source) = choice.checked_sub(MERGE_FIRST)
                && let Some(&sensor) = self
                    .board
                    .panels
                    .get(panel)
                    .and_then(|series| series.first())
                && self.board.merge(source, panel)
            {
                if let Some(merged) = self
                    .board
                    .panels
                    .iter()
                    .position(|series| series.contains(&sensor))
                {
                    self.show_panel(merged);
                }
                return true;
            }
        }
        if choice == RESET_SENSORS {
            self.board.reset_selection(board::INITIAL_SENSORS);
            self.scroll_row = 0;
            return true;
        }
        if let Some(index) = choice.checked_sub(SENSOR_FIRST)
            && choice < MERGE_FIRST
            && self.board.sensor_unit(index).is_some()
        {
            let first = self.first_slot();
            self.board.toggle(index, first);
            return true;
        }
        false
    }

    fn period_menu(&self, menu: &Menu) {
        for (index, (_, label)) in PERIODS.iter().enumerate() {
            menu.item(PERIOD_FIRST + index, label, index == self.board.period);
        }
    }

    fn grid_menu(&self, menu: &Menu) {
        menu.label("Colunas");
        for count in 1..=MAX_COLUMNS {
            menu.item(
                COLUMNS_FIRST + count - 1,
                &count.to_string(),
                count == self.board.columns,
            );
        }
        menu.separator();
        menu.label("Linhas");
        for count in 1..=MAX_ROWS {
            menu.item(
                ROWS_FIRST + count - 1,
                &count.to_string(),
                count == self.board.rows,
            );
        }
    }

    fn appearance_menu(&self, hwnd: HWND, menu: &Menu) {
        let appearance = self.board.appearance;
        menu.item(THEME_DARK, "Tema escuro", appearance.theme == Theme::Dark);
        menu.item(THEME_LIGHT, "Tema claro", appearance.theme == Theme::Light);
        menu.separator();
        menu.group("Opacidade", |items| {
            items.slider(OPACITY_SLIDER, appearance.opacity)
        });
        if appearance.translucent() && !appearance::is_click_through(hwnd) {
            menu.label("Transparência indisponível nesta janela");
        }
        if let Some(snapshot) = &self.snapshot
            && self.board.panels.iter().any(|series| !series.is_empty())
        {
            menu.separator();
            menu.group_icon("Cor dos gráficos", Icon::Color, |items| {
                for series in &self.board.panels {
                    self.curve_color_items(items, snapshot, series);
                }
            });
        }
    }

    fn gpu_menu(&self, menu: &Menu) {
        let selections = self.monitor.gpu_selections();
        menu.item(GPU_AUTO, "Automática (uma GPU)", selections.is_empty());
        let Some(snapshot) = &self.snapshot else {
            menu.label("Procurando GPUs…");
            return;
        };
        if snapshot.gpu_devices.is_empty() {
            menu.label("Nenhuma GPU compatível encontrada");
            return;
        }
        for (index, gpu) in snapshot.gpu_devices.iter().take(GPU_LIMIT).enumerate() {
            let checked = selections
                .iter()
                .any(|selection| gpu.matches_selection(selection));
            menu.item(GPU_FIRST + index, &gpu.label().replace('&', "&&"), checked);
        }
    }

    fn sensor_menu(&self, menu: &Menu) {
        let Some(snapshot) = &self.snapshot else {
            menu.label("Aguardando a primeira coleta");
            return;
        };
        for (position, gpu) in snapshot.gpus.iter().enumerate() {
            let Some(info) = snapshot
                .gpu_devices
                .iter()
                .find(|info| info.index == gpu.index)
            else {
                continue;
            };
            menu.group(&info.label().replace('&', "&&"), |items| {
                for (base, metric) in gpu.metrics.iter().enumerate() {
                    let index = if !info.persistent && position == 0 && self.board.contains(base) {
                        Some(base)
                    } else {
                        self.board.gpu_sensor_slot(&info.id, base)
                    };
                    if let Some(index) = index.filter(|&index| index < MERGE_FIRST - SENSOR_FIRST)
                        && (gpu.has_sensor(base) || self.board.contains(index))
                    {
                        items.item(
                            SENSOR_FIRST + index,
                            metric.name,
                            self.board.contains(index),
                        );
                    }
                }
                if let Some(status) = gpu.gpu_video_status {
                    items.label(status);
                }
            });
        }
        let inactive: Vec<_> = self
            .board
            .panels
            .iter()
            .flatten()
            .copied()
            .filter(|&slot| {
                self.board.gpu_sensor(slot).is_some_and(|(id, _)| {
                    !snapshot.gpu_devices.iter().any(|info| {
                        info.id == id && snapshot.gpus.iter().any(|gpu| gpu.index == info.index)
                    })
                })
            })
            .collect();
        if !inactive.is_empty() {
            menu.group("Gráficos de GPUs inativas", |items| {
                for index in inactive {
                    if index < MERGE_FIRST - SENSOR_FIRST
                        && let Some(name) = self.series_title(snapshot, index)
                    {
                        items.item(SENSOR_FIRST + index, &name, true);
                    }
                }
            });
        }
        let mut source = "";
        for (index, metric) in snapshot.metrics.iter().enumerate().skip(CPU_USAGE_INDEX) {
            let checked = self.board.contains(index);
            if metric.source != source {
                source = metric.source;
                if index != CPU_USAGE_INDEX || !snapshot.gpus.is_empty() {
                    menu.separator();
                }
                menu.label(source);
            }
            if index == CUSTOM_PING_INDEX {
                menu.group(metric.name, |items| {
                    items.item(SENSOR_FIRST + index, "Mostrar gráfico", checked);
                    items.item(EDIT_PING_TARGETS, "Editar endereços…", false);
                    if self.board.ping_targets.is_empty() {
                        items.label("Adicione endereços para começar");
                    } else if checked {
                        for status in &snapshot.custom_ping_status {
                            items.label(status);
                        }
                    }
                });
                continue;
            }
            menu.item(SENSOR_FIRST + index, metric.name, checked);
            let status = match index {
                CPU_TEMPERATURE_INDEX => snapshot.cpu_temperature_status.as_deref(),
                CPU_POWER_INDEX => snapshot.cpu_power_status.as_deref(),
                GAME_LATENCY_INDEX if checked => snapshot.game_latency_status.as_deref(),
                _ => None,
            };
            if let Some(reason) = status {
                menu.label(reason);
            }
        }
        menu.separator();
        menu.item(RESET_SENSORS, "Restaurar seleção inicial", false);
    }

    fn chart_metric<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        index: usize,
        selected: &[Selection],
    ) -> Option<(&'a crate::watch::MetricSnapshot, Option<&'a str>)> {
        let (info, base) = if let Some((id, base)) = self.board.gpu_sensor(index) {
            (snapshot.gpu_devices.iter().find(|gpu| gpu.id == id), base)
        } else if index < CPU_USAGE_INDEX {
            let primary = snapshot.gpus.first();
            (
                primary.and_then(|primary| {
                    snapshot
                        .gpu_devices
                        .iter()
                        .find(|gpu| gpu.index == primary.index)
                }),
                index,
            )
        } else {
            return snapshot.metrics.get(index).map(|metric| (metric, None));
        };
        let current = info.filter(|info| {
            if selected.is_empty() {
                snapshot.gpu_selections.is_empty()
            } else {
                selected
                    .iter()
                    .any(|selection| info.matches_selection(selection))
            }
        });
        let metric = current
            .and_then(|info| snapshot.gpus.iter().find(|gpu| gpu.index == info.index))
            .and_then(|gpu| gpu.metrics.get(base))
            .or_else(|| snapshot.metrics.get(base))?;
        Some((
            metric,
            Some(info.map_or("GPU indisponível", |info| info.short_name.as_str())),
        ))
    }

    fn series_title(&self, snapshot: &Snapshot, index: usize) -> Option<String> {
        let (metric, label) = self.chart_metric(snapshot, index, &snapshot.gpu_selections)?;
        Some(label.map_or_else(
            || metric.name.to_owned(),
            |label| format!("{label} · {}", metric.name),
        ))
    }

    fn panel_label(&self, snapshot: &Snapshot, indices: &[usize]) -> String {
        let mut names = indices
            .iter()
            .filter_map(|&index| self.series_title(snapshot, index));
        let mut label = names.next().unwrap_or_else(|| "Painel".into());
        if let Some(second) = names.next() {
            label.push_str(" + ");
            label.push_str(&second);
            let remaining = names.count();
            if remaining != 0 {
                label.push_str(&format!(" +{remaining}"));
            }
        }
        label
    }

    fn frame_menu(&self, menu: &Menu, processes: &[Process]) {
        let target = self.monitor.frame_target();
        let status = if target == FrameTarget::Off {
            "Captura desativada"
        } else {
            self.snapshot
                .as_ref()
                .filter(|snapshot| snapshot.frame_target == target)
                .and_then(|snapshot| snapshot.frame_status)
                .unwrap_or("Aguardando a coleta de quadros")
        };
        menu.label(status);
        menu.item_icon(
            FPS_CEILING_EDIT,
            &format!("Teto do gráfico · {} FPS…", self.board.fps_ceiling),
            Icon::Fps,
        );
        if let Some(snapshot) = &self.snapshot {
            menu.separator();
            menu.label("Gráficos exibidos");
            for index in FRAME_SENSORS {
                if let Some(metric) = snapshot.metrics.get(index) {
                    menu.item(
                        SENSOR_FIRST + index,
                        metric.name,
                        self.board.contains(index),
                    );
                }
            }
        }
        menu.separator();
        menu.label("Captura por aplicativo");
        menu.item(
            FRAME_TARGET_FOREGROUND,
            "Aplicativo em foco",
            target == FrameTarget::Foreground,
        );
        menu.item(FRAME_TARGET_OFF, "Desativado", target == FrameTarget::Off);
        if processes.is_empty() {
            menu.label("Nenhum aplicativo visível");
        }
        for (index, process) in processes.iter().enumerate() {
            let name = process.name.replace('&', "&&");
            menu.item(
                FRAME_TARGET_FIRST + index,
                &format!("{name} (PID {})", process.pid),
                target == FrameTarget::Process(process.pid),
            );
        }
    }

    fn panel_menu(&self, menu: &Menu, snapshot: &Snapshot, panel: usize, contextual_colors: bool) {
        menu.group_icon("Mover painel para", Icon::Move, |positions| {
            if self.page_count() == 1 {
                self.position_menu(positions, snapshot, panel, 0);
            } else {
                for page in 0..self.page_count() {
                    positions.group(&format!("Página {}", page + 1), |items| {
                        self.position_menu(items, snapshot, panel, page);
                    });
                }
            }
        });
        let targets: Vec<_> = self
            .board
            .panels
            .iter()
            .enumerate()
            .filter(|(index, _)| self.board.can_merge(*index, panel))
            .collect();
        if !targets.is_empty() {
            menu.group_icon("Mesclar com", Icon::Layers, |items| {
                for (index, target) in targets {
                    items.item(
                        MERGE_FIRST + index,
                        &self.panel_label(snapshot, target),
                        false,
                    );
                }
            });
        }
        let series = &self.board.panels[panel];
        if series.len() > 1 {
            menu.item_icon(SPLIT_PANEL, "Separar gráficos", Icon::Split);
        }
        if contextual_colors {
            menu.group_icon("Cor da curva", Icon::Color, |items| {
                self.curve_color_items(items, snapshot, series);
            });
        }
        if series
            .iter()
            .any(|&index| self.board.color(index).is_some())
        {
            menu.item_icon(RESET_COLORS, "Restaurar cores padrão", Icon::Undo);
        }
    }

    fn curve_color_items(&self, menu: &Menu, snapshot: &Snapshot, series: &[usize]) {
        for (position, &index) in series.iter().enumerate() {
            if index < MOVE_PANEL_FIRST - COLOR_FIRST
                && let Some(name) = self.series_title(snapshot, index)
            {
                menu.item_swatch(
                    COLOR_FIRST + index,
                    &name,
                    self.series_color(index, position, series.len()),
                );
            }
        }
    }

    fn position_menu(&self, menu: &Menu, snapshot: &Snapshot, source: usize, page: usize) {
        let start = page * self.slots();
        let end = (start + self.slots()).min(MAX_SLOTS);
        for target in start..end {
            let slot = target - start;
            let row = slot / self.board.columns + 1;
            let column = slot % self.board.columns + 1;
            let description = if target == source {
                "Painel atual".into()
            } else if let Some(panel) = self
                .board
                .panels
                .get(target)
                .filter(|panel| !panel.is_empty())
            {
                self.panel_label(snapshot, panel)
            } else {
                "Espaço livre".into()
            };
            let title = format!("Linha {row}, coluna {column} · {description}");
            if target == source {
                menu.label(&title);
            } else {
                menu.item(MOVE_PANEL_FIRST + target, &title, false);
            }
        }
    }

    fn series_color(&self, sensor: usize, position: usize, series_count: usize) -> COLORREF {
        let index = if series_count == 1 { sensor } else { position };
        self.board
            .color(sensor)
            .unwrap_or(SERIES[index % SERIES.len()])
    }

    fn content_bounds(&self, mut bounds: RECT) -> RECT {
        if self.board.appearance.translucent() && self.input_strip.is_some() {
            bounds.top = (bounds.top + input_strip::HEIGHT).min(bounds.bottom);
        }
        bounds
    }

    fn layout(&self, bounds: RECT) -> Layout {
        let mut bounds = self.content_bounds(bounds);
        if self.scroll_limit() > 0 {
            bounds.right = scrollbar_bounds(bounds, self.dpi).left;
        }
        Layout::new(bounds, self.board.columns, self.board.rows)
    }

    fn panel_at(&self, bounds: RECT, point: POINT) -> Option<usize> {
        self.layout(bounds).panel_at(self.first_slot(), point)
    }

    fn paint_opaque(&mut self, front: HDC, bounds: RECT) {
        if width(bounds) <= 0 || height(bounds) <= 0 {
            return;
        }
        let dc = self.buffer.target(front, width(bounds), height(bounds));
        self.draw_scene(dc, bounds, self.board.appearance.theme.palette());
        if dc != front {
            unsafe {
                BitBlt(
                    front,
                    0,
                    0,
                    width(bounds),
                    height(bounds),
                    dc,
                    0,
                    0,
                    SRCCOPY,
                );
            }
        }
    }

    fn present_layered(&mut self, hwnd: HWND, reference: HDC, bounds: RECT) -> bool {
        let mut surface = std::mem::take(&mut self.surface);
        let result = self.render_surface(&mut surface, reference, bounds)
            && surface.present(
                hwnd,
                None,
                SIZE {
                    cx: width(bounds),
                    cy: height(bounds),
                },
                POINT::default(),
            );
        self.surface = surface;
        result
    }

    fn render_surface(&self, surface: &mut Surface, reference: HDC, bounds: RECT) -> bool {
        let Some(dc) = surface.target(reference, width(bounds), height(bounds)) else {
            return false;
        };
        let mut palette = self.board.appearance.layered_palette();
        let notice_colors = self.board.appearance.theme.menu_colors();
        let alpha = ((u16::from(self.board.appearance.opacity) * 255 + 50) / 100) as u8;
        let foreground = |color| {
            [palette.border, palette.text, palette.muted, palette.grid].contains(&color)
                || palette.text_outline == Some(color)
                || [
                    notice_colors.background,
                    notice_colors.border,
                    notice_colors.text,
                ]
                .contains(&color)
                || SERIES.contains(&color)
                || self
                    .board
                    .panels
                    .iter()
                    .flatten()
                    .any(|&sensor| self.board.color(sensor) == Some(color))
        };
        let Some(background) = Background::new(palette.background, alpha, |color| {
            color == palette.plot || foreground(color)
        }) else {
            return false;
        };
        let Some(plot) = Background::new(palette.plot, alpha, |color| {
            color == background.marker || color == palette.background || foreground(color)
        }) else {
            return false;
        };
        let grid = if palette.text_outline.is_some() {
            let Some(grid) = Background::new(palette.grid, alpha.max(76), |color| {
                color == background.marker || color == plot.marker || foreground(color)
            }) else {
                return false;
            };
            palette.grid = grid.marker;
            Some(grid)
        } else {
            None
        };
        palette.background = background.marker;
        palette.plot = plot.marker;
        self.draw_scene(dc, bounds, palette);
        let opaque = self.paint_scrollbar(dc);
        if let Some(grid) = grid {
            surface.finish([background, plot, grid], opaque);
        } else {
            surface.finish([background, plot], opaque);
        }
        true
    }

    fn paint_scrollbar(&self, dc: HDC) -> Option<RECT> {
        let [x, y, w, h] = self.scrollbar_state?.position;
        let rect = area(x, y, x + w, y + h);
        unsafe {
            let saved = SaveDC(dc);
            if saved == 0 {
                return None;
            }
            IntersectClipRect(dc, rect.left, rect.top, rect.right, rect.bottom);
            SetViewportOrgEx(dc, x, y, null_mut());
            SendMessageW(
                self.scrollbar,
                WM_PRINTCLIENT,
                dc as WPARAM,
                (PRF_CLIENT | PRF_ERASEBKGND) as LPARAM,
            );
            RestoreDC(dc, saved);
        }
        Some(rect)
    }

    fn draw_scene(&self, dc: HDC, bounds: RECT, palette: Palette) {
        let background = unsafe { CreateSolidBrush(palette.background) };
        if !background.is_null() {
            unsafe {
                FillRect(dc, &bounds, background);
                DeleteObject(background);
            }
        }
        let layout = self.layout(bounds);
        let now = Instant::now();
        if let Some(snapshot) = &self.snapshot
            && !self.board.panels.is_empty()
        {
            let at = if self.snapshot_is_stale(now) {
                now
            } else {
                snapshot.at
            };
            let period = Duration::from_secs(PERIODS[self.board.period].0);
            let frames_current = snapshot.frame_target == self.monitor.frame_target();
            let selected = self.monitor.gpu_selections();
            let mut series = Vec::new();
            for (slot, panel) in self
                .board
                .panels
                .iter()
                .skip(self.first_slot())
                .take(layout.slots())
                .enumerate()
            {
                if panel.is_empty() {
                    continue;
                }
                series.clear();
                if panel.as_slice() == [CUSTOM_PING_INDEX]
                    && !snapshot.custom_ping_metrics.is_empty()
                {
                    series.extend(snapshot.custom_ping_metrics.iter().enumerate().map(
                        |(position, metric)| {
                            let color = self
                                .board
                                .color(CUSTOM_PING_INDEX)
                                .filter(|_| position == 0)
                                .unwrap_or(SERIES[position]);
                            (metric, color, None)
                        },
                    ));
                } else {
                    series.extend(panel.iter().enumerate().filter_map(|(position, &sensor)| {
                        if !frames_current && FRAME_SENSORS.contains(&sensor) {
                            return None;
                        }
                        let (metric, gpu) = self.chart_metric(snapshot, sensor, &selected)?;
                        Some((
                            metric,
                            self.series_color(sensor, position, panel.len()),
                            gpu,
                        ))
                    }));
                }
                chart::paint(
                    dc,
                    layout.card(slot),
                    self.dpi,
                    &series,
                    at,
                    period,
                    chart::PaintStyle {
                        fonts: &self.chart_fonts,
                        palette,
                        axis_ceiling: panel
                            .contains(&FRAME_SENSOR_START)
                            .then(|| f64::from(self.board.fps_ceiling.get())),
                    },
                );
            }
        } else {
            let hint = if self.snapshot.is_none() {
                "Aguardando a primeira coleta…"
            } else {
                "Clique com o botão direito para escolher sensores."
            };
            draw_text(dc, hint, bounds, palette.muted, palette.text_outline);
        }
        if let Some(drag) = &self.drag
            && drag.active
        {
            let first = self.first_slot();
            if let Some(source) = drag
                .source
                .checked_sub(first)
                .filter(|&slot| slot < layout.slots())
            {
                drag_outline(
                    dc,
                    layout.card(source),
                    scale(2, self.dpi),
                    chart::rgb(255, 182, 103),
                );
            }
            if let Some(target) = drag
                .target
                .filter(|&index| index != drag.source)
                .and_then(|index| index.checked_sub(first))
                .filter(|&slot| slot < layout.slots())
            {
                drag_outline(
                    dc,
                    layout.card(target),
                    scale(3, self.dpi),
                    chart::rgb(72, 215, 185),
                );
            }
        }
        if self.board.appearance.translucent() && self.input_strip.is_some() {
            let middle = bounds.left + width(bounds) / 2;
            let y = bounds.top + input_strip::HEIGHT / 2;
            let grip = area(
                (middle - 12).max(bounds.left),
                y,
                (middle + 12).min(bounds.right),
                y + 2,
            );
            let brush = unsafe { CreateSolidBrush(palette.text_outline.unwrap_or(palette.text)) };
            if !brush.is_null() {
                unsafe {
                    FillRect(dc, &grip, brush);
                    DeleteObject(brush);
                }
            }
        }
        if let Some(notice) = &self.update_notice
            && matches!(notice.visibility, NoticeVisibility::Showing(_))
        {
            self.paint_update_notice(dc, bounds, &notice.text());
        }
    }

    fn paint_update_notice(&self, dc: HDC, bounds: RECT, message: &str) {
        let colors = self.board.appearance.theme.menu_colors();
        let mut content = self.content_bounds(bounds);
        if self.scroll_limit() > 0 {
            content.right = scrollbar_bounds(content, self.dpi).left;
        }
        let margin = scale(12, self.dpi);
        let inset_x = margin + (bounds.right - content.right);
        let inset_y = margin + (content.top - bounds.top);
        let notice_width = (width(bounds) - 2 * inset_x).min(scale(460, self.dpi));
        let notice_height = (height(bounds) - 2 * inset_y).min(scale(34, self.dpi));
        if notice_width <= 0 || notice_height <= 0 {
            return;
        }
        let left = bounds.left + (width(bounds) - notice_width) / 2;
        let top = bounds.top + (height(bounds) - notice_height) / 2;
        let rect = area(left, top, left + notice_width, top + notice_height);
        unsafe {
            let brush = GetStockObject(DC_BRUSH);
            let previous = SetDCBrushColor(dc, colors.background);
            FillRect(dc, &rect, brush);
            SetDCBrushColor(dc, colors.border);
            FrameRect(dc, &rect, brush);
            SetDCBrushColor(dc, previous);
        }
        draw_text(
            dc,
            message,
            area(
                rect.left + margin,
                rect.top,
                rect.right - margin,
                rect.bottom,
            ),
            colors.text,
            None,
        );
    }
}

struct Layout {
    viewport: RECT,
    columns: usize,
    rows: usize,
}

impl Layout {
    fn new(bounds: RECT, columns: usize, rows: usize) -> Self {
        Self {
            viewport: bounds,
            columns,
            rows,
        }
    }

    fn slots(&self) -> usize {
        self.columns * self.rows
    }

    fn card(&self, slot: usize) -> RECT {
        const GAP: i32 = 1;
        let column = (slot % self.columns) as i32;
        let row = (slot / self.columns) as i32;
        let columns = self.columns as i32;
        let rows = self.rows as i32;
        let w = (width(self.viewport) - (columns - 1) * GAP).max(0);
        let h = (height(self.viewport) - (rows - 1) * GAP).max(0);
        let x = self.viewport.left + column * GAP;
        let y = self.viewport.top + row * GAP;
        area(
            x + w * column / columns,
            y + h * row / rows,
            x + w * (column + 1) / columns,
            y + h * (row + 1) / rows,
        )
    }

    fn panel_at(&self, first_slot: usize, point: POINT) -> Option<usize> {
        (0..self.slots())
            .find(|&slot| {
                let rect = self.card(slot);
                (rect.left..rect.right).contains(&point.x)
                    && (rect.top..rect.bottom).contains(&point.y)
            })
            .map(|slot| first_slot + slot)
            .filter(|&index| index < MAX_SLOTS)
    }
}

#[derive(Default)]
struct BackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    size: (i32, i32),
}

impl BackBuffer {
    fn target(&mut self, front: HDC, width: i32, height: i32) -> HDC {
        if self.size == (width, height) && !self.dc.is_null() {
            return self.dc;
        }
        let dc = unsafe { CreateCompatibleDC(front) };
        if dc.is_null() {
            return front;
        }
        let bitmap = unsafe { CreateCompatibleBitmap(front, width, height) };
        if bitmap.is_null() {
            unsafe { DeleteDC(dc) };
            return front;
        }
        let original = unsafe { SelectObject(dc, bitmap) };
        if original.is_null() {
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
            return front;
        }
        *self = Self {
            dc,
            bitmap,
            original,
            size: (width, height),
        };
        dc
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        if !self.dc.is_null() {
            unsafe {
                SelectObject(self.dc, self.original);
                DeleteObject(self.bitmap);
                DeleteDC(self.dc);
            }
        }
    }
}

struct OwnedWindow(HWND);

impl Drop for OwnedWindow {
    fn drop(&mut self) {
        if unsafe { IsWindow(self.0) } != 0 {
            unsafe { DestroyWindow(self.0) };
        }
    }
}

struct DpiContext(DPI_AWARENESS_CONTEXT);

impl Drop for DpiContext {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

fn preferred_work_area() -> Option<RECT> {
    let mut secondary: Option<RECT> = None;
    // SAFETY: a enumeração é síncrona e escreve em secondary somente durante a chamada.
    unsafe {
        if GetSystemMetrics(SM_CMONITORS) > 1 {
            EnumDisplayMonitors(
                null_mut(),
                std::ptr::null(),
                Some(find_secondary),
                (&mut secondary as *mut Option<RECT>) as LPARAM,
            );
        }
        secondary.or_else(|| {
            let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            monitor_info(primary).map(|info| info.rcWork)
        })
    }
}

unsafe extern "system" fn find_secondary(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    let Some(info) = monitor_info(monitor) else {
        return 1;
    };
    let work = info.rcWork;
    // A enumeração pode incluir superfícies de espelhamento que não hospedam janelas.
    if info.dwFlags & MONITORINFOF_PRIMARY != 0
        || width(work) <= 0
        || height(work) <= 0
        || unsafe { MonitorFromRect(&work, MONITOR_DEFAULTTONULL) } != monitor
    {
        return 1;
    }
    // SAFETY: o parâmetro aponta para o resultado vivo de preferred_work_area.
    unsafe { *(data as *mut Option<RECT>) = Some(work) };
    0
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then_some(info)
}

fn initial_bounds(work_area: Option<RECT>, dpi: u32) -> RECT {
    let Some(work) = work_area else {
        return area(48, 48, 48 + scale(1200, dpi), 48 + scale(420, dpi));
    };
    let w = scale(1920, dpi).min(width(work) * 9 / 10);
    let h = scale(420, dpi).min(height(work) * 9 / 10);
    let x = work.left + (width(work) - w) / 2;
    let y = work.top + (height(work) - h) / 2;
    area(x, y, x + w, y + h)
}

fn window_placement(hwnd: HWND) -> Option<WINDOWPLACEMENT> {
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    (unsafe { GetWindowPlacement(hwnd, &mut placement) } != 0).then_some(placement)
}

fn restore_window_bounds(hwnd: HWND, [left, top, right, bottom]: [i32; 4]) -> bool {
    let Some(mut placement) = window_placement(hwnd) else {
        return false;
    };
    placement.showCmd = SW_HIDE as u32;
    placement.rcNormalPosition = area(left, top, right, bottom);
    // O Windows usa as coordenadas da área de trabalho e ajusta posições fora dos monitores.
    unsafe { SetWindowPlacement(hwnd, &placement) != 0 }
}

fn position_window(hwnd: HWND, rect: RECT) {
    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            rect.left,
            rect.top,
            width(rect),
            height(rect),
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

fn dropdown_active(state: &RefCell<Window>) -> bool {
    state.try_borrow().is_ok_and(|app| app.dropdown.active())
}

fn refresh_tray(hwnd: HWND, state: &RefCell<Window>) {
    let (available, was_available, dropdown) = {
        let Ok(mut app) = state.try_borrow_mut() else {
            return;
        };
        let Some(tray) = app.tray.as_mut() else {
            return;
        };
        let was_available = tray.available();
        let available = tray.register();
        let dropdown = app.dropdown.active();
        (available, was_available, dropdown)
    };

    unsafe {
        if available {
            KillTimer(hwnd, TRAY_RETRY_TIMER);
        } else {
            SetTimer(hwnd, TRAY_RETRY_TIMER, 3_000, None);
        }
        if dropdown {
            return;
        }
        let visible = IsWindowVisible(hwnd) != 0;
        if available && !was_available && visible && IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
        } else if !available && was_available && !visible {
            ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
        }
    }
}

fn reveal_panel(hwnd: HWND, state: &RefCell<Window>) {
    if unsafe { IsWindowEnabled(hwnd) } == 0 {
        unsafe { SetForegroundWindow(GetLastActivePopup(hwnd)) };
        return;
    }
    if dropdown_active(state) {
        dropdown_mode::reveal(hwnd, state, false);
        return;
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else if IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd);
    }
}

fn handle_tray_event(hwnd: HWND, state: &RefCell<Window>, wp: WPARAM, lp: LPARAM) {
    let event = state.try_borrow().ok().and_then(|app| {
        if app.open_menu.is_some() {
            return None;
        }
        app.tray.as_ref()?.event(wp, lp)
    });
    match event {
        Some(tray::Event::Show) => reveal_panel(hwnd, state),
        Some(tray::Event::Menu(point)) if unsafe { IsWindowEnabled(hwnd) } != 0 => {
            show_tray_menu(hwnd, state, point);
        }
        _ => {}
    }
}

fn show_tray_menu(hwnd: HWND, state: &RefCell<Window>, point: POINT) {
    let Some(menu) = Menu::new() else { return };
    menu.item_icon(TRAY_SHOW, "Mostrar ARGOS", Icon::Restore);
    menu.item_icon(CLOSE_WINDOW, "Fechar ARGOS", Icon::Close);
    let (theme, dpi) = {
        let mut app = state.borrow_mut();
        app.open_menu = Some(Rc::new(OpenMenu {
            root: menu.0,
            section: MenuSection::All,
            processes: Vec::new(),
            displays: Vec::new(),
            panel_sensors: Vec::new(),
        }));
        (app.board.appearance.theme, app.dpi)
    };
    unsafe { SetForegroundWindow(hwnd) };
    let choice = menu.track(hwnd, point, |_| false, theme, dpi);
    state.borrow_mut().open_menu = None;
    unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) };
    match choice {
        TRAY_SHOW => reveal_panel(hwnd, state),
        CLOSE_WINDOW => {
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        }
        _ => {
            if let Some(tray) = &state.borrow().tray {
                tray.return_focus();
            }
        }
    }
}

fn show_normal(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if IsWindowVisible(hwnd) == 0 {
            // O launcher pode substituir a primeira chamada a ShowWindow.
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }
}

fn client(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) };
    rect
}

fn invalidate(hwnd: HWND) {
    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
}

fn draw_text(dc: HDC, text: &str, bounds: RECT, color: COLORREF, outline: Option<COLORREF>) {
    unsafe {
        let previous = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        SetBkMode(dc, TRANSPARENT as i32);
        chart::text(
            dc,
            (bounds.left, bounds.top, bounds.right, bounds.bottom),
            text,
            color,
            outline,
            DT_CENTER,
        );
        SelectObject(dc, previous);
    }
}

fn drag_outline(dc: HDC, rect: RECT, thickness: i32, color: COLORREF) {
    let brush = unsafe { CreateSolidBrush(color) };
    if brush.is_null() {
        return;
    }
    let thickness = thickness.min(width(rect) / 2).min(height(rect) / 2);
    for inset in 0..thickness {
        let border = area(
            rect.left + inset,
            rect.top + inset,
            rect.right - inset,
            rect.bottom - inset,
        );
        unsafe { FrameRect(dc, &border, brush) };
    }
    unsafe { DeleteObject(brush) };
}

fn area(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}
fn width(rect: RECT) -> i32 {
    rect.right - rect.left
}
fn height(rect: RECT) -> i32 {
    rect.bottom - rect.top
}
fn scale(dip: i32, dpi: u32) -> i32 {
    (dip * dpi as i32 + 48) / 96
}
fn wide(label: &str) -> Vec<u16> {
    label.encode_utf16().chain(std::iter::once(0)).collect()
}
fn window_error(action: &str) -> String {
    format!("falha ao {action} ({})", io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::w;

    #[test]
    fn explicit_grid_keeps_its_capacity_at_different_sizes() {
        for (width, height) in [(510, 380), (1719, 409), (2573, 709)] {
            let layout = Layout::new(area(0, 0, width, height), 6, 2);
            assert_eq!(layout.slots(), 12);
            assert_eq!(layout.card(0).left, layout.viewport.left);
            assert_eq!(layout.card(11).right, layout.viewport.right);
            assert_eq!(layout.card(11).bottom, layout.viewport.bottom);
            assert!(layout.card(5).right > layout.card(5).left);
            assert_eq!(layout.card(1).left - layout.card(0).right, 1);
        }
        assert_eq!(Layout::new(area(0, 0, 800, 480), 6, 4).slots(), 24);
    }

    #[test]
    fn panel_hit_uses_scroll_offset_and_skips_grid_gaps() {
        let layout = Layout::new(area(0, 0, 1118, 419), 3, 2);
        let card = layout.card(4);
        let center = POINT {
            x: (card.left + card.right) / 2,
            y: (card.top + card.bottom) / 2,
        };
        assert_eq!(layout.panel_at(0, center), Some(4));
        assert_eq!(layout.panel_at(3, center), Some(7));
        let gap = POINT {
            x: card.right,
            y: center.y,
        };
        assert_eq!(layout.panel_at(3, gap), None);
    }

    #[test]
    fn drag_ignores_a_click_and_clears_the_target_outside_the_grid() {
        let mut drag = PanelDrag {
            source: 2,
            origin: POINT { x: 40, y: 50 },
            target: Some(2),
            active: false,
        };
        assert!(!drag.update(POINT { x: 41, y: 51 }, Some(3), 4));
        assert!(!drag.active);
        assert!(drag.update(POINT { x: 45, y: 51 }, Some(3), 4));
        assert!(drag.active);
        assert!(drag.update(POINT { x: -1, y: 51 }, None, 4));
        assert_eq!(drag.target, None);
    }

    #[test]
    fn window_drag_requests_native_move_at_the_screen_point() {
        use std::cell::Cell;
        use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

        unsafe extern "system" fn observe_move(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
            _id: usize,
            data: usize,
        ) -> LRESULT {
            if msg == WM_SYSCOMMAND && wp as u32 & 0xfff0 == SC_MOVE {
                // Stop before the interactive loop to keep the test independent of mouse input.
                unsafe { &*(data as *const Cell<Option<LPARAM>>) }.set(Some(lp));
                return 0;
            }
            unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
        }

        let observed = Cell::new(None::<LPARAM>);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("STATIC"),
                null(),
                WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX,
                -300,
                50,
                320,
                200,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!hwnd.is_null());
        let _guard = OwnedWindow(hwnd);
        assert_ne!(
            unsafe { SetWindowSubclass(hwnd, Some(observe_move), 1, &raw const observed as usize) },
            0
        );
        let press = POINT { x: 25, y: 35 };
        let mut screen = press;
        assert_ne!(unsafe { ClientToScreen(hwnd, &mut screen) }, 0);
        begin_window_move(hwnd, press);
        let actual = message_point(observed.get().expect("comando nativo de movimentação"));
        assert_eq!((actual.x, actual.y), (screen.x, screen.y));
    }

    #[test]
    fn last_visible_rows_have_no_targets_past_the_board_limit() {
        let layout = Layout::new(area(0, 0, 1118, 419), 3, 2);
        let first = (MAX_SLOTS.div_ceil(layout.columns) - layout.rows) * layout.columns;
        let last_slot = MAX_SLOTS - 1 - first;
        let last = layout.card(last_slot);
        let invalid = layout.card(last_slot + 1);
        assert_eq!(
            layout.panel_at(
                first,
                POINT {
                    x: last.left,
                    y: last.top
                }
            ),
            Some(MAX_SLOTS - 1)
        );
        assert_eq!(
            layout.panel_at(
                first,
                POINT {
                    x: invalid.left,
                    y: invalid.top
                }
            ),
            None
        );
    }

    #[test]
    fn native_resize_hit_supports_negative_monitor_coordinates() {
        let bounds = area(-1366, -30, 0, 738);
        assert_eq!(resize_hit(bounds, POINT { x: -1364, y: -29 }, 5), HTTOPLEFT);
        assert_eq!(
            resize_hit(bounds, POINT { x: -2, y: 736 }, 5),
            HTBOTTOMRIGHT
        );
        assert_eq!(resize_hit(bounds, POINT { x: -3, y: 500 }, 5), HTRIGHT);
        assert_eq!(resize_hit(bounds, POINT { x: -680, y: 400 }, 5), HTCLIENT);
    }

    #[test]
    fn saved_placement_restores_a_hidden_window_without_activating_it() {
        let work = preferred_work_area().expect("monitor work area");
        let (x, y) = (work.left + 20, work.top + 20);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("STATIC"),
                null(),
                WS_POPUP | WS_THICKFRAME,
                x,
                y,
                320,
                200,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!hwnd.is_null());
        let _guard = OwnedWindow(hwnd);
        let normal = window_placement(hwnd).unwrap().rcNormalPosition;
        let offset = 30;
        assert!(restore_window_bounds(
            hwnd,
            [
                normal.left + offset,
                normal.top + offset,
                normal.right + offset,
                normal.bottom + offset,
            ],
        ));
        let mut restored = RECT::default();
        assert_ne!(unsafe { GetWindowRect(hwnd, &mut restored) }, 0);
        assert_eq!((restored.left, restored.top), (x + offset, y + offset));
        assert_eq!((width(restored), height(restored)), (320, 200));
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);
        assert_ne!(unsafe { GetForegroundWindow() }, hwnd);
    }
}
