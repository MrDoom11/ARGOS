use std::collections::HashSet;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{HWND, LPARAM, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible,
};

const MAX_PROCESSES: usize = 32;
const IMAGE_PATH_CAPACITY: usize = 32_768;

pub(crate) struct Process {
    pub(crate) pid: u32,
    pub(crate) name: String,
}

pub(crate) struct Target(OwnedHandle);

impl Target {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        open_process(pid, PROCESS_SYNCHRONIZE).map(Self)
    }

    pub(crate) fn is_running(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) }) == WAIT_TIMEOUT
    }
}

pub(crate) fn foreground_pid() -> Option<u32> {
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    (thread != 0 && pid != 0 && pid != std::process::id()).then_some(pid)
}

pub(crate) fn process_name(pid: u32) -> Option<String> {
    executable_name(pid, &mut vec![0; IMAGE_PATH_CAPACITY])
}

pub(crate) fn visible_processes() -> Vec<Process> {
    let mut list = WindowProcessList {
        own_pid: std::process::id(),
        seen: HashSet::new(),
        entries: Vec::new(),
        image_path: vec![0; IMAGE_PATH_CAPACITY],
    };
    // EnumWindows calls collect_process synchronously, so list remains valid for each call.
    unsafe {
        EnumWindows(
            Some(collect_process),
            &mut list as *mut WindowProcessList as LPARAM,
        )
    };

    list.entries
        .sort_by_cached_key(|process| (process.name.to_lowercase(), process.pid));
    list.entries
}

struct WindowProcessList {
    own_pid: u32,
    seen: HashSet<u32>,
    entries: Vec<Process>,
    image_path: Vec<u16>,
}

unsafe extern "system" fn collect_process(window: HWND, context: LPARAM) -> i32 {
    if unsafe { IsWindowVisible(window) } == 0 {
        return 1;
    }

    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(window, &mut pid) } == 0 {
        return 1;
    }
    let list = unsafe { &mut *(context as *mut WindowProcessList) };
    if pid == 0 || pid == list.own_pid || !list.seen.insert(pid) {
        return 1;
    }

    if let Some(name) = executable_name(pid, &mut list.image_path) {
        list.entries.push(Process { pid, name });
    }
    i32::from(list.entries.len() < MAX_PROCESSES)
}

fn executable_name(pid: u32, image_path: &mut [u16]) -> Option<String> {
    let handle = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let mut len = image_path.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(handle.as_raw_handle(), 0, image_path.as_mut_ptr(), &mut len)
    } == 0
    {
        return None;
    }

    let name = image_path
        .get(..len as usize)?
        .rsplit(|unit| *unit == u16::from(b'\\'))
        .next()?;
    (!name.is_empty()).then(|| String::from_utf16_lossy(name))
}

fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> io::Result<OwnedHandle> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // A successful OpenProcess transfers ownership of a CloseHandle-compatible handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
