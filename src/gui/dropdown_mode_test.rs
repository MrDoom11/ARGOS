use super::*;
use crate::watch::{MetricSnapshot, RAM_USED_INDEX, StorageSnapshot};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, MOD_CONTROL, MOD_SHIFT, VK_F9, VK_F10,
};
use windows_sys::w;

#[test]
fn hotkey_and_relaunch_reveal_the_panel_without_taking_focus() {
    let _dpi = DpiContext(unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    });
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let display = displays().into_iter().next().expect("monitor disponível");
    let start = initial_bounds(Some(display.work), dpi);
    let mut board = Board::new([CPU_USAGE_INDEX, RAM_USED_INDEX, CPU_POWER_INDEX]);
    board.columns = 1;
    let monitor = Monitor::start(Vec::new(), 1_000, board.cpu_metrics()).unwrap();
    board.dropdown.enabled = true;
    board.dropdown.monitor = Some(display.id.clone());
    let trigger = display.trigger(board.dropdown.alignment, board.dropdown.width);
    let state = RefCell::new(Window::new(monitor, board, dpi, Source::PortableExact));
    assert!(state.borrow().startup_directory().is_none());
    let module = unsafe { GetModuleHandleW(null()) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module,
        lpszClassName: w!("ArgosDropdownNativeTest"),
        ..Default::default()
    };
    assert_ne!(unsafe { RegisterClassW(&class) }, 0);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            class.lpszClassName,
            null(),
            WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_CLIPCHILDREN,
            start.left,
            start.top,
            width(start),
            height(start),
            null_mut(),
            null_mut(),
            module,
            (&raw const state).cast(),
        )
    };
    assert!(!hwnd.is_null());
    let guard = OwnedWindow(hwnd);
    let normal = rect(hwnd);
    let saved = coords(window_placement(hwnd).unwrap().rcNormalPosition);
    // A posição já corresponde à configuração em memória, dispensando escrita no perfil.
    state.borrow_mut().board.window_bounds = Some(saved);
    let send = |msg, wp, lp| unsafe { SendMessageW(hwnd, msg, wp, lp) };
    let visible = || unsafe { IsWindowVisible(hwnd) != 0 };
    let strip = || {
        state
            .borrow()
            .input_strip
            .as_ref()
            .map(|strip| strip.hwnd())
    };
    let strip_visible = || strip().is_some_and(|strip| unsafe { IsWindowVisible(strip) != 0 });
    let (overlay, control) = {
        let app = state.borrow();
        let overlay = app.scrollbar_overlay.as_ref().unwrap();
        (overlay.hwnd(), overlay.control)
    };
    let overlay_visible = || unsafe { IsWindowVisible(overlay) != 0 };
    let change_opacity = |opacity| {
        state.borrow_mut().board.appearance.opacity = opacity;
        sync_appearance(hwnd, &state);
    };
    let hit_at_height = |y| {
        let bounds = rect(hwnd);
        unsafe {
            WindowFromPoint(POINT {
                x: bounds.left + width(bounds) / 2,
                y: bounds.top + y,
            })
        }
    };
    let assert_translucent_input = || {
        assert!(appearance::is_layered(hwnd));
        assert!(appearance::is_click_through(hwnd));
        assert!(strip_visible());
        let strip = strip().unwrap();
        let bounds = rect(hwnd);
        assert_eq!(input_strip::HEIGHT, 10);
        assert_eq!(
            coords(rect(strip)),
            [bounds.left, bounds.top, bounds.right, bounds.top + 10],
        );
        assert_eq!(hit_at_height(0), strip);
        assert_eq!(hit_at_height(9), strip);
        let assert_passthrough = |hit| {
            assert_ne!(hit, hwnd);
            assert_ne!(hit, strip);
            assert_ne!(hit, overlay);
            assert_ne!(hit, control);
            assert_eq!(unsafe { IsChild(hwnd, hit) }, 0);
        };
        assert_passthrough(hit_at_height(10));

        // A barra recebe os cliques no mesmo retângulo usado para desenhá-la.
        let [x, y, w, h] = state.borrow().scrollbar_state.unwrap().position;
        let mut origin = POINT { x, y };
        assert_ne!(unsafe { ClientToScreen(hwnd, &mut origin) }, 0);
        assert!(overlay_visible());
        assert_eq!(
            coords(rect(overlay)),
            [origin.x, origin.y, origin.x + w, origin.y + h],
        );
        assert_eq!(
            unsafe {
                WindowFromPoint(POINT {
                    x: origin.x + w / 2,
                    y: origin.y + h / 2,
                })
            },
            control,
        );
        assert_passthrough(unsafe {
            WindowFromPoint(POINT {
                x: origin.x - 1,
                y: origin.y + h / 2,
            })
        });
    };

    send(RECONFIGURE, 0, 0);
    assert!(state.borrow().dropdown.active());
    assert!(!visible());
    assert!(!strip_visible());
    assert!(!overlay_visible());

    let key = DropdownHotkey::new(MOD_CONTROL | MOD_SHIFT, u32::from(VK_F10)).unwrap();
    // Exercita o despacho de WM_HOTKEY sem reservar uma combinação global no desktop.
    state.borrow_mut().dropdown.registered = Some(key);
    let press = |vk| {
        send(
            WM_HOTKEY,
            HOTKEY as WPARAM,
            (key.modifiers | (u32::from(vk) << 16)) as LPARAM,
        )
    };
    press(VK_F9);
    assert!(!visible());

    press(VK_F10);
    assert!(visible());
    assert!(state.borrow().dropdown.pinned());
    assert_eq!(
        coords(rect(hwnd)),
        coords(display.panel(width(normal), height(normal), trigger))
    );
    assert_ne!(unsafe { GetForegroundWindow() }, hwnd);
    assert_eq!(send(WM_MOUSEACTIVATE, 0, 0), MA_NOACTIVATE as LRESULT);
    assert!(!strip_visible());

    send(instance::REVEAL_PANEL, 0, 0);
    assert!(visible());
    state.borrow_mut().board.appearance.theme = Theme::Light;
    change_opacity(0);
    assert_translucent_input();

    let foreground_before_notice = unsafe { GetForegroundWindow() };
    state
        .borrow_mut()
        .updater
        .complete_manual_check_for_test(semver::Version::new(0, 3, 99));
    let menu = Menu::new().unwrap();
    state.borrow_mut().open_menu = Some(Rc::new(OpenMenu {
        root: menu.0,
        section: MenuSection::All,
        processes: Vec::new(),
        displays: Vec::new(),
        panel_sensors: Vec::new(),
    }));
    send(WM_TIMER, super::super::TIMER, 0);
    {
        let app = state.borrow();
        assert!(app.open_menu.is_some());
        assert!(matches!(app.updater.status(), Status::Ready(_, false)));
        assert!(matches!(
            app.update_notice.as_ref().unwrap().visibility,
            NoticeVisibility::Pending,
        ));
    }
    state.borrow_mut().open_menu = None;
    drop(menu);
    send(WM_TIMER, super::super::TIMER, 0);
    assert!(matches!(
        state.borrow().update_notice.as_ref().unwrap().visibility,
        NoticeVisibility::Showing(_),
    ));
    assert_ne!(unsafe { IsWindowEnabled(hwnd) }, 0);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground_before_notice);
    // Uma ferramenta externa pode tirar o foco e cancelar a interação em curso.
    send(WM_ACTIVATE, WA_INACTIVE as WPARAM, 0);
    send(WM_CANCELMODE, 0, 0);
    assert_ne!(unsafe { IsWindowEnabled(hwnd) }, 0);
    assert_translucent_input();
    {
        let app = state.borrow();
        let bounds = client(hwnd);
        let mut surface = Surface::default();
        let dc = surface
            .target(null_mut(), width(bounds), height(bounds))
            .unwrap();
        app.paint_update_notice(dc, bounds, "");
        assert_eq!(
            unsafe { GetPixel(dc, width(bounds) / 2, height(bounds) / 2) },
            app.board.appearance.theme.menu_colors().background,
        );
    }

    let scroll = |command, row: usize| unsafe {
        SendMessageW(
            overlay,
            WM_VSCROLL,
            command as usize | (row << 16),
            control as LPARAM,
        );
    };
    scroll(SB_LINEDOWN, 0);
    assert_eq!(state.borrow().scroll_row, 1);
    scroll(SB_THUMBTRACK, 2);
    assert_eq!(state.borrow().scroll_row, 2);
    scroll(SB_PAGEUP, 0);
    assert_eq!(state.borrow().scroll_row, 1);
    scroll(SB_TOP, 0);
    assert_eq!(state.borrow().scroll_row, 0);
    let foreground = unsafe { GetForegroundWindow() };
    let mut info = SCROLLBARINFO {
        cbSize: size_of::<SCROLLBARINFO>() as u32,
        ..Default::default()
    };
    assert_ne!(
        unsafe { GetScrollBarInfo(control, OBJID_CLIENT, &mut info) },
        0
    );
    let x = width(info.rcScrollBar) / 2;
    let y = height(info.rcScrollBar) - info.dxyLineButton / 2;
    let arrow = (x | (y << 16)) as LPARAM;
    unsafe {
        assert_ne!(PostMessageW(control, WM_LBUTTONUP, 0, arrow), 0);
        SendMessageW(control, WM_LBUTTONDOWN, 1, arrow);
    }
    assert_eq!(state.borrow().scroll_row, 1);
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    unsafe { SendMessageW(control, WM_MOUSEWHEEL, (WHEEL_DELTA as usize) << 16, 0) };
    assert_eq!(state.borrow().scroll_row, 0);
    if !appearance::high_contrast() {
        assert_scrollbar_background(&state.borrow(), Theme::Light);
    }
    press(VK_F10);
    assert!(!visible());
    assert!(!strip_visible());
    assert!(!overlay_visible());
    send(instance::REVEAL_PANEL, 0, 0);
    assert!(visible());
    assert!(state.borrow().dropdown.pinned());
    assert_ne!(unsafe { GetForegroundWindow() }, hwnd);
    assert_translucent_input();

    change_opacity(50);
    assert_translucent_input();
    if !appearance::high_contrast() {
        assert_scrollbar_background(&state.borrow(), Theme::Light);
        state.borrow_mut().board.appearance.theme = Theme::Dark;
        sync_appearance(hwnd, &state);
        assert_scrollbar_background(&state.borrow(), Theme::Dark);
    }
    assert_translucent_color_menu(hwnd, &mut state.borrow_mut());
    change_opacity(100);
    assert!(visible());
    assert!(!appearance::is_layered(hwnd));
    assert!(!appearance::is_click_through(hwnd));
    assert!(!strip_visible());
    assert!(!overlay_visible());
    assert!(matches!(
        state.borrow().update_notice.as_ref().unwrap().visibility,
        NoticeVisibility::Showing(_),
    ));
    assert_eq!(hit_at_height(10), hwnd);
    sync_scrollbar(hwnd, &state);
    let scrollbar = state.borrow().scrollbar;
    assert_ne!(unsafe { IsWindowVisible(scrollbar) }, 0);
    send(WM_VSCROLL, SB_LINEDOWN as WPARAM, scrollbar as LPARAM);
    assert_eq!(state.borrow().scroll_row, 1);
    send(WM_VSCROLL, SB_LINEUP as WPARAM, scrollbar as LPARAM);
    assert_eq!(state.borrow().scroll_row, 0);
    change_opacity(0);
    assert_translucent_input();

    let missing_monitor = "monitor-desconectado".to_string();
    state.borrow_mut().board.dropdown.monitor = Some(missing_monitor.clone());
    send(RECONFIGURE, 0, 0);
    assert!(!state.borrow().dropdown.active());
    assert!(visible());
    assert_eq!(coords(rect(hwnd)), coords(normal));
    assert!(appearance::is_layered(hwnd));
    assert_eq!(state.borrow().board.window_bounds, Some(saved));
    assert!(state.borrow().board.dropdown.enabled);
    assert_eq!(
        state.borrow().board.dropdown.monitor.as_ref(),
        Some(&missing_monitor)
    );
    assert_eq!(
        unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST,
        0
    );
    assert_translucent_input();
    unsafe { EnableWindow(hwnd, 0) };
    assert!(!overlay_visible());
    assert!(!strip_visible());
    unsafe { EnableWindow(hwnd, 1) };
    assert_translucent_input();
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    assert!(!overlay_visible());
    show_normal(hwnd);
    assert_translucent_input();
    state.borrow_mut().board.rows = 3;
    sync_scrollbar(hwnd, &state);
    assert!(!overlay_visible());
    drop(guard);
    assert_ne!(unsafe { UnregisterClassW(class.lpszClassName, module) }, 0);
}

