use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crate::cli::WatchOptions;
use crate::nvml::{Device, Error as NvmlError, GPM_METRICS, GpmSampler, Nvml};
use crate::system::{self, SystemSampler};
use crate::telemetry::{Cadence, History};

const HISTORY_WINDOW: Duration = Duration::from_secs(300);
const GPU_SENSORS: [Sensor; 11] = [
    ("Temperatura", " °C"),
    ("Potência", " W"),
    ("VRAM usada", " MiB"),
    ("VRAM total", " MiB"),
    ("VRAM livre", " MiB"),
    ("Atividade GPU", "%"),
    ("Atividade de memória", "%"),
    ("Clock gráfico", " MHz"),
    ("Clock de memória", " MHz"),
    ("NVENC", "%"),
    ("NVDEC", "%"),
];
const SYSTEM_SENSORS: [Sensor; 3] = [
    ("Uso de CPU", "%"),
    ("RAM usada", " GiB"),
    ("RAM total", " GiB"),
];
const GPM_COUNT: usize = GPM_METRICS.len();
const GPU: &str = "GPU / NVML";
const GPM: &str = "GPM / NVML";
const SYSTEM: &str = "Sistema / Windows";
type Sensor = (&'static str, &'static str);

#[derive(Clone, Copy)]
enum Reading {
    Value(f64),
    Pending,
    Unavailable,
    Failed,
}

impl Reading {
    fn from_nvml_error(error: &NvmlError) -> Self {
        if error.is_unsupported() {
            Self::Unavailable
        } else {
            Self::Failed
        }
    }
}

#[derive(Clone, Copy)]
struct Frame<const N: usize> {
    values: [Reading; N],
    window: Option<Duration>,
}

impl<const N: usize> From<[Reading; N]> for Frame<N> {
    fn from(values: [Reading; N]) -> Self {
        Self {
            values,
            window: None,
        }
    }
}

struct Group<const N: usize> {
    cadence: Cadence,
    history: History<Frame<N>>,
    capacity: usize,
}

impl<const N: usize> Group<N> {
    fn new(start: Instant, interval: Duration) -> Self {
        let capacity = HISTORY_WINDOW.as_millis().div_ceil(interval.as_millis()) as usize;
        Self {
            cadence: Cadence::new(start, interval),
            history: History::new(HISTORY_WINDOW, capacity),
            capacity,
        }
    }

    fn capture(
        &mut self,
        start: Instant,
        collect: impl FnOnce() -> Frame<N>,
    ) -> Option<(Duration, Frame<N>)> {
        if Instant::now() < self.cadence.deadline() {
            return None;
        }
        let frame = collect();
        let captured = Instant::now();
        self.cadence.complete(captured);
        self.history.push(captured, frame);
        Some((captured - start, frame))
    }

    fn summary(&mut self, end: Instant, name: &'static str) -> HistorySummary {
        self.history.prune(end);
        let mut points = self.history.iter();
        let span = match (points.next(), self.history.iter().next_back()) {
            (Some((first, _)), Some((last, _))) => last - first,
            _ => Duration::ZERO,
        };
        let gaps = self
            .history
            .iter()
            .filter(|(_, frame)| {
                frame
                    .values
                    .iter()
                    .any(|value| !matches!(value, Reading::Value(_)))
            })
            .count();
        HistorySummary {
            name,
            count: self.history.len(),
            capacity: self.capacity,
            gaps,
            span,
        }
    }
}

struct HistorySummary {
    name: &'static str,
    count: usize,
    capacity: usize,
    gaps: usize,
    span: Duration,
}

enum Update {
    Notice(String),
    Gpu(Duration, Frame<{ GPU_SENSORS.len() }>),
    Gpm(Duration, Frame<GPM_COUNT>),
    System(Duration, Frame<{ SYSTEM_SENSORS.len() }>),
    Finished {
        elapsed: Duration,
        groups: [Option<HistorySummary>; 3],
        skipped_updates: u64,
    },
}

