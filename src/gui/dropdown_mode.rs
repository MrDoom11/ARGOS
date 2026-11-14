use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    IsWindowEnabled, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};

use super::board::DropdownHotkey;
use super::dropdown::{
    Display, Preview, contains, displays, set_dropdown_effects, set_visible, slide,
};
use super::*;

pub(super) const TIMER: usize = 2;
pub(super) const RECONFIGURE: u32 = WM_APP + 3;
const HOTKEY: i32 = 1;
const POLL_MS: u32 = 50;
const OPEN_AFTER: Duration = Duration::from_millis(150);
const HIDE_AFTER: Duration = Duration::from_millis(300);

#[derive(Default)]
pub(super) struct Runtime {
    active: Option<Active>,
    registered: Option<DropdownHotkey>,
    preview: Option<(Preview, Instant)>,
    fresh_until: Option<Instant>,
    pub(super) save_failed: bool,
}

impl Runtime {
    pub(super) fn active(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn pinned(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.gesture.exposure == Exposure::Held)
    }

    pub(super) fn hotkey_bound(&self) -> bool {
        self.registered.is_some()
    }
}

struct Active {
    display: Display,
    normal_style: isize,
    normal_bounds: [i32; 4],
    normal_dpi: u32,
    gesture: Gesture,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Exposure {
    #[default]
    Hidden,
    Hover,
    Held,
}

#[derive(Default)]
struct Gesture {
    exposure: Exposure,
    since: Option<Instant>,
    wait_for_exit: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Prefetch,
    Show,
    Hide,
}

impl Gesture {
    fn tick(&mut self, now: Instant, edge: bool, panel: bool, busy: bool) -> Action {
        match self.exposure {
            Exposure::Hidden if self.wait_for_exit => {
                self.wait_for_exit = edge;
                Action::None
            }
            Exposure::Hidden if edge && !busy => match self.since {
                None => {
                    self.since = Some(now);
                    Action::Prefetch
                }
                Some(since) if now.duration_since(since) >= OPEN_AFTER => {
                    self.exposure = Exposure::Hover;
                    self.since = None;
                    Action::Show
                }
                Some(_) => Action::None,
            },
            Exposure::Hover if !edge && !panel && !busy => {
                let since = *self.since.get_or_insert(now);
                if now.duration_since(since) >= HIDE_AFTER {
                    self.hide(false);
                    Action::Hide
                } else {
                    Action::None
                }
            }
            _ => {
                self.since = None;
                Action::None
            }
        }
    }

    fn hide(&mut self, wait_for_exit: bool) {
        self.exposure = Exposure::Hidden;
        self.since = None;
        self.wait_for_exit = wait_for_exit;
    }

    fn hold(&mut self) {
        self.exposure = Exposure::Held;
        self.since = None;
    }
}

pub(super) fn sync(hwnd: HWND, state: &RefCell<Window>) {
    let selected = {
        let app = state.borrow();
        let settings = &app.board.dropdown;
        settings.monitor.clone().filter(|_| settings.enabled)
    };
    let display = selected.and_then(|id| displays().into_iter().find(|display| display.id == id));
    match display {
        Some(display) => {
            if state.borrow().dropdown.active() {
                {
                    let mut app = state.borrow_mut();
                    let active = app.dropdown.active.as_mut().unwrap();
                    active.display = display;
                    active.gesture.since = None;
                }
                if !place(hwnd, state) {
                    leave(hwnd, state);
                }
            } else {
                enter(hwnd, state, display);
            }
        }
        None => leave(hwnd, state),
    }
}

fn enter(hwnd: HWND, state: &RefCell<Window>, display: Display) {
    let Some(placement) = window_placement(hwnd) else {
        return;
    };
    if unsafe { SetTimer(hwnd, TIMER, POLL_MS, None) } == 0 {
        return;
    }
    let rect = placement.rcNormalPosition;
    let normal_bounds = [rect.left, rect.top, rect.right, rect.bottom];
    let normal_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let normal_dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    {
        let mut app = state.borrow_mut();
        if app.board.window_bounds != Some(normal_bounds) {
            app.board.window_bounds = Some(normal_bounds);
            app.save_failed = app.board.save().is_err();
        }
        app.dropdown.active = Some(Active {
            display,
            normal_style,
            normal_bounds,
            normal_dpi,
            gesture: Gesture::default(),
        });
    }
    unsafe {
        if IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
    }
    set_dropdown_effects(hwnd, true);
    if !set_visible(hwnd, false) {
        leave(hwnd, state);
        return;
    }
    unsafe {
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ((normal_style as u32 & !WS_EX_APPWINDOW) | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE)
                as isize,
        );
    }
    if !place(hwnd, state) {
        leave(hwnd, state);
        return;
    }
    let key = state.borrow().board.dropdown.hotkey;
    if let Some(key) = key {
        bind(hwnd, state, key);
    }
}

