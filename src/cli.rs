use std::ffi::OsString;
use std::num::NonZeroU32;
use std::time::Duration;

use semver::Version;

use crate::update::Source;

const DEFAULT_INTERVAL_MS: u32 = 1000;
const MIN_INTERVAL_MS: u32 = 101;
const MIN_WATCH_INTERVAL_MS: u32 = 100;

pub(crate) const HELP: &str = "ARGOS — sonda de telemetria para Windows

Uso: argos [--gpu ÍNDICE] [--samples N] [--interval-ms MS]
     argos watch [OPÇÕES]
     argos gui [--gpu ÍNDICE]

  --gpu ÍNDICE      Selecione NVIDIA com 0 ou nvidia:0 e Radeon com amd:0.
                    Sem seleção, consulta todas as GPUs encontradas.
  --samples N       Número de intervalos GPM por GPU (padrão: 1).
  --interval-ms MS  Tempo entre amostras GPM (padrão: 1000, mínimo: 101).
  watch --help      Exibe as opções da coleta contínua.
  gui               Abre a janela de monitoramento.
  -h, --help        Exibe esta ajuda.
  -V, --version     Exibe a versão do ARGOS.
";

pub(crate) const GUI_HELP: &str = "ARGOS — janela de monitoramento

Uso: argos gui [--gpu ÍNDICE]

  --gpu ÍNDICE  Selecione NVIDIA com 0 ou nvidia:0 e Radeon com amd:0.
  -h, --help    Exibe esta ajuda.
";

pub(crate) const WATCH_HELP: &str = "ARGOS — coleta contínua (Ctrl+C para encerrar)

Uso: argos watch [--gpu ÍNDICE]... [--gpu-interval-ms MS] [--gpm-interval-ms MS]
                 [--system-interval-ms MS] [--duration-secs S]

  --gpu ÍNDICE           Selecione NVIDIA com 0 ou nvidia:0 e Radeon com amd:0.
                         Repita a opção para coletar de várias GPUs.
  --gpu-interval-ms MS   Intervalo dos sensores básicos da GPU (mínimo: 100).
  --gpm-interval-ms MS   Intervalo dos contadores GPM (mínimo: 101).
  --system-interval-ms MS  Intervalo de CPU e RAM (mínimo: 100).
  --duration-secs S      Encerra automaticamente após S segundos.

Todos os intervalos começam em 1000 ms por padrão. O histórico em memória retém
até os últimos 5 minutos de cada grupo.
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GpuIndex {
    Nvidia(u32),
    Radeon(u32),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) gpu: Option<GpuIndex>,
    pub(crate) samples: NonZeroU32,
    pub(crate) interval: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WatchOptions {
    pub(crate) gpus: Vec<GpuIndex>,
    pub(crate) gpu_interval: Duration,
    pub(crate) gpm_interval: Duration,
    pub(crate) system_interval: Duration,
    pub(crate) duration: Option<Duration>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Probe(Options),
    Watch(WatchOptions),
    Gui {
        gpu: Option<GpuIndex>,
        source: Source,
    },
    PrepareInstall {
        silent: bool,
        installed_version: Option<Version>,
    },
    WatchHelp,
    GuiHelp,
    Help,
    Version,
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "watch") {
        args.next();
        return parse_watch(args);
    }
    if args.peek().is_some_and(|arg| arg == "gui") {
        args.next();
        return parse_gui(args);
    }
    if args.peek().is_some_and(|arg| arg == "prepare-install") {
        args.next();
        return parse_prepare_install(args);
    }

    let mut gpu = None;
    let mut samples = None;
    let mut interval_ms = None;

    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or("argumento com texto inválido")?;
        let target = match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--gpu" => {
                read_gpu(&mut gpu, &mut args)?;
                continue;
            }
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

fn parse_gui(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut gpu = None;
    let mut source = Source::Direct;
    while let Some(flag) = args.next() {
        match flag.to_str().ok_or("argumento com texto inválido")? {
            "-h" | "--help" => return Ok(Command::GuiHelp),
            "--gpu" => read_gpu(&mut gpu, &mut args)?,
            "--portable" if source == Source::Direct => source = Source::Portable,
            "--portable-exact" if source == Source::Direct => source = Source::PortableExact,
            flag => {
                return Err(format!(
                    "opção desconhecida {flag:?} (consulte argos gui --help)"
                ));
            }
        }
    }
    Ok(Command::Gui { gpu, source })
}

fn parse_prepare_install(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut silent = false;
    let mut installed_version = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--silent") if !silent => silent = true,
            Some("--installed-version") if installed_version.is_none() => {
                installed_version = Some(
                    args.next()
                        .and_then(|version| Version::parse(version.to_str()?).ok())
                        .ok_or("versão instalada inválida")?,
                );
            }
            _ => return Err("argumento de instalação desconhecido ou repetido".into()),
        }
    }
    Ok(Command::PrepareInstall {
        silent,
        installed_version,
    })
}

