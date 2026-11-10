use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use crate::adlx;
use crate::cli::{GpuIndex, WatchOptions};
use crate::cpu_sensor::CpuSensor;
use crate::frames::{self, FrameSampler, FrameTarget};
use crate::gpu::{self, GpuInfo, Inventory, Selection};
use crate::gpu_video;
use crate::nvml::{Device as NvmlDevice, Error as NvmlError, GPM_METRICS, GpmSampler};
use crate::system::{self, SystemSampler};
use crate::telemetry::{Cadence, History};

const HISTORY_WINDOW: Duration = Duration::from_secs(300);
pub(crate) const GUI_INTERVALS_MS: [u32; 4] = [100, 200, 500, 1000];
const FASTEST_GUI_INTERVAL: Duration = Duration::from_millis(GUI_INTERVALS_MS[0] as u64);
const GPM_MIN_INTERVAL: Duration = Duration::from_millis(101);
const VIDEO_MIN_INTERVAL: Duration = Duration::from_secs(1);
const CPU_SENSOR_RETRY: Duration = Duration::from_secs(5);
const CPU_POWER_AXIS_START: f64 = 150.0;
const SNAPSHOT_INTERVAL: Duration = FASTEST_GUI_INTERVAL;
const BYTES_PER_MIB: f64 = 1_048_576.0;
const BYTES_PER_GIB: f64 = 1_073_741_824.0;
const PERCENT_AXIS: Axis = Axis::Fixed(100.0);
const GPU_SENSORS: [Sensor; 12] = [
    (
        "gpu-temperature",
        "Temperatura da GPU",
        " °C",
        Axis::Fixed(100.0),
    ),
    ("gpu-power", "Potência da GPU", " W", Axis::Power),
    ("vram-used", "VRAM usada", " MiB", Axis::Vram),
    ("vram-total", "VRAM total", " MiB", Axis::Vram),
    ("vram-free", "VRAM livre", " MiB", Axis::Vram),
    ("gpu-activity", "Atividade da GPU", "%", PERCENT_AXIS),
    (
        "gpu-memory-activity",
        "Tempo ativo da VRAM",
        "%",
        PERCENT_AXIS,
    ),
    (
        "gpu-graphics-clock",
        "Clock gráfico da GPU",
        " MHz",
        Axis::GraphicsClock,
    ),
    (
        "gpu-memory-clock",
        "Clock da VRAM",
        " MHz",
        Axis::MemoryClock,
    ),
    ("gpu-nvenc", "Codificação da GPU (NVENC)", "%", PERCENT_AXIS),
    (
        "gpu-nvdec",
        "Decodificação da GPU (NVDEC)",
        "%",
        PERCENT_AXIS,
    ),
    (
        "gpu-shared-memory",
        "RAM compartilhada da GPU",
        " MiB",
        Axis::SharedMemory,
    ),
];
const GPU_VIDEO_SENSORS: [Sensor; 3] = [
    (
        "radeon-video-encode",
        "Codificação de vídeo da GPU (Radeon)",
        "%",
        PERCENT_AXIS,
    ),
    (
        "radeon-video-decode",
        "Decodificação de vídeo da GPU (Radeon)",
        "%",
        PERCENT_AXIS,
    ),
    (
        "radeon-video-codec",
        "Uso do motor de vídeo da GPU (Radeon)",
        "%",
        PERCENT_AXIS,
    ),
];
const SYSTEM_SENSORS: [Sensor; 4] = [
    ("cpu-usage", "Uso da CPU", "%", PERCENT_AXIS),
    ("ram-used", "RAM usada", " GiB", Axis::Ram),
    ("ram-total", "RAM total", " GiB", Axis::Ram),
    ("ram-available", "RAM disponível", " GiB", Axis::Ram),
];
const FRAME_SENSORS: [Sensor; frames::SENSOR_COUNT] = [
    (
        "present-fps",
        "FPS do aplicativo",
        " FPS",
        Axis::Fixed(DEFAULT_FPS_CEILING as f64),
    ),
    (
        "present-frametime",
        "Intervalo entre quadros (pico)",
        " ms",
        Axis::Fixed(100.0),
    ),
    (
        "present-cpu-wait",
        "Tempo em Present() (pico)",
        " ms",
        Axis::Fixed(100.0),
    ),
];
const CPU_SENSORS: [Sensor; 2] = [
    (
        "cpu-temperature",
        "Temperatura da CPU",
        " °C",
        Axis::Fixed(100.0),
    ),
    (
        "cpu-power",
        "Potência da CPU",
        " W",
        Axis::Fixed(CPU_POWER_AXIS_START),
    ),
];
const GPM_COUNT: usize = GPM_METRICS.len();
const NVIDIA_MEDIA_FIRST: usize = 9;
const GPU_SHARED_MEMORY_INDEX: usize = GPU_SENSORS.len() - 1;
pub(crate) const GPU_ACTIVITY_INDEX: usize = 5;
const GPU_VIDEO_START: usize = GPU_SENSORS.len() + GPM_COUNT;
pub(crate) const CPU_USAGE_INDEX: usize = GPU_VIDEO_START + GPU_VIDEO_SENSORS.len();
pub(crate) const RAM_USED_INDEX: usize = CPU_USAGE_INDEX + 1;
pub(crate) const FRAME_SENSOR_START: usize = CPU_USAGE_INDEX + SYSTEM_SENSORS.len();
pub(crate) const DEFAULT_FPS_CEILING: u16 = 360;
pub(crate) const CPU_TEMPERATURE_INDEX: usize = FRAME_SENSOR_START + FRAME_SENSORS.len();
pub(crate) const CPU_POWER_INDEX: usize = CPU_TEMPERATURE_INDEX + 1;
const GPU: &str = "GPU / NVML";
const RADEON: &str = "GPU / AMD ADLX";
const GPM: &str = "GPM / NVML";
const GPU_VIDEO: &str = "Vídeo da GPU / Windows";
const SYSTEM: &str = "Sistema / Windows";
const FRAMES: &str = "Quadros / DirectX";
const CPU: &str = "CPU / PawnIO";
// A chave persistida independe da tradução e da ordem de exibição do sensor.
type Sensor = (&'static str, &'static str, &'static str, Axis);

#[derive(Clone, Copy)]
enum Axis {
    Fixed(f64),
    Power,
    Vram,
    Ram,
    SharedMemory,
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
    fn detect(device: Option<&GpuSource<'_>>) -> Self {
        let mut limits = FALLBACK_AXES;
        let ram = system::memory_usage()
            .ok()
            .map(|memory| memory.total as f64 / BYTES_PER_GIB);
        limits.ram = axis_limit(ram, limits.ram);

        if let Some(GpuSource::Nvidia(device)) = device {
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
        } else if let Some(GpuSource::Radeon(sampler)) = device {
            limits.vram = axis_limit(sampler.total_vram_mib(), limits.vram);
        }
        limits
    }

    fn max_for(&self, axis: Axis) -> f64 {
        match axis {
            Axis::Fixed(value) => value,
            Axis::Power => self.power,
            Axis::Vram => self.vram,
            Axis::Ram => self.ram,
            Axis::SharedMemory => self.ram * 1024.0,
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
        .chain(GPU_VIDEO_SENSORS)
        .chain(SYSTEM_SENSORS)
        .chain(FRAME_SENSORS)
        .chain(CPU_SENSORS)
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
    pub(crate) gpu_devices: Arc<[GpuInfo]>,
    pub(crate) gpu_selections: Vec<Selection>,
    pub(crate) gpus: Vec<GpuSnapshot>,
    pub(crate) frame_target: FrameTarget,
    pub(crate) frame_status: Option<&'static str>,
    pub(crate) cpu_temperature_status: Option<String>,
    pub(crate) cpu_power_status: Option<String>,
}

pub(crate) struct GpuSnapshot {
    pub(crate) index: GpuIndex,
    pub(crate) metrics: Vec<MetricSnapshot>,
    gpu_video_support: [bool; GPU_VIDEO_SENSORS.len()],
    pub(crate) gpu_video_status: Option<&'static str>,
}

impl GpuSnapshot {
    pub(crate) fn has_sensor(&self, index: usize) -> bool {
        if (GPU_VIDEO_START..CPU_USAGE_INDEX).contains(&index) {
            return self.gpu_video_support[index - GPU_VIDEO_START];
        }
        match self.index {
            GpuIndex::Radeon(_) => {
                source_has_metric(RADEON, index)
                    && !(GPU_SENSORS.len()..GPU_VIDEO_START).contains(&index)
            }
            _ => source_has_metric(GPU, index),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuMetrics {
    pub(crate) temperature: bool,
    pub(crate) power: bool,
}

impl CpuMetrics {
    fn bits(self) -> u8 {
        u8::from(self.temperature) | (u8::from(self.power) << 1)
    }

    fn from_bits(bits: u8) -> Self {
        Self {
            temperature: bits & 1 != 0,
            power: bits & 2 != 0,
        }
    }

    fn any(self) -> bool {
        self.temperature || self.power
    }
}

struct WindowFeed {
    latest: Mutex<Option<Arc<Snapshot>>>,
    requested: AtomicBool,
    interval_ms: AtomicU32,
    gpu_selections: Mutex<Vec<Selection>>,
    frame_target: Mutex<FrameTarget>,
    cpu_metrics: AtomicU8,
}

impl WindowFeed {
    fn gpu_selections(&self) -> Vec<Selection> {
        self.gpu_selections
            .lock()
            .map_or_else(|_| Vec::new(), |selection| selection.clone())
    }

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
        gpus: Vec<Selection>,
        interval_ms: u32,
        cpu_metrics: CpuMetrics,
    ) -> Result<Self, String> {
        if !GUI_INTERVALS_MS.contains(&interval_ms) {
            return Err(format!("intervalo de coleta inválido: {interval_ms} ms"));
        }
        let feed = Arc::new(WindowFeed {
            latest: Mutex::new(None),
            requested: AtomicBool::new(true),
            interval_ms: AtomicU32::new(interval_ms),
            gpu_selections: Mutex::new(gpus),
            frame_target: Mutex::default(),
            cpu_metrics: AtomicU8::new(cpu_metrics.bits()),
        });
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_feed = Arc::clone(&feed);
        let stop = Arc::clone(&stopped);
        let interval = Duration::from_millis(u64::from(interval_ms));
        let options = WatchOptions {
            gpus: Vec::new(),
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

    pub(crate) fn latest_snapshot(&self) -> Option<Arc<Snapshot>> {
        self.feed.latest.lock().ok()?.clone()
    }

    pub(crate) fn gpu_selections(&self) -> Vec<Selection> {
        self.feed.gpu_selections()
    }

    pub(crate) fn set_gpu_selections(&self, selections: Vec<Selection>) {
        if let Ok(mut current) = self.feed.gpu_selections.lock()
            && *current != selections
        {
            *current = selections;
            self.feed.requested.store(true, Ordering::Relaxed);
            self.wake();
        }
    }

    pub(crate) fn set_interval_ms(&self, interval_ms: u32) {
        if GUI_INTERVALS_MS.contains(&interval_ms)
            && self.feed.interval_ms.swap(interval_ms, Ordering::Relaxed) != interval_ms
        {
            self.wake();
        }
    }

    pub(crate) fn set_cpu_metrics(&self, metrics: CpuMetrics) {
        let bits = metrics.bits();
        if self.feed.cpu_metrics.swap(bits, Ordering::Relaxed) != bits {
            self.feed.requested.store(true, Ordering::Relaxed);
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
        self.capture_optional(start, || Some(collect()))
    }

    fn capture_optional(
        &mut self,
        start: Instant,
        collect: impl FnOnce() -> Option<Frame<N>>,
    ) -> Option<(Duration, Frame<N>)> {
        if Instant::now() < self.cadence.deadline() {
            return None;
        }
        let frame = collect();
        let captured = Instant::now();
        self.cadence.complete(captured);
        let frame = frame?;
        self.history.push(
            captured,
            StoredFrame {
                frame,
                interval: self.cadence.interval(),
            },
        );
        Some((captured - start, frame))
    }

    fn summary(
        &mut self,
        end: Instant,
        name: &'static str,
        device: Option<Arc<str>>,
    ) -> HistorySummary {
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
                    .enumerate()
                    .any(|(index, value)| {
                        source_has_metric(name, index) && !matches!(value, Reading::Value(_))
                    })
            })
            .count();
        HistorySummary {
            name,
            count: self.history.len(),
            capacity: self.capacity,
            gaps,
            span,
            device,
        }
    }
}

struct HistorySummary {
    name: &'static str,
    count: usize,
    capacity: usize,
    gaps: usize,
    span: Duration,
    device: Option<Arc<str>>,
}

struct FrameMonitor {
    pid: u32,
    sampler: FrameSampler,
    group: Group<{ FRAME_SENSORS.len() }>,
    status: Option<&'static str>,
}

#[derive(Default)]
struct DashboardReadings {
    gpu_devices: Arc<[GpuInfo]>,
    gpu_selections: Vec<Selection>,
    frames: Option<FrameMonitor>,
    frame_target: FrameTarget,
    cpu: Option<CpuMonitor>,
}

enum GpuSource<'a> {
    Nvidia(NvmlDevice<'a>),
    Radeon(adlx::Sampler<'a>),
}

impl GpuSource<'_> {
    fn label(&self) -> &'static str {
        match self {
            Self::Nvidia(_) => GPU,
            Self::Radeon(_) => RADEON,
        }
    }

    fn interval(&self, requested: Duration) -> Duration {
        match self {
            Self::Nvidia(_) => requested,
            Self::Radeon(sampler) => requested.max(sampler.interval()),
        }
    }

    fn sample(&mut self) -> Option<Frame<{ GPU_SENSORS.len() }>> {
        match self {
            Self::Nvidia(device) => Some(read_gpu(device).into()),
            Self::Radeon(sampler) => match sampler.read() {
                Ok(Some(sample)) => Some(read_radeon(sample, sampler.total_vram_mib()).into()),
                Ok(None) => None,
                Err(_) => Some([Reading::Failed; GPU_SENSORS.len()].into()),
            },
        }
    }
}

struct GpuMonitor<'a> {
    index: GpuIndex,
    label: Arc<str>,
    source: GpuSource<'a>,
    group: Group<{ GPU_SENSORS.len() }>,
    gpm: Option<Group<GPM_COUNT>>,
    gpm_source: GpmSource<'a>,
    video: Option<VideoMonitor>,
    limits: AxisLimits,
}

struct VideoMonitor {
    source: Option<gpu_video::Sampler>,
    group: Group<{ GPU_VIDEO_SENSORS.len() }>,
}

impl VideoMonitor {
    fn capture(
        &mut self,
        start: Instant,
    ) -> Option<(Duration, Frame<{ GPU_VIDEO_SENSORS.len() }>)> {
        self.group.capture(start, || {
            let sample = self
                .source
                .as_mut()
                .map_or_else(gpu_video::Sample::default, |source| source.read());
            [
                sample.encode_percent,
                sample.decode_percent,
                sample.codec_percent,
            ]
            .map(|value| value.map_or(Reading::Unavailable, Reading::Value))
            .into()
        })
    }
}

impl<'a> GpuMonitor<'a> {
    fn open(
        inventory: &'a Inventory,
        device: &GpuInfo,
        options: &WatchOptions,
        gui: bool,
    ) -> Result<Self, String> {
        let now = Instant::now();
        let fastest = |interval| if gui { FASTEST_GUI_INTERVAL } else { interval };
        let (source, video) = match inventory.open(device.index)? {
            gpu::Device::Nvidia(device) => (GpuSource::Nvidia(device), None),
            gpu::Device::Radeon(device) => {
                let luid = device.adapter_luid();
                let source = GpuSource::Radeon(device.start()?);
                let video_source = luid.and_then(gpu_video::Sampler::open);
                let video_interval = options.gpu_interval.max(VIDEO_MIN_INTERVAL);
                let video = VideoMonitor {
                    source: video_source,
                    group: Group::new(
                        Instant::now() + video_interval,
                        video_interval,
                        fastest(options.gpu_interval).max(VIDEO_MIN_INTERVAL),
                    ),
                };
                (source, Some(video))
            }
        };
        let limits = AxisLimits::detect(Some(&source));
        let group = Group::new(
            now,
            source.interval(options.gpu_interval),
            source.interval(fastest(options.gpu_interval)),
        );
        let gpm = matches!(source, GpuSource::Nvidia(_)).then(|| {
            Group::new(
                now,
                options.gpm_interval,
                fastest(options.gpm_interval).max(GPM_MIN_INTERVAL),
            )
        });
        Ok(Self {
            index: device.index,
            label: Arc::from(device.label()),
            source,
            group,
            gpm,
            gpm_source: GpmSource::Discover,
            video,
            limits,
        })
    }

    fn set_interval(&mut self, interval: Duration, gpm_interval: Duration) {
        self.group
            .cadence
            .set_interval(self.source.interval(interval));
        if let Some(gpm) = &mut self.gpm {
            gpm.cadence.set_interval(gpm_interval);
        }
        if let Some(video) = &mut self.video {
            video
                .group
                .cadence
                .set_interval(interval.max(VIDEO_MIN_INTERVAL));
        }
    }
}

struct CpuMonitor {
    source: Option<CpuSensor>,
    group: Group<{ CPU_SENSORS.len() }>,
    retry_at: Instant,
    status: [Option<String>; CPU_SENSORS.len()],
    power_axis_max: f64,
}

impl CpuMonitor {
    fn new(start: Instant, interval: Duration) -> Self {
        Self {
            source: None,
            group: Group::new(start, interval, FASTEST_GUI_INTERVAL),
            retry_at: start,
            status: [None, None],
            power_axis_max: CPU_POWER_AXIS_START,
        }
    }

    fn collect(&mut self, start: Instant, metrics: CpuMetrics) -> bool {
        self.group
            .capture(start, || {
                let now = Instant::now();
                if self.source.is_none() && now >= self.retry_at {
                    self.retry_at = now + CPU_SENSOR_RETRY;
                    match CpuSensor::open() {
                        Ok(source) => self.source = Some(source),
                        Err(error) => {
                            let reason = cpu_sensor_error(&error);
                            self.status = [Some(reason.clone()), Some(reason)];
                        }
                    }
                }
                let enabled = [metrics.temperature, metrics.power];
                let readings = match self
                    .source
                    .as_mut()
                    .map(|source| source.sample(metrics.temperature, metrics.power))
                {
                    Some(Ok(samples)) => {
                        let [temperature, power] = &mut self.status;
                        [
                            read_cpu_metric(samples.temperature, temperature),
                            read_cpu_metric(samples.power, power),
                        ]
                    }
                    Some(Err(error)) => {
                        self.status = std::array::from_fn(|index| {
                            enabled[index].then(|| cpu_sensor_error(&error))
                        });
                        [cpu_reading_error(&error); 2]
                    }
                    None => [Reading::Unavailable; 2],
                };
                if let Reading::Value(watts) = readings[1] {
                    self.power_axis_max = self.power_axis_max.max((watts / 25.0).ceil() * 25.0);
                }
                std::array::from_fn(|index| {
                    if enabled[index] {
                        readings[index]
                    } else {
                        self.status[index] = None;
                        Reading::Pending
                    }
                })
                .into()
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
    Gpu(
        Duration,
        Arc<str>,
        &'static str,
        Frame<{ GPU_SENSORS.len() }>,
    ),
    Gpm(Duration, Arc<str>, Frame<GPM_COUNT>),
    GpuVideo(Duration, Arc<str>, Frame<{ GPU_VIDEO_SENSORS.len() }>),
    System(Duration, Frame<{ SYSTEM_SENSORS.len() }>),
    Finished {
        elapsed: Duration,
        groups: Vec<HistorySummary>,
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
    let inventory = Inventory::discover();
    let selections = window.map_or_else(
        || options.gpus.iter().copied().map(Selection::Index).collect(),
        |feed| feed.gpu_selections(),
    );
    let mut gpus = Vec::new();
    let mut notice = reconcile_gpus(
        &mut gpus,
        &inventory,
        &selections,
        &options,
        window.is_some(),
    );
    if console.is_some() && !selections.is_empty() && gpus.is_empty() {
        return Err(notice);
    }
    if let Some(sender) = console {
        let _ = sender.send(Update::Notice(notice.clone()));
    }
    let base_limits = AxisLimits::detect(None);

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
    let mut dashboard = DashboardReadings {
        gpu_devices: Arc::from(inventory.devices()),
        gpu_selections: selections,
        cpu: window
            .as_ref()
            .map(|_| CpuMonitor::new(start, options.system_interval)),
        ..Default::default()
    };
    let mut skipped_updates = 0;
    let mut snapshot_due = start;
    let mut snapshot_pending = true;

    while !stopped.load(Ordering::Relaxed) && end.is_none_or(|end| Instant::now() < end) {
        if let Some(feed) = window {
            let interval =
                Duration::from_millis(u64::from(feed.interval_ms.load(Ordering::Relaxed)));
            if interval != options.gpu_interval {
                options.gpu_interval = interval;
                options.system_interval = interval;
                options.gpm_interval = interval.max(GPM_MIN_INTERVAL);
                system.cadence.set_interval(interval);
                for gpu in &mut gpus {
                    gpu.set_interval(interval, options.gpm_interval);
                }
                if let Some(frames) = &mut dashboard.frames {
                    frames.group.cadence.set_interval(interval);
                }
                if let Some(cpu) = &mut dashboard.cpu {
                    cpu.group.cadence.set_interval(interval);
                }
                snapshot_pending = true;
            }
            let selected = feed.gpu_selections();
            if dashboard.gpu_selections != selected {
                notice = reconcile_gpus(&mut gpus, &inventory, &selected, &options, true);
                dashboard.gpu_selections = selected;
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
        for gpu in &mut gpus {
            if let Some((elapsed, frame)) =
                gpu.group.capture_optional(start, || gpu.source.sample())
            {
                if let Some(sender) = console {
                    publish(
                        sender,
                        Update::Gpu(elapsed, Arc::clone(&gpu.label), gpu.source.label(), frame),
                        &mut skipped_updates,
                    );
                } else {
                    snapshot_pending = true;
                }
            }
            if let GpuSource::Nvidia(device) = &gpu.source
                && let Some(gpm) = &mut gpu.gpm
                && let Some((elapsed, frame)) =
                    gpm.capture(start, || gpu.gpm_source.read(device, options.gpm_interval))
            {
                if let Some(sender) = console {
                    publish(
                        sender,
                        Update::Gpm(elapsed, Arc::clone(&gpu.label), frame),
                        &mut skipped_updates,
                    );
                } else {
                    snapshot_pending = true;
                }
            }
            if let Some(video) = &mut gpu.video
                && let Some((elapsed, frame)) = video.capture(start)
            {
                if let Some(sender) = console {
                    publish(
                        sender,
                        Update::GpuVideo(elapsed, Arc::clone(&gpu.label), frame),
                        &mut skipped_updates,
                    );
                } else {
                    snapshot_pending = true;
                }
            }
        }

        let cpu_metrics = window.map_or_else(CpuMetrics::default, |feed| {
            CpuMetrics::from_bits(feed.cpu_metrics.load(Ordering::Relaxed))
        });
        if let Some(cpu) = &mut dashboard.cpu {
            if cpu_metrics.any() {
                snapshot_pending |= cpu.collect(start, cpu_metrics);
            } else {
                cpu.source = None;
                cpu.status = [None, None];
            }
        }
        let mut next = system.cadence.deadline();
        for gpu in &gpus {
            next = next.min(gpu.group.cadence.deadline());
            if let Some(gpm) = &gpu.gpm {
                next = next.min(gpm.cadence.deadline());
            }
            if let Some(video) = &gpu.video {
                next = next.min(video.group.cadence.deadline());
            }
        }
        if let Some(frames) = &dashboard.frames {
            next = next.min(frames.group.cadence.deadline());
        }
        if cpu_metrics.any()
            && let Some(cpu) = &dashboard.cpu
        {
            next = next.min(cpu.group.cadence.deadline());
        }
        if let Some(end) = end {
            next = next.min(end);
        }
        if let Some(feed) =
            window.filter(|feed| snapshot_pending && feed.requested.load(Ordering::Relaxed))
        {
            let now = Instant::now();
            if now >= snapshot_due {
                feed.requested.store(false, Ordering::Relaxed);
                let snapshot =
                    build_snapshot(now, &notice, &system, &gpus, &dashboard, &base_limits);
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
        let mut groups = Vec::new();
        for mut gpu in gpus {
            groups.push(gpu.group.summary(
                finished,
                gpu.source.label(),
                Some(Arc::clone(&gpu.label)),
            ));
            if let Some(mut gpm) = gpu.gpm {
                groups.push(gpm.summary(finished, GPM, Some(Arc::clone(&gpu.label))));
            }
            if let Some(mut video) = gpu.video {
                groups.push(video.group.summary(finished, GPU_VIDEO, Some(gpu.label)));
            }
        }
        groups.push(system.summary(finished, SYSTEM, None));
        let _ = sender.send(Update::Finished {
            elapsed: finished - start,
            groups,
            skipped_updates,
        });
    }
    Ok(())
}

fn build_snapshot(
    at: Instant,
    notice: &str,
    system: &Group<{ SYSTEM_SENSORS.len() }>,
    gpus: &[GpuMonitor<'_>],
    dashboard: &DashboardReadings,
    base_limits: &AxisLimits,
) -> Arc<Snapshot> {
    let interval = system.cadence.interval();
    let mut metrics = gpu_metrics(at, None, interval, base_limits);
    let axis_limits = base_limits;
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
    let cpu = dashboard.cpu.as_ref();
    append_metrics(
        &mut metrics,
        at,
        cpu.map(|monitor| &monitor.group),
        &CPU_SENSORS,
        CPU,
        interval,
        axis_limits,
    );
    if let Some(cpu) = cpu {
        metrics[CPU_POWER_INDEX].axis_max = cpu.power_axis_max;
    } else {
        for metric in &mut metrics[CPU_TEMPERATURE_INDEX..] {
            metric.points.push(MetricPoint {
                at,
                reading: Reading::Unavailable,
                interval,
            });
        }
    }
    let [cpu_temperature_status, cpu_power_status] =
        cpu.map_or([None, None], |cpu| cpu.status.clone());
    Arc::new(Snapshot {
        at,
        notice: notice.into(),
        metrics,
        gpu_devices: Arc::clone(&dashboard.gpu_devices),
        gpu_selections: dashboard.gpu_selections.clone(),
        gpus: gpus
            .iter()
            .map(|gpu| gpu_snapshot(at, gpu, interval, base_limits))
            .collect(),
        frame_target: dashboard.frame_target,
        frame_status: frames.and_then(|frames| frames.status).or_else(|| {
            (frames.is_none() && dashboard.frame_target == FrameTarget::Foreground)
                .then_some("Aguardando um aplicativo em foco")
        }),
        cpu_temperature_status,
        cpu_power_status,
    })
}

fn gpu_snapshot(
    at: Instant,
    gpu: &GpuMonitor<'_>,
    interval: Duration,
    base_limits: &AxisLimits,
) -> GpuSnapshot {
    let video = gpu.video.as_ref();
    let video_source = video.and_then(|video| video.source.as_ref());
    let gpu_video_support = video_source.map_or([false; GPU_VIDEO_SENSORS.len()], |source| {
        let support = source.support();
        [support.encode, support.decode, support.codec]
    });
    let gpu_video_status = video.and_then(|_| match video_source {
        None => Some("Contadores de vídeo Radeon indisponíveis"),
        Some(_) if !gpu_video_support.contains(&true) => {
            Some("Aguardando o Windows reportar os motores de vídeo Radeon")
        }
        Some(_) => None,
    });
    GpuSnapshot {
        index: gpu.index,
        metrics: gpu_metrics(at, Some(gpu), interval, base_limits),
        gpu_video_support,
        gpu_video_status,
    }
}

fn gpu_metrics(
    at: Instant,
    gpu: Option<&GpuMonitor<'_>>,
    interval: Duration,
    base_limits: &AxisLimits,
) -> Vec<MetricSnapshot> {
    let axis_limits = gpu.map_or(base_limits, |gpu| &gpu.limits);
    let mut metrics = Vec::with_capacity(CPU_USAGE_INDEX);
    append_metrics(
        &mut metrics,
        at,
        gpu.map(|gpu| &gpu.group),
        &GPU_SENSORS,
        gpu.map_or(GPU, |gpu| gpu.source.label()),
        gpu.map_or(interval, |gpu| gpu.group.cadence.interval()),
        axis_limits,
    );
    append_metrics(
        &mut metrics,
        at,
        gpu.and_then(|gpu| gpu.gpm.as_ref()),
        &gpm_sensors(),
        GPM,
        interval.max(GPM_MIN_INTERVAL),
        axis_limits,
    );
    let video = gpu.and_then(|gpu| gpu.video.as_ref());
    let video_group = video.map(|video| &video.group);
    let video_interval = video_group.map_or(interval.max(VIDEO_MIN_INTERVAL), |group| {
        group.cadence.interval()
    });
    append_metrics(
        &mut metrics,
        at,
        video_group,
        &GPU_VIDEO_SENSORS,
        GPU_VIDEO,
        video_interval,
        axis_limits,
    );
    if video.is_some() {
        for metric in &mut metrics[GPU_SENSORS.len()..GPU_VIDEO_START] {
            metric.points.push(MetricPoint {
                at,
                reading: Reading::Unavailable,
                interval,
            });
        }
    } else {
        for metric in &mut metrics[GPU_VIDEO_START..CPU_USAGE_INDEX] {
            metric.points.push(MetricPoint {
                at,
                reading: Reading::Unavailable,
                interval: video_interval,
            });
        }
    }
    metrics
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

fn reconcile_gpus<'a>(
    gpus: &mut Vec<GpuMonitor<'a>>,
    inventory: &'a Inventory,
    selections: &[Selection],
    options: &WatchOptions,
    gui: bool,
) -> String {
    let mut errors = Vec::new();
    let automatic = selections.is_empty();
    let candidates: Vec<_> = if automatic {
        inventory.devices().iter().collect()
    } else {
        selections
            .iter()
            .filter_map(|selection| match inventory.resolve(selection) {
                Ok(device) => device,
                Err(error) => {
                    errors.push(error);
                    None
                }
            })
            .collect()
    };
    gpus.retain(|gpu| candidates.iter().any(|device| device.index == gpu.index));
    let mut active = Vec::new();
    for device in candidates {
        if active.contains(&device.index) {
            continue;
        }
        if !gpus.iter().any(|gpu| gpu.index == device.index) {
            match GpuMonitor::open(inventory, device, options, gui) {
                Ok(gpu) => gpus.push(gpu),
                Err(error) => {
                    errors.push(format!("{} · {error}", device.label()));
                    continue;
                }
            }
        }
        active.push(device.index);
        if automatic {
            break;
        }
    }
    gpus.retain(|gpu| active.contains(&gpu.index));
    gpus.sort_by_key(|gpu| active.iter().position(|&index| index == gpu.index));
    let labels = gpus
        .iter()
        .map(|gpu| gpu.label.as_ref())
        .collect::<Vec<_>>();
    match (labels.is_empty(), errors.is_empty()) {
        (true, true) => "Nenhuma GPU compatível encontrada.".into(),
        (true, false) => format!("Coleta de GPU indisponível ({})", errors.join(" · ")),
        (false, true) => labels.join(" | "),
        (false, false) => format!(
            "{} | Indisponíveis: {}",
            labels.join(" | "),
            errors.join(" · ")
        ),
    }
}

fn publish(sender: &SyncSender<Update>, update: Update, skipped_updates: &mut u64) {
    if let Err(TrySendError::Full(_)) = sender.try_send(update) {
        *skipped_updates += 1;
    }
}

fn read_gpu(device: &NvmlDevice<'_>) -> [Reading; GPU_SENSORS.len()] {
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
        Reading::Unavailable,
    ]
}

fn read_radeon(sample: adlx::Sample, total: Option<f64>) -> [Reading; GPU_SENSORS.len()] {
    let free = total
        .zip(sample.vram_used_mib)
        .and_then(|(total, used)| (used >= 0.0 && used <= total).then_some(total - used));
    let value = |number: Option<f64>| {
        number
            .filter(|number| number.is_finite())
            .map_or(Reading::Unavailable, Reading::Value)
    };
    [
        value(sample.temperature),
        value(sample.board_power_watts),
        value(sample.vram_used_mib),
        value(total),
        value(free),
        value(sample.usage_percent),
        Reading::Unavailable,
        value(sample.graphics_clock_mhz),
        value(sample.memory_clock_mhz),
        Reading::Unavailable,
        Reading::Unavailable,
        value(sample.shared_memory_mib),
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
    fn read(&mut self, device: &NvmlDevice<'a>, interval: Duration) -> Frame<GPM_COUNT> {
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
    let [used, total, available] = match system::memory_usage() {
        Ok(memory) => [
            Reading::Value(memory.used as f64 / BYTES_PER_GIB),
            Reading::Value(memory.total as f64 / BYTES_PER_GIB),
            Reading::Value((memory.total - memory.used) as f64 / BYTES_PER_GIB),
        ],
        Err(_) => [Reading::Failed; 3],
    };
    [cpu, used, total, available]
}

fn read_cpu_metric(result: io::Result<Option<f64>>, status: &mut Option<String>) -> Reading {
    match result {
        Ok(Some(value)) => {
            *status = None;
            Reading::Value(value)
        }
        Ok(None) => {
            *status = None;
            Reading::Pending
        }
        Err(error) => {
            *status = Some(cpu_sensor_error(&error));
            cpu_reading_error(&error)
        }
    }
}

fn cpu_reading_error(error: &io::Error) -> Reading {
    if error.kind() == io::ErrorKind::Unsupported {
        Reading::Unavailable
    } else {
        Reading::Failed
    }
}

fn cpu_sensor_error(error: &io::Error) -> String {
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
            Update::Gpu(elapsed, device, source, frame) => report_frame(
                &mut out,
                source,
                Some(&device),
                elapsed,
                &GPU_SENSORS,
                frame,
            )?,
            Update::Gpm(elapsed, device, frame) => {
                report_frame(&mut out, GPM, Some(&device), elapsed, &gpm_sensors, frame)?
            }
            Update::GpuVideo(elapsed, device, frame) => report_frame(
                &mut out,
                GPU_VIDEO,
                Some(&device),
                elapsed,
                &GPU_VIDEO_SENSORS,
                frame,
            )?,
            Update::System(elapsed, frame) => {
                report_frame(&mut out, SYSTEM, None, elapsed, &SYSTEM_SENSORS, frame)?
            }
            Update::Finished {
                elapsed,
                groups,
                skipped_updates,
            } => {
                writeln!(out, "Coleta encerrada após {:.2} s.", elapsed.as_secs_f64())?;
                for summary in groups {
                    let device = summary
                        .device
                        .as_deref()
                        .map_or(String::new(), |label| format!(" · {label}"));
                    writeln!(
                        out,
                        "Histórico {}{device}: {}/{} amostras, {} com lacunas, {:.2} s entre a primeira e a última.",
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

fn source_has_metric(source: &str, index: usize) -> bool {
    match source {
        GPU => index != GPU_SHARED_MEMORY_INDEX,
        RADEON => !(NVIDIA_MEDIA_FIRST..GPU_SHARED_MEMORY_INDEX).contains(&index),
        _ => true,
    }
}

fn report_frame<const N: usize>(
    out: &mut impl Write,
    group: &str,
    device: Option<&str>,
    elapsed: Duration,
    sensors: &[Sensor; N],
    frame: Frame<N>,
) -> io::Result<()> {
    write!(out, "[{:.3} s] {group}", elapsed.as_secs_f64())?;
    if let Some(device) = device {
        write!(out, " · {device}")?;
    }
    if let Some(window) = frame.window {
        write!(out, " ({:.1} ms)", window.as_secs_f64() * 1_000.0)?;
    }
    for (index, ((_, label, unit, _), value)) in sensors.iter().zip(frame.values).enumerate() {
        if !source_has_metric(group, index) {
            continue;
        }
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

        let summary = group.summary(Instant::now(), "teste", None);
        assert_eq!(summary.count, 2);
        assert_eq!(summary.gaps, 1);
    }

    #[test]
    fn cpu_metrics_keep_prior_sample_interval_after_refresh_change() {
        let start = Instant::now();
        let initial = Duration::from_millis(500);
        let refresh = Duration::from_millis(100);
        let mut cpu = CpuMonitor::new(start, initial);
        assert!(
            cpu.group
                .capture(start, || [Reading::Value(56.875), Reading::Value(48.5)]
                    .into())
                .is_some()
        );
        cpu.group.cadence.set_interval(refresh);
        let dashboard = DashboardReadings {
            cpu: Some(cpu),
            ..Default::default()
        };
        let system = Group::new(start, refresh, FASTEST_GUI_INTERVAL);
        let snapshot = build_snapshot(Instant::now(), "", &system, &[], &dashboard, &FALLBACK_AXES);
        let metric = &snapshot.metrics[CPU_TEMPERATURE_INDEX];
        assert_eq!(metric.interval, refresh);
        let [point] = metric.points.as_slice() else {
            panic!("o histórico da temperatura foi perdido");
        };
        assert_eq!(point.interval, initial);
        assert!(matches!(point.reading, Reading::Value(56.875)));
        let power = &snapshot.metrics[CPU_POWER_INDEX];
        assert_eq!(power.name, "Potência da CPU");
        assert_eq!(power.unit, " W");
        assert_eq!(power.interval, refresh);
        assert!(matches!(
            power.points.as_slice(),
            [MetricPoint { reading: Reading::Value(48.5), interval, .. }] if *interval == initial
        ));
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
                frame: [Reading::Value(12.0); SYSTEM_SENSORS.len()].into(),
                interval,
            },
        );
        system.history.push(
            start + interval,
            StoredFrame {
                frame: [
                    Reading::Value(0.0),
                    Reading::Failed,
                    Reading::Value(32.0),
                    Reading::Value(20.0),
                ]
                .into(),
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
            &[],
            &DashboardReadings::default(),
            &limits,
        );
        assert_eq!(
            snapshot.metrics.len(),
            CPU_TEMPERATURE_INDEX + CPU_SENSORS.len()
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
        assert_eq!(snapshot.metrics[1].name, "Potência da GPU");
        assert_eq!(snapshot.metrics[GPU_SENSORS.len()].source, GPM);
        assert!(
            snapshot.metrics[..GPU_VIDEO_START]
                .iter()
                .all(|metric| metric.points.is_empty())
        );
        assert!(matches!(
            snapshot.metrics[CPU_USAGE_INDEX].points.as_slice(),
            [MetricPoint { at, reading: Reading::Value(0.0), interval: sample_interval }]
                if *at == start + interval && *sample_interval == prior_interval
        ));
        assert!(matches!(
            snapshot.metrics[RAM_USED_INDEX].points.as_slice(),
            [MetricPoint {
                reading: Reading::Failed,
                ..
            }]
        ));
        assert_eq!(snapshot.metrics[CPU_USAGE_INDEX].interval, interval);
        let ram_available = &snapshot.metrics[FRAME_SENSOR_START - 1];
        assert_eq!(ram_available.name, "RAM disponível");
        assert_eq!(ram_available.unit, " GiB");
        assert!(matches!(
            ram_available.points.as_slice(),
            [MetricPoint { at, reading: Reading::Value(20.0), interval: sample_interval }]
                if *at == start + interval && *sample_interval == prior_interval
        ));
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
            snapshot.metrics[RAM_USED_INDEX..FRAME_SENSOR_START]
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
    fn radeon_keeps_shared_memory_separate_and_requires_both_vram_counters() {
        let sample = adlx::Sample {
            temperature: Some(34.0),
            vram_used_mib: None,
            shared_memory_mib: Some(256.0),
            ..Default::default()
        };
        let readings = read_radeon(sample, Some(512.0));
        assert!(matches!(readings[0], Reading::Value(34.0)));
        assert!(matches!(readings[1], Reading::Unavailable));
        assert!(matches!(
            readings[2..5],
            [
                Reading::Unavailable,
                Reading::Value(512.0),
                Reading::Unavailable
            ]
        ));
        assert!(matches!(
            readings[GPU_SHARED_MEMORY_INDEX],
            Reading::Value(256.0)
        ));
        let with_used = adlx::Sample {
            vram_used_mib: Some(128.0),
            ..sample
        };
        assert!(matches!(
            read_radeon(with_used, Some(512.0))[4],
            Reading::Value(384.0)
        ));
    }

    #[test]
    #[ignore = "requires a working NVIDIA and Radeon device"]
    fn monitors_two_physical_gpus_and_preserves_the_remaining_history() {
        let nvidia = GpuIndex::Nvidia(0);
        let radeon = GpuIndex::Radeon(0);
        let monitor = Monitor::start(Vec::new(), 1_000, CpuMetrics::default()).unwrap();
        monitor.set_frame_target(FrameTarget::Off);
        let wait_for_gpus = |indices: &[GpuIndex]| {
            let until = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(snapshot) = monitor.snapshot()
                    && snapshot.gpus.len() == indices.len()
                    && snapshot.gpus.iter().zip(indices).all(|(gpu, index)| {
                        gpu.index == *index
                            && gpu.metrics[0]
                                .points
                                .iter()
                                .any(|point| matches!(point.reading, Reading::Value(_)))
                    })
                {
                    break snapshot;
                }
                assert!(Instant::now() < until, "GPUs sem amostras: {indices:?}");
                thread::sleep(Duration::from_millis(50));
            }
        };
        let first = wait_for_gpus(&[nvidia]);
        let first_nvidia = first.gpus[0].metrics[0].points[0].at;
        let first_system = first.metrics[RAM_USED_INDEX].points[0].at;
        let added_at = Instant::now();
        monitor.set_gpu_selections(vec![Selection::Index(nvidia), Selection::Index(radeon)]);
        let both = wait_for_gpus(&[nvidia, radeon]);
        assert_eq!(both.gpus[0].metrics[0].source, GPU);
        assert_eq!(both.gpus[1].metrics[0].source, RADEON);
        assert_eq!(both.gpus[0].metrics[0].points[0].at, first_nvidia);
        assert!(both.gpus[1].metrics[0].points[0].at >= added_at);
        let first_radeon = both.gpus[1].metrics[0].points[0].at;
        assert!(!both.gpus[1].has_sensor(NVIDIA_MEDIA_FIRST));
        assert!(!both.gpus[0].has_sensor(GPU_VIDEO_START));
        monitor.set_gpu_selections(vec![Selection::Index(radeon)]);
        let remaining = wait_for_gpus(&[radeon]);
        assert_eq!(remaining.gpus[0].metrics[0].points[0].at, first_radeon);
        assert_eq!(remaining.metrics[RAM_USED_INDEX].points[0].at, first_system);
        monitor.set_gpu_selections(vec![Selection::Index(radeon), Selection::Index(nvidia)]);
        let again = wait_for_gpus(&[radeon, nvidia]);
        assert_eq!(again.gpus[0].metrics[0].points[0].at, first_radeon);
        assert!(again.gpus[1].metrics[0].points[0].at > first_nvidia);
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