fn assert_scrollbar_background(app: &Window, theme: Theme) {
    assert_eq!(unsafe { IsWindowVisible(app.scrollbar) }, 0);
    let [x, y, w, h] = app.scrollbar_state.unwrap().position;
    let mut surface = Surface::default();
    let dc = surface.target(null_mut(), x + w, y + h).unwrap();
    assert!(app.paint_scrollbar(dc).is_some());
    let colors = theme.menu_colors();
    assert_eq!(unsafe { GetPixel(dc, x, y) }, colors.background);
    let mut info = SCROLLBARINFO {
        cbSize: size_of::<SCROLLBARINFO>() as u32,
        ..Default::default()
    };
    assert_ne!(
        unsafe { GetScrollBarInfo(app.scrollbar, OBJID_CLIENT, &mut info) },
        0
    );
    assert!(info.xyThumbBottom > info.xyThumbTop);
    let thumb = unsafe {
        GetPixel(
            dc,
            x + w / 2,
            y + (info.xyThumbTop + info.xyThumbBottom) / 2,
        )
    };
    assert!(thumb == colors.disabled || thumb == colors.accent);
}

fn assert_translucent_color_menu(hwnd: HWND, app: &mut Window) {
    assert_eq!(app.first_slot(), 0);
    assert_eq!(app.slots(), 1);
    assert!(app.board.set_color(RAM_USED_INDEX, Some(SERIES[0])));
    let previous = app.snapshot.replace(Arc::new(test_snapshot()));
    let menu = options_menu(hwnd, app, MenuSection::All, &[], &[], Some(0)).unwrap();
    let panel = submenu(menu.0, "Painel");
    let appearance = submenu(panel, "Aparência");
    let colors = submenu(appearance, "Cor dos gráficos");
    let color_item =
        |sensor| unsafe { GetMenuState(colors, (COLOR_FIRST + sensor) as u32, MF_BYCOMMAND) };
    assert_ne!(color_item(CPU_USAGE_INDEX), u32::MAX);
    let next_page = color_item(RAM_USED_INDEX);
    assert_ne!(next_page, u32::MAX);
    assert_eq!(next_page & (MF_CHECKED | MF_GRAYED), 0);
    app.snapshot = previous;
}

