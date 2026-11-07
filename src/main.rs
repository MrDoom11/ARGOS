use std::process::ExitCode;

#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod cpu_temperature;
#[cfg(windows)]
mod frames;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod nvml;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod system;
#[cfg(windows)]
mod telemetry;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
fn main() -> ExitCode {
    use cli::Command;

    match cli::parse(std::env::args_os().skip(1)) {
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
        Ok(Command::Gui(gpu)) => report_result(gui::run(gpu)),
        Err(error) => {
            eprintln!("ARGOS: {error}\nConsulte argos --help.");
            ExitCode::from(2)
        }
    }
}

#[cfg(windows)]
fn report_result(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
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
