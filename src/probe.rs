use std::fmt::Display;

use crate::adlx;
use crate::cli::{GpuIndex, Options};
use crate::gpu::{self, Inventory, Selection};
use crate::nvml::Device;
use crate::system;

pub(crate) fn run(options: Options) -> Result<(), String> {
    println!("ARGOS {} — sonda de telemetria", env!("CARGO_PKG_VERSION"));
    system::report();

    let inventory = Inventory::discover();
    println!("\nNVIDIA NVML");
    let nvidia_count = match inventory.nvml() {
        Ok(nvml) => {
            print_value("Driver NVIDIA", nvml.driver_version());
            print_value("Versão da NVML", nvml.library_version());
            let count = nvml.device_count();
            let enumerated = count.as_ref().copied().unwrap_or(0);
            print_value("GPUs enumeradas pela NVML", count);
            enumerated
        }
        Err(error) => {
            println!("  NVML: indisponível ({error})");
            0
        }
    };

    println!("\nAMD ADLX");
    if let Some(error) = inventory.adlx_error() {
        println!("  ADLX: indisponível ({error})");
    } else {
        let count = inventory
            .devices()
            .iter()
            .filter(|device| matches!(device.index, GpuIndex::Radeon(_)))
            .count();
        println!("  GPUs enumeradas pela ADLX: {count}");
    }

    let devices = match options.gpu {
        Some(index) => inventory
            .resolve(&Selection::Index(index))?
            .map_or(&[][..], std::slice::from_ref),
        None => inventory.devices(),
    };
    if devices.is_empty() && nvidia_count == 0 {
        if inventory.nvml().is_err() && inventory.adlx_error().is_some() {
            return Err("não foi possível consultar GPUs pela NVML ou pela ADLX".into());
        }
        println!("  Nenhuma GPU compatível encontrada.");
        return Ok(());
    }

    let mut opened = false;
    for info in devices {
        match info.index {
            GpuIndex::Nvidia(index) => println!("\nGPU NVIDIA {index}"),
            GpuIndex::Radeon(index) => println!("\nGPU Radeon {index}"),
        }
        match inventory.open(info.index) {
            Ok(gpu::Device::Nvidia(device)) => {
                opened = true;
                report_baseline(&device);
                report_gpm(&device, &options);
            }
            Ok(gpu::Device::Radeon(device)) => {
                opened = true;
                report_radeon(&device);
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
        "Atividade da GPU e da VRAM",
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
        "Clock gráfico da GPU",
        device
            .graphics_clock_mhz()
            .map(|value| format!("{value} MHz")),
    );
    print_value(
        "Clock da VRAM",
        device
            .memory_clock_mhz()
            .map(|value| format!("{value} MHz")),
    );
    for (engine, utilization) in [
        ("Codificação da GPU (NVENC)", device.encoder_utilization()),
        ("Decodificação da GPU (NVDEC)", device.decoder_utilization()),
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

fn report_radeon(device: &adlx::Device<'_>) {
    println!("  Modelo: {}", device.name());
    let total = device.total_vram_mib();
    print_adlx_value("VRAM total", total, "MiB", 1);
    let sample = match device.current_sample() {
        Ok(sample) => sample,
        Err(error) => {
            println!("  Métricas ADLX: indisponíveis ({error})");
            return;
        }
    };
    let free = total
        .zip(sample.vram_used_mib)
        .and_then(|(total, used)| (used >= 0.0 && used <= total).then_some(total - used));
    for (label, value, unit, precision) in [
        ("VRAM em uso", sample.vram_used_mib, "MiB", 1),
        ("VRAM livre", free, "MiB", 1),
        (
            "Memória compartilhada em uso",
            sample.shared_memory_mib,
            "MiB",
            1,
        ),
        ("Uso da GPU", sample.usage_percent, "%", 1),
        ("Temperatura da GPU", sample.temperature, "°C", 1),
        ("Potência total da placa", sample.board_power_watts, "W", 2),
        ("Clock gráfico da GPU", sample.graphics_clock_mhz, "MHz", 0),
        ("Clock da VRAM", sample.memory_clock_mhz, "MHz", 0),
    ] {
        print_adlx_value(label, value, unit, precision);
    }
}

fn print_adlx_value(label: &str, value: Option<f64>, unit: &str, precision: usize) {
    print_value(
        label,
        value
            .filter(|value| value.is_finite())
            .map(|value| format!("{value:.precision$} {unit}"))
            .ok_or("driver não informou a leitura"),
    );
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
