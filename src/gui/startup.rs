use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use semver::Version;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
};

use super::{board, wide};

const WELCOME: &str = "Clique com o botão direito no painel para abrir o menu.\n\n\
    Use Sensores e Grade para escolher os gráficos. Arraste um gráfico para mudar a ordem.\n\n\
    Para mover o painel, escolha Mover janela e arraste com o botão esquerdo. Para redimensionar, arraste uma borda.\n\n\
    Em FPS, escolha o aplicativo em foco, um processo específico ou Desativado.\n\n\
    No menu você também pode verificar se há uma nova versão do ARGOS.";
const RELEASE_NOTES: &str = include_str!(concat!(
    "../../release-notes/",
    env!("CARGO_PKG_VERSION"),
    ".txt"
));

#[derive(Debug, PartialEq, Eq)]
enum Notice {
    Welcome,
    ReleaseNotes,
}

pub(super) struct StartupNotice {
    notice: Notice,
    current: Version,
    path: Option<PathBuf>,
}

pub(super) fn prepare(previous: Option<&Version>, restored_board: bool) -> Option<StartupNotice> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("versão Cargo válida");
    let path = board::config_path().map(|path| path.with_file_name("last-seen-version"));
    let mut text = String::new();
    let seen = path
        .as_ref()
        .and_then(|path| File::open(path).ok())
        .and_then(|file| file.take(256).read_to_string(&mut text).ok())
        .and_then(|_| Version::parse(text.trim()).ok());

    let notice = next_notice(&current, seen.as_ref(), previous, restored_board)?;
    if seen.is_none()
        && let Some(previous) = previous.filter(|version| version.cmp_precedence(&current).is_lt())
        && let Some(path) = &path
    {
        // O registro da instância agora aponta para a versão nova, mesmo se a abertura for interrompida.
        let _ = board::replace_file(path, previous.to_string().as_bytes());
    }
    Some(StartupNotice {
        notice,
        current,
        path,
    })
}

impl StartupNotice {
    pub(super) fn show(self, hwnd: HWND) {
        let Self {
            notice,
            current,
            path,
        } = self;
        let (title, message) = match notice {
            Notice::Welcome => ("Bem-vindo ao ARGOS".into(), WELCOME),
            Notice::ReleaseNotes => (
                format!("Novidades do ARGOS {current}"),
                RELEASE_NOTES.trim(),
            ),
        };
        let title = wide(&title);
        let message = wide(message);
        let dismissed = unsafe {
            MessageBoxW(
                hwnd,
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
            )
        };
        if dismissed != 0
            && let Some(path) = path
        {
            let _ = board::replace_file(&path, current.to_string().as_bytes());
        }
    }
}

fn next_notice(
    current: &Version,
    seen: Option<&Version>,
    previous: Option<&Version>,
    restored_board: bool,
) -> Option<Notice> {
    if let Some(seen) = seen {
        return current
            .cmp_precedence(seen)
            .is_gt()
            .then_some(Notice::ReleaseNotes);
    }
    if restored_board || previous.is_some_and(|previous| previous.cmp_precedence(current).is_lt()) {
        Some(Notice::ReleaseNotes)
    } else {
        Some(Notice::Welcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_first_run_upgrade_and_already_seen_version() {
        let previous = Version::new(0, 2, 0);
        let current = Version::new(0, 2, 1);
        assert_eq!(
            next_notice(&current, None, None, false),
            Some(Notice::Welcome)
        );
        assert_eq!(
            next_notice(&current, None, None, true),
            Some(Notice::ReleaseNotes)
        );
        assert_eq!(
            next_notice(&current, None, Some(&previous), false),
            Some(Notice::ReleaseNotes)
        );
        assert_eq!(
            next_notice(&current, Some(&previous), None, false),
            Some(Notice::ReleaseNotes)
        );
        assert_eq!(next_notice(&current, Some(&current), None, true), None);
        assert_eq!(next_notice(&previous, Some(&current), None, true), None);
    }
}