fn place(hwnd: HWND, state: &RefCell<Window>) -> bool {
    let (display, trigger, size, normal_dpi) = {
        let app = state.borrow();
        let active = app.dropdown.active.as_ref().unwrap();
        let settings = &app.board.dropdown;
        let [left, top, right, bottom] = active.normal_bounds;
        (
            active.display.clone(),
            active.display.trigger(settings.alignment, settings.width),
            [right - left, bottom - top],
            active.normal_dpi,
        )
    };
    let move_to = |dpi| {
        let [w, h] = size.map(|value| {
            let scaled = (i64::from(value) * i64::from(dpi) + i64::from(normal_dpi) / 2)
                / i64::from(normal_dpi);
            scaled.clamp(1, i64::from(i32::MAX)) as i32
        });
        let rect = display.panel(w, h, trigger);
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.left,
                rect.top,
                width(rect),
                height(rect),
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            ) != 0
        }
    };
    let before = unsafe { GetDpiForWindow(hwnd) }.max(96);
    if !move_to(before) {
        return false;
    }
    let after = unsafe { GetDpiForWindow(hwnd) }.max(96);
    after == before || move_to(after)
}

fn leave(hwnd: HWND, state: &RefCell<Window>) {
    let Some(active) = state.borrow_mut().dropdown.active.take() else {
        return;
    };
    unbind(hwnd, state);
    state.borrow_mut().dropdown.fresh_until = None;
    set_visible(hwnd, false);
    set_dropdown_effects(hwnd, false);
    unsafe {
        let mask = appearance::LAYERED_STYLES as isize;
        let layered = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & mask;
        let style = (active.normal_style & !mask) | layered;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE | SWP_FRAMECHANGED,
        );
    }
    restore_window_bounds(hwnd, active.normal_bounds);
    show_normal(hwnd);
    stop_if_idle(hwnd, state);
}

fn show(hwnd: HWND, state: &RefCell<Window>) {
    {
        let mut app = state.borrow_mut();
        app.refresh();
        app.dropdown.fresh_until = Some(Instant::now() + Duration::from_millis(350));
    }
    transition(hwnd, state, true);
}

fn hide(hwnd: HWND, state: &RefCell<Window>) {
    state.borrow_mut().dropdown.fresh_until = None;
    transition(hwnd, state, false);
}

fn transition(hwnd: HWND, state: &RefCell<Window>, visible: bool) {
    sync_scrollbar(hwnd, state);
    let surface = if appearance::is_layered(hwnd) {
        let screen = unsafe { GetDC(null_mut()) };
        if screen.is_null() {
            None
        } else {
            let mut surface = Surface::default();
            let ready = state
                .borrow()
                .render_surface(&mut surface, screen, client(hwnd));
            unsafe { ReleaseDC(null_mut(), screen) };
            ready.then_some(surface)
        }
    } else {
        None
    };
    if !slide(
        hwnd,
        visible,
        |dc| {
            let mut app = state.borrow_mut();
            app.paint_opaque(dc, client(hwnd));
            app.paint_scrollbar(dc);
        },
        surface,
    ) {
        leave(hwnd, state);
    }
    sync_input_strip(hwnd, state);
}

pub(super) fn tick(hwnd: HWND, state: &RefCell<Window>) {
    let now = Instant::now();
    let (action, redraw) = {
        let Ok(mut app) = state.try_borrow_mut() else {
            return;
        };
        if app
            .dropdown
            .preview
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            app.dropdown.preview = None;
        }
        let redraw = app.dropdown.fresh_until.is_some() && app.refresh_cached();
        if redraw || app.dropdown.fresh_until.is_some_and(|until| now >= until) {
            app.dropdown.fresh_until = None;
        }
        let busy = app.open_menu.is_some()
            || app.drag.is_some()
            || unsafe { !GetCapture().is_null() || IsWindowEnabled(hwnd) == 0 };
        let settings = &app.board.dropdown;
        let (alignment, width) = (settings.alignment, settings.width);
        let action = match app.dropdown.active.as_mut() {
            Some(active) => {
                let mut cursor = POINT::default();
                if unsafe { GetCursorPos(&mut cursor) } == 0 {
                    active.gesture.since = None;
                    Action::None
                } else {
                    let trigger = active.display.trigger(alignment, width);
                    let mut panel = RECT::default();
                    let on_panel = active.gesture.exposure == Exposure::Hover
                        && unsafe { GetWindowRect(hwnd, &mut panel) } != 0
                        && contains(panel, cursor);
                    active
                        .gesture
                        .tick(now, contains(trigger, cursor), on_panel, busy)
                }
            }
            None => Action::None,
        };
        (action, redraw)
    };
    if redraw {
        invalidate(hwnd);
    }
    match action {
        Action::None => stop_if_idle(hwnd, state),
        Action::Prefetch => {
            let _ = state.borrow().monitor.snapshot();
        }
        Action::Show => show(hwnd, state),
        Action::Hide => hide(hwnd, state),
    }
}

