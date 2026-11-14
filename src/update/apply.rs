use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use semver::Version;
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_SHARE_READ, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

use super::release::{self, INSTALLER_NAME, Release};

const PANEL_EXE: &str = "argos.exe";
const LAUNCHER_EXE: &str = "argos-gui.exe";
const WORKER_EXE: &str = "argos-update.exe";
const PORTABLE_POINTER: &str = "portable-current";
const PORTABLE_SWITCH: &str = "/ARGOS-RUN-PORTABLE";
const PARENT_TIMEOUT_MS: u32 = 30_000;
const PORTABLE_STARTUP_MS: u32 = 1_200;

pub(super) fn spawn(
    directory: &Path,
    installed: Option<&Path>,
    restart: bool,
) -> Result<(), String> {
    let directory = job_directory(directory)?;
    let (release, _, _package_guard) = verified_package(&directory)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("versão Cargo válida");
    require_newer(&release.version, &current)?;
    let executable = std::env::current_exe().map_err(io_error)?;
    if let Some(installed) = installed {
        let registered = checked_install_directory(installed)?;
        let running = executable.parent().ok_or("Pasta do ARGOS indisponível.")?;
        if running.canonicalize().map_err(io_error)?
            != registered.canonicalize().map_err(io_error)?
        {
            return Err("A cópia em execução não pertence à instalação do ARGOS.".into());
        }
    }
    let created = process_created(unsafe { GetCurrentProcess() })?;
    let pid = std::process::id();
    let attempt = attempt_directory(&directory, pid, created);
    fs::create_dir(&attempt).map_err(io_error)?;
    let worker = attempt.join(WORKER_EXE);
    if let Err(error) = copy_new(&executable, &worker) {
        let _ = fs::remove_dir(&attempt);
        return Err(io_error(error));
    }

    let mut command = hidden(&worker);
    command
        .current_dir(&directory)
        .arg("apply-update")
        .arg(pid.to_string())
        .arg(created.to_string())
        .arg(&directory)
        .arg(if restart { "1" } else { "0" });
    if let Some(installed) = installed {
        command.arg(installed);
    }
    if let Err(error) = crate::desktop_child::spawn(quiet(&mut command)) {
        let _ = fs::remove_file(worker);
        let _ = fs::remove_dir(attempt);
        return Err(io_error(error));
    }
    Ok(())
}

pub(super) fn run(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    if crate::instance::process_is_elevated(unsafe { GetCurrentProcess() }) != Some(false) {
        return Err("A atualização exige as permissões normais da conta do Windows.".into());
    }
    let pid = args
        .next()
        .and_then(|value| value.to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != 0 && pid != std::process::id())
        .ok_or("Processo anterior do ARGOS inválido.")?;
    let created = args
        .next()
        .and_then(|value| value.to_str()?.parse::<u64>().ok())
        .filter(|&time| time != 0)
        .ok_or("Identidade do processo anterior inválida.")?;
    let directory = job_directory(&PathBuf::from(
        args.next().ok_or("Atualização sem pasta de trabalho.")?,
    ))?;
    let restart = match args.next().as_deref().and_then(OsStr::to_str) {
        Some("0") => false,
        Some("1") => true,
        _ => return Err("Opção de reabertura inválida.".into()),
    };
    let installed = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("Argumentos extras na atualização do ARGOS.".into());
    }
    let worker = std::env::current_exe()
        .map_err(io_error)?
        .canonicalize()
        .map_err(io_error)?;
    let attempt = attempt_directory(&directory, pid, created);
    if worker != attempt.join(WORKER_EXE) {
        return Err("O atualizador precisa ser executado da pasta de trabalho.".into());
    }

    wait_for_parent(pid, created)?;
    let root = super::root()?;
    let _update = lock(
        &root.join("update.lock"),
        0,
        "Outra atualização do ARGOS está em andamento.",
    )?;
    let (release, package, _package_guard) = verified_package(&directory)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("versão Cargo válida");
    require_newer(&release.version, &current)?;

    if let Some(installed) = installed {
        apply_installed(&attempt, &package, &installed, &release.version, restart)
    } else {
        apply_portable(
            &root,
            &directory,
            &attempt,
            &package,
            &release.version,
            restart,
        )
    }
}

pub(super) fn launch_cached(current: &Version) -> Result<bool, String> {
    let Some((_, package, _package_guard)) = newer_cached_package(current)? else {
        return Ok(false);
    };
    let process = start_portable(&package)?;
    // O instalador fica aberto enquanto o painel portátil executa. Saída imediata
    // permite que o pacote original volte a oferecer sua versão embutida.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), PORTABLE_STARTUP_MS) } {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Err("A atualização portátil encerrou antes de abrir o painel.".into()),
        _ => Err(io_error(io::Error::last_os_error())),
    }
}

