use std::cell::RefCell;
use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW,
};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, VK_END, VK_ESCAPE, VK_F2, VK_F3, VK_F4, VK_HOME,
    VK_NEXT, VK_PRIOR,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use crate::frames::{Process, SENSOR_COUNT, visible_processes};
use crate::watch::{
    CPU_TEMPERATURE_INDEX, FRAME_SENSOR_START, GUI_INTERVALS_MS, Monitor, Snapshot,
};

mod board;
mod chart;

use board::{Board, MAX_COLUMNS, MAX_ROWS, MAX_SLOTS};

const CLASS: *const u16 = w!("ArgosMonitorWindow");
const TITLE: *const u16 = w!("ARGOS — Monitor de hardware");
const TIMER: usize = 1;
const PERIOD_FIRST: usize = 201;
const RESET_SENSORS: usize = 205;
const COLUMNS_FIRST: usize = 301;
const ROWS_FIRST: usize = 311;
const SPLIT_PANEL: usize = 321;
const RESET_COLORS: usize = 322;
const MOVE_WINDOW: usize = 331;
const SIZE_WINDOW: usize = 332;
const MINIMIZE_WINDOW: usize = 333;
const MAXIMIZE_WINDOW: usize = 334;
const CLOSE_WINDOW: usize = 335;
const REFRESH_FIRST: usize = 401;
const PAGE_FIRST: usize = 501;
const SENSOR_FIRST: usize = 1000;
const MERGE_FIRST: usize = 2000;
const COLOR_FIRST: usize = 3000;
const MOVE_PANEL_FIRST: usize = 4000;
const FRAME_TARGET_OFF: usize = 5000;
const FRAME_TARGET_FIRST: usize = 5001;
const FRAME_SENSORS: std::ops::Range<usize> = FRAME_SENSOR_START..FRAME_SENSOR_START + SENSOR_COUNT;
const PERIODS: [(u64, &str); 4] = [(30, "30 s"), (60, "1 min"), (120, "2 min"), (300, "5 min")];
const DEFAULT_METRICS: [usize; 12] = [0, 1, 2, 5, 6, 7, 11, 12, 13, 14, 16, 17];
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