#[test]
fn options_menu_groups_related_commands_and_shortcuts_share_controls() {
    let mut board = Board::new([CPU_USAGE_INDEX, RAM_USED_INDEX]);
    board.columns = 1;
    board.dropdown.enabled = true;
    let monitor = Monitor::start(Vec::new(), 1_000, board.cpu_metrics()).unwrap();
    let mut app = Window::new(monitor, board, 96, Source::PortableExact);
    app.elevated = false;
    assert!(app.update_snapshot(Some(Arc::new(test_snapshot()))));
    let processes = [Process {
        pid: u32::MAX,
        name: String::from("Jogo de teste"),
    }];
    app.monitor
        .set_frame_target(FrameTarget::Process(processes[0].pid));
    let root = options_menu(null_mut(), &app, MenuSection::All, &processes, &[], Some(0)).unwrap();
    assert_submenu_icons(root.0);
    let sensors = submenu(root.0, "Sensores");
    let panel = submenu(root.0, "Painel");
    let chart = submenu(root.0, "Gráfico");
    let move_chart = submenu(chart, "Mover gráfico para");
    assert!(app_menu_commands(move_chart).contains(&(MOVE_PANEL_FIRST + 1)));
    assert!(app_menu_commands(submenu(root.0, "Sobre o ARGOS")).contains(&ABOUT_GUIDE));
    assert!(find_submenu(root.0, "ARGOS").is_none());
    assert!(find_submenu(root.0, "Atualizações").is_none());
    assert!(has_direct_command(root.0, UPDATE_CHECK));
    assert!(has_direct_command(root.0, MOVE_WINDOW));
    assert!(!has_direct_command(root.0, RUN_AS_ADMIN));
    let administrative = menu_position(panel, "Sempre abrir como administrador").unwrap();
    let autostart = menu_position(panel, "Iniciar com o Windows (requer instalação)").unwrap();
    assert_eq!(administrative.abs_diff(autostart), 1);
    assert_eq!(
        unsafe { GetMenuItemID(panel, administrative as i32) },
        RUN_AS_ADMIN as u32
    );
    let state = unsafe { GetMenuState(panel, RUN_AS_ADMIN as u32, MF_BYCOMMAND) };
    assert_eq!(state & (MF_DISABLED | MF_GRAYED | MF_CHECKED), 0);
    assert_menu_icon(panel, administrative);
    assert!(menu_position(panel, "Sessão atual como administrador").is_none());
    assert!(!app.should_elevate_on_open());

    let without_chart =
        options_menu(null_mut(), &app, MenuSection::All, &processes, &[], None).unwrap();
    assert!(find_submenu(without_chart.0, "Gráfico").is_none());
    let f2 = options_menu(
        null_mut(),
        &app,
        MenuSection::Sensors,
        &processes,
        &[],
        None,
    )
    .unwrap();
    assert_submenu_icons(f2.0);
    for group in ["CPU", "Memória", "GPU", "FPS", "Rede", "Armazenamento"] {
        assert_eq!(
            app_menu_commands(submenu(sensors, group)),
            app_menu_commands(submenu(f2.0, group)),
        );
    }
    assert!(
        app_menu_commands(submenu(submenu(f2.0, "GPU"), "GPUs monitoradas")).contains(&GPU_AUTO)
    );
    let network = submenu(f2.0, "Rede");
    assert!(app_menu_commands(submenu(network, "Interface de rede")).contains(&NETWORK_AUTO));
    let fps = submenu(f2.0, "FPS");
    assert!(app_menu_commands(fps).contains(&(SENSOR_FIRST + FRAME_SENSOR_START)));
    let target = submenu(fps, "Aplicativo monitorado");
    let state = unsafe { GetMenuState(target, FRAME_TARGET_FIRST as u32, MF_BYCOMMAND) };
    assert_ne!(state, u32::MAX);
    assert_ne!(state & MF_CHECKED, 0);

    let network_commands = app_menu_commands(network);
    for command in [
        SENSOR_FIRST + NETWORK_DOWNLOAD_INDEX,
        SENSOR_FIRST + NETWORK_UPLOAD_INDEX,
        SENSOR_FIRST + GAME_LATENCY_INDEX,
        SENSOR_FIRST + CUSTOM_PING_INDEX,
        EDIT_PING_TARGETS,
    ] {
        assert!(network_commands.contains(&command));
    }
    let mut sensor_commands: Vec<_> = app_menu_commands(root.0)
        .into_iter()
        .filter(|&id| {
            (NETWORK_AUTO..NETWORK_FIRST + NETWORK_LIMIT).contains(&id)
                || (SENSOR_FIRST..MERGE_FIRST).contains(&id)
                || id >= FRAME_TARGET_FOREGROUND
        })
        .collect();
    sensor_commands.sort_unstable();
    assert!(sensor_commands.windows(2).all(|ids| ids[0] != ids[1]));
    let f3 = options_menu(null_mut(), &app, MenuSection::Period, &[], &[], None).unwrap();
    let f4 = options_menu(null_mut(), &app, MenuSection::Grid, &[], &[], None).unwrap();
    assert_eq!(
        app_menu_commands(submenu(panel, "Histórico")),
        app_menu_commands(f3.0)
    );
    assert_eq!(
        app_menu_commands(submenu(panel, "Grade")),
        app_menu_commands(f4.0)
    );
    assert!(app_menu_commands(submenu(panel, "Aparência")).contains(&THEME_DARK));
    assert!(app_menu_commands(submenu(panel, "Intervalo de amostragem")).contains(&REFRESH_FIRST));
    assert!(app_menu_commands(submenu(panel, "Modo dropdown")).contains(&DROPDOWN_TOGGLE));
    assert!(app_menu_commands(submenu(panel, "Página")).contains(&PAGE_FIRST));

    app.board.run_as_admin = true;
    for elevated in [false, true] {
        app.elevated = elevated;
        assert_eq!(app.should_elevate_on_open(), !elevated);
        let root = options_menu(null_mut(), &app, MenuSection::All, &[], &[], None).unwrap();
        let panel = submenu(root.0, "Painel");
        assert!(has_direct_command(panel, RUN_AS_ADMIN));
        let state = unsafe { GetMenuState(panel, RUN_AS_ADMIN as u32, MF_BYCOMMAND) };
        assert_eq!(state & (MF_DISABLED | MF_GRAYED | MF_CHECKED), MF_CHECKED);
        let administrative = menu_position(panel, "Abertura como administrador ativada").unwrap();
        assert_eq!(menu_item_data(panel, administrative), 0);
        assert!(menu_position(panel, "Sempre abrir como administrador").is_none());
        assert!(menu_position(panel, "Sessão atual como administrador").is_none());
    }
    app.board.run_as_admin = false;
    let root = options_menu(null_mut(), &app, MenuSection::All, &[], &[], None).unwrap();
    let panel = submenu(root.0, "Painel");
    let status = menu_position(panel, "Sessão atual como administrador").unwrap();
    let state = unsafe { GetMenuState(panel, status, MF_BYPOSITION) };
    assert_ne!(state, u32::MAX);
    assert_ne!(state & (MF_DISABLED | MF_GRAYED), 0);
}

