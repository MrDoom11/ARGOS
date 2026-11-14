pub(crate) mod custom_ping;
mod etw;
mod icmp;
mod tcp;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::frames::{ProcessTarget, process_name};

use self::etw::Trace;
use self::icmp::IcmpRtt;
use self::tcp::TcpRtt;

pub const INTERVAL: Duration = Duration::from_secs(1);
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(250);
const TRACE_RETRY: Duration = Duration::from_secs(5);
const MAX_CANDIDATES: usize = 256;
const WINDOW: usize = 6;
const IDLE_SECONDS: u64 = 3;
const TARGET_IDLE_SECONDS: u64 = 15;

pub struct Monitor {
    feed: Arc<Feed>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Feed {
    state: Mutex<FeedState>,
    changed: Condvar,
}

#[derive(Default)]
struct FeedState {
    request: Request,
    latest: Option<Observation>,
    stopped: bool,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct Request {
    pid: Option<u32>,
    generation: u64,
}

pub struct Observation {
    pub timestamp: Instant,
    pub target: Option<Target>,
    pub rtt: Rtt,
    pub detail: String,
}

pub enum Rtt {
    Pending,
    Unavailable,
    Milliseconds(f64),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Target {
    pid: u32,
    transport: Transport,
    local: SocketAddr,
    remote: SocketAddr,
}

impl Target {
    pub fn remote_ip(self) -> IpAddr {
        self.remote.ip().to_canonical()
    }

    fn label(self) -> String {
        match self.transport {
            Transport::Tcp => self.remote.to_string(),
            Transport::Udp => self.remote_ip().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Transport {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Default)]
enum GameProfile {
    #[default]
    Generic,
    Warframe,
    Elite,
}

impl GameProfile {
    fn from_process(pid: u32) -> Self {
        match process_name(pid)
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("warframe.x64.exe") => Self::Warframe,
            Some("elitedangerous64.exe") => Self::Elite,
            _ => Self::Generic,
        }
    }

    fn preferred_udp_port(self, port: u16) -> bool {
        match self {
            // These local ports are hints. Both games can use other ports.
            Self::Warframe => matches!(
                port,
                4950 | 4955 | 4960 | 4965 | 4970 | 4975 | 4980 | 4985 | 4990 | 4995
            ),
            Self::Elite => (5100..=5200).contains(&port),
            Self::Generic => false,
        }
    }
}

struct Packet {
    pid: u32,
    transport: Transport,
    local: SocketAddr,
    remote: SocketAddr,
    inbound: bool,
}

impl Monitor {
    pub fn start() -> Result<Self, String> {
        let feed = Arc::new(Feed::default());
        let worker_feed = Arc::clone(&feed);
        let worker = thread::Builder::new()
            .name("argos-game-network".to_owned())
            .spawn(move || collect(&worker_feed))
            .map_err(|error| format!("Não foi possível iniciar o monitor de rede ({error})."))?;
        Ok(Self {
            feed,
            worker: Some(worker),
        })
    }

    pub fn set_pid(&self, pid: Option<u32>) {
        let mut state = self.feed.state.lock().expect("alvo da latência");
        if state.request.pid != pid {
            state.request.pid = pid;
            state.request.generation = state.request.generation.wrapping_add(1);
            state.latest = None;
            self.feed.changed.notify_one();
        }
    }

