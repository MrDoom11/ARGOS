use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    ERROR_UNSUPPORTED_TYPE,
};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_NOEXPAND, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows_sys::w;

const RUN_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: *const u16 = w!("ARGOS (MrDoom11)");
const MAX_COMMAND_UNITS: usize = 260;
const QUOTE: u16 = b'"' as u16;
const CSTR_EQUAL: i32 = 2;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CompareStringOrdinal(
        first: *const u16,
        first_len: i32,
        second: *const u16,
        second_len: i32,
        ignore_case: i32,
    ) -> i32;
}

enum Registration {
    Missing,
    Matches,
    Other,
}

pub(super) fn enabled(directory: &Path) -> io::Result<bool> {
    Ok(matches!(
        registration(&launcher_command(directory)?)?,
        Registration::Matches
    ))
}

pub(super) fn set_enabled(directory: &Path, enabled: bool) -> io::Result<()> {
    let command = launcher_command(directory)?;
    let status = match (enabled, registration(&command)?) {
        (true, Registration::Missing) => unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ,
                command.as_ptr().cast(),
                size_of_val(command.as_slice()) as u32,
            )
        },
        (true, Registration::Other) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Já existe uma entrada de inicialização com esse nome para outro caminho.",
            ));
        }
        (false, Registration::Matches) => {
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) };
            if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                return Ok(());
            }
            status
        }
        _ => return Ok(()),
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn launcher_command(directory: &Path) -> io::Result<Vec<u16>> {
    if !directory.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "O caminho da instalação do ARGOS precisa ser absoluto.",
        ));
    }
    let launcher = directory.join("argos-gui.exe");
    let path: Vec<u16> = launcher.as_os_str().encode_wide().collect();
    if path.len() + 2 > MAX_COMMAND_UNITS || path.iter().any(|&unit| matches!(unit, QUOTE | 0)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "O caminho da instalação do ARGOS não é válido para a inicialização do Windows.",
        ));
    }
    Ok(std::iter::once(QUOTE)
        .chain(path)
        .chain([QUOTE, 0])
        .collect())
}

fn registration(expected: &[u16]) -> io::Result<Registration> {
    let mut command = [0u16; MAX_COMMAND_UNITS + 1];
    let mut bytes = size_of_val(&command) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ | RRF_NOEXPAND,
            null_mut(),
            command.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    match status {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(Registration::Missing),
        ERROR_MORE_DATA | ERROR_UNSUPPORTED_TYPE => Ok(Registration::Other),
        ERROR_SUCCESS => {
            let bytes = bytes as usize;
            let actual = command.get(..bytes / size_of::<u16>());
            if bytes.is_multiple_of(size_of::<u16>())
                && actual.is_some_and(|actual| same_command(actual, expected))
            {
                Ok(Registration::Matches)
            } else {
                Ok(Registration::Other)
            }
        }
        _ => Err(io::Error::from_raw_os_error(status as i32)),
    }
}

fn same_command(actual: &[u16], expected: &[u16]) -> bool {
    actual == expected
        || actual.len() == expected.len()
            && unsafe {
                CompareStringOrdinal(
                    actual.as_ptr(),
                    actual.len() as i32,
                    expected.as_ptr(),
                    expected.len() as i32,
                    1,
                ) == CSTR_EQUAL
            }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_the_unicode_launcher_and_rejects_invalid_paths() {
        let command = launcher_command(Path::new(r"C:\Usuários\João\ARGOS")).unwrap();
        let expected: Vec<_> = "\"C:\\Usuários\\João\\ARGOS\\argos-gui.exe\"\0"
            .encode_utf16()
            .collect();
        assert_eq!(command, expected);

        let uppercase = launcher_command(Path::new(r"C:\USUÁRIOS\JOÃO\ARGOS")).unwrap();
        assert!(same_command(&command, &uppercase));
        let different = launcher_command(Path::new(r"C:\Usuários\José\ARGOS")).unwrap();
        assert!(!same_command(&command, &different));

        assert!(launcher_command(Path::new("ARGOS")).is_err());
        assert!(launcher_command(Path::new(&format!("C:\\{}", "a".repeat(260)))).is_err());
    }
}
