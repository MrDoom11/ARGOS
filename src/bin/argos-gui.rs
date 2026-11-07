#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

#[cfg(windows)]
#[path = "../desktop.rs"]
mod desktop;

#[cfg(windows)]
fn main() -> ExitCode {
    match launch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            desktop::show_error(&format!(
                "Não foi possível abrir o painel do ARGOS. {error}"
            ));
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn launch() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let launcher = std::env::current_exe()?;
    let directory = launcher
        .parent()
        .ok_or_else(|| std::io::Error::other("pasta do ARGOS indisponível"))?;
    Command::new(directory.join("argos.exe"))
        .current_dir(directory)
        .arg("gui")
        .env(desktop::LAUNCH_ENV, "1")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    ExitCode::FAILURE
}