#[test]
fn update_actions_remain_available_from_the_main_menu() {
    let board = Board::new([]);
    let monitor = Monitor::start(Vec::new(), 1_000, board.cpu_metrics()).unwrap();
    let mut app = Window::new(monitor, board, 96, Source::PortableExact);
    let menu =
        |app: &Window| options_menu(null_mut(), app, MenuSection::All, &[], &[], None).unwrap();

    app.update_notice = Some(UpdateNotice::new(CheckNotice::Failed(
        "Falha de teste".into(),
    )));
    let failed = menu(&app);
    assert!(has_direct_command(failed.0, UPDATE_CHECK));
    assert!(has_direct_command(failed.0, UPDATE_CHECK_DETAILS));

    let version = semver::Version::new(0, 4, 1);
    app.updater.complete_manual_check_for_test(version.clone());
    let checking = menu(&app);
    assert!(!has_direct_command(checking.0, UPDATE_CHECK));
    assert!(find_submenu(checking.0, "Atualizações").is_none());

    assert!(matches!(app.updater.poll(), Some(CheckNotice::Ready(ready)) if ready == version));
    let ready = menu(&app);
    let updates = submenu(ready.0, "Atualizações");
    assert!(has_direct_command(updates, UPDATE_RESTART));
    assert!(has_direct_command(updates, UPDATE_SCHEDULE));
    assert!(!has_direct_command(updates, UPDATE_CANCEL));

    app.updater.schedule(true);
    let scheduled = menu(&app);
    let updates = submenu(scheduled.0, "Atualizações");
    assert!(has_direct_command(updates, UPDATE_RESTART));
    assert!(has_direct_command(updates, UPDATE_CANCEL));
    assert!(!has_direct_command(updates, UPDATE_SCHEDULE));
}