    pub fn try_recv(&self) -> Option<Observation> {
        self.feed.state.lock().ok()?.latest.take()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.feed.state.lock().expect("alvo da latência").stopped = true;
        self.feed.changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn collect(feed: &Feed) {
    let mut worker = Worker::new();
    let mut state = feed.state.lock().expect("alvo da latência");
    while !state.stopped {
        let request = state.request;
        drop(state);

        let started = Instant::now();
        let observation = worker.sample(started, request, |target| {
            let mut state = feed.state.lock().expect("alvo da latência");
            if !state.stopped && state.request == request {
                state.latest = Some(Observation {
                    timestamp: Instant::now(),
                    target: Some(target),
                    rtt: Rtt::Pending,
                    detail: format!("{} · aguardando medição", target.label()),
                });
            }
        });
        state = feed.state.lock().expect("alvo da latência");
        if state.stopped || state.request != request {
            continue;
        }
        let interval = if observation.target.is_none() && matches!(observation.rtt, Rtt::Pending) {
            DISCOVERY_INTERVAL
        } else {
            INTERVAL
        };
        state.latest = Some(observation);

        let same_request = |state: &mut FeedState| !state.stopped && state.request == request;
        state = if request.pid.is_none() {
            feed.changed
                .wait_while(state, same_request)
                .expect("alvo da latência")
        } else {
            let remaining = interval.saturating_sub(started.elapsed());
            feed.changed
                .wait_timeout_while(state, remaining, same_request)
                .expect("alvo da latência")
                .0
        };
    }
}

struct Worker {
    started: Instant,
    discovery: Arc<Mutex<Discovery>>,
    trace: Option<Trace>,
    retry_at: Instant,
    trace_error: String,
    measurement: Option<(Target, Measurement)>,
    request: Request,
    process: Option<io::Result<ProcessTarget>>,
}

impl Worker {
    fn new() -> Self {
        let started = Instant::now();
        Self {
            started,
            discovery: Arc::new(Mutex::new(Discovery::default())),
            trace: None,
            retry_at: started,
            trace_error: String::new(),
            measurement: None,
            request: Request::default(),
            process: None,
        }
    }

    fn sample(
        &mut self,
        now: Instant,
        request: Request,
        report_target: impl FnOnce(Target),
    ) -> Observation {
        if self.request != request {
            self.request = request;
            self.process = request.pid.map(ProcessTarget::open);
            self.measurement = None;
            let profile = request
                .pid
                .map_or(GameProfile::Generic, GameProfile::from_process);
            let mut discovery = self.discovery.lock().expect("descoberta de rede");
            discovery.set_pid(request.pid);
            discovery.profile = profile;
        }
        let unavailable = match &self.process {
            None => Some((Rtt::Pending, "Aguardando um aplicativo.".to_owned())),
            Some(Err(error)) => Some((
                Rtt::Unavailable,
                format!("Não foi possível acompanhar o aplicativo ({error})."),
            )),
            Some(Ok(process)) if !process.is_running() => Some((
                Rtt::Unavailable,
                "O aplicativo selecionado foi encerrado.".to_owned(),
            )),
            Some(Ok(_)) => None,
        };
        if let Some((rtt, detail)) = unavailable {
            self.clear();
            return observation(None, rtt, detail);
        }

        let current = self.measurement.as_ref().map(|(target, _)| *target);
        let capture = self.ensure_trace(now, current);
        let tick = self.started.elapsed().as_secs();
        let (target, saw_traffic) = {
            let mut discovery = self.discovery.lock().expect("descoberta de rede");
            discovery.expire(tick, current);
            let target = if capture.is_ok() {
                discovery.select(tick, current)
            } else {
                discovery.retained_target(current)
            };
            (target, !discovery.candidates.is_empty())
        };
        if let Err(detail) = capture {
            if target.is_none() {
                self.measurement = None;
            }
            let detail = if let Some(target) = target {
                format!("{} · {detail}", target.label())
            } else {
                detail
            };
            return observation(target, Rtt::Unavailable, detail);
        }
        let Some(target) = target else {
            self.measurement = None;
            let detail = if saw_traffic {
                "Tráfego observado · aguardando confirmação do destino"
            } else {
                "Aguardando tráfego de rede do aplicativo"
            };
            return observation(None, Rtt::Pending, detail);
        };
        if current != Some(target) {
            report_target(target);
            self.measurement = Some((target, Measurement::new(target)));
        }
        let (_, measurement) = self.measurement.as_mut().expect("destino selecionado");
        let (source, result) = measurement.sample();
        let prefix = format!("{} · {source}", target.label());
        let (rtt, detail) = match result {
            Ok(Some(rtt_ms)) => (Rtt::Milliseconds(rtt_ms), prefix),
            Ok(None) => (Rtt::Pending, format!("{prefix} · aguardando amostra")),
            Err(error) => (Rtt::Unavailable, format!("{prefix} · {error}")),
        };
        observation(Some(target), rtt, detail)
    }

    fn ensure_trace(&mut self, now: Instant, current: Option<Target>) -> Result<(), String> {
        if let Some(trace) = &self.trace {
            if trace.is_running() {
                if trace.take_loss() {
                    self.discovery
                        .lock()
                        .expect("descoberta de rede")
                        .reset_scores(current);
                    return Err("Captura de rede incompleta. Redetectando o destino.".to_owned());
                }
                return Ok(());
            }
            self.trace = None;
            self.discovery
                .lock()
                .expect("descoberta de rede")
                .reset_scores(current);
            self.trace_error =
                "A captura de rede foi interrompida. Tentando reconectar.".to_owned();
            self.retry_at = now + TRACE_RETRY;
        }
        if now < self.retry_at {
            return Err(self.trace_error.clone());
        }

        let discovery = Arc::clone(&self.discovery);
        let started = self.started;
        match Trace::start(move |packet| {
            if let Ok(mut discovery) = discovery.try_lock() {
                discovery.record(started.elapsed().as_secs(), packet);
            }
        }) {
            Ok(trace) => {
                self.trace = Some(trace);
                Ok(())
            }
            Err(error) => {
                self.retry_at = now + TRACE_RETRY;
                self.trace_error.clone_from(&error);
                Err(error)
            }
        }
    }

    fn clear(&mut self) {
        self.trace = None;
        self.measurement = None;
        let mut discovery = self.discovery.lock().expect("descoberta de rede");
        discovery.set_pid(None);
        self.retry_at = Instant::now();
    }
}

fn observation(target: Option<Target>, rtt: Rtt, detail: impl Into<String>) -> Observation {
    Observation {
        timestamp: Instant::now(),
        target,
        rtt,
        detail: detail.into(),
    }
}

enum Measurement {
    Tcp(TcpRtt),
    Icmp(IcmpRtt),
}

impl Measurement {
    fn new(target: Target) -> Self {
        match target.transport {
            Transport::Tcp => Self::Tcp(TcpRtt::new(target)),
            Transport::Udp => Self::Icmp(IcmpRtt::new(target)),
        }
    }

    fn sample(&mut self) -> (&'static str, Result<Option<f64>, String>) {
        match self {
            Self::Tcp(rtt) => ("RTT TCP", rtt.sample()),
            Self::Icmp(rtt) => ("UDP observado · sonda ICMP", rtt.sample()),
        }
    }
}

#[derive(Default)]
struct Discovery {
    pid: Option<u32>,
    profile: GameProfile,
    candidates: HashMap<Target, Candidate>,
}

impl Discovery {
    fn set_pid(&mut self, pid: Option<u32>) {
        self.pid = pid;
        self.candidates.clear();
    }

    fn record(&mut self, tick: u64, packet: Packet) {
        if self.pid != Some(packet.pid) {
            return;
        }
        let remote = packet.remote.ip().to_canonical();
        if packet.remote.port() == 0
            || remote.is_unspecified()
            || remote.is_loopback()
            || remote.is_multicast()
            || matches!(remote, IpAddr::V4(ip) if ip.is_broadcast() || ip.is_link_local())
            || matches!(remote, IpAddr::V6(ip) if ip.is_unicast_link_local())
        {
            return;
        }
        let (local, remote) = match packet.transport {
            // ICMP measures a path to an address regardless of the game's UDP
            // ports. Keep interfaces separate so the probe can use the source IP.
            Transport::Udp => (
                SocketAddr::new(packet.local.ip().to_canonical(), 0),
                SocketAddr::new(remote, 0),
            ),
            Transport::Tcp => (packet.local, packet.remote),
        };
        let target = Target {
            pid: packet.pid,
            transport: packet.transport,
            local,
            remote,
        };
        let preferred_port = packet.transport == Transport::Udp
            && self.profile.preferred_udp_port(packet.local.port());
        if let Some(candidate) = self.candidates.get_mut(&target) {
            candidate.record(tick, packet.inbound, preferred_port);
        } else if self.candidates.len() < MAX_CANDIDATES {
            let mut candidate = Candidate::default();
            candidate.record(tick, packet.inbound, preferred_port);
            self.candidates.insert(target, candidate);
        }
    }

    fn expire(&mut self, tick: u64, current: Option<Target>) {
        self.candidates.retain(|target, candidate| {
            let lifetime = if Some(*target) == current {
                TARGET_IDLE_SECONDS
            } else if target.transport == Transport::Udp {
                WINDOW as u64
            } else {
                IDLE_SECONDS
            };
            tick.saturating_sub(candidate.last_tick) < lifetime
        });
    }

    fn reset_scores(&mut self, current: Option<Target>) {
        self.candidates.retain(|target, candidate| {
            if Some(*target) == current {
                candidate.buckets.fill(Bucket::default());
                true
            } else {
                false
            }
        });
    }

    fn retained_target(&self, current: Option<Target>) -> Option<Target> {
        current.filter(|target| self.candidates.contains_key(target))
    }

    fn select(&self, tick: u64, current: Option<Target>) -> Option<Target> {
        let current = self.retained_target(current);
        let score = |target: Target, candidate: &Candidate| candidate.score(tick, target.transport);
        let Some((best, best_score)) = self
            .candidates
            .iter()
            .filter_map(|(&target, candidate)| {
                score(target, candidate).map(|score| (target, score))
            })
            .max_by_key(|&(target, score)| (score, target))
        else {
            return current;
        };
        if let Some(current) = current
            && let Some(current_score) = self
                .candidates
                .get(&current)
                .and_then(|candidate| score(current, candidate))
            && !best_score.clearly_better_than(current_score)
        {
            return Some(current);
        }
        Some(best)
    }
}

#[derive(Default)]
struct Candidate {
    buckets: [Bucket; WINDOW],
    last_tick: u64,
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    tick: u64,
    sent: u16,
    received: u16,
    preferred_port: bool,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Score {
    confirmed: bool,
    active: u8,
    udp: bool,
    balanced_packets: u32,
    preferred_port: bool,
}

impl Score {
    fn clearly_better_than(self, current: Self) -> bool {
        let rank = (self.confirmed, self.active, self.udp);
        let current_rank = (current.confirmed, current.active, current.udp);
        rank > current_rank
            || (rank == current_rank
                && (self.balanced_packets >= current.balanced_packets * 2 + 6
                    || (self.preferred_port
                        && !current.preferred_port
                        && self.balanced_packets == current.balanced_packets)))
    }
}

impl Candidate {
    fn record(&mut self, tick: u64, inbound: bool, preferred_port: bool) {
        self.last_tick = tick;
        let bucket = &mut self.buckets[(tick % WINDOW as u64) as usize];
        if bucket.tick != tick {
            *bucket = Bucket {
                tick,
                ..Bucket::default()
            };
        }
        bucket.preferred_port |= preferred_port;
        let count = if inbound {
            &mut bucket.received
        } else {
            &mut bucket.sent
        };
        *count = count.saturating_add(1);
    }

    fn score(&self, tick: u64, transport: Transport) -> Option<Score> {
        if tick.saturating_sub(self.last_tick) >= IDLE_SECONDS {
            return None;
        }
        let mut sent_seconds = 0;
        let mut received_seconds = 0;
        let mut sent = 0;
        let mut received = 0;
        let mut preferred_port = false;
        for bucket in &self.buckets {
            if tick.saturating_sub(bucket.tick) < WINDOW as u64 {
                sent_seconds += u8::from(bucket.sent != 0);
                received_seconds += u8::from(bucket.received != 0);
                sent += u32::from(bucket.sent);
                received += u32::from(bucket.received);
                preferred_port |= bucket.preferred_port;
            }
        }
        let active = sent_seconds.min(received_seconds);
        let balanced_packets = sent.min(received);
        let udp = transport == Transport::Udp;
        let confirmed = balanced_packets >= 3 || (udp && active >= 2);
        (confirmed || (udp && balanced_packets >= 1 && sent + received >= 4)).then_some(Score {
            confirmed,
            active,
            udp,
            preferred_port,
            balanced_packets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(port: u16, transport: Transport, inbound: bool) -> Packet {
        packet_to(2, port, transport, inbound)
    }

    fn packet_to(host: u8, port: u16, transport: Transport, inbound: bool) -> Packet {
        Packet {
            pid: 1,
            transport,
            local: "192.0.2.1:50000".parse().unwrap(),
            remote: SocketAddr::from(([192, 0, 2, host], port)),
            inbound,
        }
    }

    fn exchange(discovery: &mut Discovery, tick: u64, port: u16, transport: Transport) {
        exchange_with(discovery, tick, 2, port, transport);
    }

    fn exchange_with(
        discovery: &mut Discovery,
        tick: u64,
        host: u8,
        port: u16,
        transport: Transport,
    ) {
        for _ in 0..2 {
            discovery.record(tick, packet_to(host, port, transport, false));
            discovery.record(tick, packet_to(host, port, transport, true));
        }
    }

    #[test]
    fn selects_sustained_bidirectional_traffic_and_ignores_untracked_processes() {
        let mut discovery = Discovery::default();
        exchange(&mut discovery, 0, 443, Transport::Tcp);
        assert!(discovery.candidates.is_empty());
        discovery.set_pid(Some(1));

        for tick in 0..3 {
            for _ in 0..20 {
                discovery.record(tick, packet(27015, Transport::Udp, false));
            }
            for inbound in [false, true] {
                let mut other = packet(27016, Transport::Udp, inbound);
                other.pid = 2;
                discovery.record(tick, other);
            }
        }
        exchange(&mut discovery, 2, 443, Transport::Tcp);
        assert_eq!(discovery.select(2, None), None);
        exchange(&mut discovery, 3, 443, Transport::Tcp);
        let target = discovery.select(3, None).unwrap();
        assert_eq!(target.pid, 1);
        assert_eq!(target.remote.port(), 443);
        assert_eq!(target.remote_ip(), IpAddr::from([192, 0, 2, 2]));
    }

    #[test]
    fn identifies_a_destination_in_the_first_bidirectional_batch() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for _ in 0..3 {
            discovery.record(0, packet(27015, Transport::Udp, false));
        }
        assert!(discovery.select(0, None).is_none());
        discovery.record(0, packet(27015, Transport::Udp, true));
        assert_eq!(
            discovery.select(0, None).unwrap().remote_ip(),
            IpAddr::from([192, 0, 2, 2])
        );
    }

    #[test]
    fn udp_combines_ports_across_the_same_ip_after_a_sparse_response() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for _ in 0..3 {
            discovery.record(0, packet(4950, Transport::Udp, false));
        }
        discovery.expire(4, None);
        discovery.record(4, packet_to(3, 4955, Transport::Udp, true));
        assert!(discovery.select(4, None).is_none());
        let mut reply = packet(4955, Transport::Udp, true);
        reply.local = "192.0.2.4:50001".parse().unwrap();
        discovery.record(4, reply);
        assert!(discovery.select(4, None).is_none());
        let mut reply = packet(4955, Transport::Udp, true);
        reply.local.set_port(50001);
        discovery.record(4, reply);
        let selected = discovery.select(4, None).unwrap();
        assert_eq!(selected.remote, "192.0.2.2:0".parse().unwrap());
        assert_eq!(selected.local, "192.0.2.1:0".parse().unwrap());
    }

    #[test]
    fn tcp_counts_only_the_packets_from_the_same_connection() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for _ in 0..3 {
            discovery.record(0, packet(443, Transport::Tcp, false));
            discovery.record(0, packet(444, Transport::Tcp, true));
        }
        assert!(discovery.select(0, None).is_none());
        for _ in 0..3 {
            discovery.record(0, packet(443, Transport::Tcp, true));
        }
        let selected = discovery.select(0, None).unwrap();
        assert_eq!(selected.remote, "192.0.2.2:443".parse().unwrap());
        assert_eq!(selected.local, "192.0.2.1:50000".parse().unwrap());
    }

    #[test]
    fn tentative_udp_does_not_supplant_a_confirmed_connection() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for _ in 0..3 {
            discovery.record(0, packet(4950, Transport::Udp, false));
            discovery.record(0, packet(443, Transport::Tcp, false));
            discovery.record(0, packet(443, Transport::Tcp, true));
        }
        discovery.record(0, packet(4950, Transport::Udp, true));
        let selected = discovery.select(0, None).unwrap();
        assert_eq!(selected.transport, Transport::Tcp);
        discovery.record(1, packet(4950, Transport::Udp, false));
        discovery.record(1, packet(4950, Transport::Udp, true));
        assert_eq!(
            discovery.select(1, Some(selected)).unwrap().transport,
            Transport::Udp
        );
    }

    #[test]
    fn keeps_a_stable_target_then_moves_to_the_new_server() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for tick in 0..=1 {
            exchange(&mut discovery, tick, 443, Transport::Tcp);
            exchange(&mut discovery, tick, 27015, Transport::Udp);
        }
        let current = discovery.select(1, None).unwrap();
        assert_eq!(current.remote_ip(), IpAddr::from([192, 0, 2, 2]));
        exchange_with(&mut discovery, 1, 3, 27016, Transport::Udp);
        exchange_with(&mut discovery, 2, 3, 27016, Transport::Udp);
        assert_eq!(discovery.select(2, Some(current)), Some(current));

        discovery.expire(4, Some(current));
        let next = discovery.select(4, Some(current)).unwrap();
        assert_eq!(next.remote_ip(), IpAddr::from([192, 0, 2, 3]));
        discovery.set_pid(Some(2));
        exchange(&mut discovery, 4, 27016, Transport::Udp);
        assert!(discovery.candidates.is_empty());
        assert_eq!(discovery.select(4, Some(next)), None);
    }

    #[test]
    fn keeps_only_the_selected_destination_during_a_bounded_quiet_spell() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for tick in 0..=1 {
            exchange(&mut discovery, tick, 27015, Transport::Udp);
            exchange(&mut discovery, tick, 443, Transport::Tcp);
        }
        let selected = discovery.select(1, None).unwrap();
        assert_eq!(selected.transport, Transport::Udp);

        discovery.expire(1 + IDLE_SECONDS, Some(selected));
        assert_eq!(discovery.candidates.len(), 1);
        assert_eq!(
            discovery.select(1 + IDLE_SECONDS, Some(selected)),
            Some(selected)
        );
        assert_eq!(discovery.select(1 + IDLE_SECONDS, None), None);

        let expires = 1 + TARGET_IDLE_SECONDS;
        discovery.expire(expires - 1, Some(selected));
        assert_eq!(
            discovery.select(expires - 1, Some(selected)),
            Some(selected)
        );
        discovery.expire(expires, Some(selected));
        assert_eq!(discovery.select(expires, Some(selected)), None);
    }