pub(super) fn cached_version(current: &Version) -> Result<Option<Version>, String> {
    Ok(newer_cached_package(current)?.map(|(release, _, _)| release.version))
}

fn newer_cached_package(current: &Version) -> Result<Option<(Release, PathBuf, File)>, String> {
    let Some(directory) = cached_directory(&super::root()?)? else {
        return Ok(None);
    };
    let package = verified_package(&directory)?;
    Ok(package
        .0
        .version
        .cmp_precedence(current)
        .is_gt()
        .then_some(package))
}

fn verified_package(directory: &Path) -> Result<(Release, PathBuf, File), String> {
    let package = directory.join(INSTALLER_NAME);
    // Bloqueie alterações e substituição enquanto o manifesto e o pacote são verificados e usados.
    let guard = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&package)
        .map_err(io_error)?;
    let release = release::staged(directory)?;
    Ok((release, package, guard))
}

fn require_newer(candidate: &Version, current: &Version) -> Result<(), String> {
    if candidate.cmp_precedence(current).is_gt() {
        Ok(())
    } else {
        Err(format!(
            "A versão {candidate} não é mais nova que a versão {current} do ARGOS."
        ))
    }
}

fn apply_portable(
    root: &Path,
    directory: &Path,
    attempt: &Path,
    package: &Path,
    candidate: &Version,
    restart: bool,
) -> Result<(), String> {
    if let Ok(Some(cached)) = cached_directory(root)
        && let Ok((previous, cached_package, _guard)) = verified_package(&cached)
    {
        if candidate.cmp_precedence(&previous.version).is_eq() {
            return if restart {
                run_portable(&cached_package)
            } else {
                Ok(())
            };
        }
        require_newer(candidate, &previous.version)?;
    }
    write_portable_pointer(root, directory, attempt)?;
    if restart {
        run_portable(package).map_err(|error| {
            format!("A atualização portátil foi salva, mas o painel não pôde ser aberto. {error}")
        })?;
    }
    Ok(())
}

fn run_portable(package: &Path) -> Result<(), String> {
    start_portable(package).map(|_| ())
}

fn start_portable(package: &Path) -> Result<OwnedHandle, String> {
    crate::desktop_child::spawn(quiet(hidden(package).args([
        PORTABLE_SWITCH,
        "/SP-",
        "/NORESTART",
    ])))
    .map_err(io_error)
}

fn cached_directory(root: &Path) -> Result<Option<PathBuf>, String> {
    let file = match File::open(root.join(PORTABLE_POINTER)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let mut text = String::new();
    file.take(256).read_to_string(&mut text).map_err(io_error)?;
    let name = text.strip_suffix('\n').unwrap_or(&text);
    if !safe_job_name(OsStr::new(name)) {
        return Err("A referência da atualização portátil é inválida.".into());
    }
    job_directory(&root.join(name)).map(Some)
}

fn write_portable_pointer(root: &Path, directory: &Path, attempt: &Path) -> Result<(), String> {
    let name = directory
        .file_name()
        .filter(|name| safe_job_name(name))
        .and_then(OsStr::to_str)
        .ok_or("Pasta de atualização portátil inválida.")?;
    let temporary = attempt.join("portable-current.pending");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(io_error)?;
    let result = writeln!(file, "{name}").and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| replace(&temporary, &root.join(PORTABLE_POINTER)));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.map_err(io_error)
}

fn safe_job_name(name: &OsStr) -> bool {
    let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix("job-")) else {
        return false;
    };
    suffix.len() <= 120
        && suffix
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._+".contains(&byte))
}

fn job_directory(path: &Path) -> Result<PathBuf, String> {
    let root = super::root()?.canonicalize().map_err(io_error)?;
    let directory = path.canonicalize().map_err(io_error)?;
    if directory.parent() != Some(&root)
        || directory.file_name() != path.file_name()
        || !directory.is_dir()
        || !directory.file_name().is_some_and(safe_job_name)
    {
        return Err("A pasta de atualização está fora do local esperado.".into());
    }
    Ok(directory)
}

fn attempt_directory(job: &Path, pid: u32, created: u64) -> PathBuf {
    job.join(format!("apply-{pid}-{created}"))
}