fn has_direct_command(menu: HMENU, command: usize) -> bool {
    (0..unsafe { GetMenuItemCount(menu) })
        .any(|position| (unsafe { GetMenuItemID(menu, position) }) == command as u32)
}

fn app_menu_commands(menu: HMENU) -> Vec<usize> {
    let mut commands = Vec::new();
    for position in 0..unsafe { GetMenuItemCount(menu) } {
        let child = unsafe { GetSubMenu(menu, position) };
        if !child.is_null() {
            commands.extend(app_menu_commands(child));
        } else {
            let command = unsafe { GetMenuItemID(menu, position) };
            if command != 0 {
                commands.push(command as usize);
            }
        }
    }
    commands
}

fn assert_submenu_icons(menu: HMENU) {
    for position in 0..unsafe { GetMenuItemCount(menu) } {
        let child = unsafe { GetSubMenu(menu, position) };
        if child.is_null() {
            continue;
        }
        assert_menu_icon(menu, position as u32);
        assert_submenu_icons(child);
    }
}

fn assert_menu_icon(menu: HMENU, position: u32) {
    assert!(
        (1..=usize::from(u16::MAX)).contains(&menu_item_data(menu, position)),
        "item de menu sem ícone na posição {position}"
    );
}

fn menu_item_data(menu: HMENU, position: u32) -> usize {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_DATA,
        ..Default::default()
    };
    assert_ne!(unsafe { GetMenuItemInfoW(menu, position, 1, &mut info) }, 0);
    info.dwItemData
}

