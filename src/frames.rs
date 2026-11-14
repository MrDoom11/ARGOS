mod etw;
mod process;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;

use self::etw::{Capture, PresentEvent, PresentKind, Runtime};
pub(crate) use self::process::{
    Process, Target as ProcessTarget, foreground_pid, process_name, visible_processes,
};

pub(crate) const SENSOR_COUNT: usize = 3;
const MAX_PENDING: usize = 64;
const MAX_STREAMS: usize = 16;
const MAX_TIMINGS: usize = 2048;
const MAX_EVENTS_PER_READ: usize = 8192;
const CAPTURE_RETRY: Duration = Duration::from_secs(5);
const ACTIVE: &str = "Capturando apresentações DirectX";
const WAITING: &str = "Aguardando apresentações DirectX do processo";
const ACCESS_DENIED: &str = "Captura requer administrador ou Performance Log Users";
const CAPTURE_ENDED: &str = "Captura interrompida. Tentando reconectar";
const PROCESS_ENDED: &str = "O processo selecionado foi encerrado";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FrameTarget {
    Off,
    #[default]
    Foreground,
    Process(u32),
}

impl FrameTarget {
    pub(crate) fn resolve(
        self,
        previous_foreground: Option<u32>,
        foreground: impl FnOnce() -> Option<u32>,
    ) -> Option<u32> {
        match self {
            Self::Off => None,
            // Keep the last external app while ARGOS has focus or Windows transfers it.
            Self::Foreground => foreground().or(previous_foreground),
            Self::Process(pid) => Some(pid),
        }
    }
}

pub(crate) enum Sample {
    Measured([Option<f64>; SENSOR_COUNT]),
    Waiting(&'static str),
    Unavailable(CaptureFailure),
}

#[derive(Clone, Copy)]
pub(crate) enum CaptureFailure {
    Permission,
    Retryable(&'static str),
    Other(&'static str),
}

impl CaptureFailure {
    fn from_trace_error(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::PermissionDenied => Self::Permission,
            io::ErrorKind::AlreadyExists => {
                Self::Retryable("Outra instância do ARGOS já está capturando quadros")
            }
            _ if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32) => Self::Permission,
            _ => Self::Other("Windows não conseguiu iniciar a captura de quadros"),
        }
    }
}

pub(crate) struct FrameSampler {
    pid: u32,
    target: Option<ProcessTarget>,
    capture: Result<ActiveCapture, CaptureFailure>,
    retry_at: Option<Instant>,
}

struct ActiveCapture {
    capture: Capture,
    presents: Presents,
}

impl FrameSampler {
    pub(crate) fn start(pid: u32) -> Self {
        let target = ProcessTarget::open(pid).ok();
        let capture = if target.is_some() {
            ActiveCapture::start(pid)
        } else {
            Err(CaptureFailure::Other("Processo indisponível para captura"))
        };
        let retry_at = retry_deadline(&capture, Instant::now());
        Self {
            pid,
            target,
            capture,
            retry_at,
        }
    }

    pub(crate) fn sample(&mut self, interval: Duration) -> Sample {
        if self
            .target
            .as_ref()
            .is_some_and(|target| !target.is_running())
        {
            self.target = None;
            self.capture = Err(CaptureFailure::Other(PROCESS_ENDED));
            self.retry_at = None;
        }
        if self.retry_at.is_some_and(|at| Instant::now() >= at) {
            self.capture = ActiveCapture::start(self.pid);
            self.retry_at = retry_deadline(&self.capture, Instant::now());
        }
        match &mut self.capture {
            Ok(capture) => match capture.sample(interval) {
                Ok(sample) => sample,
                Err(failure) => {
                    self.capture = Err(failure);
                    self.retry_at = retry_deadline(&self.capture, Instant::now());
                    Sample::Unavailable(failure)
                }
            },
            Err(failure) => Sample::Unavailable(*failure),
        }
    }
}

fn retry_deadline(
    capture: &Result<ActiveCapture, CaptureFailure>,
    now: Instant,
) -> Option<Instant> {
    matches!(capture, Err(CaptureFailure::Retryable(_))).then_some(now + CAPTURE_RETRY)
}

impl Sample {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Measured(_) => ACTIVE,
            Self::Waiting(reason)
            | Self::Unavailable(
                CaptureFailure::Retryable(reason) | CaptureFailure::Other(reason),
            ) => reason,
            Self::Unavailable(CaptureFailure::Permission) => ACCESS_DENIED,
        }
    }

    pub(crate) fn requires_admin(&self) -> bool {
        matches!(self, Self::Unavailable(CaptureFailure::Permission))
    }
}