fn checked_install_directory(requested: &Path) -> Result<PathBuf, String> {
    let registered = super::registered_install_directory()
        .filter(|directory| directory.is_absolute())
        .ok_or("A instalação do ARGOS não está mais registrada no Windows.")?;
    if !requested.is_absolute()
        || requested.canonicalize().map_err(io_error)?
            != registered.canonicalize().map_err(io_error)?
    {
        return Err("A pasta de instalação do ARGOS foi alterada.".into());
    }
    Ok(registered)
}

fn apply_installed(
    job: &Path,
    package: &Path,
    requested: &Path,
    candidate: &Version,
    restart: bool,
) -> Result<(), String> {
    let (installed, previous, backup) = {
        let _reservation = reserve_inactive_panel()?;
        let installed = checked_install_directory(requested)?;
        let previous = installed_version(&installed)?;
        require_newer(candidate, &previous)?;
        let backup = Backup::save(job, &installed)?;
        (installed, previous, backup)
    };

    let mut dir_flag = OsString::from("/DIR=");
    dir_flag.push(&installed);
    let mut log_flag = OsString::from("/LOG=");
    log_flag.push(job.join("setup.log"));
    let status = quiet(
        hidden(package)
            .args([
                "/SP-",
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/NOCLOSEAPPLICATIONS",
            ])
            .arg(dir_flag)
            .arg(log_flag),
    )
    .status();
    // Os códigos 4 e 5 do Inno indicam que a fase que altera arquivos já começou.
    // Em falhas de preparação, os arquivos são deixados como estavam.
    let may_have_changed = status
        .as_ref()
        .is_ok_and(|status| matches!(status.code(), Some(0 | 4 | 5)));
    let failure = match status {
        Ok(status) if status.success() => {
            let installed_version =
                checked_install_directory(&installed).and_then(|path| installed_version(&path));
            if installed_version.as_ref() == Ok(candidate) && installed.join(LAUNCHER_EXE).is_file()
            {
                backup.discard();
                if restart {
                    reopen_installed(&installed).map_err(|error| {
                        format!("O ARGOS foi atualizado, mas o painel não pôde ser aberto. {error}")
                    })?;
                }
                return Ok(());
            }
            "O instalador terminou, mas a nova versão do ARGOS não pôde ser validada.".into()
        }
        Ok(status) => format!("O instalador do ARGOS encerrou com {status}."),
        Err(error) => format!("Não foi possível iniciar o instalador do ARGOS. {error}"),
    };

    let recovery = if may_have_changed {
        restore_installation(&backup, &installed, candidate, &previous)
    } else {
        checked_install_directory(&installed).and_then(|path| {
            if installed_version(&path)? == previous {
                Ok(())
            } else {
                Err("A instalação mudou desde o início da atualização.".into())
            }
        })
    };
    match recovery {
        Ok(()) => {
            backup.discard();
            if restart && let Err(error) = reopen_installed(&installed) {
                return Err(format!(
                    "{failure} A versão anterior não pôde ser reaberta. {error}"
                ));
            }
            Err(failure)
        }
        Err(error) => Err(format!(
            "{failure} A cópia anterior foi mantida para recuperação. {error}"
        )),
    }
}

fn installed_version(directory: &Path) -> Result<Version, String> {
    let result = hidden(&directory.join(PANEL_EXE))
        .arg("--version")
        .output()
        .map_err(io_error)?;
    if result.status.success() {
        let version = std::str::from_utf8(&result.stdout)
            .ok()
            .and_then(|text| text.trim().strip_prefix("ARGOS "))
            .and_then(|text| Version::parse(text).ok());
        if let Some(version) = version {
            return Ok(version);
        }
    }
    Err("Não foi possível confirmar a versão instalada do ARGOS.".into())
}

fn reopen_installed(directory: &Path) -> Result<(), String> {
    let launcher = directory.join(LAUNCHER_EXE);
    let mut command = if launcher.is_file() {
        hidden(&launcher)
    } else {
        let mut command = hidden(&directory.join(PANEL_EXE));
        command.arg("gui");
        command
    };
    quiet(command.current_dir(directory))
        .spawn()
        .map(|_| ())
        .map_err(io_error)
}

struct Backup {
    job: PathBuf,
    launcher: bool,
}

impl Backup {
    fn save(job: &Path, installed: &Path) -> Result<Self, String> {
        copy_new(&installed.join(PANEL_EXE), &job.join(PANEL_EXE)).map_err(io_error)?;
        let launcher = installed.join(LAUNCHER_EXE);
        let launcher_exists = launcher.is_file();
        if launcher_exists && let Err(error) = copy_new(&launcher, &job.join(LAUNCHER_EXE)) {
            let _ = fs::remove_file(job.join(PANEL_EXE));
            return Err(io_error(error));
        }
        Ok(Self {
            job: job.to_owned(),
            launcher: launcher_exists,
        })
    }

