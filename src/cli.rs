use std::ffi::OsString;
use std::num::NonZeroU32;
use std::time::Duration;

const DEFAULT_INTERVAL_MS: u32 = 1000;
const MIN_INTERVAL_MS: u32 = 101;

pub(crate) const HELP: &str = "ARGOS — sonda de telemetria para Windows

Uso: argos [--gpu ÍNDICE] [--samples N] [--interval-ms MS]

  --gpu ÍNDICE      Seleciona uma GPU NVIDIA pelo índice NVML (começa em 0).
                    Por padrão, consulta todas as GPUs NVIDIA encontradas.
  --samples N       Número de intervalos GPM por GPU (padrão: 1).
  --interval-ms MS  Tempo entre amostras GPM (padrão: 1000, mínimo: 101).
  -h, --help        Exibe esta ajuda.
  -V, --version     Exibe a versão do ARGOS.
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) gpu: Option<u32>,
    pub(crate) samples: NonZeroU32,
    pub(crate) interval: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Probe(Options),
    Help,
    Version,
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
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

        if target.is_some() {
            return Err(format!("opção repetida {flag}"));
        }
        let value = args
            .next()
            .and_then(|value| value.to_str()?.parse::<u32>().ok())
            .ok_or_else(|| format!("{flag} requer um número inteiro entre 0 e {}", u32::MAX))?;
        *target = Some(value);
    }

    let samples =
        NonZeroU32::new(samples.unwrap_or(1)).ok_or("--samples deve ser maior que zero")?;
    let interval_ms = interval_ms.unwrap_or(DEFAULT_INTERVAL_MS);
    if interval_ms < MIN_INTERVAL_MS {
        return Err(format!(
            "--interval-ms deve ser de pelo menos {MIN_INTERVAL_MS} ms"
        ));
    }

    Ok(Command::Probe(Options {
        gpu,
        samples,
        interval: Duration::from_millis(u64::from(interval_ms)),
    }))
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
}