pub(crate) fn run(gpu: Option<u32>) -> Result<(), String> {
    let saved_board = Board::load();
    let restored = saved_board.is_some();
    let board = saved_board.unwrap_or_else(|| Board::new(DEFAULT_METRICS));
    let refresh_ms = board.refresh_ms;
    let monitor = Monitor::start(gpu, refresh_ms)?;
    let _dpi_context = DpiContext(unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    });
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let state = Box::new(RefCell::new(Window::new(monitor, board, restored, dpi)));
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        lpszClassName: CLASS,
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
            CLASS,
            TITLE,
            WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX,
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
    let window_dpi = unsafe { GetDpiForWindow(hwnd) };
    if window_dpi != 0 && window_dpi != dpi {
        state.borrow_mut().set_dpi(window_dpi);
        position_window(hwnd, initial_bounds(work_area, window_dpi));
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

    let mut message = MSG::default();
    loop {
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(window_error("processar mensagens da janela")),
            0 => break,
            _ => {}
        }
        if message.message == WM_KEYDOWN && keyboard(hwnd, &state, message.wParam as u16) {
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
        match msg {
            WM_NCHITTEST => {
                let mut bounds = RECT::default();
                let dpi = state.try_borrow().map_or(96, |app| app.dpi);
                if IsZoomed(hwnd) != 0 || GetWindowRect(hwnd, &mut bounds) == 0 {
                    HTCLIENT as LRESULT
                } else {
                    resize_hit(bounds, message_point(lp), scale(5, dpi)) as LRESULT
                }
            }
            WM_GETMINMAXINFO => {
                let dpi = state.try_borrow().map_or(96, |app| app.dpi);
                let info = &mut *(lp as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: scale(510, dpi),
                    y: scale(200, dpi),
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
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                if !dc.is_null()
                    && IsIconic(hwnd) == 0
                    && let Ok(mut app) = state.try_borrow_mut()
                {
                    app.paint(dc, client(hwnd));
                }
                EndPaint(hwnd, &paint);
                0
            }
            WM_ERASEBKGND => 1,
            WM_SIZE => {
                cancel_drag(hwnd, state);
                if wp as u32 != SIZE_MINIMIZED {
                    invalidate(hwnd);
                }
                0
            }
            WM_TIMER if wp == TIMER => {
                if IsIconic(hwnd) == 0
                    && let Ok(mut app) = state.try_borrow_mut()
                {
                    let redraw = app.refresh() || app.snapshot_is_stale(Instant::now());
                    drop(app);
                    if redraw {
                        invalidate(hwnd);
                    }
                }
                0
            }
            WM_MOUSEWHEEL => {
                navigate(hwnd, state, PageMove::Wheel((wp >> 16) as i16));
                0
            }
            WM_LBUTTONDOWN => {
                start_drag(hwnd, state, message_point(lp));
                0
            }
            WM_MOUSEMOVE => {
                update_drag(hwnd, state, message_point(lp));
                0
            }
            WM_LBUTTONUP => {
                finish_drag(hwnd, state, message_point(lp));
                0
            }
            WM_CAPTURECHANGED => {
                clear_drag(hwnd, state);
                0
            }
            WM_CANCELMODE => {
                cancel_drag(hwnd, state);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_CONTEXTMENU | WM_NCRBUTTONUP => {
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
        let Some(source) = app.layout(client(hwnd)).panel_at(app.page, point) else {
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
        let target = app.layout(client(hwnd)).panel_at(app.page, point);
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
        let target = app.layout(client(hwnd)).panel_at(app.page, point);
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
        VK_ESCAPE => return cancel_drag(hwnd, state),
        VK_F2 => MenuSection::Sensors,
        VK_F3 => MenuSection::Period,
        VK_F4 => MenuSection::Grid,
        VK_PRIOR | VK_NEXT | VK_HOME | VK_END => {
            let step = match key {
                VK_PRIOR => PageMove::Previous,
                VK_NEXT => PageMove::Next,
                VK_HOME => PageMove::First,
                _ => PageMove::Last,
            };
            navigate(hwnd, state, step);
            return true;
        }
        _ => return false,
    };
    cancel_drag(hwnd, state);
    show_options(hwnd, state, menu_center(hwnd), section);
    true
}

enum PageMove {
    Previous,
    Next,
    First,
    Last,
    Wheel(i16),
}

fn navigate(hwnd: HWND, state: &RefCell<Window>, step: PageMove) {
    let changed = state.try_borrow_mut().is_ok_and(|mut app| {
        if app.drag.is_some() {
            return false;
        }
        let last = app.page_count() - 1;
        let page = match step {
            PageMove::Previous => app.page.saturating_sub(1),
            PageMove::Next => (app.page + 1).min(last),
            PageMove::First => 0,
            PageMove::Last => last,
            PageMove::Wheel(delta) => {
                let motion = app.wheel_remainder + i32::from(delta);
                let pages = motion / WHEEL_DELTA as i32;
                app.wheel_remainder = motion % WHEEL_DELTA as i32;
                app.page.saturating_add_signed(-pages as isize).min(last)
            }
        };
        let changed = page != app.page;
        app.page = page;
        changed
    });
    if changed {
        invalidate(hwnd);
    }
}

enum MenuSection {
    All,
    Sensors,
    Period,
    Grid,
}

fn show_options(hwnd: HWND, state: &RefCell<Window>, screen: POINT, section: MenuSection) {
    let processes = if matches!(section, MenuSection::All) {
        visible_processes()
    } else {
        Vec::new()
    };
    let (menu, panel) = {
        let Ok(app) = state.try_borrow() else { return };
        let Some(menu) = Menu::new() else { return };
        match section {
            MenuSection::Sensors => app.sensor_menu(&menu),
            MenuSection::Period => app.period_menu(&menu),
            MenuSection::Grid => app.grid_menu(&menu),
            MenuSection::All => {
                if let Some(snapshot) = &app.snapshot {
                    if app.snapshot_is_stale(Instant::now()) {
                        menu.label("Coleta atrasada");
                    }
                    menu.label(&snapshot.notice);
                    menu.separator();
                }
                menu.group("Atualização", |items| {
                    for (index, &ms) in GUI_INTERVALS_MS.iter().enumerate() {
                        items.item(
                            REFRESH_FIRST + index,
                            &format!("{ms} ms"),
                            ms == app.board.refresh_ms,
                        );
                    }
                });
                menu.group("Histórico", |items| app.period_menu(items));
                menu.group("Sensores", |items| app.sensor_menu(items));
                menu.group("Quadros do jogo", |items| app.frame_menu(items, &processes));
                menu.group("Grade", |items| app.grid_menu(items));
                if app.page_count() > 1 {
                    menu.group("Página", |items| {
                        for page in 0..app.page_count() {
                            items.item(
                                PAGE_FIRST + page,
                                &format!("Página {}", page + 1),
                                page == app.page,
                            );
                        }
                    });
                }
                if app.save_failed {
                    menu.label("Configuração disponível nesta sessão");
                }
            }
        }
        let panel = if matches!(section, MenuSection::All) {
            let mut point = screen;
            if unsafe { ScreenToClient(hwnd, &mut point) } == 0 {
                None
            } else {
                app.layout(client(hwnd))
                    .panel_at(app.page, point)
                    .filter(|&index| app.panel_is_occupied(index))
            }
        } else {
            None
        };
        if let Some(index) = panel
            && let Some(snapshot) = &app.snapshot
        {
            menu.separator();
            app.panel_menu(&menu, snapshot, index);
        }
        if matches!(section, MenuSection::All) {
            menu.separator();
            if unsafe { IsZoomed(hwnd) } != 0 {
                menu.label("Mover");
                menu.label("Redimensionar");
                menu.item(MAXIMIZE_WINDOW, "Restaurar", false);
            } else {
                menu.item(MOVE_WINDOW, "Mover", false);
                menu.item(SIZE_WINDOW, "Redimensionar", false);
                menu.item(MAXIMIZE_WINDOW, "Maximizar", false);
            }
            menu.item(MINIMIZE_WINDOW, "Minimizar", false);
            menu.item(CLOSE_WINDOW, "Fechar", false);
        }
        (menu, panel)
    };

    // TrackPopupMenu e ChooseColorW despacham mensagens. Nenhum empréstimo vive aqui.
    let choice = unsafe {
        TrackPopupMenu(
            menu.0,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            screen.x,
            screen.y,
            0,
            hwnd,
            null(),
        )
    } as usize;
    match choice {
        0 => {}
        MOVE_WINDOW | SIZE_WINDOW => {
            let command = if choice == MOVE_WINDOW {
                SC_MOVE
            } else {
                SC_SIZE
            };
            unsafe { SendMessageW(hwnd, WM_SYSCOMMAND, command as WPARAM, -1) };
        }
        MINIMIZE_WINDOW => {
            unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
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
        choice if (PAGE_FIRST..SENSOR_FIRST).contains(&choice) => {
            if let Ok(mut app) = state.try_borrow_mut() {
                app.page = (choice - PAGE_FIRST).min(app.page_count() - 1);
            }
            invalidate(hwnd);
        }
        choice if (COLOR_FIRST..MOVE_PANEL_FIRST).contains(&choice) => {
            if let Some(panel) = panel {
                choose_series_color(hwnd, state, panel, choice - COLOR_FIRST);
            }
        }
        choice if choice >= FRAME_TARGET_OFF => {
            let pid = if choice == FRAME_TARGET_OFF {
                0
            } else if let Some(process) = processes.get(choice - FRAME_TARGET_FIRST) {
                process.pid
            } else {
                return;
            };
            if let Ok(mut app) = state.try_borrow_mut() {
                app.select_frame_target(pid);
            }
            invalidate(hwnd);
        }
        choice => edit_board(hwnd, state, |app| app.apply_selection(choice, panel)),
    }
}

fn choose_series_color(hwnd: HWND, state: &RefCell<Window>, panel: usize, sensor: usize) {
    let (initial, mut custom) = {
        let Ok(app) = state.try_borrow() else { return };
        let Some(panel) = app.board.panels.get(panel) else {
            return;
        };
        let Some(position) = panel.iter().position(|&index| index == sensor) else {
            return;
        };
        (
            app.series_color(sensor, position, panel.len()),
            app.custom_colors,
        )
    };
    let mut dialog = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: hwnd,
        rgbResult: initial,
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    if unsafe { ChooseColorW(&mut dialog) } != 0 {
        edit_board(hwnd, state, |app| {
            app.custom_colors = custom;
            app.board.set_color(sensor, Some(dialog.rgbResult))
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
    app.page = app.page.min(app.page_count() - 1);
    let interval = app.board.refresh_ms;
    if interval != old_interval {
        app.monitor.set_interval_ms(interval);
    }
    app.save_failed = app.board.save().is_err();
    drop(app);
    if interval != old_interval {
        unsafe { SetTimer(hwnd, TIMER, interval, None) };
    }
    invalidate(hwnd);
}

fn panel_label(snapshot: &Snapshot, metrics: &[usize]) -> String {
    let mut names = metrics
        .iter()
        .filter_map(|&index| snapshot.metrics.get(index))
        .map(|metric| metric.name);
    let mut label = names.next().unwrap_or("Painel").to_owned();
    if let Some(second) = names.next() {
        label.push_str(" + ");
        label.push_str(second);
        let remaining = names.count();
        if remaining != 0 {
            label.push_str(&format!(" +{remaining}"));
        }
    }
    label
}

struct Window {
    monitor: Monitor,
    snapshot: Option<Arc<Snapshot>>,
    board: Board,
    restored: bool,
    save_failed: bool,
    custom_colors: [COLORREF; 16],
    page: usize,
    wheel_remainder: i32,
    drag: Option<PanelDrag>,
    dpi: u32,
    chart_fonts: chart::ChartFonts,
    buffer: BackBuffer,
}

impl Window {
    fn new(monitor: Monitor, board: Board, restored: bool, dpi: u32) -> Self {
        Self {
            monitor,
            snapshot: None,
            board,
            restored,
            save_failed: false,
            custom_colors: SERIES,
            page: 0,
            wheel_remainder: 0,
            drag: None,
            dpi,
            chart_fonts: chart::ChartFonts::new(dpi),
            buffer: BackBuffer::default(),
        }
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi != 0 && dpi != self.dpi {
            self.dpi = dpi;
            self.chart_fonts = chart::ChartFonts::new(dpi);
        }
    }

    fn refresh(&mut self) -> bool {
        let Some(next) = self.monitor.snapshot() else {
            return false;
        };
        if self
            .snapshot
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &next))
        {
            return false;
        }
        if self.snapshot.is_none() && !self.restored {
            self.board
                .reset_selection(default_selection(&next).iter().copied());
            self.page = self.page.min(self.page_count() - 1);
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
        self.page = target / self.slots();
        true
    }

    fn snapshot_is_stale(&self, now: Instant) -> bool {
        let limit = Duration::from_millis((u64::from(self.board.refresh_ms) * 5 / 2).max(750));
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| now.saturating_duration_since(snapshot.at) >= limit)
    }

    fn select_frame_target(&mut self, pid: u32) {
        if self.monitor.frame_target() == pid {
            return;
        }
        self.monitor.set_frame_target(pid);
        if pid != 0
            && let Some(panel) = self
                .board
                .panels
                .iter()
                .position(|panel| panel.iter().any(|sensor| FRAME_SENSORS.contains(sensor)))
        {
            self.page = panel / self.slots();
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
        let first = self.page * self.slots();
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
            self.page = first / self.slots();
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
                    self.page = merged / self.slots();
                }
                return true;
            }
        }
        let Some(snapshot) = &self.snapshot else {
            return false;
        };
        if choice == RESET_SENSORS {
            self.board
                .reset_selection(default_selection(snapshot).iter().copied());
            self.page = 0;
            return true;
        }
        if let Some(index) = choice.checked_sub(SENSOR_FIRST)
            && snapshot.metrics.get(index).is_some()
        {
            self.board.toggle(index);
            if (FRAME_SENSORS.contains(&index) || index == CPU_TEMPERATURE_INDEX)
                && let Some(panel) = self
                    .board
                    .panels
                    .iter()
                    .position(|panel| panel.contains(&index))
            {
                self.page = panel / self.slots();
            }
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

    fn sensor_menu(&self, menu: &Menu) {
        let Some(snapshot) = &self.snapshot else {
            menu.label("Aguardando a primeira coleta");
            return;
        };
        let mut source = "";
        for (index, metric) in snapshot.metrics.iter().enumerate() {
            if metric.source != source {
                source = metric.source;
                if index != 0 {
                    menu.separator();
                }
                menu.label(source);
            }
            menu.item(
                SENSOR_FIRST + index,
                metric.name,
                self.board.contains(index),
            );
        }
        menu.separator();
        menu.item(RESET_SENSORS, "Restaurar seleção inicial", false);
    }

    fn frame_menu(&self, menu: &Menu, processes: &[Process]) {
        let target = self.monitor.frame_target();
        let status = if target == 0 {
            "Captura desativada"
        } else {
            self.snapshot
                .as_ref()
                .filter(|snapshot| snapshot.frame_target == target)
                .and_then(|snapshot| snapshot.frame_status)
                .unwrap_or("Aguardando a coleta de quadros")
        };
        menu.label(status);
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
        menu.item(FRAME_TARGET_OFF, "Desativado", target == 0);
        if processes.is_empty() {
            menu.label("Nenhum aplicativo visível");
        }
        for (index, process) in processes.iter().enumerate() {
            let name = process.name.replace('&', "&&");
            menu.item(
                FRAME_TARGET_FIRST + index,
                &format!("{name} (PID {})", process.pid),
                process.pid == target,
            );
        }
    }

    fn panel_menu(&self, menu: &Menu, snapshot: &Snapshot, panel: usize) {
        menu.group("Mover painel para", |positions| {
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
            menu.group("Mesclar com", |items| {
                for (index, target) in targets {
                    items.item(MERGE_FIRST + index, &panel_label(snapshot, target), false);
                }
            });
        }
        let series = &self.board.panels[panel];
        if series.len() > 1 {
            menu.item(SPLIT_PANEL, "Separar gráficos", false);
        }
        menu.group("Cor da curva", |items| {
            for &index in series {
                if let Some(metric) = snapshot.metrics.get(index) {
                    items.item(
                        COLOR_FIRST + index,
                        metric.name,
                        self.board.color(index).is_some(),
                    );
                }
            }
        });
        if series
            .iter()
            .any(|&index| self.board.color(index).is_some())
        {
            menu.item(RESET_COLORS, "Restaurar cores padrão", false);
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
                panel_label(snapshot, panel)
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

    fn layout(&self, bounds: RECT) -> Layout {
        Layout::new(bounds, self.board.columns, self.board.rows)
    }

    fn paint(&mut self, front: HDC, bounds: RECT) {
        if width(bounds) <= 0 || height(bounds) <= 0 {
            return;
        }
        let dc = self.buffer.target(front, width(bounds), height(bounds));
        unsafe { FillRect(dc, &bounds, GetStockObject(BLACK_BRUSH)) };
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
            let mut series = Vec::new();
            for (slot, panel) in self
                .board
                .panels
                .iter()
                .skip(self.page * layout.slots())
                .take(layout.slots())
                .enumerate()
            {
                if panel.is_empty() {
                    continue;
                }
                series.clear();
                series.extend(panel.iter().enumerate().filter_map(|(position, &sensor)| {
                    if !frames_current && FRAME_SENSORS.contains(&sensor) {
                        return None;
                    }
                    let metric = snapshot.metrics.get(sensor)?;
                    Some((metric, self.series_color(sensor, position, panel.len())))
                }));
                chart::paint(
                    dc,
                    layout.card(slot),
                    self.dpi,
                    &series,
                    at,
                    period,
                    &self.chart_fonts,
                );
            }
        } else {
            let hint = if self.snapshot.is_none() {
                "Aguardando a primeira coleta…"
            } else {
                "Clique com o botão direito para escolher sensores."
            };
            draw_text(dc, hint, bounds);
        }
        if let Some(drag) = &self.drag
            && drag.active
        {
            let first = self.page * layout.slots();
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
                )
            };
        }
    }
}

fn default_selection(snapshot: &Snapshot) -> &'static [usize] {
    let gpu_present = snapshot
        .metrics
        .iter()
        .any(|metric| metric.source.contains("NVML") && !metric.points.is_empty());
    if gpu_present {
        &DEFAULT_METRICS
    } else {
        &[16, 17]
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

    fn panel_at(&self, page: usize, point: POINT) -> Option<usize> {
        (0..self.slots())
            .find(|&slot| {
                let rect = self.card(slot);
                (rect.left..rect.right).contains(&point.x)
                    && (rect.top..rect.bottom).contains(&point.y)
            })
            .map(|slot| page * self.slots() + slot)
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

struct Menu(HMENU);

impl Menu {
    fn new() -> Option<Self> {
        let menu = unsafe { CreatePopupMenu() };
        (!menu.is_null()).then_some(Self(menu))
    }

    fn item(&self, id: usize, label: &str, checked: bool) {
        let label = wide(label);
        unsafe {
            AppendMenuW(
                self.0,
                MF_STRING | if checked { MF_CHECKED } else { 0 },
                id,
                label.as_ptr(),
            )
        };
    }

    fn label(&self, label: &str) {
        let label = wide(label);
        unsafe { AppendMenuW(self.0, MF_STRING | MF_GRAYED, 0, label.as_ptr()) };
    }

    fn separator(&self) {
        unsafe { AppendMenuW(self.0, MF_SEPARATOR, 0, std::ptr::null()) };
    }

    fn group(&self, label: &str, items: impl FnOnce(&Self)) {
        let Some(submenu) = Self::new() else { return };
        items(&submenu);
        let label = wide(label);
        // Uma inclusão bem-sucedida transfere o submenu para o menu pai.
        if unsafe { AppendMenuW(self.0, MF_POPUP, submenu.0 as usize, label.as_ptr()) } != 0 {
            std::mem::forget(submenu);
        }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        unsafe { DestroyMenu(self.0) };
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

fn client(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) };
    rect
}

fn invalidate(hwnd: HWND) {
    unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
}

fn draw_text(dc: HDC, text: &str, mut bounds: RECT) {
    let text = wide(text);
    unsafe {
        let previous = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, chart::MUTED);
        DrawTextW(
            dc,
            text.as_ptr(),
            -1,
            &mut bounds,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | DT_CENTER,
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
    fn panel_hit_uses_page_offset_and_skips_grid_gaps() {
        let layout = Layout::new(area(0, 0, 1118, 419), 3, 2);
        let card = layout.card(4);
        let center = POINT {
            x: (card.left + card.right) / 2,
            y: (card.top + card.bottom) / 2,
        };
        assert_eq!(layout.panel_at(0, center), Some(4));
        assert_eq!(layout.panel_at(2, center), Some(16));
        let gap = POINT {
            x: card.right,
            y: center.y,
        };
        assert_eq!(layout.panel_at(2, gap), None);
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
    fn last_partial_page_has_no_targets_past_the_board_limit() {
        let layout = Layout::new(area(0, 0, 1118, 419), 3, 2);
        let page = (MAX_SLOTS - 1) / layout.slots();
        let last_slot = (MAX_SLOTS - 1) % layout.slots();
        let last = layout.card(last_slot);
        let invalid = layout.card(last_slot + 1);
        assert_eq!(
            layout.panel_at(
                page,
                POINT {
                    x: last.left,
                    y: last.top
                }
            ),
            Some(MAX_SLOTS - 1)
        );
        assert_eq!(
            layout.panel_at(
                page,
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
}
