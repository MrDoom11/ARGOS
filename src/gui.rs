use std::ffi::c_void;
use std::io;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, VK_END, VK_F2, VK_F3, VK_F4, VK_HOME, VK_NEXT, VK_PRIOR,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

use crate::watch::{Monitor, Snapshot};

mod board;
mod chart;

use board::{Board, MAX_COLUMNS, MAX_ROWS};

const CLASS: *const u16 = w!("ArgosMonitorWindow");
const TITLE: *const u16 = w!("ARGOS — Monitor de hardware");
const TIMER: usize = 1;
const STALE_AFTER: Duration = Duration::from_millis(2_500);
const SENSORS_BUTTON: usize = 101;
const PERIOD_BUTTON: usize = 102;
const GRID_BUTTON: usize = 103;
const PREVIOUS_BUTTON: usize = 104;
const NEXT_BUTTON: usize = 105;
const BUTTONS: [usize; 5] = [
    PERIOD_BUTTON,
    SENSORS_BUTTON,
    GRID_BUTTON,
    PREVIOUS_BUTTON,
    NEXT_BUTTON,
];
const PERIOD_FIRST: usize = 201;
const RESET_SENSORS: usize = 205;
const COLUMNS_FIRST: usize = 301;
const ROWS_FIRST: usize = 311;
const SPLIT_PANEL: usize = 321;
const SENSOR_FIRST: usize = 1000;
const MERGE_FIRST: usize = 2000;
const PERIODS: [(u64, &str); 4] = [(30, "30 s"), (60, "1 min"), (120, "2 min"), (300, "5 min")];
const DEFAULT_METRICS: [usize; 12] = [0, 1, 2, 5, 6, 7, 11, 12, 13, 14, 16, 17];
const BACKGROUND: COLORREF = chart::rgb(11, 16, 25);
const ACCENT: COLORREF = chart::rgb(72, 215, 185);
const WARNING: COLORREF = chart::rgb(255, 182, 103);
const SERIES: [COLORREF; 16] = [
    ACCENT,
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
    let monitor = Monitor::start(gpu)?;
    // A janela e os controles pertencem a esta thread, inclusive durante a troca de monitor.
    let _dpi_context = DpiContext(unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    });
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let state = Box::new(std::cell::RefCell::new(Window::new(monitor, dpi)));
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
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
    // SAFETY: a caixa mantém o endereço de Window estável até a destruição de hwnd.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            CLASS,
            TITLE,
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            start.left,
            start.top,
            width(start),
            height(start),
            null_mut(),
            null_mut(),
            instance,
            (&*state as *const std::cell::RefCell<Window>).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(window_error("criar a janela"));
    }
    let hwnd_guard = OwnedWindow(hwnd);
    let window_dpi = unsafe { GetDpiForWindow(hwnd) };
    if window_dpi != 0 && window_dpi != dpi {
        position_window(hwnd, initial_bounds(work_area, window_dpi));
    }
    let dark_mode = 1_i32;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark_mode as *const i32).cast(),
            4,
        );
    }

    let labels = state.borrow().labels();
    let mut buttons = [null_mut(); BUTTONS.len()];
    for ((slot, id), label) in buttons.iter_mut().zip(BUTTONS).zip(labels) {
        *slot = make_button(hwnd, instance, id, &label)?;
    }
    {
        let mut app = state.borrow_mut();
        app.buttons = buttons;
        app.set_dpi(window_dpi);
    }
    resize(hwnd, &state);
    update_controls(&state);
    if unsafe { SetTimer(hwnd, TIMER, 500, None) } == 0 {
        return Err(window_error("agendar a atualização da janela"));
    }
    unsafe {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if IsWindowVisible(hwnd) == 0 {
            // Um launcher pode substituir a primeira chamada a ShowWindow.
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
        if unsafe { IsDialogMessageW(hwnd, &message) } == 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    drop(hwnd_guard);
    Ok(())
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: Win32 fornece os parâmetros das mensagens e o ponteiro pertence ao thread da janela.
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const std::cell::RefCell<Window>;
        let Some(state) = ptr.as_ref() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match msg {
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
                if wp as u32 != SIZE_MINIMIZED {
                    resize(hwnd, state);
                }
                0
            }
            WM_GETMINMAXINFO => {
                let dpi = state.try_borrow().map_or(96, |app| app.dpi);
                let info = &mut *(lp as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: scale(510, dpi),
                    y: scale(380, dpi),
                };
                0
            }
            WM_DPICHANGED => {
                if let Ok(mut app) = state.try_borrow_mut() {
                    app.set_dpi((wp & 0xffff) as u32);
                }
                let suggested = *(lp as *const RECT);
                position_window(hwnd, suggested);
                resize(hwnd, state);
                0
            }
            WM_TIMER if wp == TIMER => {
                if IsIconic(hwnd) == 0
                    && let Ok(mut app) = state.try_borrow_mut()
                {
                    let refresh = app.refresh();
                    let redraw = refresh.is_some() || app.snapshot_is_stale(Instant::now());
                    drop(app);
                    if refresh == Some(true) {
                        update_controls(state);
                    }
                    if redraw {
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                0
            }
            WM_DRAWITEM => {
                let item = &*(lp as *const DRAWITEMSTRUCT);
                if let Ok(app) = state.try_borrow() {
                    app.paint_button(item);
                }
                1
            }
            WM_COMMAND if wp >> 16 == 0 => match wp & 0xffff {
                button @ (PERIOD_BUTTON | SENSORS_BUTTON | GRID_BUTTON) => {
                    show_menu(hwnd, state, button);
                    0
                }
                PREVIOUS_BUTTON => {
                    navigate(hwnd, state, PageMove::Previous);
                    0
                }
                NEXT_BUTTON => {
                    navigate(hwnd, state, PageMove::Next);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            },
            WM_MOUSEWHEEL => {
                navigate(hwnd, state, PageMove::Wheel((wp >> 16) as i16));
                0
            }
            WM_RBUTTONUP => {
                let point = POINT {
                    x: lp as i16 as i32,
                    y: (lp >> 16) as i16 as i32,
                };
                show_panel_menu(hwnd, state, point);
                0
            }
            WM_DESTROY => {
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

fn keyboard(hwnd: HWND, state: &std::cell::RefCell<Window>, key: u16) -> bool {
    match key {
        VK_F2 => show_menu(hwnd, state, SENSORS_BUTTON),
        VK_F3 => show_menu(hwnd, state, PERIOD_BUTTON),
        VK_F4 => show_menu(hwnd, state, GRID_BUTTON),
        VK_PRIOR => navigate(hwnd, state, PageMove::Previous),
        VK_NEXT => navigate(hwnd, state, PageMove::Next),
        VK_HOME => navigate(hwnd, state, PageMove::First),
        VK_END => navigate(hwnd, state, PageMove::Last),
        _ => return false,
    }
    true
}

enum PageMove {
    Previous,
    Next,
    First,
    Last,
    Wheel(i16),
}

fn navigate(hwnd: HWND, state: &std::cell::RefCell<Window>, step: PageMove) {
    let changed = state.try_borrow_mut().is_ok_and(|mut app| {
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
        update_controls(state);
        invalidate(hwnd);
    }
}

fn show_menu(hwnd: HWND, state: &std::cell::RefCell<Window>, button: usize) {
    let (menu, anchor) = {
        let Ok(app) = state.try_borrow() else { return };
        let Some(menu) = Menu::new() else { return };
        let layout = app.layout(client(hwnd));
        let anchor = match button {
            PERIOD_BUTTON => {
                for (index, (_, label)) in PERIODS.iter().enumerate() {
                    menu.item(PERIOD_FIRST + index, label, index == app.board.period);
                }
                layout.buttons[0]
            }
            GRID_BUTTON => {
                menu.label("Colunas");
                for count in 1..=MAX_COLUMNS {
                    menu.item(
                        COLUMNS_FIRST + count - 1,
                        &count.to_string(),
                        count == app.board.columns,
                    );
                }
                menu.separator();
                menu.label("Linhas");
                for count in 1..=MAX_ROWS {
                    menu.item(
                        ROWS_FIRST + count - 1,
                        &count.to_string(),
                        count == app.board.rows,
                    );
                }
                layout.buttons[2]
            }
            _ => {
                if let Some(snapshot) = &app.snapshot {
                    let mut source = "";
                    for (index, metric) in snapshot.metrics.iter().enumerate() {
                        if metric.source != source {
                            source = metric.source;
                            if index != 0 {
                                menu.separator();
                            }
                            menu.label(source);
                        }
                        menu.item(SENSOR_FIRST + index, metric.name, app.board.contains(index));
                    }
                    menu.separator();
                    menu.item(RESET_SENSORS, "Restaurar seleção inicial", false);
                } else {
                    menu.label("Aguardando a primeira coleta");
                }
                layout.buttons[1]
            }
        };
        (menu, anchor)
    };
    let point = POINT {
        x: anchor.right,
        y: anchor.bottom,
    };
    if let Some(choice) = pick_menu(hwnd, &menu, point, TPM_RIGHTALIGN) {
        edit_board(hwnd, state, |app| app.apply_selection(choice));
    }
}

fn show_panel_menu(hwnd: HWND, state: &std::cell::RefCell<Window>, point: POINT) {
    let (menu, clicked) = {
        let Ok(app) = state.try_borrow() else { return };
        let Some(snapshot) = &app.snapshot else {
            return;
        };
        let layout = app.layout(client(hwnd));
        let Some(clicked) = layout.panel_at(app.page, point) else {
            return;
        };
        let Some(panel) = app.board.panels.get(clicked) else {
            return;
        };
        let (Some(menu), Some(targets)) = (Menu::new(), Menu::new()) else {
            return;
        };
        let mut has_target = false;
        for (index, target) in app.board.panels.iter().enumerate() {
            if app.board.can_merge(index, clicked) {
                targets.item(MERGE_FIRST + index, &panel_label(snapshot, target), false);
                has_target = true;
            }
        }
        if has_target {
            menu.submenu(targets, "Mesclar com");
        } else {
            menu.label("Nenhum painel compatível para mesclar");
        }
        if panel.len() > 1 {
            menu.item(SPLIT_PANEL, "Separar gráficos", false);
            menu.separator();
            menu.label("Curvas deste painel");
            for &index in panel {
                if let Some(metric) = snapshot.metrics.get(index) {
                    menu.label(metric.name);
                }
            }
        }
        (menu, clicked)
    };
    if let Some(choice) = pick_menu(hwnd, &menu, point, TPM_RIGHTBUTTON) {
        edit_board(hwnd, state, |app| {
            if choice == SPLIT_PANEL {
                return app.board.split(clicked);
            }
            let Some(source) = choice.checked_sub(MERGE_FIRST) else {
                return false;
            };
            if !app.board.merge(source, clicked) {
                return false;
            }
            let merged = clicked - usize::from(source < clicked);
            app.page = merged / app.slots();
            true
        });
    }
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

fn pick_menu(hwnd: HWND, menu: &Menu, mut point: POINT, flags: u32) -> Option<usize> {
    if unsafe { ClientToScreen(hwnd, &mut point) } == 0 {
        return None;
    }
    // TrackPopupMenu despacha mensagens enquanto está aberto. Nenhum empréstimo do estado vive aqui.
    let choice = unsafe {
        TrackPopupMenu(
            menu.0,
            TPM_RETURNCMD | TPM_NONOTIFY | flags,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        )
    } as usize;
    (choice != 0).then_some(choice)
}

fn edit_board(
    hwnd: HWND,
    state: &std::cell::RefCell<Window>,
    edit: impl FnOnce(&mut Window) -> bool,
) {
    let changed = state.try_borrow_mut().is_ok_and(|mut app| {
        if !edit(&mut app) {
            return false;
        }
        app.page = app.page.min(app.page_count() - 1);
        app.save_failed = app.board.save().is_err();
        true
    });
    if changed {
        update_controls(state);
        invalidate(hwnd);
    }
}

fn make_button(hwnd: HWND, instance: *mut c_void, id: usize, label: &str) -> Result<HWND, String> {
    let label = wide(label);
    let button = unsafe {
        CreateWindowExW(
            0,
            w!("BUTTON"),
            label.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32,
            0,
            0,
            0,
            0,
            hwnd,
            id as HMENU,
            instance,
            std::ptr::null(),
        )
    };
    if button.is_null() {
        Err(window_error("criar os controles"))
    } else {
        Ok(button)
    }
}

fn resize(hwnd: HWND, state: &std::cell::RefCell<Window>) {
    if let Ok(app) = state.try_borrow() {
        let layout = app.layout(client(hwnd));
        let buttons = app.buttons;
        drop(app);
        for (button, rect) in buttons.into_iter().zip(layout.buttons) {
            if !button.is_null() {
                unsafe { MoveWindow(button, rect.left, rect.top, width(rect), height(rect), 1) };
            }
        }
    }
    invalidate(hwnd);
}

fn update_controls(state: &std::cell::RefCell<Window>) {
    let Ok(app) = state.try_borrow() else { return };
    let buttons = app.buttons;
    let labels = app.labels();
    let pages = app.page_count();
    let enabled = [app.page != 0, app.page + 1 < pages];
    drop(app);
    for (button, label) in buttons.into_iter().zip(labels) {
        if !button.is_null() {
            let label = wide(&label);
            unsafe { SetWindowTextW(button, label.as_ptr()) };
        }
    }
    for (button, enabled) in buttons[3..].iter().zip(enabled) {
        if !button.is_null() {
            unsafe {
                EnableWindow(*button, i32::from(enabled));
                ShowWindow(*button, if pages > 1 { SW_SHOWNA } else { SW_HIDE });
            }
        }
    }
}

struct Window {
    monitor: Monitor,
    snapshot: Option<Arc<Snapshot>>,
    board: Board,
    restored: bool,
    save_failed: bool,
    page: usize,
    wheel_remainder: i32,
    dpi: u32,
    fonts: Fonts,
    buffer: BackBuffer,
    buttons: [HWND; BUTTONS.len()],
}

impl Window {
    fn new(monitor: Monitor, dpi: u32) -> Self {
        let restored_board = Board::load();
        let restored = restored_board.is_some();
        Self {
            monitor,
            snapshot: None,
            board: restored_board.unwrap_or_else(|| Board::new(DEFAULT_METRICS)),
            restored,
            save_failed: false,
            page: 0,
            wheel_remainder: 0,
            dpi,
            fonts: Fonts::new(dpi),
            buffer: BackBuffer::default(),
            buttons: [null_mut(); BUTTONS.len()],
        }
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi != 0 && dpi != self.dpi {
            self.dpi = dpi;
            self.fonts = Fonts::new(dpi);
        }
    }

    fn refresh(&mut self) -> Option<bool> {
        let next = self.monitor.snapshot()?;
        if self
            .snapshot
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &next))
        {
            return None;
        }
        let previous_count = self.active_count();
        if self.snapshot.is_none() && !self.restored {
            self.board
                .reset_selection(default_selection(&next).iter().copied());
            self.page = self.page.min(self.page_count() - 1);
        }
        self.snapshot = Some(next);
        Some(self.active_count() != previous_count)
    }

    fn active_count(&self) -> usize {
        self.board.panels.iter().map(Vec::len).sum()
    }

    fn slots(&self) -> usize {
        self.board.columns * self.board.rows
    }

    fn page_count(&self) -> usize {
        self.board.panels.len().div_ceil(self.slots()).max(1)
    }

    fn apply_selection(&mut self, choice: usize) -> bool {
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
            return true;
        }
        false
    }

    fn snapshot_is_stale(&self, now: Instant) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| now.saturating_duration_since(snapshot.at) >= STALE_AFTER)
    }

    fn labels(&self) -> [String; BUTTONS.len()] {
        [
            format!("Histórico · {}", PERIODS[self.board.period].1),
            format!("Sensores · {}", self.active_count()),
            format!("Grade · {} × {}", self.board.columns, self.board.rows),
            "Página anterior".into(),
            "Próxima página".into(),
        ]
    }

    fn layout(&self, bounds: RECT) -> Layout {
        Layout::new(bounds, self.dpi, self.board.columns, self.board.rows)
    }

    fn paint(&mut self, front: HDC, bounds: RECT) {
        if width(bounds) <= 0 || height(bounds) <= 0 {
            return;
        }
        let dc = self.buffer.target(front, width(bounds), height(bounds));
        fill(dc, bounds, BACKGROUND);
        let layout = self.layout(bounds);
        let now = Instant::now();
        let stale = self.snapshot_is_stale(now);
        let chart_time = self
            .snapshot
            .as_ref()
            .filter(|_| !stale)
            .map_or(now, |snapshot| snapshot.at);
        self.paint_header(dc, bounds, &layout, stale);
        self.paint_charts(dc, &layout, chart_time);
        self.paint_footer(dc, bounds, &layout);
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

    fn paint_header(&self, dc: HDC, bounds: RECT, layout: &Layout, stale: bool) {
        let s = |dip| scale(dip, self.dpi);
        let indicator = if stale { WARNING } else { ACCENT };
        fill(dc, area(s(7), s(10), s(10), s(25)), indicator);
        draw_text(
            dc,
            self.fonts.brand.0,
            "ARGOS",
            area(s(17), 0, s(82), s(35)),
            chart::TEXT,
            DT_LEFT,
        );
        let note = self
            .snapshot
            .as_ref()
            .map_or("Aguardando a primeira coleta…", |snapshot| {
                snapshot.notice.as_str()
            });
        let warning = stale.then(|| format!("Coleta atrasada · {note}"));
        let status = warning.as_deref().unwrap_or(note);
        draw_text(
            dc,
            self.fonts.small.0,
            status,
            area(s(84), 0, layout.buttons[0].left - s(6), s(35)),
            if stale { WARNING } else { chart::MUTED },
            DT_LEFT,
        );
        fill(
            dc,
            area(s(5), s(34), bounds.right - s(5), s(34) + 1),
            chart::BORDER,
        );
    }

    fn paint_charts(&self, dc: HDC, layout: &Layout, chart_time: Instant) {
        let s = |dip| scale(dip, self.dpi);
        let saved = unsafe { SaveDC(dc) };
        if saved == 0 {
            return;
        }
        unsafe {
            IntersectClipRect(
                dc,
                layout.viewport.left,
                layout.viewport.top,
                layout.viewport.right,
                layout.viewport.bottom,
            )
        };
        match &self.snapshot {
            Some(snapshot) if !self.board.panels.is_empty() => {
                let period = Duration::from_secs(PERIODS[self.board.period].0);
                let mut series = Vec::new();
                for (slot, panel) in self
                    .board
                    .panels
                    .iter()
                    .skip(self.page * layout.slots())
                    .take(layout.slots())
                    .enumerate()
                {
                    series.clear();
                    series.extend(panel.iter().enumerate().filter_map(|(position, &index)| {
                        let metric = snapshot.metrics.get(index)?;
                        let color = if panel.len() == 1 { index } else { position };
                        Some((metric, SERIES[color % SERIES.len()]))
                    }));
                    chart::paint(dc, layout.card(slot), self.dpi, &series, chart_time, period);
                }
            }
            _ => {
                let waiting = self.snapshot.is_none();
                let title = if waiting {
                    "Preparando as leituras"
                } else {
                    "Escolha os sensores"
                };
                let subtitle = if waiting {
                    "Os gráficos aparecem conforme as amostras chegam."
                } else {
                    "Use Sensores ou pressione F2 para mostrar os gráficos."
                };
                let card = area(
                    layout.viewport.left,
                    layout.viewport.top,
                    layout.viewport.right,
                    layout.viewport.top + s(134),
                );
                rounded(dc, card, s(10), chart::CARD, chart::BORDER);
                draw_text(
                    dc,
                    self.fonts.brand.0,
                    title,
                    area(card.left, card.top + s(22), card.right, card.top + s(62)),
                    chart::TEXT,
                    DT_CENTER,
                );
                draw_text(
                    dc,
                    self.fonts.body.0,
                    subtitle,
                    area(
                        card.left + s(12),
                        card.top + s(62),
                        card.right - s(12),
                        card.top + s(96),
                    ),
                    chart::MUTED,
                    DT_CENTER,
                );
            }
        }
        unsafe { RestoreDC(dc, saved) };
    }

    fn paint_footer(&self, dc: HDC, bounds: RECT, layout: &Layout) {
        let s = |dip| scale(dip, self.dpi);
        let top = bounds.bottom - s(23);
        fill(
            dc,
            area(s(5), top, bounds.right - s(5), top + 1),
            chart::BORDER,
        );
        let pages = self.page_count();
        let (hint, color) = if self.save_failed {
            ("Grade só nesta sessão", WARNING)
        } else {
            ("Botão direito no gráfico: mesclar ou separar", chart::MUTED)
        };
        draw_text(
            dc,
            self.fonts.small.0,
            hint,
            area(s(7), top, layout.buttons[3].left - s(5), bounds.bottom),
            color,
            DT_LEFT,
        );
        if pages > 1 {
            let page = format!("Página {} / {pages}", self.page + 1);
            draw_text(
                dc,
                self.fonts.small.0,
                &page,
                area(
                    layout.buttons[3].right,
                    top,
                    layout.buttons[4].left,
                    bounds.bottom,
                ),
                chart::MUTED,
                DT_CENTER,
            );
        }
    }

    fn paint_button(&self, item: &DRAWITEMSTRUCT) {
        let s = |dip| scale(dip, self.dpi);
        let Some(index) = BUTTONS.iter().position(|&id| id == item.CtlID as usize) else {
            return;
        };
        let mut bounds = item.rcItem;
        fill(item.hDC, bounds, BACKGROUND);
        bounds.right -= 1;
        bounds.bottom -= 1;
        let pressed = item.itemState & ODS_SELECTED != 0;
        let focused = item.itemState & ODS_FOCUS != 0;
        let background = if pressed {
            chart::rgb(30, 54, 66)
        } else {
            chart::CARD
        };
        rounded(
            item.hDC,
            bounds,
            s(8),
            background,
            if focused { ACCENT } else { chart::BORDER },
        );
        if index >= 3 {
            let arrow = if index == 3 { "‹" } else { "›" };
            let color = if item.itemState & ODS_DISABLED == 0 {
                chart::TEXT
            } else {
                chart::BORDER
            };
            draw_text(
                item.hDC,
                self.fonts.brand.0,
                arrow,
                bounds,
                color,
                DT_CENTER,
            );
            return;
        }
        let labels = self.labels();
        draw_text(
            item.hDC,
            self.fonts.body.0,
            &labels[index],
            area(
                bounds.left + s(8),
                bounds.top,
                bounds.right - s(20),
                bounds.bottom,
            ),
            chart::TEXT,
            DT_LEFT,
        );
        let x = bounds.right - s(14);
        let y = (bounds.top + bounds.bottom) / 2;
        let chevron = [
            POINT {
                x: x - s(3),
                y: y - s(1),
            },
            POINT { x, y: y + s(2) },
            POINT {
                x: x + s(3),
                y: y - s(1),
            },
        ];
        let pen = unsafe { CreatePen(PS_SOLID, 1, chart::MUTED) };
        if !pen.is_null() {
            unsafe {
                let old = SelectObject(item.hDC, pen);
                Polyline(item.hDC, chevron.as_ptr(), chevron.len() as i32);
                SelectObject(item.hDC, old);
                DeleteObject(pen);
            }
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
    buttons: [RECT; BUTTONS.len()],
    gap: i32,
    columns: usize,
    rows: usize,
}

impl Layout {
    fn new(bounds: RECT, dpi: u32, columns: usize, rows: usize) -> Self {
        let s = |dip| scale(dip, dpi);
        let right = bounds.right - s(6);
        let grid = area(right - s(105), s(4), right, s(30));
        let sensors = area(grid.left - s(107), s(4), grid.left - s(5), s(30));
        let period = area(sensors.left - s(159), s(4), sensors.left - s(5), s(30));
        let next = area(
            right - s(25),
            bounds.bottom - s(21),
            right,
            bounds.bottom - s(2),
        );
        let previous = area(next.left - s(105), next.top, next.left - s(80), next.bottom);
        let viewport = area(
            s(5),
            s(39),
            bounds.right - s(5),
            (bounds.bottom - s(26)).max(s(39)),
        );
        Self {
            viewport,
            buttons: [period, sensors, grid, previous, next],
            gap: s(4),
            columns,
            rows,
        }
    }

    fn slots(&self) -> usize {
        self.columns * self.rows
    }

    fn card(&self, slot: usize) -> RECT {
        let column = (slot % self.columns) as i32;
        let row = (slot / self.columns) as i32;
        let columns = self.columns as i32;
        let rows = self.rows as i32;
        let w = (width(self.viewport) - (columns - 1) * self.gap).max(0);
        let h = (height(self.viewport) - (rows - 1) * self.gap).max(0);
        let x = self.viewport.left + column * self.gap;
        let y = self.viewport.top + row * self.gap;
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
    }
}

struct Fonts {
    brand: Font,
    body: Font,
    small: Font,
}

impl Fonts {
    fn new(dpi: u32) -> Self {
        Self {
            brand: Font::new(17, FW_BOLD, dpi),
            body: Font::new(12, FW_SEMIBOLD, dpi),
            small: Font::new(11, FW_SEMIBOLD, dpi),
        }
    }
}

struct Font(HFONT);

impl Font {
    fn new(size: i32, weight: u32, dpi: u32) -> Self {
        Self(unsafe {
            CreateFontW(
                -scale(size, dpi),
                0,
                0,
                0,
                weight as i32,
                0,
                0,
                0,
                u32::from(DEFAULT_CHARSET),
                0,
                0,
                u32::from(CLEARTYPE_QUALITY),
                0,
                w!("Segoe UI"),
            )
        })
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
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

    fn submenu(&self, submenu: Self, label: &str) {
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

fn draw_text(dc: HDC, font: HFONT, text: &str, mut bounds: RECT, color: COLORREF, align: u32) {
    if width(bounds) <= 0 {
        return;
    }
    let text = wide(text);
    unsafe {
        let font = if font.is_null() {
            GetStockObject(DEFAULT_GUI_FONT)
        } else {
            font
        };
        let previous = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            text.as_ptr(),
            -1,
            &mut bounds,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | align,
        );
        SelectObject(dc, previous);
    }
}

fn fill(dc: HDC, bounds: RECT, color: COLORREF) {
    let brush = unsafe { CreateSolidBrush(color) };
    if !brush.is_null() {
        unsafe {
            FillRect(dc, &bounds, brush);
            DeleteObject(brush);
        }
    }
}

fn rounded(dc: HDC, rect: RECT, radius: i32, fill: COLORREF, border: COLORREF) {
    let brush = unsafe { CreateSolidBrush(fill) };
    let pen = unsafe { CreatePen(PS_SOLID, 1, border) };
    if !brush.is_null() && !pen.is_null() {
        unsafe {
            let old_brush = SelectObject(dc, brush);
            let old_pen = SelectObject(dc, pen);
            RoundRect(
                dc,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                radius,
                radius,
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
        }
    }
    unsafe {
        DeleteObject(pen);
        DeleteObject(brush);
    }
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
        for (width, height, dpi) in [(510, 380, 96), (1719, 409, 120), (2573, 709, 144)] {
            let layout = Layout::new(area(0, 0, width, height), dpi, 6, 2);
            assert_eq!(layout.slots(), 12);
            assert_eq!(layout.card(0).left, layout.viewport.left);
            assert_eq!(layout.card(11).right, layout.viewport.right);
            assert_eq!(layout.card(11).bottom, layout.viewport.bottom);
            assert!(layout.card(5).right > layout.card(5).left);
        }
        assert_eq!(Layout::new(area(0, 0, 800, 480), 96, 6, 4).slots(), 24);
    }

    #[test]
    fn panel_hit_uses_page_offset_and_skips_grid_gaps() {
        let layout = Layout::new(area(0, 0, 1118, 419), 96, 3, 2);
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
}