fn parse_watch(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut gpus = Vec::new();
    let mut gpu_ms = None;
    let mut gpm_ms = None;
    let mut system_ms = None;
    let mut duration = None;

    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or("argumento com texto inválido")?;
        let target = match flag {
            "-h" | "--help" => return Ok(Command::WatchHelp),
            "--gpu" => {
                let gpu = read_gpu_index(&mut args)?;
                if gpus.contains(&gpu) {
                    return Err("GPU repetida em --gpu".into());
                }
                gpus.push(gpu);
                continue;
            }
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
        gpus,
        gpu_interval: interval(gpu_ms, "--gpu-interval-ms", MIN_WATCH_INTERVAL_MS)?,
        gpm_interval: interval(gpm_ms, "--gpm-interval-ms", MIN_INTERVAL_MS)?,
        system_interval: interval(system_ms, "--system-interval-ms", MIN_WATCH_INTERVAL_MS)?,
        duration: duration.map(|seconds| Duration::from_secs(u64::from(seconds))),
    }))
}

fn read_gpu(
    target: &mut Option<GpuIndex>,
    args: &mut impl Iterator<Item = OsString>,
) -> Result<(), String> {
    if target.is_some() {
        return Err("opção repetida --gpu".into());
    }
    *target = Some(read_gpu_index(args)?);
    Ok(())
}

fn read_gpu_index(args: &mut impl Iterator<Item = OsString>) -> Result<GpuIndex, String> {
    let invalid = "--gpu requer um índice NVIDIA ou um índice AMD, como 0 ou amd:0";
    let value = args.next().ok_or(invalid)?;
    let text = value.to_str().ok_or(invalid)?;
    let (vendor, index) = text.split_once(':').unwrap_or(("nvidia", text));
    let index = index.parse::<u32>().map_err(|_| invalid)?;
    match vendor {
        "nvidia" => Ok(GpuIndex::Nvidia(index)),
        "amd" => Ok(GpuIndex::Radeon(index)),
        _ => Err(invalid.into()),
    }
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
    fn parses_installer_options_and_rejects_invalid_versions() {
        let args = [
            "prepare-install",
            "--silent",
            "--installed-version",
            "1.2.3-rc.1",
        ];
        assert_eq!(
            parse(args.map(OsString::from)),
            Ok(Command::PrepareInstall {
                silent: true,
                installed_version: Some(Version::parse("1.2.3-rc.1").unwrap()),
            })
        );
        for args in [
            &["prepare-install", "--installed-version"][..],
            &["prepare-install", "--installed-version", "1.2"][..],
            &["prepare-install", "--silent", "--silent"][..],
        ] {
            assert!(parse(args.iter().map(OsString::from)).is_err());
        }
    }

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
                gpu: Some(GpuIndex::Nvidia(0)),
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
                gpus: vec![GpuIndex::Nvidia(0)],
                gpu_interval: Duration::from_millis(100),
                gpm_interval: Duration::from_millis(101),
                system_interval: Duration::from_secs(1),
                duration: Some(Duration::from_secs(30)),
            }))
        );
    }

    #[test]
    fn watch_accepts_multiple_distinct_gpus() {
        let args = ["watch", "--gpu", "0", "--gpu", "amd:0", "--gpu", "nvidia:1"];
        let Ok(Command::Watch(options)) = parse(args.map(OsString::from)) else {
            panic!("argumentos válidos para três GPUs");
        };
        assert_eq!(
            options.gpus,
            [
                GpuIndex::Nvidia(0),
                GpuIndex::Radeon(0),
                GpuIndex::Nvidia(1)
            ]
        );
        assert!(parse(["watch", "--gpu", "0", "--gpu", "nvidia:0"].map(OsString::from)).is_err());
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

    #[test]
    fn gui_accepts_an_optional_gpu_and_rejects_watch_flags() {
        assert_eq!(
            parse(["gui"].map(OsString::from)),
            Ok(Command::Gui {
                gpu: None,
                source: Source::Direct
            })
        );
        assert_eq!(
            parse(["gui", "--gpu", "1"].map(OsString::from)),
            Ok(Command::Gui {
                gpu: Some(GpuIndex::Nvidia(1)),
                source: Source::Direct
            })
        );
        assert_eq!(
            parse(["gui", "--help"].map(OsString::from)),
            Ok(Command::GuiHelp)
        );
        for args in [["gui", "--gpu", "-1"], ["gui", "--duration-secs", "1"]] {
            assert!(parse(args.map(OsString::from)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn gui_distinguishes_portable_cache_from_an_explicit_package() {
        for (flag, source) in [
            ("--portable", Source::Portable),
            ("--portable-exact", Source::PortableExact),
        ] {
            assert_eq!(
                parse(["gui", flag].map(OsString::from)),
                Ok(Command::Gui { gpu: None, source })
            );
        }
        assert!(parse(["gui", "--portable", "--portable-exact"].map(OsString::from)).is_err());
    }

    #[test]
    fn distinguishes_radeon_from_nvidia_in_every_mode() {
        assert!(matches!(
            parse(["--gpu", "amd:1"].map(OsString::from)),
            Ok(Command::Probe(Options {
                gpu: Some(GpuIndex::Radeon(1)),
                ..
            }))
        ));
        assert!(matches!(
            parse(["watch", "--gpu", "amd:0"].map(OsString::from)),
            Ok(Command::Watch(WatchOptions { gpus, .. })) if gpus == [GpuIndex::Radeon(0)]
        ));
        assert!(matches!(
            parse(["gui", "--gpu", "nvidia:2"].map(OsString::from)),
            Ok(Command::Gui {
                gpu: Some(GpuIndex::Nvidia(2)),
                ..
            })
        ));
        for value in ["amd:", "amd:-1", "intel:0"] {
            assert!(parse(["--gpu", value].map(OsString::from)).is_err());
        }
    }
}
