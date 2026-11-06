use std::process::ExitCode;

#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod nvml;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod system;

#[cfg(windows)]
fn main() -> ExitCode {
    use cli::Command;

    match cli::parse(std::env::args_os().skip(1)) {
        Ok(Command::Help) => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("ARGOS {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Probe(options)) => match probe::run(options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("ARGOS: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("ARGOS: {error}\nConsulte argos --help.");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("Este protótipo do ARGOS requer Windows.");
    ExitCode::FAILURE
}
