mod apply;
mod release;

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows_sys::w;

pub(crate) const RELEASES_URL: &str = "https://github.com/MrDoom11/ARGOS/releases";

const INSTALL_KEY: *const u16 = w!(
    "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{618CF44B-59D7-424F-89F7-6B2714DF1BD8}_is1"
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Source {
    #[default]
    Direct,
    Portable,
    PortableExact,
}

enum Mode {
    Installed(PathBuf),
    Portable,
}

struct Staged {
    directory: PathBuf,
    version: Version,
}

enum Phase {
    Idle,
    Checking {
        result: Receiver<Result<Option<Staged>, String>>,
        manual: bool,
    },
    Ready(Staged),
}

pub(crate) enum Status<'a> {
    Idle,
    Checking,
    Ready(&'a Version, bool),
}

pub(crate) enum CheckNotice {
    Ready(Version),
    Current,
    Failed(String),
}

pub(crate) struct Updater {
    mode: Option<Mode>,
    phase: Phase,
    scheduled: bool,
}

impl Updater {
    pub(crate) fn new(source: Source) -> Self {
        let mode = match source {
            Source::Portable | Source::PortableExact => Some(Mode::Portable),
            Source::Direct => registered_install_directory()
                .filter(|dir| {
                    std::env::current_exe()
                        .is_ok_and(|exe| same_installation(&exe, &dir.join("argos.exe")))
                })
                .map(Mode::Installed),
        };
        Self {
            mode,
            phase: Phase::Idle,
            scheduled: false,
        }
    }

    pub(crate) fn status(&self) -> Status<'_> {
        match &self.phase {
            Phase::Idle => Status::Idle,
            Phase::Checking { .. } => Status::Checking,
            Phase::Ready(update) => Status::Ready(&update.version, self.scheduled),
        }
    }

    pub(crate) fn installed_directory(&self) -> Option<&Path> {
        match &self.mode {
            Some(Mode::Installed(directory)) => Some(directory),
            _ => None,
        }
    }

    pub(crate) fn check(&mut self, manual: bool) -> Result<(), String> {
        if self.mode.is_none() {
            return Err("Abra o ARGOS pelo instalador ou pelo menu Iniciar para atualizar.".into());
        }
        if let Phase::Checking { manual: notify, .. } = &mut self.phase {
            *notify |= manual;
            return Ok(());
        }
        if matches!(self.phase, Phase::Ready(_)) {
            return Ok(());
        }
        let (sender, result) = mpsc::channel();
        std::thread::Builder::new()
            .name("argos-update-check".into())
            .spawn(move || {
                let _ = sender.send(check_and_stage());
            })
            .map_err(|error| format!("Não foi possível iniciar a verificação. {error}"))?;
        self.phase = Phase::Checking { result, manual };
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> Option<CheckNotice> {
        let Phase::Checking { result, manual } = &self.phase else {
            return None;
        };
        let update = match result.try_recv() {
            Ok(update) => update,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("A verificação foi interrompida.".into()),
        };
        let manual = *manual;
        self.phase = Phase::Idle;
        match update {
            Ok(Some(update)) => {
                let notice = manual.then(|| CheckNotice::Ready(update.version.clone()));
                self.phase = Phase::Ready(update);
                notice
            }
            Ok(None) => manual.then_some(CheckNotice::Current),
            Err(error) => manual.then_some(CheckNotice::Failed(error)),
        }
    }

    #[cfg(test)]
    pub(crate) fn complete_manual_check_for_test(&mut self, version: Version) {
        let (sender, result) = mpsc::channel();
        sender
            .send(Ok(Some(Staged {
                directory: PathBuf::new(),
                version,
            })))
            .unwrap();
        self.phase = Phase::Checking {
            result,
            manual: true,
        };
    }

    pub(crate) fn schedule(&mut self, enabled: bool) {
        self.scheduled = enabled && matches!(self.phase, Phase::Ready(_));
    }

    pub(crate) fn apply(&mut self, restart: bool) -> Result<bool, String> {
        if !restart && !self.scheduled {
            return Ok(false);
        }
        let (Phase::Ready(update), Some(mode)) = (&self.phase, &self.mode) else {
            return Ok(false);
        };
        let installed = match mode {
            Mode::Installed(directory) => Some(directory.as_path()),
            Mode::Portable => None,
        };
        apply::spawn(&update.directory, installed, restart)?;
        self.phase = Phase::Idle;
        self.scheduled = false;
        Ok(true)
    }
}

pub(crate) fn launch_cached(source: Source) -> bool {
    if source != Source::Portable {
        return false;
    }
    apply::launch_cached(&current_version()).unwrap_or_else(|error| {
        record_error(&error);
        false
    })
}

pub(crate) fn cached_portable_version() -> Option<Version> {
    apply::cached_version(&current_version()).ok().flatten()
}

pub(crate) fn run_worker(args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let result = apply::run(args);
    if let Err(error) = &result {
        record_error(error);
    } else if let Ok(root) = root() {
        let _ = fs::remove_file(root.join("last-update-error"));
    }
    result
}

pub(crate) fn last_error() -> Option<String> {
    let mut message = String::new();
    File::open(root().ok()?.join("last-update-error"))
        .ok()?
        .take(4096)
        .read_to_string(&mut message)
        .ok()?;
    (!message.is_empty()).then_some(message)
}

pub(crate) fn record_error(error: &str) {
    if let Ok(root) = root() {
        let _ = fs::write(root.join("last-update-error"), error);
    }
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("versão Cargo válida")
}

fn check_and_stage() -> Result<Option<Staged>, String> {
    let Some(release) = release::discover(&current_version())? else {
        return Ok(None);
    };
    let root = root()?;
    fs::create_dir_all(&root).map_err(storage_error)?;
    let version_key = ring::digest::digest(
        &ring::digest::SHA256,
        release.version.to_string().as_bytes(),
    );
    let key = u64::from_be_bytes(version_key.as_ref()[..8].try_into().expect("oito bytes"));
    let mut directory = root.join(format!("job-{key}"));
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if release::staged(&directory).is_ok_and(|staged| staged.same_artifact(&release)) {
                return Ok(Some(Staged {
                    directory,
                    version: release.version,
                }));
            }
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            directory = root.join(format!("job-{}-{nonce}", std::process::id()));
            fs::create_dir(&directory).map_err(storage_error)?;
        }
        Err(error) => return Err(storage_error(error)),
    }
    if let Err(error) = release::download(&release, &directory) {
        for name in [release::MANIFEST_NAME, release::INSTALLER_NAME] {
            let _ = fs::remove_file(directory.join(name));
        }
        let _ = fs::remove_dir(&directory);
        return Err(error);
    }
    Ok(Some(Staged {
        directory,
        version: release.version,
    }))
}

fn root() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("ARGOS").join("updates"))
        .ok_or_else(|| "A pasta LocalAppData do Windows está indisponível.".into())
}

fn registered_install_directory() -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let mut size = (buffer.len() * size_of::<u16>()) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            INSTALL_KEY,
            w!("InstallLocation"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 || size as usize > buffer.len() * size_of::<u16>() {
        return None;
    }
    let end = buffer.iter().position(|&unit| unit == 0)?;
    let path = PathBuf::from(OsString::from_wide(&buffer[..end]));
    path.is_absolute().then_some(path)
}

fn storage_error(error: std::io::Error) -> String {
    format!("Não foi possível preparar a atualização no perfil do Windows. {error}")
}

fn same_installation(first: &Path, second: &Path) -> bool {
    first
        .canonicalize()
        .ok()
        .zip(second.canonicalize().ok())
        .is_some_and(|(a, b)| a == b)
}
