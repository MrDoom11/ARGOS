use super::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MOD_CONTROL, MOD_SHIFT, VK_F9, VK_F10};
use windows_sys::w;

#[test]
fn hotkey_and_relaunch_reveal_the_panel_without_taking_focus() {
    let _dpi = DpiContext(unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    });
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    let display = displays().into_iter().next().expect("monitor disponível");
    let start = initial_bounds(Some(display.work), dpi);
    let mut board = Board::new([CPU_USAGE_INDEX, CPU_USAGE_INDEX + 1]);
    board.columns = 1;
    let monitor = Monitor::start(Vec::new(), 1_000, board.cpu_metrics()).unwrap();
    board.dropdown.enabled = true;
    board.dropdown.monitor = Some(display.id.clone());
    let trigger = display.trigger(board.dropdown.alignment, board.dropdown.width);
    let state = RefCell::new(Window::new(monitor, board, dpi, Source::PortableExact));
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
            assert_eq!(unsafe { IsChild(hwnd, hit) }, 0);
        };
        assert_passthrough(hit_at_height(10));

        // A barra de rolagem permanece opaca na imagem, inclusive a 0%.
        let [x, y, w, h] = state.borrow().scrollbar_state.unwrap().position;
        let mut scrollbar = POINT {
            x: x + w / 2,
            y: y + h / 2,
        };
        assert!(scrollbar.y >= 10);
        assert_ne!(unsafe { ClientToScreen(hwnd, &mut scrollbar) }, 0);
        assert_passthrough(unsafe { WindowFromPoint(scrollbar) });
    };

    send(RECONFIGURE, 0, 0);
    assert!(state.borrow().dropdown.active());
    assert!(!visible());
    assert!(!strip_visible());

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
    press(VK_F10);
    assert!(!visible());
    assert!(!strip_visible());
    send(instance::REVEAL_PANEL, 0, 0);
    assert!(visible());
    assert!(state.borrow().dropdown.pinned());
    assert_ne!(unsafe { GetForegroundWindow() }, hwnd);
    assert_translucent_input();

    change_opacity(50);
    assert_translucent_input();
    change_opacity(100);
    assert!(visible());
    assert!(!appearance::is_layered(hwnd));
    assert!(!appearance::is_click_through(hwnd));
    assert!(!strip_visible());
    assert_eq!(hit_at_height(10), hwnd);
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
    drop(guard);
    assert_ne!(unsafe { UnregisterClassW(class.lpszClassName, module) }, 0);
}

fn rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    assert_ne!(unsafe { GetWindowRect(hwnd, &mut rect) }, 0);
    rect
}

fn coords(rect: RECT) -> [i32; 4] {
    [rect.left, rect.top, rect.right, rect.bottom]
}
