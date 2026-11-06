use std::fmt::Display;

use crate::cli::Options;
use crate::nvml::{Device, Nvml};
use crate::system;

pub(crate) fn run(options: Options) -> Result<(), String> {
    println!("ARGOS {} — sonda de telemetria", env!("CARGO_PKG_VERSION"));
    system::report();

    println!("\nNVIDIA NVML");
    let nvml = Nvml::load().map_err(|error| error.to_string())?;
    print_value("Driver NVIDIA", nvml.driver_version());
    print_value("Versão da NVML", nvml.library_version());
    let count = nvml
        .device_count()
        .map_err(|error| format!("falha ao enumerar as GPUs NVIDIA ({error})"))?;
    println!("  GPUs enumeradas pela NVML: {count}");

    let indices = match options.gpu {
        Some(index) if index < count => index..index + 1,
        Some(index) => return Err(format!("índice NVML {index} fora da lista de GPUs")),
        None if count == 0 => {
            println!("  Nenhuma GPU NVIDIA encontrada pela NVML.");
            return Ok(());
        }
        None => 0..count,
    };

    let mut opened = false;
    for index in indices {
        println!("\nGPU NVIDIA {index}");
        match nvml.device(index) {
            Ok(device) => {
                opened = true;
                report_baseline(&device);
                report_gpm(&device, &options);
            }
            Err(error) => println!("  Acesso indisponível ({error})"),
        }
    }

    if opened {
        Ok(())
    } else {
        Err("não foi possível acessar nenhuma das GPUs selecionadas".into())
    }
}

fn report_baseline(device: &Device<'_>) {
    print_value("Modelo", device.name());
    print_value("Modo do driver", device.driver_model());
    print_value(
        "VRAM segundo a NVML",
        device.memory().map(|memory| {
            const BYTES_PER_MIB: f64 = 1_048_576.0;
            format!(
                "{:.1} MiB usados / {:.1} MiB totais ({:.1} MiB livres)",
                memory.used as f64 / BYTES_PER_MIB,
                memory.total as f64 / BYTES_PER_MIB,
                memory.free as f64 / BYTES_PER_MIB,
            )
        }),
    );
    print_value(
        "Atividade geral da GPU e da memória",
        device.utilization().map(|usage| {
            format!(
                "{}% do tempo com kernels em execução / {}% com leitura ou escrita na memória",
                usage.gpu, usage.memory,
            )
        }),
    );
    print_value(
        "Temperatura da GPU",
        device.temperature().map(|value| format!("{value} °C")),
    );
    print_value(
        "Potência da GPU e circuitos associados",
        device
            .power_milliwatts()
            .map(|value| format!("{:.2} W", f64::from(value) / 1_000.0)),
    );
    print_value(
        "Clock gráfico",
        device
            .graphics_clock_mhz()
            .map(|value| format!("{value} MHz")),
    );
    print_value(
        "Clock da memória",
        device
            .memory_clock_mhz()
            .map(|value| format!("{value} MHz")),
    );
    for (engine, utilization) in [
        ("NVENC", device.encoder_utilization()),
        ("NVDEC", device.decoder_utilization()),
    ] {
        print_value(
            engine,
            utilization.map(|value| {
                format!(
                    "{}% (janela de {} µs)",
                    value.percent, value.sampling_period_us
                )
            }),
        );
    }
}

fn report_gpm(device: &Device<'_>, options: &Options) {
    match device.gpm_supported() {
        Ok(true) => println!("  GPM: suporte informado pelo driver."),
        Ok(false) => {
            println!("  GPM: indisponível (driver informou falta de suporte nesta GPU).");
            return;
        }
        Err(error) => {
            println!("  GPM: consulta de suporte indisponível ({error}).");
            return;
        }
    }

    let mut sampler = match device.gpm_sampler() {
        Ok(sampler) => sampler,
        Err(error) => {
            println!("  GPM: falha ao iniciar a coleta ({error}).");
            return;
        }
    };

    for index in 1..=options.samples.get() {
        match sampler.read(options.interval) {
            Ok(reading) => {
                println!(
                    "  GPM intervalo {index}/{} ({:.1} ms entre coletas):",
                    options.samples,
                    reading.elapsed.as_secs_f64() * 1_000.0,
                );
                for (metric, value) in reading.metrics {
                    match value {
                        Ok(value) => {
                            println!("    {} [GPM {}]: {value:.2}%", metric.name, metric.id)
                        }
                        Err(error) => println!(
                            "    {} [GPM {}]: indisponível ({error})",
                            metric.name, metric.id,
                        ),
                    }
                }
            }
            Err(error) => println!(
                "  GPM intervalo {index}/{}: falha de coleta ({error}).",
                options.samples,
            ),
        }
    }
}

fn print_value(label: &str, value: Result<impl Display, impl Display>) {
    match value {
        Ok(value) => println!("  {label}: {value}"),
        Err(error) => println!("  {label}: indisponível ({error})"),
    }
}
