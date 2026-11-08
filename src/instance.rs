use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::thread;
use std::time::{Duration, Instant};

use semver::Version;
use windows_sys::Win32::Foundation::{
    ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, FILETIME, HANDLE, HWND, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FlashWindow, GetWindowThreadProcessId, IDYES, IsIconic, MB_DEFBUTTON2,
    MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_YESNO, MessageBoxW,
    PostMessageW, SW_RESTORE, SetForegroundWindow, ShowWindowAsync, WM_CLOSE,
};
use windows_sys::w;

pub(crate) const WINDOW_CLASS: *const u16 = w!("ArgosMonitorWindow");
pub(crate) const WINDOW_TITLE: *const u16 = w!("ARGOS — Monitor de hardware");
pub(crate) const REPLACEMENT_CLOSE: usize = 0x4152_474f;
const PANEL_BUSY: &str = "O painel do ARGOS está aberto. Feche-o antes de continuar.";
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT_MS: u32 = 10_000;

pub(crate) struct Instance {
    _panel: File,
    launch: Option<File>,
    previous_version: Option<Version>,
}

impl Instance {
    pub(crate) fn ready(&mut self) {
        self.launch = None;
    }

    pub(crate) fn previous_version(&self) -> Option<&Version> {
        self.previous_version.as_ref()
    }
}

pub(crate) fn enter() -> Result<Option<Instance>, String> {
    let (root, directory) = profile_directory()?;
    let launch = wait_for_launch(&directory.join("launch.lock"))?;
    let path = directory.join("panel.lock");
    let version = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("Versão do ARGOS inválida. {error}"))?;

    let mut panel = try_lock(&path, FILE_SHARE_READ)?;
    if panel.is_none() {
        let owner = read_owner(&path);
        // O painel pode ter fechado entre a primeira tentativa e a leitura.
        panel = try_lock(&path, FILE_SHARE_READ)?;
        if panel.is_none() {
            let window = owner.as_ref().and_then(|owner| find_panel(Some(owner.pid)));
            let Some(owner) =
                owner.filter(|owner| version.cmp_precedence(&owner.version) == Ordering::Greater)
            else {
                let window = window.or_else(|| legacy_panel(&root));
                if let Some(window) = window {
                    activate(window);
                } else {
                    inform(PANEL_BUSY);
                }
                return Ok(None);
            };

            let process = open_panel_process(owner.pid, Some(owner.created))?;
            if !exited(&process) {
                let window = window.ok_or(PANEL_BUSY)?;
                if !confirm(&format!(
                    "O ARGOS {} está aberto. Fechar o painel e abrir a versão {version}?",
                    owner.version
                )) {
                    activate(window);
                    return Ok(None);
                }
                close_panel(window, owner.pid, &process)?;
            }
            panel = try_lock(&path, FILE_SHARE_READ)?;
        }
    }
    let mut panel = panel.ok_or(PANEL_BUSY)?;
    if let Some(window) = legacy_panel(&root) {
        activate(window);
        return Ok(None);
    }

    let previous_version = read_owner(&path).map(|owner| owner.version);
    let owner = Owner {
        pid: std::process::id(),
        created: creation_time(unsafe { GetCurrentProcess() })
            .ok_or("Não foi possível identificar o processo do ARGOS.")?,
        version,
    };
    panel
        .set_len(0)
        .and_then(|()| writeln!(panel, "{} {} {}", owner.pid, owner.created, owner.version))
        .map_err(|error| format!("Não foi possível registrar a instância do ARGOS. {error}"))?;

    Ok(Some(Instance {
        _panel: panel,
        launch: Some(launch),
        previous_version,
    }))
}