pub(crate) fn run(options: WatchOptions) -> Result<(), String> {
    let _control = ConsoleControl::install()
        .map_err(|error| format!("falha ao habilitar o encerramento com Ctrl+C ({error})"))?;
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(8);
        let worker = scope.spawn(move || collect(options, sender));
        let output = report_updates(&receiver);
        drop(receiver);
        if output.is_err() {
            request_stop();
        }
        let result = worker
            .join()
            .map_err(|_| "falha interna na coleta contínua")?;
        match output {
            Ok(()) => result,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(format!("falha ao escrever no console ({error})")),
        }
    })
}

fn collect(options: WatchOptions, sender: SyncSender<Update>) -> Result<(), String> {
    let _ = COLLECTOR.set(thread::current());
    let nvml = Nvml::load();
    let device = match select_gpu(&nvml, options.gpu.unwrap_or(0)) {
        Ok(device) => {
            let name = device
                .name()
                .unwrap_or_else(|_| "modelo indisponível".into());
            let _ = sender.send(Update::Notice(format!(
                "GPU NVIDIA {} · {name}",
                options.gpu.unwrap_or(0)
            )));
            Some(device)
        }
        Err(error) if options.gpu.is_some() => return Err(error),
        Err(error) => {
            let _ = sender.send(Update::Notice(format!(
                "Coleta NVIDIA indisponível ({error})."
            )));
            None
        }
    };

    let start = Instant::now();
    let end = options.duration.map(|duration| start + duration);
    let mut system = Group::new(start, options.system_interval);
    let mut system_source = SystemSampler::default();
    let mut gpu = device
        .as_ref()
        .map(|_| Group::new(start, options.gpu_interval));
    let mut gpm = device
        .as_ref()
        .map(|_| Group::new(start, options.gpm_interval));
    let mut gpm_source = GpmSource::Discover;
    let mut skipped_updates = 0;

    while !STOPPED.load(Ordering::Relaxed) && end.is_none_or(|end| Instant::now() < end) {
        if let Some((elapsed, frame)) =
            system.capture(start, || read_system(&mut system_source).into())
        {
            publish(
                &sender,
                Update::System(elapsed, frame),
                &mut skipped_updates,
            );
        }
        if let (Some(device), Some(gpu), Some(gpm)) = (&device, &mut gpu, &mut gpm) {
            if let Some((elapsed, frame)) = gpu.capture(start, || read_gpu(device).into()) {
                publish(&sender, Update::Gpu(elapsed, frame), &mut skipped_updates);
            }
            if let Some((elapsed, frame)) =
                gpm.capture(start, || gpm_source.read(device, options.gpm_interval))
            {
                publish(&sender, Update::Gpm(elapsed, frame), &mut skipped_updates);
            }
        }

        let mut next = system.cadence.deadline();
        if let (Some(gpu), Some(gpm)) = (&gpu, &gpm) {
            next = next.min(gpu.cadence.deadline()).min(gpm.cadence.deadline());
        }
        if let Some(end) = end {
            next = next.min(end);
        }
        thread::park_timeout(next.saturating_duration_since(Instant::now()));
    }

    let finished = Instant::now();
    let _ = sender.send(Update::Finished {
        elapsed: finished - start,
        groups: [
            gpu.map(|mut group| group.summary(finished, GPU)),
            gpm.map(|mut group| group.summary(finished, GPM)),
            Some(system.summary(finished, SYSTEM)),
        ],
        skipped_updates,
    });
    Ok(())
}

fn select_gpu(nvml: &Result<Nvml, NvmlError>, index: u32) -> Result<Device<'_>, String> {
    let nvml = nvml.as_ref().map_err(ToString::to_string)?;
    let count = nvml.device_count().map_err(|error| error.to_string())?;
    if index >= count {
        return Err(format!(
            "índice NVML {index} fora da lista de {count} GPU(s)"
        ));
    }
    nvml.device(index).map_err(|error| error.to_string())
}