impl ActiveCapture {
    fn start(pid: u32) -> Result<Self, CaptureFailure> {
        let capture = Capture::start(pid).map_err(CaptureFailure::from_trace_error)?;
        let presents = Presents::new(capture.ticks_per_second());
        Ok(Self { capture, presents })
    }

    fn sample(&mut self, interval: Duration) -> Result<Sample, CaptureFailure> {
        let mut lost = self.capture.take_loss();
        for _ in 0..MAX_EVENTS_PER_READ {
            match self.capture.try_recv() {
                Ok(event) if !lost => self.presents.record(event),
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(CaptureFailure::Retryable(CAPTURE_ENDED));
                }
            }
        }
        lost |= self.capture.take_loss();
        if lost {
            self.presents.clear();
            return Ok(Sample::Waiting(
                "Eventos perdidos. Aguardando novas apresentações",
            ));
        }
        let mut now = 0;
        // SAFETY: `now` is a writable LARGE_INTEGER for the duration of the call.
        if unsafe { QueryPerformanceCounter(&mut now) } == 0 {
            return Err(CaptureFailure::Other("Relógio de captura indisponível"));
        }
        Ok(self
            .presents
            .measure(now as u64, interval)
            .map_or(Sample::Waiting(WAITING), Sample::Measured))
    }
}

#[derive(Clone, Copy)]
struct Start {
    swap_chain: u64,
    tick: u64,
}

struct Timing {
    start: u64,
    stop: u64,
    interval: Option<u64>,
}

type Stream = (Runtime, u64);

struct Presents {
    frequency: u64,
    pending: HashMap<(Runtime, u32), Start>,
    streams: HashMap<Stream, VecDeque<Timing>>,
    selected: Option<Stream>,
}

