use std::ffi::OsString;
use std::num::NonZeroU32;
use std::time::Duration;

const DEFAULT_INTERVAL_MS: u32 = 1000;
const MIN_INTERVAL_MS: u32 = 101;
const MIN_WATCH_INTERVAL_MS: u32 = 100;

pub(crate) const HELP: &str = "ARGOS — sonda de telemetria para Windows

Uso: argos [--gpu ÍNDICE] [--samples N] [--interval-ms MS]
     argos watch [OPÇÕES]

  --gpu ÍNDICE      Seleciona uma GPU NVIDIA pelo índice NVML (começa em 0).
                    Por padrão, consulta todas as GPUs NVIDIA encontradas.
  --samples N       Número de intervalos GPM por GPU (padrão: 1).
  --interval-ms MS  Tempo entre amostras GPM (padrão: 1000, mínimo: 101).
  watch --help      Exibe as opções da coleta contínua.
  -h, --help        Exibe esta ajuda.
  -V, --version     Exibe a versão do ARGOS.
";

pub(crate) const WATCH_HELP: &str = "ARGOS — coleta contínua (Ctrl+C para encerrar)

Uso: argos watch [--gpu ÍNDICE] [--gpu-interval-ms MS] [--gpm-interval-ms MS]
                 [--system-interval-ms MS] [--duration-secs S]

  --gpu ÍNDICE           Seleciona uma GPU NVIDIA (padrão: 0, quando disponível).
  --gpu-interval-ms MS   Intervalo dos sensores básicos da GPU (mínimo: 100).
  --gpm-interval-ms MS   Intervalo dos contadores GPM (mínimo: 101).
  --system-interval-ms MS  Intervalo de CPU e RAM (mínimo: 100).
  --duration-secs S      Encerra automaticamente após S segundos.

Todos os intervalos começam em 1000 ms por padrão. O histórico em memória retém
até os últimos 5 minutos de cada grupo.
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) gpu: Option<u32>,
    pub(crate) samples: NonZeroU32,
    pub(crate) interval: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WatchOptions {
    pub(crate) gpu: Option<u32>,
    pub(crate) gpu_interval: Duration,
    pub(crate) gpm_interval: Duration,
    pub(crate) system_interval: Duration,
    pub(crate) duration: Option<Duration>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Probe(Options),
    Watch(WatchOptions),
    WatchHelp,
    Help,
    Version,
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "watch") {
        args.next();
        return parse_watch(args);
    }

    let mut gpu = None;
    let mut samples = None;
    let mut interval_ms = None;

    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or("argumento com texto inválido")?;
        let target = match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--gpu" => &mut gpu,
            "--samples" => &mut samples,
            "--interval-ms" => &mut interval_ms,
            _ => return Err(format!("opção desconhecida {flag:?}")),
        };

        read_number(target, flag, &mut args)?;
    }

    let samples =
        NonZeroU32::new(samples.unwrap_or(1)).ok_or("--samples deve ser maior que zero")?;
    Ok(Command::Probe(Options {
        gpu,
        samples,
        interval: interval(interval_ms, "--interval-ms", MIN_INTERVAL_MS)?,
    }))
}

fn parse_watch(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut gpu = None;
    let mut gpu_ms = None;
    let mut gpm_ms = None;
    let mut system_ms = None;
    let mut duration = None;

    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or("argumento com texto inválido")?;
        let target = match flag {
            "-h" | "--help" => return Ok(Command::WatchHelp),
            "--gpu" => &mut gpu,
            "--gpu-interval-ms" => &mut gpu_ms,
            "--gpm-interval-ms" => &mut gpm_ms,
            "--system-interval-ms" => &mut system_ms,
            "--duration-secs" => &mut duration,
            _ => {
                return Err(format!(
                    "opção desconhecida {flag:?} (consulte argos watch --help)"
                ));
            }
        };
        read_number(target, flag, &mut args)?;
    }

    if duration == Some(0) {
        return Err("--duration-secs deve ser maior que zero".into());
    }

    Ok(Command::Watch(WatchOptions {
        gpu,
        gpu_interval: interval(gpu_ms, "--gpu-interval-ms", MIN_WATCH_INTERVAL_MS)?,
        gpm_interval: interval(gpm_ms, "--gpm-interval-ms", MIN_INTERVAL_MS)?,
        system_interval: interval(system_ms, "--system-interval-ms", MIN_WATCH_INTERVAL_MS)?,
        duration: duration.map(|seconds| Duration::from_secs(u64::from(seconds))),
    }))
}

fn read_number(
    target: &mut Option<u32>,
    flag: &str,
    args: &mut impl Iterator<Item = OsString>,
) -> Result<(), String> {
    if target.is_some() {
        return Err(format!("opção repetida {flag}"));
    }
    let value = args
        .next()
        .and_then(|value| value.to_str()?.parse::<u32>().ok())
        .ok_or_else(|| format!("{flag} requer um número inteiro entre 0 e {}", u32::MAX))?;
    *target = Some(value);
    Ok(())
}

fn interval(value: Option<u32>, flag: &str, minimum: u32) -> Result<Duration, String> {
    let milliseconds = value.unwrap_or(DEFAULT_INTERVAL_MS);
    if milliseconds < minimum {
        return Err(format!("{flag} deve ser de pelo menos {minimum} ms"));
    }
    Ok(Duration::from_millis(u64::from(milliseconds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_all_gpus_and_one_gpm_interval() {
        assert_eq!(
            parse([]),
            Ok(Command::Probe(Options {
                gpu: None,
                samples: NonZeroU32::MIN,
                interval: Duration::from_secs(1),
            }))
        );
    }

    #[test]
    fn accepts_a_gpu_and_sampling_parameters() {
        let args = ["--samples", "3", "--interval-ms", "101", "--gpu", "0"];
        assert_eq!(
            parse(args.map(OsString::from)),
            Ok(Command::Probe(Options {
                gpu: Some(0),
                samples: NonZeroU32::new(3).unwrap(),
                interval: Duration::from_millis(101),
            }))
        );
    }

    #[test]
    fn rejects_invalid_arguments() {
        for args in [
            ["--interval-ms", "100"],
            ["--samples", "0"],
            ["--gpu", "-1"],
            ["--gpu", "4294967296"],
            ["--unknown", "1"],
        ] {
            assert!(parse(args.map(OsString::from)).is_err(), "{args:?}");
        }
        assert!(parse([OsString::from("--gpu")]).is_err());
        assert!(parse(["--gpu", "0", "--gpu", "1"].map(OsString::from)).is_err());
    }

    #[test]
    fn watch_supports_independent_intervals_and_a_duration() {
        let args = [
            "watch",
            "--gpu",
            "0",
            "--gpu-interval-ms",
            "100",
            "--gpm-interval-ms",
            "101",
            "--duration-secs",
            "30",
        ];
        assert_eq!(
            parse(args.map(OsString::from)),
            Ok(Command::Watch(WatchOptions {
                gpu: Some(0),
                gpu_interval: Duration::from_millis(100),
                gpm_interval: Duration::from_millis(101),
                system_interval: Duration::from_secs(1),
                duration: Some(Duration::from_secs(30)),
            }))
        );
    }

    #[test]
    fn watch_rejects_subminimum_periods_and_zero_duration() {
        for args in [
            ["watch", "--gpm-interval-ms", "100"],
            ["watch", "--gpu-interval-ms", "99"],
            ["watch", "--system-interval-ms", "99"],
            ["watch", "--duration-secs", "0"],
            ["watch", "--samples", "2"],
        ] {
            assert!(parse(args.map(OsString::from)).is_err(), "{args:?}");
        }
    }
}