fn publish(sender: &SyncSender<Update>, update: Update, skipped_updates: &mut u64) {
    if let Err(TrySendError::Full(_)) = sender.try_send(update) {
        *skipped_updates += 1;
    }
}

fn read_gpu(device: &Device<'_>) -> [Reading; GPU_SENSORS.len()] {
    let [used, total, free] = nvml_values(device.memory(), |memory| {
        const MIB: f64 = 1_048_576.0;
        [
            memory.used as f64 / MIB,
            memory.total as f64 / MIB,
            memory.free as f64 / MIB,
        ]
    });
    let [gpu, memory] = nvml_values(device.utilization(), |usage| {
        [f64::from(usage.gpu), f64::from(usage.memory)]
    });
    [
        nvml_value(device.temperature().map(f64::from)),
        nvml_value(
            device
                .power_milliwatts()
                .map(|value| f64::from(value) / 1_000.0),
        ),
        used,
        total,
        free,
        gpu,
        memory,
        nvml_value(device.graphics_clock_mhz().map(f64::from)),
        nvml_value(device.memory_clock_mhz().map(f64::from)),
        nvml_value(
            device
                .encoder_utilization()
                .map(|usage| f64::from(usage.percent)),
        ),
        nvml_value(
            device
                .decoder_utilization()
                .map(|usage| f64::from(usage.percent)),
        ),
    ]
}

fn nvml_values<T, const N: usize>(
    result: Result<T, NvmlError>,
    convert: impl FnOnce(T) -> [f64; N],
) -> [Reading; N] {
    match result {
        Ok(value) => convert(value).map(Reading::Value),
        Err(error) => [Reading::from_nvml_error(&error); N],
    }
}

fn nvml_value(result: Result<f64, NvmlError>) -> Reading {
    match result {
        Ok(value) => Reading::Value(value),
        Err(error) => Reading::from_nvml_error(&error),
    }
}

enum GpmSource<'a> {
    Discover,
    Active(GpmSampler<'a>),
    Unavailable,
}

impl<'a> GpmSource<'a> {
    fn read(&mut self, device: &Device<'a>, interval: Duration) -> Frame<GPM_COUNT> {
        if matches!(self, Self::Discover) {
            match device.gpm_supported() {
                Ok(true) => match device.gpm_sampler() {
                    Ok(sampler) => *self = Self::Active(sampler),
                    Err(error) => return self.discovery_error(&error),
                },
                Ok(false) => *self = Self::Unavailable,
                Err(error) => return self.discovery_error(&error),
            }
        }
        let Self::Active(sampler) = self else {
            return [Reading::Unavailable; GPM_COUNT].into();
        };
        match sampler.try_read(interval) {
            Ok(Some(reading)) => Frame {
                values: reading.metrics.map(|(_, value)| nvml_value(value)),
                window: Some(reading.elapsed),
            },
            Ok(None) => [Reading::Pending; GPM_COUNT].into(),
            Err(error) => [Reading::from_nvml_error(&error); GPM_COUNT].into(),
        }
    }

    fn discovery_error(&mut self, error: &NvmlError) -> Frame<GPM_COUNT> {
        if error.is_unsupported() {
            *self = Self::Unavailable;
        }
        [Reading::from_nvml_error(error); GPM_COUNT].into()
    }
}

fn read_system(sampler: &mut SystemSampler) -> [Reading; SYSTEM_SENSORS.len()] {
    let cpu = match sampler.cpu_usage() {
        Ok(Some(value)) => Reading::Value(value),
        Ok(None) => Reading::Pending,
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Reading::Unavailable,
        Err(_) => Reading::Failed,
    };
    let [used, total] = match system::memory_usage() {
        Ok(memory) => {
            const GIB: f64 = 1_073_741_824.0;
            [
                Reading::Value(memory.used as f64 / GIB),
                Reading::Value(memory.total as f64 / GIB),
            ]
        }
        Err(_) => [Reading::Failed; 2],
    };
    [cpu, used, total]
}