    fn discard(self) {
        let _ = fs::remove_file(self.job.join(PANEL_EXE));
        if self.launcher {
            let _ = fs::remove_file(self.job.join(LAUNCHER_EXE));
        }
    }
}

fn restore_installation(
    backup: &Backup,
    installed: &Path,
    candidate: &Version,
    previous: &Version,
) -> Result<(), String> {
    let _reservation = reserve_inactive_panel()?;
    let installed = checked_install_directory(installed)?;
    if installed_version(&installed).is_ok_and(|actual| actual != *previous && actual != *candidate)
    {
        return Err("Outra versão do ARGOS foi instalada durante a atualização.".into());
    }
    if backup.launcher {
        restore_file(&backup.job, &installed, LAUNCHER_EXE).map_err(io_error)?;
    }
    restore_file(&backup.job, &installed, PANEL_EXE).map_err(io_error)?;
    if installed_version(&installed)? != *previous {
        return Err("Não foi possível confirmar a restauração da versão anterior.".into());
    }
    Ok(())
}

fn reserve_inactive_panel() -> Result<(File, File), String> {
    let updates = super::root()?;
    let state = updates.parent().ok_or("Pasta do ARGOS indisponível.")?;
    let launch = lock(
        &state.join("launch.lock"),
        0,
        "O ARGOS está abrindo ou sendo instalado.",
    )?;
    let panel = lock(
        &state.join("panel.lock"),
        FILE_SHARE_READ,
        "O painel do ARGOS está aberto.",
    )?;
    Ok((panel, launch))
}

fn restore_file(job: &Path, installed: &Path, name: &str) -> io::Result<()> {
    let job_name = job
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("job-update");
    let stage = installed.join(format!(".{name}.{job_name}.restore"));
    copy_new(&job.join(name), &stage)?;
    let result = replace(&stage, &installed.join(name));
    if result.is_err() {
        let _ = fs::remove_file(stage);
    }
    result
}

fn lock(path: &Path, share_mode: u32, busy: &str) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(share_mode)
        .open(path)
        .map_err(|error| format!("{busy} {error}"))
}

fn copy_new(source: &Path, destination: &Path) -> io::Result<()> {
    let mut source = File::open(source)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = io::copy(&mut source, &mut file).and_then(|_| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn process_created(process: HANDLE) -> Result<u64, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let result =
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
    if result == 0 {
        return Err(io_error(io::Error::last_os_error()));
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn wait_for_parent(pid: u32, created: u64) -> Result<(), String> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZATION_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(());
        }
        return Err(io_error(error));
    }
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    // Um PID reutilizado indica que o processo original já terminou.
    if process_created(process.as_raw_handle())? != created {
        return Ok(());
    }
    match unsafe { WaitForSingleObject(process.as_raw_handle(), PARENT_TIMEOUT_MS) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err("O painel anterior do ARGOS demorou a encerrar.".into()),
        _ => Err(io_error(io::Error::last_os_error())),
    }
}

fn hidden(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null());
    command
}

fn quiet(command: &mut Command) -> &mut Command {
    command.stdout(Stdio::null()).stderr(Stdio::null())
}

fn io_error(error: io::Error) -> String {
    format!("Falha ao aplicar a atualização do ARGOS. {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_name_rejects_paths_streams_and_windows_aliases() {
        for valid in ["job-12-345", "job-0.2.0+build-42"] {
            assert!(safe_job_name(OsStr::new(valid)), "{valid}");
        }
        for invalid in [
            "job-",
            "job-1.",
            "job-1 ",
            "job-..\\file",
            "job-1/2",
            "job-1:stream",
            "../job-1",
        ] {
            assert!(!safe_job_name(OsStr::new(invalid)), "{invalid}");
        }
    }

    #[test]
    fn build_metadata_does_not_make_a_release_newer() {
        let current = Version::parse("0.2.0+old").unwrap();
        assert!(require_newer(&Version::parse("0.2.0+new").unwrap(), &current).is_err());
        assert!(require_newer(&Version::parse("0.2.1").unwrap(), &current).is_ok());
    }

    #[test]
    fn reused_pid_is_not_waited_on() {
        let process = unsafe { GetCurrentProcess() };
        let created = process_created(process).unwrap();
        assert!(wait_for_parent(std::process::id(), created + 1).is_ok());
    }
}