impl Presents {
    fn new(frequency: u64) -> Self {
        Self {
            frequency,
            pending: HashMap::new(),
            streams: HashMap::new(),
            selected: None,
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.streams.clear();
        self.selected = None;
    }

    fn record(&mut self, event: PresentEvent) {
        let thread = (event.runtime, event.thread_id);
        match event.kind {
            PresentKind::Start { swap_chain, flags } => {
                self.pending.remove(&thread);
                // DXGI_PRESENT_TEST checks status without presenting a frame.
                if swap_chain != 0 && !(event.runtime == Runtime::Dxgi && flags & 1 != 0) {
                    if self.pending.len() == MAX_PENDING {
                        self.pending.clear();
                    }
                    self.pending.insert(
                        thread,
                        Start {
                            swap_chain,
                            tick: event.tick,
                        },
                    );
                }
            }
            PresentKind::Stop { result } => {
                if let Some(start) = self.pending.remove(&thread)
                    && result == 0
                    && event.tick >= start.tick
                {
                    self.record_timing((event.runtime, start.swap_chain), start.tick, event.tick);
                }
            }
        }
    }

    fn record_timing(&mut self, stream: Stream, start: u64, stop: u64) {
        if self.streams.len() == MAX_STREAMS
            && !self.streams.contains_key(&stream)
            && let Some((&oldest, _)) = self
                .streams
                .iter()
                .min_by_key(|(_, timings)| timings.back().map(|timing| timing.stop))
        {
            self.streams.remove(&oldest);
        }
        let timings = self.streams.entry(stream).or_default();
        let interval = if let Some(previous) = timings.back() {
            if start <= previous.start || stop < previous.stop {
                return;
            }
            Some(start - previous.start)
        } else {
            None
        };
        if timings.len() == MAX_TIMINGS {
            timings.pop_front();
        }
        timings.push_back(Timing {
            start,
            stop,
            interval,
        });
    }

    fn measure(&mut self, now: u64, peak_window: Duration) -> Option<[Option<f64>; SENSOR_COUNT]> {
        // ETW may deliver a full second of events in one batch. Only discard the
        // last reading after allowing for that delay and one missed batch.
        let oldest = now.saturating_sub(self.frequency.saturating_mul(2));
        self.streams
            .retain(|_, timings| timings.back().is_some_and(|timing| timing.stop >= oldest));
        let latest = self
            .streams
            .values()
            .filter_map(VecDeque::back)
            .map(|timing| timing.stop)
            .max()?;
        let cutoff = latest.saturating_sub(self.frequency);
        let peak_ticks = (u128::from(self.frequency) * peak_window.as_millis() / 1_000) as u64;
        let peak_cutoff = latest.saturating_sub(peak_ticks);
        self.streams.retain(|_, timings| {
            while timings.front().is_some_and(|timing| timing.stop < cutoff) {
                timings.pop_front();
            }
            !timings.is_empty()
        });
        let (&stream, timings) = self.streams.iter().max_by_key(|(stream, timings)| {
            (
                timings.len(),
                Some(**stream) == self.selected,
                timings.back().map(|timing| timing.stop),
            )
        })?;
        self.selected = Some(stream);
        let to_ms = |ticks: u64| ticks as f64 * 1_000.0 / self.frequency as f64;
        let mut interval_count = 0;
        let mut total_ms = 0.0;
        let mut peak_interval = None;
        let mut peak_wait = None;
        for timing in timings {
            if timing.stop >= peak_cutoff {
                peak_wait = peak_wait.max(Some(timing.stop - timing.start));
                peak_interval = peak_interval.max(timing.interval);
            }
            if let Some(interval) = timing.interval {
                interval_count += 1;
                total_ms += to_ms(interval);
            }
        }
        Some([
            (interval_count > 0).then(|| interval_count as f64 * 1_000.0 / total_ms),
            peak_interval.map(to_ms),
            peak_wait.map(to_ms),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn frames_require_administrator_only_after_a_permissions_failure() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};

        for code in [ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD] {
            let failure =
                CaptureFailure::from_trace_error(io::Error::from_raw_os_error(code as i32));
            let sample = Sample::Unavailable(failure);
            assert!(sample.requires_admin());
            assert_eq!(sample.status(), ACCESS_DENIED);
        }
        let busy = CaptureFailure::from_trace_error(io::Error::from_raw_os_error(
            ERROR_ALREADY_EXISTS as i32,
        ));
        assert!(!Sample::Unavailable(busy).requires_admin());
        assert!(!Sample::Waiting(WAITING).requires_admin());
        assert!(!Sample::Measured([None; SENSOR_COUNT]).requires_admin());
    }

    #[test]
    fn retries_busy_and_interrupted_capture_with_a_backoff() {
        use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;

        let now = Instant::now();
        let busy = CaptureFailure::from_trace_error(io::Error::from_raw_os_error(
            ERROR_ALREADY_EXISTS as i32,
        ));
        for failure in [busy, CaptureFailure::Retryable(CAPTURE_ENDED)] {
            let capture = Err(failure);
            let mut sampler = FrameSampler {
                pid: std::process::id(),
                target: Some(ProcessTarget::open(std::process::id()).unwrap()),
                retry_at: retry_deadline(&capture, now),
                capture,
            };
            assert_eq!(sampler.retry_at, Some(now + CAPTURE_RETRY));
            assert!(matches!(
                sampler.sample(ONE_SECOND),
                Sample::Unavailable(CaptureFailure::Retryable(_))
            ));
            assert_eq!(sampler.retry_at, Some(now + CAPTURE_RETRY));
        }
        assert_eq!(retry_deadline(&Err(CaptureFailure::Permission), now), None);
        assert_eq!(
            retry_deadline(&Err(CaptureFailure::Other(PROCESS_ENDED)), now),
            None
        );
    }

    #[test]
    fn frame_target_follows_focus_without_overriding_manual_modes() {
        assert_eq!(FrameTarget::Foreground.resolve(None, || None), None);
        assert_eq!(
            FrameTarget::Foreground.resolve(Some(10), || Some(20)),
            Some(20)
        );
        assert_eq!(FrameTarget::Foreground.resolve(Some(20), || None), Some(20));
        assert_eq!(
            FrameTarget::Process(30).resolve(Some(20), || unreachable!()),
            Some(30)
        );
        assert_eq!(FrameTarget::Off.resolve(Some(20), || unreachable!()), None);
    }

    fn present(presents: &mut Presents, thread: u32, chain: u64, at: u64, wait: u64) {
        pair(presents, Runtime::Dxgi, thread, chain, at, wait, (0, 0));
    }

    fn pair(
        presents: &mut Presents,
        runtime: Runtime,
        thread_id: u32,
        swap_chain: u64,
        at: u64,
        wait: u64,
        (flags, result): (u32, i32),
    ) {
        for (tick, kind) in [
            (at, PresentKind::Start { swap_chain, flags }),
            (at + wait, PresentKind::Stop { result }),
        ] {
            presents.record(PresentEvent {
                thread_id,
                tick,
                runtime,
                kind,
            });
        }
    }

    #[test]
    fn measures_timestamps_and_keeps_spikes_inside_the_window() {
        let mut presents = Presents::new(1_000);
        present(&mut presents, 1, 10, 100, 0);
        assert_eq!(
            presents.measure(150, ONE_SECOND),
            Some([None, None, Some(0.0)])
        );
        present(&mut presents, 1, 10, 120, 4);
        present(&mut presents, 1, 10, 200, 9);
        present(&mut presents, 1, 10, 220, 2);
        assert_eq!(
            presents.measure(300, ONE_SECOND),
            Some([Some(25.0), Some(80.0), Some(9.0)])
        );
        present(&mut presents, 1, 10, 1_211, 1);
        assert_eq!(presents.measure(1_212, ONE_SECOND).unwrap()[2], Some(2.0));
        assert!(presents.measure(3_213, ONE_SECOND).is_none());
    }

    #[test]
    fn sampling_interval_limits_the_two_peaks_while_fps_uses_one_second() {
        let mut presents = Presents::new(10_000);
        present(&mut presents, 1, 10, 1_000, 0);
        present(&mut presents, 1, 10, 5_000, 120);
        present(&mut presents, 1, 10, 6_000, 30);
        present(&mut presents, 1, 10, 6_200, 10);

        assert_eq!(
            presents.measure(6_500, Duration::from_millis(200)),
            Some([Some(3_000.0 / 520.0), Some(400.0), Some(12.0)])
        );
        assert_eq!(
            presents.measure(6_500, Duration::from_millis(100)),
            Some([Some(3_000.0 / 520.0), Some(100.0), Some(3.0)])
        );
    }

    #[test]
    fn excludes_test_failed_and_occluded_calls_without_counting_other_chains() {
        let mut presents = Presents::new(1_000);
        present(&mut presents, 1, 10, 100, 3);
        pair(&mut presents, Runtime::Dxgi, 1, 10, 110, 6, (1, 0));
        pair(&mut presents, Runtime::Dxgi, 1, 10, 120, 20, (0, -1));
        pair(
            &mut presents,
            Runtime::Dxgi,
            1,
            10,
            140,
            20,
            (0, 0x087a_0001),
        );
        pair(&mut presents, Runtime::D3d9, 1, 10, 145, 30, (1, 0));
        present(&mut presents, 2, 11, 160, 7);
        present(&mut presents, 1, 10, 200, 2);
        assert_eq!(
            presents.measure(250, ONE_SECOND),
            Some([Some(10.0), Some(100.0), Some(3.0)])
        );
        presents.clear();
        present(&mut presents, 1, 10, 300, 1);
        assert_eq!(
            presents.measure(350, ONE_SECOND),
            Some([None, None, Some(1.0)])
        );
    }
}