    #[test]
    fn lost_capture_retains_the_destination_without_refreshing_its_age_or_score() {
        let mut discovery = Discovery::default();
        discovery.set_pid(Some(1));
        for tick in 0..=1 {
            exchange(&mut discovery, tick, 27015, Transport::Udp);
            exchange(&mut discovery, tick, 443, Transport::Tcp);
        }
        let selected = discovery.select(1, None).unwrap();

        discovery.reset_scores(Some(selected));
        assert_eq!(discovery.candidates.len(), 1);
        assert_eq!(discovery.retained_target(Some(selected)), Some(selected));
        assert_eq!(discovery.select(2, None), None);
        assert_eq!(discovery.select(2, Some(selected)), Some(selected));

        discovery.expire(1 + TARGET_IDLE_SECONDS, Some(selected));
        assert_eq!(discovery.retained_target(Some(selected)), None);
    }

    #[test]
    fn reselecting_a_pid_invalidates_its_old_observations() {
        let monitor = Monitor {
            feed: Arc::new(Feed::default()),
            worker: None,
        };
        monitor.set_pid(Some(1));
        let old_request = {
            let mut state = monitor.feed.state.lock().unwrap();
            state.latest = Some(observation(None, Rtt::Pending, "amostra anterior"));
            state.request
        };
        monitor.set_pid(Some(2));
        monitor.set_pid(Some(1));
        let new_request = monitor.feed.state.lock().unwrap().request;
        assert_eq!(old_request.pid, new_request.pid);
        assert_ne!(old_request.generation, new_request.generation);
        assert!(monitor.try_recv().is_none());
    }
}
