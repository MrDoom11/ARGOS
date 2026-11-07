use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use crate::cli::WatchOptions;
use crate::nvml::{Device, Error as NvmlError, GPM_METRICS, GpmSampler, Nvml};
use crate::system::{self, SystemSampler};
use crate::telemetry::{Cadence, History};

const HISTORY_WINDOW: Duration = Duration::from_secs(300);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);
const GPU_SENSORS: [Sensor; 11] = [
    ("gpu-temperature", "Temperatura", " °C"),
    ("gpu-power", "Potência", " W"),
    ("vram-used", "VRAM usada", " MiB"),
    ("vram-total", "VRAM total", " MiB"),
    ("vram-free", "VRAM livre", " MiB"),
    ("gpu-activity", "Atividade GPU", "%"),
    ("gpu-memory-activity", "Atividade de memória", "%"),
    ("gpu-graphics-clock", "Clock gráfico", " MHz"),
    ("gpu-memory-clock", "Clock de memória", " MHz"),
    ("gpu-nvenc", "NVENC", "%"),
    ("gpu-nvdec", "NVDEC", "%"),
];
const SYSTEM_SENSORS: [Sensor; 3] = [
    ("cpu-usage", "Uso de CPU", "%"),
    ("ram-used", "RAM usada", " GiB"),
    ("ram-total", "RAM total", " GiB"),
];
const GPM_COUNT: usize = GPM_METRICS.len();
const GPU: &str = "GPU / NVML";
const GPM: &str = "GPM / NVML";
const SYSTEM: &str = "Sistema / Windows";
// A chave persistida independe da tradução e da ordem de exibição do sensor.
type Sensor = (&'static str, &'static str, &'static str);

fn gpm_sensors() -> [Sensor; GPM_COUNT] {
    GPM_METRICS
        .each_ref()
        .map(|metric| (metric.key, metric.name, "%"))
}

pub(crate) fn sensor_catalog() -> impl Iterator<Item = (&'static str, &'static str)> {
    GPU_SENSORS
        .into_iter()
        .chain(gpm_sensors())
        .chain(SYSTEM_SENSORS)
        .map(|(key, _, unit)| (key, unit))
}

#[derive(Clone, Copy)]
pub(crate) enum Reading {
    Value(f64),
    Pending,
    Unavailable,
    Failed,
}

pub(crate) struct MetricSnapshot {
    pub(crate) name: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) source: &'static str,
    pub(crate) interval: Duration,
    pub(crate) points: Vec<(Instant, Reading)>,
}

pub(crate) struct Snapshot {
    pub(crate) at: Instant,
    pub(crate) notice: String,
    pub(crate) metrics: Vec<MetricSnapshot>,
}

type LatestSnapshot = Arc<Mutex<Option<Arc<Snapshot>>>>;

