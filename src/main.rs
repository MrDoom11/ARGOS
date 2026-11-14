use std::process::ExitCode;

#[cfg(windows)]
mod adlx;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod cpu_sensor;
#[cfg(windows)]
mod cpu_temperature;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod frames;
#[cfg(windows)]
mod game_network;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod gpu_video;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod network;
#[cfg(windows)]
mod nvml;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod storage;
#[cfg(windows)]
mod storage_worker;
#[cfg(windows)]
mod system;
#[cfg(windows)]
mod telemetry;
#[cfg(windows)]
mod update;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
fn main() -> ExitCode {
    use cli::Command;

    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "apply-update") {
        args.next();
        return report_result(update::run_worker(args));
    }
    if args
        .peek()
        .is_some_and(|arg| arg == "cpu-temperature-supported")
    {
        args.next();
        return if args.next().is_none() && cpu_temperature::supported() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        };
    }
    if args
        .peek()
        .is_some_and(|arg| arg == "portable-cache-version")
    {
        args.next();
        if args.next().is_some() {
            return ExitCode::from(2);
        }
        if let Some(version) = update::cached_portable_version() {
            println!("{version}");
        }
        return ExitCode::SUCCESS;
    }
    match cli::parse(args) {
        Ok(Command::Help) => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::WatchHelp) => {
            print!("{}", cli::WATCH_HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::GuiHelp) => {
            print!("{}", cli::GUI_HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("ARGOS {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Probe(options)) => report_result(probe::run(options)),
        Ok(Command::Watch(options)) => report_result(watch::run(options)),
        Ok(Command::Gui { gpu, source }) => report_result(gui::run(gpu, source)),
        Ok(Command::PrepareInstall {
            silent,
            installed_version,
        }) => prepare_install(silent, installed_version),
        Err(error) => {
            eprintln!("ARGOS: {error}\nConsulte argos --help.");
            ExitCode::from(2)
        }
    }
}

#[cfg(windows)]
fn prepare_install(silent: bool, installed_version: Option<semver::Version>) -> ExitCode {
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("versão Cargo válida");
    if installed_version.is_some_and(|installed| current.cmp_precedence(&installed).is_lt()) {
        return ExitCode::from(3);
    }
    match instance::prepare_install(silent) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => report_result(Err(error)),
    }
}

#[cfg(windows)]
fn report_result(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if std::env::var_os(desktop::LAUNCH_ENV).is_some()
                && std::env::args_os()
                    .nth(1)
                    .is_some_and(|argument| argument == "gui")
            {
                desktop::show_error(&error);
            }
            eprintln!("ARGOS: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("Este protótipo do ARGOS requer Windows.");
    ExitCode::FAILURE
}