// O instalador mantém launch.lock aberto até concluir a instalação.
pub(crate) fn prepare_install(silent: bool) -> Result<bool, String> {
    let (root, directory) = profile_directory()?;
    let path = directory.join("panel.lock");
    let mut panel = try_lock(&path, FILE_SHARE_READ)?;
    let owner = if panel.is_none() {
        let owner = if silent { None } else { read_owner(&path) };
        panel = try_lock(&path, FILE_SHARE_READ)?;
        owner
    } else {
        None
    };
    let (window, pid, created) = if panel.is_some() {
        let Some(window) = legacy_panel(&root) else {
            return Ok(true);
        };
        (window, window_pid(window).ok_or(PANEL_BUSY)?, None)
    } else {
        if silent {
            return Ok(false);
        }
        let owner = owner.ok_or(PANEL_BUSY)?;
        let window = find_panel(Some(owner.pid)).ok_or(PANEL_BUSY)?;
        (window, owner.pid, Some(owner.created))
    };

    if silent {
        return Ok(false);
    }
    let process = open_panel_process(pid, created)?;
    if !exited(&process) {
        if !confirm("Fechar o painel do ARGOS para continuar a instalação?") {
            return Ok(false);
        }
        close_panel(window, pid, &process)?;
    }
    let panel = match panel {
        Some(panel) => panel,
        None => try_lock(&path, FILE_SHARE_READ)?.ok_or(PANEL_BUSY)?,
    };
    let legacy_running = legacy_panel(&root).is_some();
    drop(panel);
    Ok(!legacy_running)
}

fn profile_directory() -> Result<(PathBuf, PathBuf), String> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("A pasta LocalAppData do Windows está indisponível.")?;
    let directory = root.join("ARGOS");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Não foi possível abrir a pasta do ARGOS. {error}"))?;
    Ok((root, directory))
}

fn try_lock(path: &Path, share_mode: u32) -> Result<Option<File>, String> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(share_mode)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                code as u32 == ERROR_SHARING_VIOLATION || code as u32 == ERROR_LOCK_VIOLATION
            }) =>
        {
            Ok(None)
        }
        Err(error) => Err(format!(
            "Não foi possível reservar o painel do ARGOS. {error}"
        )),
    }
}