fn report_updates(receiver: &Receiver<Update>) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "ARGOS — coleta contínua (Ctrl+C para encerrar)")?;
    let gpm_sensors = GPM_METRICS.each_ref().map(|metric| (metric.name, "%"));
    for update in receiver {
        match update {
            Update::Notice(message) => writeln!(out, "{message}")?,
            Update::Gpu(elapsed, frame) => {
                report_frame(&mut out, GPU, elapsed, &GPU_SENSORS, frame)?
            }
            Update::Gpm(elapsed, frame) => {
                report_frame(&mut out, GPM, elapsed, &gpm_sensors, frame)?
            }
            Update::System(elapsed, frame) => {
                report_frame(&mut out, SYSTEM, elapsed, &SYSTEM_SENSORS, frame)?
            }
            Update::Finished {
                elapsed,
                groups,
                skipped_updates,
            } => {
                writeln!(out, "Coleta encerrada após {:.2} s.", elapsed.as_secs_f64())?;
                for summary in groups.into_iter().flatten() {
                    writeln!(
                        out,
                        "Histórico {}: {}/{} amostras, {} com lacunas, {:.2} s entre a primeira e a última.",
                        summary.name,
                        summary.count,
                        summary.capacity,
                        summary.gaps,
                        summary.span.as_secs_f64(),
                    )?;
                }
                if skipped_updates != 0 {
                    writeln!(out, "Atualizações descartadas na saída: {skipped_updates}.")?;
                }
            }
        }
    }
    out.flush()
}

fn report_frame<const N: usize>(
    out: &mut impl Write,
    group: &str,
    elapsed: Duration,
    sensors: &[Sensor; N],
    frame: Frame<N>,
) -> io::Result<()> {
    write!(out, "[{:.3} s] {group}", elapsed.as_secs_f64())?;
    if let Some(window) = frame.window {
        write!(out, " ({:.1} ms)", window.as_secs_f64() * 1_000.0)?;
    }
    for ((label, unit), value) in sensors.iter().zip(frame.values) {
        write!(out, " | {label}: ")?;
        match value {
            Reading::Value(value) => write!(out, "{value:.2}{unit}")?,
            Reading::Pending => write!(out, "aguardando amostra")?,
            Reading::Unavailable => write!(out, "indisponível")?,
            Reading::Failed => write!(out, "falha de leitura")?,
        }
    }
    writeln!(out)
}

static STOPPED: AtomicBool = AtomicBool::new(false);
static COLLECTOR: OnceLock<Thread> = OnceLock::new();
const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

struct ConsoleControl;

impl ConsoleControl {
    fn install() -> io::Result<Self> {
        // SAFETY: o callback tem a ABI da API e permanece válido durante o processo.
        if unsafe { SetConsoleCtrlHandler(Some(console_control), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }
}

impl Drop for ConsoleControl {
    fn drop(&mut self) {
        // SAFETY: remove o mesmo callback registrado por install.
        unsafe { SetConsoleCtrlHandler(Some(console_control), 0) };
    }
}

extern "system" fn console_control(event: u32) -> i32 {
    if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        request_stop();
        return 1;
    }
    0
}

fn request_stop() {
    STOPPED.store(true, Ordering::Relaxed);
    if let Some(worker) = COLLECTOR.get() {
        worker.unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_keeps_a_failure_and_recovers_at_the_next_interval() {
        let start = Instant::now();
        let mut group = Group::new(start, Duration::from_millis(1));
        let mut source = [Reading::Failed, Reading::Value(0.0)].into_iter();

        let first = group.capture(start, || [source.next().unwrap()].into());
        assert!(matches!(first.unwrap().1.values, [Reading::Failed]));
        while let Some(wait) = group
            .cadence
            .deadline()
            .checked_duration_since(Instant::now())
        {
            thread::sleep(wait);
        }
        let next = group.capture(start, || [source.next().unwrap()].into());
        assert!(matches!(next.unwrap().1.values, [Reading::Value(0.0)]));

        let summary = group.summary(Instant::now(), "teste");
        assert_eq!(summary.count, 2);
        assert_eq!(summary.gaps, 1);
    }
}
