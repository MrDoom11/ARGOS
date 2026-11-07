mod etw;
mod process;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::mpsc::TryRecvError;

use windows_sys::Win32::System::Performance::QueryPerformanceCounter;

use self::etw::{Capture, PresentEvent, PresentKind, Runtime};
use self::process::Target;
pub(crate) use self::process::{Process, visible_processes};

pub(crate) const SENSOR_COUNT: usize = 3;
const MAX_PENDING: usize = 64;
const MAX_STREAMS: usize = 16;
const MAX_TIMINGS: usize = 2048;
const MAX_EVENTS_PER_READ: usize = 8192;
const ACTIVE: &str = "Capturando apresentações DirectX";
const WAITING: &str = "Aguardando apresentações DirectX do processo";
const ACCESS_DENIED: &str = "Captura requer administrador ou Performance Log Users";
const CAPTURE_ENDED: &str = "Captura interrompida. Desative e selecione o processo novamente";

pub(crate) enum Sample {
    Measured([Option<f64>; SENSOR_COUNT]),
    Waiting(&'static str),
    Unavailable(&'static str),
}

pub(crate) struct FrameSampler(Result<ActiveCapture, &'static str>);

struct ActiveCapture {
    capture: Capture,
    target: Target,
    presents: Presents,
}

impl FrameSampler {
    pub(crate) fn start(pid: u32) -> Self {
        Self(ActiveCapture::start(pid))
    }

    pub(crate) fn sample(&mut self) -> Sample {
        match &mut self.0 {
            Ok(capture) => match capture.sample() {
                Ok(sample) => sample,
                Err(reason) => {
                    self.0 = Err(reason);
                    Sample::Unavailable(reason)
                }
            },
            Err(reason) => Sample::Unavailable(reason),
        }
    }
}

impl Sample {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Measured(_) => ACTIVE,
            Self::Waiting(reason) | Self::Unavailable(reason) => reason,
        }
    }
}

impl ActiveCapture {
    fn start(pid: u32) -> Result<Self, &'static str> {
        let target = Target::open(pid).map_err(|_| "Processo indisponível para captura")?;
        let capture = Capture::start(pid).map_err(|error| match error.kind() {
            io::ErrorKind::PermissionDenied => ACCESS_DENIED,
            io::ErrorKind::AlreadyExists => "Outra instância do ARGOS já está capturando quadros",
            _ => "Windows não conseguiu iniciar a captura de quadros",
        })?;
        let presents = Presents::new(capture.ticks_per_second());
        Ok(Self {
            capture,
            target,
            presents,
        })
    }

    fn sample(&mut self) -> Result<Sample, &'static str> {
        if !self.target.is_running() {
            return Err("O processo selecionado foi encerrado");
        }
        let mut lost = self.capture.take_loss();
        for _ in 0..MAX_EVENTS_PER_READ {
            match self.capture.try_recv() {
                Ok(event) if !lost => self.presents.record(event),
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Err(CAPTURE_ENDED),
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
            return Err("Relógio de captura indisponível");
        }
        Ok(self
            .presents
            .measure(now as u64)
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

    fn measure(&mut self, now: u64) -> Option<[Option<f64>; SENSOR_COUNT]> {
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
        let mut peak_wait = 0;
        for timing in timings {
            peak_wait = peak_wait.max(timing.stop - timing.start);
            if let Some(interval) = timing.interval {
                interval_count += 1;
                total_ms += to_ms(interval);
                peak_interval = peak_interval.max(Some(interval));
            }
        }
        Some([
            (interval_count > 0).then(|| interval_count as f64 * 1_000.0 / total_ms),
            peak_interval.map(to_ms),
            Some(to_ms(peak_wait)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(presents.measure(150), Some([None, None, Some(0.0)]));
        present(&mut presents, 1, 10, 120, 4);
        present(&mut presents, 1, 10, 200, 9);
        present(&mut presents, 1, 10, 220, 2);
        assert_eq!(
            presents.measure(300),
            Some([Some(25.0), Some(80.0), Some(9.0)])
        );
        present(&mut presents, 1, 10, 1_211, 1);
        assert_eq!(presents.measure(1_212).unwrap()[2], Some(2.0));
        assert!(presents.measure(3_213).is_none());
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
            presents.measure(250),
            Some([Some(10.0), Some(100.0), Some(3.0)])
        );
        presents.clear();
        present(&mut presents, 1, 10, 300, 1);
        assert_eq!(presents.measure(350), Some([None, None, Some(1.0)]));
    }
}
