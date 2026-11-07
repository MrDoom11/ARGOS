use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows_sys::w;

pub(crate) const LAUNCH_ENV: &str = "ARGOS_DESKTOP_LAUNCH";

pub(crate) fn show_error(message: &str) {
    let message: Vec<u16> = message.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            w!("ARGOS"),
            MB_OK | MB_ICONERROR,
        )
    };
}