pub(crate) struct Monitor {
    latest: LatestSnapshot,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Monitor {
    pub(crate) fn start(gpu: Option<u32>) -> Result<Self, String> {
        let latest = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let feed = Arc::clone(&latest);
        let stop = Arc::clone(&stopped);
        let interval = Duration::from_secs(1);
        let options = WatchOptions {
            gpu,
            gpu_interval: interval,
            gpm_interval: interval,
            system_interval: interval,
            duration: None,
        };
        let worker = thread::Builder::new()
            .name("argos-coleta".into())
            .spawn(move || {
                let _ = collect(options, Sink::Window(feed), &stop);
            })
            .map_err(|error| format!("falha ao iniciar a coleta ({error})"))?;
        Ok(Self {
            latest,
            stopped,
            worker: Some(worker),
        })
    }

    pub(crate) fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.latest.lock().ok()?.clone()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
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

enum Sink {
    Console(SyncSender<Update>),
    Window(LatestSnapshot),
}

pub(crate) fn run(options: WatchOptions) -> Result<(), String> {
    let _control = ConsoleControl::install()
        .map_err(|error| format!("falha ao habilitar o encerramento com Ctrl+C ({error})"))?;
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(8);
        let worker = scope.spawn(move || {
            let _ = COLLECTOR.set(thread::current());
            collect(options, Sink::Console(sender), &STOPPED)
        });
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

fn collect(options: WatchOptions, sink: Sink, stopped: &AtomicBool) -> Result<(), String> {
    let (console, window) = match &sink {
        Sink::Console(sender) => (Some(sender), None),
        Sink::Window(latest) => (None, Some(latest)),
    };
    let nvml = Nvml::load();
    let index = options.gpu.unwrap_or(0);
    let (device, notice) = match select_gpu(&nvml, index) {
        Ok(device) => {
            let name = device
                .name()
                .unwrap_or_else(|_| "modelo indisponível".into());
            (Some(device), format!("GPU NVIDIA {index} · {name}"))
        }
        Err(error) if options.gpu.is_some() && console.is_some() => return Err(error),
        Err(error) => (None, format!("Coleta NVIDIA indisponível ({error}).")),
    };
    let notice = if let Some(sender) = console {
        let _ = sender.send(Update::Notice(notice));
        String::new()
    } else {
        notice
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
    let mut snapshot_due = start;
    let mut snapshot_pending = true;

    while !stopped.load(Ordering::Relaxed) && end.is_none_or(|end| Instant::now() < end) {
        if let Some((elapsed, frame)) =
            system.capture(start, || read_system(&mut system_source).into())
        {
            if let Some(sender) = console {
                publish(sender, Update::System(elapsed, frame), &mut skipped_updates);
            } else {
                snapshot_pending = true;
            }
        }
        if let (Some(device), Some(gpu), Some(gpm)) = (&device, &mut gpu, &mut gpm) {
            if let Some((elapsed, frame)) = gpu.capture(start, || read_gpu(device).into()) {
                if let Some(sender) = console {
                    publish(sender, Update::Gpu(elapsed, frame), &mut skipped_updates);
                } else {
                    snapshot_pending = true;
                }
            }
            if let Some((elapsed, frame)) =
                gpm.capture(start, || gpm_source.read(device, options.gpm_interval))
            {
                if let Some(sender) = console {
                    publish(sender, Update::Gpm(elapsed, frame), &mut skipped_updates);
                } else {
                    snapshot_pending = true;
                }
            }
        }

        let mut next = system.cadence.deadline();
        if let (Some(gpu), Some(gpm)) = (&gpu, &gpm) {
            next = next.min(gpu.cadence.deadline()).min(gpm.cadence.deadline());
        }
        if let Some(end) = end {
            next = next.min(end);
        }
        if let Some(latest) = window.filter(|_| snapshot_pending) {
            let now = Instant::now();
            if now >= snapshot_due {
                let snapshot =
                    build_snapshot(now, &notice, &options, &system, gpu.as_ref(), gpm.as_ref());
                // O custo de copiar as séries e descartar a cópia anterior fica fora da trava.
                let previous = latest
                    .try_lock()
                    .ok()
                    .map(|mut published| published.replace(snapshot));
                snapshot_pending = previous.is_none();
                drop(previous);
                snapshot_due = now + SNAPSHOT_INTERVAL;
                if snapshot_pending {
                    next = next.min(snapshot_due);
                }
            } else {
                next = next.min(snapshot_due);
            }
        }
        thread::park_timeout(next.saturating_duration_since(Instant::now()));
    }

    if let Some(sender) = console {
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
    }
    Ok(())
}

fn build_snapshot(
    at: Instant,
    notice: &str,
    options: &WatchOptions,
    system: &Group<{ SYSTEM_SENSORS.len() }>,
    gpu: Option<&Group<{ GPU_SENSORS.len() }>>,
    gpm: Option<&Group<GPM_COUNT>>,
) -> Arc<Snapshot> {
    let mut metrics = Vec::with_capacity(GPU_SENSORS.len() + GPM_COUNT + SYSTEM_SENSORS.len());
    append_metrics(
        &mut metrics,
        at,
        gpu,
        &GPU_SENSORS,
        GPU,
        options.gpu_interval,
    );
    append_metrics(
        &mut metrics,
        at,
        gpm,
        &gpm_sensors(),
        GPM,
        options.gpm_interval,
    );
    append_metrics(
        &mut metrics,
        at,
        Some(system),
        &SYSTEM_SENSORS,
        SYSTEM,
        options.system_interval,
    );
    Arc::new(Snapshot {
        at,
        notice: notice.into(),
        metrics,
    })
}

fn append_metrics<const N: usize>(
    metrics: &mut Vec<MetricSnapshot>,
    now: Instant,
    group: Option<&Group<N>>,
    sensors: &[Sensor; N],
    source: &'static str,
    interval: Duration,
) {
    let start = metrics.len();
    let capacity = group.map_or(0, |group| group.history.len());
    metrics.extend(sensors.iter().map(|&(_, name, unit)| MetricSnapshot {
        name,
        unit,
        source,
        interval,
        points: Vec::with_capacity(capacity),
    }));
    if let Some(group) = group {
        for (at, frame) in group.history.iter() {
            if now.saturating_duration_since(at) < HISTORY_WINDOW {
                for (metric, reading) in metrics[start..].iter_mut().zip(frame.values) {
                    metric.points.push((at, reading));
                }
            }
        }
    }
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
    let gpm_sensors = gpm_sensors();
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
    for ((_, label, unit), value) in sensors.iter().zip(frame.values) {
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

    #[test]
    fn snapshot_keeps_sensor_order_and_preserves_only_the_live_history() {
        let start = Instant::now();
        let now = start + HISTORY_WINDOW;
        let interval = Duration::from_secs(1);
        let options = WatchOptions {
            gpu: None,
            gpu_interval: interval,
            gpm_interval: interval,
            system_interval: interval,
            duration: None,
        };
        let mut system = Group::new(start, interval);
        system.history.push(start, [Reading::Value(12.0); 3].into());
        system.history.push(
            start + interval,
            [Reading::Value(0.0), Reading::Failed, Reading::Value(32.0)].into(),
        );

        let snapshot = build_snapshot(now, "Sem GPU", &options, &system, None, None);
        let gpu_and_gpm = GPU_SENSORS.len() + GPM_COUNT;
        assert_eq!(snapshot.metrics.len(), gpu_and_gpm + SYSTEM_SENSORS.len());
        let mut keys = std::collections::HashSet::new();
        assert_eq!(sensor_catalog().count(), snapshot.metrics.len());
        assert!(
            sensor_catalog()
                .zip(&snapshot.metrics)
                .all(|((key, unit), metric)| { keys.insert(key) && unit == metric.unit })
        );
        assert_eq!(snapshot.metrics[0].source, GPU);
        assert_eq!(snapshot.metrics[GPU_SENSORS.len()].source, GPM);
        assert!(
            snapshot.metrics[..gpu_and_gpm]
                .iter()
                .all(|metric| metric.points.is_empty())
        );
        assert!(matches!(
            snapshot.metrics[gpu_and_gpm].points.as_slice(),
            [(_, Reading::Value(0.0))]
        ));
        assert!(matches!(
            snapshot.metrics[gpu_and_gpm + 1].points.as_slice(),
            [(_, Reading::Failed)]
        ));
    }
}
