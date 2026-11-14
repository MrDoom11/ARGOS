use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::Command;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    TokenPrimary,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithTokenW, GetCurrentProcess,
    LOGON_WITH_PROFILE, OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

use crate::instance;

// The update worker and desktop integrations need the ordinary user token,
// even when the monitoring UI has been opened as administrator.
pub(crate) fn spawn(command: &mut Command) -> io::Result<OwnedHandle> {
    let elevated = instance::process_is_elevated(unsafe { GetCurrentProcess() })
        .ok_or_else(|| io::Error::other("Não foi possível verificar as permissões do ARGOS."))?;
    if !elevated {
        return command.spawn().map(OwnedHandle::from);
    }

    let program = Path::new(command.get_program());
    let directory = command.get_current_dir().or_else(|| program.parent());
    let Some(directory) = directory.filter(|directory| directory.is_absolute()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Pasta da operação inválida.",
        ));
    };
    if !program.is_absolute() || command.get_envs().next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Comando da operação inválido.",
        ));
    }
    let program = wide_string(program.as_os_str())?;
    let directory = wide_string(directory.as_os_str())?;
    let mut arguments = Vec::new();
    for argument in std::iter::once(command.get_program()).chain(command.get_args()) {
        if !arguments.is_empty() {
            arguments.push(b' ' as u16);
        }
        append_argument(&mut arguments, argument)?;
    }
    arguments.push(0);
    let token = desktop_token()?;
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let started = unsafe {
        CreateProcessWithTokenW(
            token.as_raw_handle(),
            LOGON_WITH_PROFILE,
            program.as_ptr(),
            arguments.as_mut_ptr(),
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            null(),
            directory.as_ptr(),
            &startup,
            &mut process,
        )
    };
    if started == 0 {
        return Err(io::Error::last_os_error());
    }
    let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    Ok(unsafe { OwnedHandle::from_raw_handle(process.hProcess) })
}

fn desktop_token() -> io::Result<OwnedHandle> {
    let window = unsafe { GetShellWindow() };
    let mut pid = 0;
    if window.is_null() || unsafe { GetWindowThreadProcessId(window, &mut pid) } == 0 {
        return Err(io::Error::other(
            "Área de trabalho do Windows indisponível.",
        ));
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut token = null_mut();
    let opened = unsafe {
        OpenProcessToken(
            process.as_raw_handle(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        )
    };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut primary = null_mut();
    let duplicated = unsafe {
        DuplicateTokenEx(
            token.as_raw_handle(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    if duplicated == 0 {
        return Err(io::Error::last_os_error());
    }
    let primary = unsafe { OwnedHandle::from_raw_handle(primary) };
    if !instance::token_is_ordinary_user(primary.as_raw_handle()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "A área de trabalho precisa estar aberta com as permissões normais da mesma conta.",
        ));
    }
    Ok(primary)
}

fn wide_string(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut units = value.encode_wide().collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Texto de comando inválido.",
        ));
    }
    units.push(0);
    Ok(units)
}

fn append_argument(command: &mut Vec<u16>, value: &OsStr) -> io::Result<()> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    command.push(QUOTE);
    let mut backslashes = 0;
    for unit in value.encode_wide() {
        match unit {
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Argumento inválido.",
                ));
            }
            BACKSLASH => backslashes += 1,
            _ => {
                let escaped = if unit == QUOTE {
                    2 * backslashes + 1
                } else {
                    backslashes
                };
                command.extend(std::iter::repeat_n(BACKSLASH, escaped));
                backslashes = 0;
                command.push(unit);
            }
        }
    }
    command.extend(std::iter::repeat_n(BACKSLASH, 2 * backslashes));
    command.push(QUOTE);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{LocalFree, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    #[test]
    fn child_starts_with_ordinary_permissions() {
        let path = std::env::current_exe().unwrap();
        let worker = spawn(
            Command::new(path)
                .arg("--exact")
                .arg("--ignored")
                .arg("desktop_child::tests::worker_token_is_ordinary"),
        )
        .unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(worker.as_raw_handle(), 5_000) },
            WAIT_OBJECT_0
        );
        let mut exit = u32::MAX;
        assert_ne!(
            unsafe { GetExitCodeProcess(worker.as_raw_handle(), &mut exit) },
            0
        );
        assert_eq!(exit, 0);
    }

    #[test]
    #[ignore = "launched explicitly by child_starts_with_ordinary_permissions"]
    fn worker_token_is_ordinary() {
        assert_eq!(
            instance::process_is_elevated(unsafe { GetCurrentProcess() }),
            Some(false)
        );
    }

    #[test]
    fn child_command_line_preserves_paths_and_arguments() {
        use std::os::windows::ffi::OsStringExt;
        let expected = [
            r"C:\ARGOS do usuário\argos.exe",
            "",
            "um argumento com espaços",
            r#"aspas" e \ no argumento"#,
            r"C:\pasta\",
        ];
        let mut command = Vec::new();
        for (index, argument) in expected.iter().enumerate() {
            if index > 0 {
                command.push(b' ' as u16);
            }
            append_argument(&mut command, OsStr::new(argument)).unwrap();
        }
        command.push(0);
        let mut count = 0;
        let parsed = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
        assert!(!parsed.is_null());
        let arguments = unsafe { std::slice::from_raw_parts(parsed, count as usize) };
        let arguments = arguments
            .iter()
            .map(|&argument| {
                let len = (0..)
                    .take_while(|&index| unsafe { *argument.add(index) } != 0)
                    .count();
                std::ffi::OsString::from_wide(unsafe { std::slice::from_raw_parts(argument, len) })
            })
            .collect::<Vec<_>>();
        unsafe { LocalFree(parsed.cast()) };
        assert_eq!(arguments, expected.map(std::ffi::OsString::from));
    }
}