fn wait_for_launch(path: &Path) -> Result<File, String> {
    let start = Instant::now();
    loop {
        if let Some(file) = try_lock(path, 0)? {
            return Ok(file);
        }
        if start.elapsed() >= LAUNCH_TIMEOUT {
            return Err("Outra abertura ou instalação do ARGOS está em andamento.".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

struct Owner {
    pid: u32,
    created: u64,
    version: Version,
}

fn read_owner(path: &Path) -> Option<Owner> {
    let mut record = String::new();
    File::open(path)
        .ok()?
        .take(1024)
        .read_to_string(&mut record)
        .ok()?;
    let (record, "") = record.split_once('\n')? else {
        return None;
    };
    let mut fields = record.split_whitespace();
    let owner = Owner {
        pid: fields.next()?.parse().ok()?,
        created: fields.next()?.parse().ok()?,
        version: Version::parse(fields.next()?).ok()?,
    };
    (owner.pid != 0 && owner.created != 0 && fields.next().is_none()).then_some(owner)
}

fn creation_time(process: HANDLE) -> Option<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
    (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn open_panel_process(pid: u32, created: Option<u64>) -> Result<OwnedHandle, String> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZATION_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return Err(PANEL_BUSY.into());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    if created.is_some_and(|created| creation_time(handle) != Some(created)) || !same_user(handle) {
        return Err(PANEL_BUSY.into());
    }
    Ok(process)
}

fn exited(process: &OwnedHandle) -> bool {
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

fn close_panel(window: HWND, pid: u32, process: &OwnedHandle) -> Result<(), String> {
    if exited(process) {
        return Ok(());
    }
    // Revalide a janela depois de uma confirmação, pois o painel pode ter fechado sozinho.
    if find_panel(Some(pid)) == Some(window) {
        let posted = unsafe { PostMessageW(window, WM_CLOSE, REPLACEMENT_CLOSE, 0) };
        if posted == 0 && !exited(process) {
            return Err(PANEL_BUSY.into());
        }
    }
    match unsafe { WaitForSingleObject(process.as_raw_handle(), CLOSE_TIMEOUT_MS) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => {
            Err("O painel do ARGOS demorou a fechar. Feche-o antes de continuar.".into())
        }
        _ => Err(PANEL_BUSY.into()),
    }
}

fn find_panel(pid: Option<u32>) -> Option<HWND> {
    let mut previous = null_mut();
    loop {
        let window = unsafe { FindWindowExW(null_mut(), previous, WINDOW_CLASS, WINDOW_TITLE) };
        if window.is_null() {
            return None;
        }
        if let Some(candidate) = window_pid(window)
            && pid.map_or_else(
                || open_panel_process(candidate, None).is_ok(),
                |pid| candidate == pid,
            )
        {
            return Some(window);
        }
        previous = window;
    }
}

fn window_pid(window: HWND) -> Option<u32> {
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    (thread != 0 && pid != 0).then_some(pid)
}

fn same_user(process: HANDLE) -> bool {
    let Some(current) = token_user(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    let Some(target) = token_user(process) else {
        return false;
    };
    // SAFETY: as duas áreas alinhadas contêm TOKEN_USER e os SIDs ainda estão válidos.
    unsafe {
        let current = (*current.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let target = (*target.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        EqualSid(current, target) != 0
    }
}

fn token_user(process: HANDLE) -> Option<Vec<usize>> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size) };
    if (size as usize) < std::mem::size_of::<TOKEN_USER>() {
        return None;
    }
    let mut buffer = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    (ok != 0).then_some(buffer)
}

fn legacy_panel(root: &Path) -> Option<HWND> {
    let known = known_local_app_data()?;
    let same_profile = root == known
        || fs::canonicalize(root)
            .ok()
            .zip(fs::canonicalize(&known).ok())
            .is_some_and(|(root, known)| root == known);
    same_profile.then(|| find_panel(None)).flatten()
}

fn known_local_app_data() -> Option<PathBuf> {
    let mut value = null_mut();
    // SAFETY: o Shell devolve uma cadeia UTF-16 terminada em NUL e alocada com CoTaskMemAlloc.
    unsafe {
        let status = SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DONT_VERIFY as u32,
            null_mut(),
            &mut value,
        );
        let path = if status >= 0 && !value.is_null() {
            let len = (0..).take_while(|&index| *value.add(index) != 0).count();
            Some(PathBuf::from(OsString::from_wide(
                std::slice::from_raw_parts(value, len),
            )))
        } else {
            None
        };
        CoTaskMemFree(value.cast());
        path
    }
}

fn activate(window: HWND) {
    unsafe {
        if IsIconic(window) != 0 {
            ShowWindowAsync(window, SW_RESTORE);
        }
        if SetForegroundWindow(window) == 0 {
            FlashWindow(window, 1);
        }
    }
}

fn confirm(message: &str) -> bool {
    let text: Vec<u16> = message.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            w!("ARGOS"),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn inform(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            w!("ARGOS"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("argos-instance-test-{}-{sequence}", std::process::id());
            let path = std::env::temp_dir().join(name);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0.join("launch.lock"));
            let _ = fs::remove_file(self.0.join("panel.lock"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn launch_reservation_releases_before_the_panel_owner() {
        let scratch = Scratch::new();
        let launch_path = scratch.0.join("launch.lock");
        let panel_path = scratch.0.join("panel.lock");
        let mut panel = try_lock(&panel_path, FILE_SHARE_READ).unwrap().unwrap();
        writeln!(panel, "123 456 0.2.0+test").unwrap();
        let mut instance = Instance {
            _panel: panel,
            launch: try_lock(&launch_path, 0).unwrap(),
            previous_version: None,
        };
        assert!(try_lock(&launch_path, 0).unwrap().is_none());
        assert!(try_lock(&panel_path, FILE_SHARE_READ).unwrap().is_none());
        let owner = read_owner(&panel_path).unwrap();
        assert_eq!((owner.pid, owner.created), (123, 456));
        assert_eq!(owner.version.to_string(), "0.2.0+test");

        instance.ready();
        assert!(try_lock(&launch_path, 0).unwrap().is_some());
        assert!(try_lock(&panel_path, FILE_SHARE_READ).unwrap().is_none());
        drop(instance);
        assert!(try_lock(&panel_path, FILE_SHARE_READ).unwrap().is_some());
    }

    #[test]
    fn process_identity_checks_creation_time_before_allowing_close() {
        let pid = std::process::id();
        let created = creation_time(unsafe { GetCurrentProcess() }).unwrap();
        assert!(open_panel_process(pid, Some(created + 1)).is_err());
        assert!(!exited(&open_panel_process(pid, Some(created)).unwrap()));
    }
}
