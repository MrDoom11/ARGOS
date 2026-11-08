use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use crate::cli::WatchOptions;
use crate::cpu_sensor::CpuSensor;
use crate::frames::{self, FrameSampler, FrameTarget};
use crate::nvml::{Device, Error as NvmlError, GPM_METRICS, GpmSampler, Nvml};
use crate::system::{self, SystemSampler};
use crate::telemetry::{Cadence, History};

const HISTORY_WINDOW: Duration = Duration::from_secs(300);
pub(crate) const GUI_INTERVALS_MS: [u32; 4] = [100, 200, 500, 1000];
const FASTEST_GUI_INTERVAL: Duration = Duration::from_millis(GUI_INTERVALS_MS[0] as u64);
const GPM_MIN_INTERVAL: Duration = Duration::from_millis(101);
const CPU_TEMPERATURE_RETRY: Duration = Duration::from_secs(5);
const SNAPSHOT_INTERVAL: Duration = FASTEST_GUI_INTERVAL;
const BYTES_PER_MIB: f64 = 1_048_576.0;
const BYTES_PER_GIB: f64 = 1_073_741_824.0;
const PERCENT_AXIS: Axis = Axis::Fixed(100.0);
const GPU_SENSORS: [Sensor; 11] = [
    (
        "gpu-temperature",
        "Temperatura da GPU",
        " °C",
        Axis::Fixed(100.0),
    ),
    ("gpu-power", "Potência", " W", Axis::Power),
    ("vram-used", "VRAM usada", " MiB", Axis::Vram),
    ("vram-total", "VRAM total", " MiB", Axis::Vram),
    ("vram-free", "VRAM livre", " MiB", Axis::Vram),
    ("gpu-activity", "Atividade GPU", "%", PERCENT_AXIS),
    ("gpu-memory-activity", "VRAM · atividade", "%", PERCENT_AXIS),
    (
        "gpu-graphics-clock",
        "Clock gráfico",
        " MHz",
        Axis::GraphicsClock,
    ),
    (
        "gpu-memory-clock",
        "VRAM · clock",
        " MHz",
        Axis::MemoryClock,
    ),
    ("gpu-nvenc", "NVENC", "%", PERCENT_AXIS),
    ("gpu-nvdec", "NVDEC", "%", PERCENT_AXIS),
];
const SYSTEM_SENSORS: [Sensor; 3] = [
    ("cpu-usage", "Uso de CPU", "%", PERCENT_AXIS),
    ("ram-used", "RAM usada", " GiB", Axis::Ram),
    ("ram-total", "RAM total", " GiB", Axis::Ram),
];
const FRAME_SENSORS: [Sensor; frames::SENSOR_COUNT] = [
    ("present-fps", "FPS apresentado", " FPS", Axis::Fixed(360.0)),
    (
        "present-frametime",
        "Frametime (pico)",
        " ms",
        Axis::Fixed(100.0),
    ),
    (
        "present-cpu-wait",
        "Espera CPU (pico)",
        " ms",
        Axis::Fixed(100.0),
    ),
];
const CPU_TEMPERATURE_SENSORS: [Sensor; 1] = [(
    "cpu-temperature",
    "Temperatura da CPU",
    " °C",
    Axis::Fixed(100.0),
)];
const GPM_COUNT: usize = GPM_METRICS.len();
pub(crate) const CPU_USAGE_INDEX: usize = GPU_SENSORS.len() + GPM_COUNT;
pub(crate) const RAM_USED_INDEX: usize = CPU_USAGE_INDEX + 1;
pub(crate) const FRAME_SENSOR_START: usize = CPU_USAGE_INDEX + SYSTEM_SENSORS.len();
pub(crate) const CPU_TEMPERATURE_INDEX: usize = FRAME_SENSOR_START + FRAME_SENSORS.len();
const GPU: &str = "GPU / NVML";
const GPM: &str = "GPM / NVML";
const SYSTEM: &str = "Sistema / Windows";
const FRAMES: &str = "Quadros / DirectX";
const CPU_TEMPERATURE: &str = "CPU / PawnIO";
// A chave persistida independe da tradução e da ordem de exibição do sensor.
type Sensor = (&'static str, &'static str, &'static str, Axis);

#[derive(Clone, Copy)]
enum Axis {
    Fixed(f64),
    Power,
    Vram,
    Ram,
    GraphicsClock,
    MemoryClock,
}

struct AxisLimits {
    power: f64,
    vram: f64,
    ram: f64,
    graphics_clock: f64,
    memory_clock: f64,
}

const FALLBACK_AXES: AxisLimits = AxisLimits {
    power: 1_000.0,
    vram: 131_072.0,
    ram: 128.0,
    graphics_clock: 4_000.0,
    memory_clock: 20_000.0,
};

impl AxisLimits {
    fn detect(device: Option<&Device<'_>>) -> Self {
        let mut limits = FALLBACK_AXES;
        let ram = system::memory_usage()
            .ok()
            .map(|memory| memory.total as f64 / BYTES_PER_GIB);
        limits.ram = axis_limit(ram, limits.ram);

        if let Some(device) = device {
            let vram = device
                .memory()
                .ok()
                .map(|memory| memory.total as f64 / BYTES_PER_MIB);
            let power = device
                .max_power_milliwatts()
                .ok()
                .map(|value| f64::from(value.div_ceil(25_000)) * 25.0);
            let graphics_clock = device
                .max_graphics_clock_mhz()
                .ok()
                .map(|value| f64::from(value.div_ceil(100)) * 100.0);
            let memory_clock = device
                .max_memory_clock_mhz()
                .ok()
                .map(|value| f64::from(value.div_ceil(100)) * 100.0);
            limits.vram = axis_limit(vram, limits.vram);
            limits.power = axis_limit(power, limits.power);
            limits.graphics_clock = axis_limit(graphics_clock, limits.graphics_clock);
            limits.memory_clock = axis_limit(memory_clock, limits.memory_clock);
        }
        limits
    }

    fn max_for(&self, axis: Axis) -> f64 {
        match axis {
            Axis::Fixed(value) => value,
            Axis::Power => self.power,
            Axis::Vram => self.vram,
            Axis::Ram => self.ram,
            Axis::GraphicsClock => self.graphics_clock,
            Axis::MemoryClock => self.memory_clock,
        }
    }
}

fn axis_limit(value: Option<f64>, fallback: f64) -> f64 {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(fallback)
}

fn gpm_sensors() -> [Sensor; GPM_COUNT] {
    GPM_METRICS
        .each_ref()
        .map(|metric| (metric.key, metric.name, "%", PERCENT_AXIS))
}

pub(crate) fn sensor_catalog() -> impl Iterator<Item = (&'static str, &'static str)> {
    GPU_SENSORS
        .into_iter()
        .chain(gpm_sensors())
        .chain(SYSTEM_SENSORS)
        .chain(FRAME_SENSORS)
        .chain(CPU_TEMPERATURE_SENSORS)
        .map(|(key, _, unit, _)| (key, unit))
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
    pub(crate) axis_max: f64,
    pub(crate) points: Vec<MetricPoint>,
}

#[derive(Clone, Copy)]
pub(crate) struct MetricPoint {
    pub(crate) at: Instant,
    pub(crate) reading: Reading,
    pub(crate) interval: Duration,
}

pub(crate) struct Snapshot {
    pub(crate) at: Instant,
    pub(crate) notice: String,
    pub(crate) metrics: Vec<MetricSnapshot>,
    pub(crate) frame_target: FrameTarget,
    pub(crate) frame_status: Option<&'static str>,
    pub(crate) cpu_temperature_status: Option<String>,
}

struct WindowFeed {
    latest: Mutex<Option<Arc<Snapshot>>>,
    requested: AtomicBool,
    interval_ms: AtomicU32,
    frame_target: Mutex<FrameTarget>,
    cpu_temperature_enabled: AtomicBool,
}

impl WindowFeed {
    fn frame_target(&self) -> FrameTarget {
        self.frame_target
            .lock()
            .map_or(FrameTarget::Off, |target| *target)
    }
}

pub(crate) struct Monitor {
    feed: Arc<WindowFeed>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Monitor {
    pub(crate) fn start(
        gpu: Option<u32>,
        interval_ms: u32,
        cpu_temperature_enabled: bool,
    ) -> Result<Self, String> {
        if !GUI_INTERVALS_MS.contains(&interval_ms) {
            return Err(format!("intervalo de coleta inválido: {interval_ms} ms"));
        }
        let feed = Arc::new(WindowFeed {
            latest: Mutex::new(None),
            requested: AtomicBool::new(true),
            interval_ms: AtomicU32::new(interval_ms),
            frame_target: Mutex::default(),
            cpu_temperature_enabled: AtomicBool::new(cpu_temperature_enabled),
        });
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_feed = Arc::clone(&feed);
        let stop = Arc::clone(&stopped);
        let interval = Duration::from_millis(u64::from(interval_ms));
        let options = WatchOptions {
            gpu,
            gpu_interval: interval,
            gpm_interval: interval.max(GPM_MIN_INTERVAL),
            system_interval: interval,
            duration: None,
        };
        let worker = thread::Builder::new()
            .name("argos-coleta".into())
            .spawn(move || {
                let _ = collect(options, Sink::Window(worker_feed), &stop);
            })
            .map_err(|error| format!("falha ao iniciar a coleta ({error})"))?;
        Ok(Self {
            feed,
            stopped,
            worker: Some(worker),
        })
    }

    pub(crate) fn snapshot(&self) -> Option<Arc<Snapshot>> {
        let current = self.feed.latest.lock().ok()?.clone();
        if !self.feed.requested.swap(true, Ordering::Relaxed) {
            self.wake();
        }
        current
    }

    pub(crate) fn set_interval_ms(&self, interval_ms: u32) {
        if GUI_INTERVALS_MS.contains(&interval_ms)
            && self.feed.interval_ms.swap(interval_ms, Ordering::Relaxed) != interval_ms
        {
            self.wake();
        }
    }

    pub(crate) fn set_cpu_temperature_enabled(&self, enabled: bool) {
        if self
            .feed
            .cpu_temperature_enabled
            .swap(enabled, Ordering::Relaxed)
            != enabled
        {
            self.wake();
        }
    }

    pub(crate) fn frame_target(&self) -> FrameTarget {
        self.feed.frame_target()
    }

    pub(crate) fn set_frame_target(&self, target: FrameTarget) {
        if let Ok(mut current) = self.feed.frame_target.lock() {
            *current = target;
        }
        self.feed.requested.store(true, Ordering::Relaxed);
        self.wake();
    }

    fn wake(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
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
    history: History<StoredFrame<N>>,
    capacity: usize,
}

struct StoredFrame<const N: usize> {
    frame: Frame<N>,
    interval: Duration,
}

impl<const N: usize> Group<N> {
    fn new(start: Instant, interval: Duration, fastest: Duration) -> Self {
        let capacity = HISTORY_WINDOW.as_millis().div_ceil(fastest.as_millis()) as usize;
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
        self.history.push(
            captured,
            StoredFrame {
                frame,
                interval: self.cadence.interval(),
            },
        );
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
            .filter(|(_, sample)| {
                sample
                    .frame
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

struct FrameMonitor {
    pid: u32,
    sampler: FrameSampler,
    group: Group<{ FRAME_SENSORS.len() }>,
    status: Option<&'static str>,
}

#[derive(Default)]
struct DashboardReadings {
    frames: Option<FrameMonitor>,
    frame_target: FrameTarget,
    cpu_temperature: Option<TemperatureMonitor>,
}

struct TemperatureMonitor {
    source: Option<CpuSensor>,
    group: Group<1>,
    retry_at: Instant,
    status: Option<String>,
}

impl TemperatureMonitor {
    fn new(start: Instant, interval: Duration) -> Self {
        Self {
            source: None,
            group: Group::new(start, interval, FASTEST_GUI_INTERVAL),
            retry_at: start,
            status: None,
        }
    }

    fn collect(&mut self, start: Instant) -> bool {
        self.group
            .capture(start, || {
                let now = Instant::now();
                if self.source.is_none() && now >= self.retry_at {
                    self.retry_at = now + CPU_TEMPERATURE_RETRY;
                    match CpuSensor::open() {
                        Ok(source) => self.source = Some(source),
                        Err(error) => self.status = Some(cpu_temperature_error(&error)),
                    }
                }
                let reading = self.source.as_mut().map_or(Reading::Unavailable, |source| {
                    read_cpu_temperature(source, &mut self.status)
                });
                [reading].into()
            })
            .is_some()
    }
}

impl FrameMonitor {
    fn new(pid: u32, interval: Duration) -> Self {
        let sampler = FrameSampler::start(pid);
        Self {
            pid,
            sampler,
            group: Group::new(Instant::now(), interval, FASTEST_GUI_INTERVAL),
            status: None,
        }
    }

    fn collect(&mut self, start: Instant) -> bool {
        let interval = self.group.cadence.interval();
        self.group
            .capture(start, || {
                let sample = self.sampler.sample(interval);
                self.status = Some(sample.status());
                match sample {
                    frames::Sample::Measured(values) => {
                        values.map(|value| value.map_or(Reading::Pending, Reading::Value))
                    }
                    frames::Sample::Waiting(_) => [Reading::Pending; FRAME_SENSORS.len()],
                    frames::Sample::Unavailable(_) => [Reading::Unavailable; FRAME_SENSORS.len()],
                }
                .into()
            })
            .is_some()
    }
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
    Window(Arc<WindowFeed>),
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

fn collect(mut options: WatchOptions, sink: Sink, stopped: &AtomicBool) -> Result<(), String> {
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
    let window = window.map(|feed| (feed, AxisLimits::detect(device.as_ref())));

    let start = Instant::now();
    let end = options.duration.map(|duration| start + duration);
    let fastest = |interval| {
        if window.is_some() {
            FASTEST_GUI_INTERVAL
        } else {
            interval
        }
    };
    let mut system = Group::new(
        start,
        options.system_interval,
        fastest(options.system_interval),
    );
    let mut system_source = SystemSampler::default();
    let mut gpu = device
        .as_ref()
        .map(|_| Group::new(start, options.gpu_interval, fastest(options.gpu_interval)));
    let mut gpm = device.as_ref().map(|_| {
        Group::new(
            start,
            options.gpm_interval,
            fastest(options.gpm_interval).max(GPM_MIN_INTERVAL),
        )
    });
    let mut gpm_source = GpmSource::Discover;
    let mut dashboard = DashboardReadings {
        cpu_temperature: window
            .as_ref()
            .map(|_| TemperatureMonitor::new(start, options.system_interval)),
        ..Default::default()
    };
    let mut skipped_updates = 0;
    let mut snapshot_due = start;
    let mut snapshot_pending = true;

    while !stopped.load(Ordering::Relaxed) && end.is_none_or(|end| Instant::now() < end) {
        if let Some((feed, _)) = &window {
            let interval =
                Duration::from_millis(u64::from(feed.interval_ms.load(Ordering::Relaxed)));
            if interval != options.gpu_interval {
                options.gpu_interval = interval;
                options.system_interval = interval;
                options.gpm_interval = interval.max(GPM_MIN_INTERVAL);
                system.cadence.set_interval(interval);
                if let Some(gpu) = &mut gpu {
                    gpu.cadence.set_interval(interval);
                }
                if let Some(gpm) = &mut gpm {
                    gpm.cadence.set_interval(options.gpm_interval);
                }
                if let Some(frames) = &mut dashboard.frames {
                    frames.group.cadence.set_interval(interval);
                }
                if let Some(temperature) = &mut dashboard.cpu_temperature {
                    temperature.group.cadence.set_interval(interval);
                }
                snapshot_pending = true;
            }
            let target = feed.frame_target();
            let previous_pid = dashboard.frames.as_ref().map(|frames| frames.pid);
            let previous_foreground =
                previous_pid.filter(|_| dashboard.frame_target == FrameTarget::Foreground);
            let pid = target.resolve(previous_foreground, frames::foreground_pid);
            if target != dashboard.frame_target || pid != previous_pid {
                // Drop the previous session before starting a capture for the new process.
                drop(dashboard.frames.take());
                dashboard.frames = pid.map(|pid| FrameMonitor::new(pid, interval));
                dashboard.frame_target = target;
                snapshot_pending = true;
            }
            if let Some(frames) = &mut dashboard.frames {
                snapshot_pending |= frames.collect(start);
            }
        }
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

        let temperature_enabled = window
            .as_ref()
            .is_some_and(|(feed, _)| feed.cpu_temperature_enabled.load(Ordering::Relaxed));
        if let Some(temperature) = &mut dashboard.cpu_temperature {
            if temperature_enabled {
                snapshot_pending |= temperature.collect(start);
            } else {
                temperature.source = None;
            }
        }
        let mut next = system.cadence.deadline();
        if let (Some(gpu), Some(gpm)) = (&gpu, &gpm) {
            next = next.min(gpu.cadence.deadline()).min(gpm.cadence.deadline());
        }
        if let Some(frames) = &dashboard.frames {
            next = next.min(frames.group.cadence.deadline());
        }
        if temperature_enabled && let Some(temperature) = &dashboard.cpu_temperature {
            next = next.min(temperature.group.cadence.deadline());
        }
        if let Some(end) = end {
            next = next.min(end);
        }
        if let Some((feed, axis_limits)) = window
            .as_ref()
            .filter(|(feed, _)| snapshot_pending && feed.requested.load(Ordering::Relaxed))
        {
            let now = Instant::now();
            if now >= snapshot_due {
                feed.requested.store(false, Ordering::Relaxed);
                let snapshot = build_snapshot(
                    now,
                    &notice,
                    &system,
                    gpu.as_ref(),
                    gpm.as_ref(),
                    &dashboard,
                    axis_limits,
                );
                // O custo de copiar as séries e descartar a cópia anterior fica fora da trava.
                let previous = feed
                    .latest
                    .try_lock()
                    .ok()
                    .map(|mut published| published.replace(snapshot));
                snapshot_pending = previous.is_none();
                drop(previous);
                snapshot_due = now + SNAPSHOT_INTERVAL;
                if snapshot_pending {
                    feed.requested.store(true, Ordering::Relaxed);
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
    system: &Group<{ SYSTEM_SENSORS.len() }>,
    gpu: Option<&Group<{ GPU_SENSORS.len() }>>,
    gpm: Option<&Group<GPM_COUNT>>,
    dashboard: &DashboardReadings,
    axis_limits: &AxisLimits,
) -> Arc<Snapshot> {
    let interval = system.cadence.interval();
    let mut metrics = Vec::with_capacity(CPU_TEMPERATURE_INDEX + CPU_TEMPERATURE_SENSORS.len());
    append_metrics(
        &mut metrics,
        at,
        gpu,
        &GPU_SENSORS,
        GPU,
        interval,
        axis_limits,
    );
    append_metrics(
        &mut metrics,
        at,
        gpm,
        &gpm_sensors(),
        GPM,
        interval.max(GPM_MIN_INTERVAL),
        axis_limits,
    );
    append_metrics(
        &mut metrics,
        at,
        Some(system),
        &SYSTEM_SENSORS,
        SYSTEM,
        interval,
        axis_limits,
    );
    let frames = dashboard.frames.as_ref();
    append_metrics(
        &mut metrics,
        at,
        frames.map(|frames| &frames.group),
        &FRAME_SENSORS,
        FRAMES,
        interval,
        axis_limits,
    );
    let temperature = dashboard.cpu_temperature.as_ref();
    append_metrics(
        &mut metrics,
        at,
        temperature.map(|monitor| &monitor.group),
        &CPU_TEMPERATURE_SENSORS,
        CPU_TEMPERATURE,
        interval,
        axis_limits,
    );
    if temperature.is_none() {
        metrics[CPU_TEMPERATURE_INDEX].points.push(MetricPoint {
            at,
            reading: Reading::Unavailable,
            interval,
        });
    }
    Arc::new(Snapshot {
        at,
        notice: notice.into(),
        metrics,
        frame_target: dashboard.frame_target,
        frame_status: frames.and_then(|frames| frames.status).or_else(|| {
            (frames.is_none() && dashboard.frame_target == FrameTarget::Foreground)
                .then_some("Aguardando um aplicativo em foco")
        }),
        cpu_temperature_status: temperature.and_then(|monitor| monitor.status.clone()),
    })
}

fn append_metrics<const N: usize>(
    metrics: &mut Vec<MetricSnapshot>,
    now: Instant,
    group: Option<&Group<N>>,
    sensors: &[Sensor; N],
    source: &'static str,
    interval: Duration,
    axis_limits: &AxisLimits,
) {
    let start = metrics.len();
    let capacity = group.map_or(0, |group| group.history.len());
    metrics.extend(sensors.iter().map(|&(_, name, unit, axis)| MetricSnapshot {
        name,
        unit,
        source,
        interval,
        axis_max: axis_limits.max_for(axis),
        points: Vec::with_capacity(capacity),
    }));
    if let Some(group) = group {
        for (at, sample) in group.history.iter() {
            if now.saturating_duration_since(at) < HISTORY_WINDOW {
                for (metric, reading) in metrics[start..].iter_mut().zip(sample.frame.values) {
                    metric.points.push(MetricPoint {
                        at,
                        reading,
                        interval: sample.interval,
                    });
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
        [
            memory.used as f64 / BYTES_PER_MIB,
            memory.total as f64 / BYTES_PER_MIB,
            memory.free as f64 / BYTES_PER_MIB,
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
        Ok(memory) => [
            Reading::Value(memory.used as f64 / BYTES_PER_GIB),
            Reading::Value(memory.total as f64 / BYTES_PER_GIB),
        ],
        Err(_) => [Reading::Failed; 2],
    };
    [cpu, used, total]
}

fn read_cpu_temperature(source: &mut CpuSensor, status: &mut Option<String>) -> Reading {
    let result = source.celsius();
    match result {
        Ok(Some(value)) => {
            *status = None;
            Reading::Value(value)
        }
        Ok(None) => Reading::Pending,
        Err(error) => {
            *status = Some(cpu_temperature_error(&error));
            if error.kind() == io::ErrorKind::Unsupported {
                Reading::Unavailable
            } else {
                Reading::Failed
            }
        }
    }
}

fn cpu_temperature_error(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        "Acesso à leitura do PawnIO negado".into()
    } else {
        error.to_string()
    }
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
    for ((_, label, unit, _), value) in sensors.iter().zip(frame.values) {
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
        let interval = Duration::from_millis(1);
        let mut group = Group::new(start, interval, interval);
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
    fn temperature_keeps_prior_sample_interval_after_refresh_change() {
        let start = Instant::now();
        let initial = Duration::from_millis(500);
        let refresh = Duration::from_millis(100);
        let mut temperature = TemperatureMonitor::new(start, initial);
        assert!(
            temperature
                .group
                .capture(start, || [Reading::Value(56.875)].into())
                .is_some()
        );
        temperature.group.cadence.set_interval(refresh);
        let dashboard = DashboardReadings {
            cpu_temperature: Some(temperature),
            ..Default::default()
        };
        let system = Group::new(start, refresh, FASTEST_GUI_INTERVAL);
        let snapshot = build_snapshot(
            Instant::now(),
            "",
            &system,
            None,
            None,
            &dashboard,
            &FALLBACK_AXES,
        );
        let metric = &snapshot.metrics[CPU_TEMPERATURE_INDEX];
        assert_eq!(metric.interval, refresh);
        let [point] = metric.points.as_slice() else {
            panic!("o histórico da temperatura foi perdido");
        };
        assert_eq!(point.interval, initial);
        assert!(matches!(point.reading, Reading::Value(56.875)));
    }

    #[test]
    fn snapshot_keeps_sensor_order_and_preserves_only_the_live_history() {
        let start = Instant::now();
        let now = start + HISTORY_WINDOW;
        let interval = Duration::from_secs(1);
        let prior_interval = Duration::from_millis(200);
        let mut system = Group::new(start, interval, FASTEST_GUI_INTERVAL);
        system.history.push(
            start,
            StoredFrame {
                frame: [Reading::Value(12.0); 3].into(),
                interval,
            },
        );
        system.history.push(
            start + interval,
            StoredFrame {
                frame: [Reading::Value(0.0), Reading::Failed, Reading::Value(32.0)].into(),
                interval: prior_interval,
            },
        );

        let limits = AxisLimits {
            vram: 16_384.0,
            ram: 31.5,
            ..FALLBACK_AXES
        };
        let snapshot = build_snapshot(
            now,
            "Sem GPU",
            &system,
            None,
            None,
            &DashboardReadings::default(),
            &limits,
        );
        let gpu_and_gpm = GPU_SENSORS.len() + GPM_COUNT;
        assert_eq!(
            snapshot.metrics.len(),
            CPU_TEMPERATURE_INDEX + CPU_TEMPERATURE_SENSORS.len()
        );
        assert_eq!(snapshot.frame_target, FrameTarget::Foreground);
        assert_eq!(
            snapshot.frame_status,
            Some("Aguardando um aplicativo em foco")
        );
        assert!(
            snapshot.metrics[FRAME_SENSOR_START..CPU_TEMPERATURE_INDEX]
                .iter()
                .all(|metric| { metric.points.is_empty() && metric.source == FRAMES })
        );
        let temperature = &snapshot.metrics[CPU_TEMPERATURE_INDEX];
        assert_eq!(temperature.interval, interval);
        assert!(matches!(
            temperature.points.as_slice(),
            [MetricPoint {
                reading: Reading::Unavailable,
                ..
            }]
        ));
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
            [MetricPoint { at, reading: Reading::Value(0.0), interval: sample_interval }]
                if *at == start + interval && *sample_interval == prior_interval
        ));
        assert!(matches!(
            snapshot.metrics[gpu_and_gpm + 1].points.as_slice(),
            [MetricPoint {
                reading: Reading::Failed,
                ..
            }]
        ));
        assert_eq!(snapshot.metrics[gpu_and_gpm].interval, interval);
        assert_eq!(system.capacity, 3000);
        assert_eq!(snapshot.metrics[0].axis_max, 100.0);
        assert!(
            snapshot.metrics[2..5]
                .iter()
                .all(|metric| metric.axis_max == limits.vram)
        );
        assert!(
            snapshot
                .metrics
                .iter()
                .filter(|metric| metric.unit == "%")
                .all(|metric| metric.axis_max == 100.0)
        );
        assert!(
            snapshot.metrics[gpu_and_gpm + 1..FRAME_SENSOR_START]
                .iter()
                .all(|metric| metric.axis_max == limits.ram)
        );
        assert!(
            snapshot
                .metrics
                .iter()
                .all(|metric| metric.axis_max.is_finite() && metric.axis_max > 0.0)
        );
    }

    #[test]
    fn invalid_hardware_limit_uses_fixed_fallback() {
        for invalid in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
        ] {
            assert_eq!(
                axis_limit(invalid, FALLBACK_AXES.power),
                FALLBACK_AXES.power
            );
        }
        assert_eq!(axis_limit(Some(175.0), FALLBACK_AXES.power), 175.0);
    }
}