pub(super) fn reveal(hwnd: HWND, state: &RefCell<Window>, toggle: bool) {
    let action = {
        let mut app = state.borrow_mut();
        if app.open_menu.is_some() || unsafe { IsWindowEnabled(hwnd) } == 0 {
            return;
        }
        let Some(active) = &mut app.dropdown.active else {
            return;
        };
        match active.gesture.exposure {
            Exposure::Hidden => {
                active.gesture.hold();
                Action::Show
            }
            _ if toggle => {
                active.gesture.hide(true);
                Action::Hide
            }
            _ => {
                active.gesture.hold();
                Action::None
            }
        }
    };
    match action {
        Action::Show => show(hwnd, state),
        Action::Hide => hide(hwnd, state),
        _ => {}
    }
}

pub(super) fn pin(state: &RefCell<Window>) {
    if let Some(active) = &mut state.borrow_mut().dropdown.active {
        if active.gesture.exposure == Exposure::Held {
            active.gesture.exposure = Exposure::Hover;
            active.gesture.since = None;
        } else {
            active.gesture.hold();
        }
    }
}

pub(super) fn preview(hwnd: HWND, state: &RefCell<Window>, display: &Display) {
    let (alignment, width) = {
        let app = state.borrow();
        (app.board.dropdown.alignment, app.board.dropdown.width)
    };
    let Some(preview) = Preview::show(display.trigger(alignment, width), &display.label) else {
        return;
    };
    if unsafe { SetTimer(hwnd, TIMER, POLL_MS, None) } != 0 {
        state.borrow_mut().dropdown.preview =
            Some((preview, Instant::now() + Duration::from_secs(2)));
    }
}

fn stop_if_idle(hwnd: HWND, state: &RefCell<Window>) {
    let app = state.borrow();
    if !app.dropdown.active() && app.dropdown.preview.is_none() {
        unsafe { KillTimer(hwnd, TIMER) };
    }
}

pub(super) fn bind(hwnd: HWND, state: &RefCell<Window>, key: DropdownHotkey) -> bool {
    unbind(hwnd, state);
    let bound = unsafe { RegisterHotKey(hwnd, HOTKEY, key.modifiers | MOD_NOREPEAT, key.key) } != 0;
    if bound {
        state.borrow_mut().dropdown.registered = Some(key);
    }
    bound
}

pub(super) fn unbind(hwnd: HWND, state: &RefCell<Window>) {
    let registered = state.borrow_mut().dropdown.registered.take();
    if registered.is_some() {
        unsafe { UnregisterHotKey(hwnd, HOTKEY) };
    }
}

pub(super) fn hotkey_message(hwnd: HWND, state: &RefCell<Window>, wp: WPARAM, lp: LPARAM) {
    if wp == HOTKEY as WPARAM
        && state.borrow().dropdown.registered.is_some_and(|key| {
            (u32::from(lp as u16) & !MOD_NOREPEAT) == key.modifiers
                && u32::from((lp >> 16) as u16) == key.key
        })
    {
        reveal(hwnd, state, true);
    }
}

#[cfg(test)]
#[path = "dropdown_mode_test.rs"]
mod native_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_requires_a_dwell_and_recollects_after_a_pause() {
        let now = Instant::now();
        let mut gesture = Gesture::default();
        assert_eq!(gesture.tick(now, true, false, false), Action::Prefetch);
        assert_eq!(
            gesture.tick(now + OPEN_AFTER / 2, false, false, false),
            Action::None
        );
        let reentered = now + OPEN_AFTER;
        assert_eq!(
            gesture.tick(reentered, true, false, false),
            Action::Prefetch
        );
        assert_eq!(
            gesture.tick(reentered + OPEN_AFTER, true, false, false),
            Action::Show
        );
        let left = reentered + OPEN_AFTER * 2;
        assert_eq!(gesture.tick(left, false, false, false), Action::None);
        assert_eq!(
            gesture.tick(left + HIDE_AFTER, false, false, true),
            Action::None
        );
        let closed_menu = left + HIDE_AFTER * 2;
        assert_eq!(gesture.tick(closed_menu, false, false, false), Action::None);
        assert_eq!(
            gesture.tick(closed_menu + HIDE_AFTER, false, false, false),
            Action::Hide
        );
    }

    #[test]
    fn held_panel_stays_open_and_keyboard_hide_requires_leaving_edge() {
        let now = Instant::now();
        let mut gesture = Gesture::default();
        gesture.hold();
        assert_eq!(
            gesture.tick(now + HIDE_AFTER * 2, false, false, false),
            Action::None
        );
        gesture.hide(true);
        assert_eq!(gesture.tick(now, true, false, false), Action::None);
        assert_eq!(
            gesture.tick(now + OPEN_AFTER, true, false, false),
            Action::None
        );
        assert_eq!(
            gesture.tick(now + OPEN_AFTER * 2, false, false, false),
            Action::None
        );
        assert_eq!(
            gesture.tick(now + OPEN_AFTER * 3, true, false, false),
            Action::Prefetch
        );
    }
}