fn submenu(menu: HMENU, label: &str) -> HMENU {
    find_submenu(menu, label).unwrap_or_else(|| panic!("submenu ausente: {label}"))
}

fn find_submenu(menu: HMENU, label: &str) -> Option<HMENU> {
    let position = menu_position(menu, label)?;
    let child = unsafe { GetSubMenu(menu, position as i32) };
    (!child.is_null()).then_some(child)
}

fn menu_position(menu: HMENU, label: &str) -> Option<u32> {
    let expected: Vec<_> = label.encode_utf16().collect();
    for position in 0..unsafe { GetMenuItemCount(menu) } {
        let mut name = [0; 128];
        let length = unsafe {
            GetMenuStringW(
                menu,
                position as u32,
                name.as_mut_ptr(),
                name.len() as i32,
                MF_BYPOSITION,
            )
        };
        if name[..length as usize] == expected {
            return Some(position as u32);
        }
    }
    None
}

#[test]
fn storage_menu_shows_independent_checks_and_drive_names() {
    let board = Board::new([]);
    let monitor = Monitor::start(Vec::new(), 1_000, board.cpu_metrics()).unwrap();
    let mut app = Window::new(monitor, board, 96, Source::PortableExact);
    let mut snapshot = test_snapshot();
    let ids = [
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
    ];
    snapshot.storage = Arc::from(std::array::from_fn::<_, 2, _>(|index| StorageSnapshot {
        id: ids[index].to_owned(),
        label: Arc::from(["C: Sistema", "D: Arquivos"][index]),
        metrics: (0..STORAGE_SENSOR_COUNT)
            .map(storage_metric_template)
            .collect(),
    }));
    assert!(app.update_snapshot(Some(Arc::new(snapshot))));
    let system_read = app.board.storage_sensor_slot(ids[0], 0).unwrap();
    let data_read = app.board.storage_sensor_slot(ids[1], 0).unwrap();
    let data_write = app.board.storage_sensor_slot(ids[1], 1).unwrap();
    let disconnected = app
        .board
        .register_storage_sensor("33333333-3333-3333-3333-333333333333", 0);
    app.board.toggle(system_read, 0);
    app.board.toggle(data_write, 0);
    app.board.toggle(disconnected, 0);

    let snapshot = app.snapshot.as_ref().unwrap();
    assert_eq!(
        app.series_title(snapshot, system_read).as_deref(),
        Some("C: · Leitura")
    );
    assert_eq!(
        app.series_title(snapshot, data_write).as_deref(),
        Some("D: · Escrita")
    );
    let menu = options_menu(null_mut(), &app, MenuSection::Sensors, &[], &[], None).unwrap();
    let disks = submenu(menu.0, "Armazenamento");
    assert!(!submenu(disks, "Volumes indisponíveis").is_null());
    assert_submenu_icons(menu.0);
    let state = |slot| unsafe { GetMenuState(menu.0, (SENSOR_FIRST + slot) as u32, MF_BYCOMMAND) };
    for slot in [system_read, data_read, data_write] {
        assert_ne!(state(slot), u32::MAX);
    }
    assert_ne!(state(system_read) & MF_CHECKED, 0);
    assert_eq!(state(data_read) & MF_CHECKED, 0);
    assert_ne!(state(data_write) & MF_CHECKED, 0);
}

fn test_snapshot() -> Snapshot {
    Snapshot {
        at: Instant::now(),
        notice: String::new(),
        metrics: (0..=NETWORK_UPLOAD_INDEX)
            .map(|_| MetricSnapshot {
                name: "Métrica de teste",
                address_label: None,
                remote_ip: None,
                unit: "",
                source: "",
                requires_admin: false,
                interval: Duration::from_secs(1),
                axis_max: 100.0,
                points: Vec::new(),
            })
            .collect(),
        gpu_devices: Arc::from([]),
        gpu_selections: Vec::new(),
        gpus: Vec::new(),
        frame_target: FrameTarget::Off,
        frame_status: None,
        cpu_temperature_status: None,
        cpu_power_status: None,
        game_latency_status: None,
        custom_ping_metrics: Vec::new(),
        custom_ping_status: Vec::new(),
        network_interfaces: Arc::from([]),
        network_interface: None,
        network_status: None,
        storage: Arc::from([]),
    }
}

fn rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    assert_ne!(unsafe { GetWindowRect(hwnd, &mut rect) }, 0);
    rect
}

fn coords(rect: RECT) -> [i32; 4] {
    [rect.left, rect.top, rect.right, rect.bottom]
}
