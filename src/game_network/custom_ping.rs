use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{Rtt, icmp::IcmpRtt};

pub(crate) const MAX_TARGETS: usize = 4;
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);
const DNS_REFRESH: Duration = Duration::from_secs(60);
const DNS_RETRY: Duration = Duration::from_secs(10);
const MAX_RESOLVED_ADDRESSES: usize = 8;
const FAILOVER_AFTER: u8 = 3;

pub(crate) struct Observation {
    pub(crate) address: String,
    pub(crate) resolved_ip: Option<IpAddr>,
    pub(crate) timestamp: Instant,
    pub(crate) rtt: Rtt,
    pub(crate) detail: String,
}

pub(crate) struct Monitor {
    slots: Vec<Slot>,
}

impl Monitor {
    pub(crate) fn start() -> Result<Self, String> {
        let slots = (0..MAX_TARGETS)
            .map(Slot::start)
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| format!("Não foi possível iniciar o Ping dos endereços ({error})."))?;
        Ok(Self { slots })
    }

    pub(crate) fn set_targets(&self, targets: &[String]) {
        let mut remaining = targets
            .iter()
            .map(String::as_str)
            .take(MAX_TARGETS)
            .collect::<Vec<_>>();
        let mut states = self
            .slots
            .iter()
            .map(|slot| slot.feed.state.lock().expect("destinos do Ping"))
            .collect::<Vec<_>>();
        let mut available = Vec::new();
        for (index, state) in states.iter().enumerate() {
            if let Some(kept) = remaining
                .iter()
                .position(|&target| state.address.as_deref() == Some(target))
            {
                remaining.remove(kept);
            } else {
                available.push(index);
            }
        }
        let mut remaining = remaining.into_iter();
        for index in available {
            let state = &mut states[index];
            let address = remaining.next();
            if state.address.as_deref() == address {
                continue;
            }
            state.address = address.map(str::to_owned);
            state.generation = state.generation.wrapping_add(1);
            state.latest = address.map(|address| Observation {
                address: address.to_owned(),
                resolved_ip: None,
                timestamp: Instant::now(),
                rtt: Rtt::Pending,
                detail: "Aguardando medição".to_owned(),
            });
            self.slots[index].feed.changed.notify_one();
        }
    }

    pub(crate) fn drain(&self) -> Vec<Observation> {
        self.slots
            .iter()
            .filter_map(|slot| slot.feed.state.lock().ok()?.latest.take())
            .collect()
    }
}

struct Slot {
    feed: Arc<Feed>,
}

impl Slot {
    fn start(index: usize) -> io::Result<Self> {
        let slot = Self {
            feed: Arc::new(Feed::default()),
        };
        let worker = Arc::clone(&slot.feed);
        // Name resolution can block in the system resolver. The worker is signalled
        // to stop on drop, while the collector remains free to continue.
        thread::Builder::new()
            .name(format!("argos-custom-ping-{index}"))
            .spawn(move || collect(&worker))?;
        Ok(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.feed.state.lock().expect("destino do Ping").stopped = true;
        self.feed.changed.notify_one();
    }
}

#[derive(Default)]
struct Feed {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    address: Option<String>,
    generation: u64,
    latest: Option<Observation>,
    stopped: bool,
}

impl State {
    fn is_current(&self, generation: u64) -> bool {
        !self.stopped && self.generation == generation
    }
}

fn collect(feed: &Feed) {
    let mut probe = None;
    let mut generation = 0;
    let mut state = feed.state.lock().expect("destino do Ping");
    loop {
        if state.address.is_none() {
            probe = None;
            state = feed
                .changed
                .wait_while(state, |state| !state.stopped && state.address.is_none())
                .expect("destino do Ping");
        }
        if state.stopped {
            return;
        }
        let started = Instant::now();
        if generation != state.generation || probe.is_none() {
            let address = state.address.clone().expect("destino configurado");
            probe = Some(Destination::new(address, started));
            generation = state.generation;
        }
        drop(state);

        let mut sampled_at = started;
        let observation = probe
            .as_mut()
            .expect("destino configurado")
            .sample(started, || {
                sampled_at = Instant::now();
                feed.state
                    .lock()
                    .expect("destino do Ping")
                    .is_current(generation)
            });
        state = feed.state.lock().expect("destino do Ping");
        if !state.is_current(generation) {
            continue;
        }
        state.latest = observation;
        let remaining = INTERVAL.saturating_sub(sampled_at.elapsed());
        state = feed
            .changed
            .wait_timeout_while(state, remaining, |state| state.is_current(generation))
            .expect("destino do Ping")
            .0;
    }
}

struct Destination {
    address: String,
    addresses: Vec<SocketAddr>,
    active: usize,
    lookup_at: Option<Instant>,
    lookup_error: Option<String>,
    probe: Option<IcmpRtt>,
    failures: u8,
}

impl Destination {
    fn new(address: String, now: Instant) -> Self {
        let literal = parse_literal(&address);
        Self {
            address,
            addresses: literal
                .filter(|&address| valid_destination(address))
                .into_iter()
                .collect(),
            active: 0,
            lookup_at: literal.is_none().then_some(now),
            lookup_error: None,
            probe: None,
            failures: 0,
        }
    }

    fn sample(&mut self, now: Instant, selected: impl FnOnce() -> bool) -> Option<Observation> {
        if self.lookup_at.is_some_and(|deadline| now >= deadline) {
            self.refresh_dns();
        }
        // A destination removed while DNS was in progress must never start a new probe.
        if !selected() {
            return None;
        }
        let Some(&address) = self.addresses.get(self.active) else {
            return Some(
                self.observation(
                    None,
                    Rtt::Unavailable,
                    self.lookup_error
                        .clone()
                        .unwrap_or_else(|| "Endereço indisponível para sondagem".to_owned()),
                ),
            );
        };
        let probe = self
            .probe
            .get_or_insert_with(|| IcmpRtt::for_address(address));
        let (rtt, detail) = match probe.sample() {
            Ok(Some(value)) => {
                self.failures = 0;
                (Rtt::Milliseconds(value), "ICMP".to_owned())
            }
            Ok(None) => (Rtt::Pending, "Aguardando resposta ICMP".to_owned()),
            Err(error) => {
                if self.addresses.len() > 1 {
                    self.failures += 1;
                    if self.failures >= FAILOVER_AFTER {
                        self.select_address((self.active + 1) % self.addresses.len());
                    }
                }
                (Rtt::Unavailable, format!("ICMP · {error}"))
            }
        };
        Some(self.observation(Some(address), rtt, detail))
    }

    fn refresh_dns(&mut self) {
        match resolve(&self.address) {
            Ok(addresses) => {
                let previous = self.addresses.get(self.active);
                if let Some(active) = previous
                    .and_then(|previous| addresses.iter().position(|address| address == previous))
                {
                    self.active = active;
                } else {
                    self.select_address(0);
                }
                self.addresses = addresses;
                self.lookup_at = Some(Instant::now() + DNS_REFRESH);
                self.lookup_error = None;
            }
            Err(error) => {
                self.lookup_at = Some(Instant::now() + DNS_RETRY);
                self.lookup_error = Some(format!("Falha na resolução DNS ({error})"));
            }
        }
    }

    fn select_address(&mut self, index: usize) {
        self.active = index;
        self.probe = None;
        self.failures = 0;
    }

    fn observation(&self, address: Option<SocketAddr>, rtt: Rtt, detail: String) -> Observation {
        Observation {
            address: self.address.clone(),
            resolved_ip: address.map(|address| address.ip().to_canonical()),
            timestamp: Instant::now(),
            rtt,
            detail,
        }
    }
}

fn parse_literal(address: &str) -> Option<SocketAddr> {
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip.to_canonical(), 0));
    }
    if address.contains('%') {
        return format!("[{address}]:0").parse().ok();
    }
    None
}

fn resolve(name: &str) -> io::Result<Vec<SocketAddr>> {
    let mut addresses = Vec::new();
    for address in (name, 0).to_socket_addrs()? {
        if valid_destination(address) && !addresses.contains(&address) {
            addresses.push(address);
            if addresses.len() == MAX_RESOLVED_ADDRESSES {
                break;
            }
        }
    }
    if addresses.is_empty() {
        Err(io::Error::other("nenhum endereço disponível"))
    } else {
        Ok(addresses)
    }
}

fn valid_destination(address: SocketAddr) -> bool {
    let ip = address.ip().to_canonical();
    !ip.is_unspecified()
        && !ip.is_multicast()
        && !matches!(ip, IpAddr::V4(address) if address.is_broadcast())
        && !matches!(address, SocketAddr::V6(address) if address.ip().is_unicast_link_local() && address.scope_id() == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_retained_targets_and_invalidates_removed_workers() {
        let monitor = Monitor {
            slots: (0..MAX_TARGETS)
                .map(|_| Slot {
                    feed: Arc::new(Feed::default()),
                })
                .collect(),
        };
        monitor.set_targets(&["router".into(), "127.0.0.1".into()]);
        let first_generation = monitor.slots[0].feed.state.lock().unwrap().generation;
        monitor.set_targets(&["127.0.0.1".into(), "router".into(), "::1".into()]);
        let first = monitor.slots[0].feed.state.lock().unwrap();
        assert_eq!(first.address.as_deref(), Some("router"));
        assert!(first.is_current(first_generation));
        drop(first);
        assert_eq!(monitor.drain().len(), 3);

        monitor.set_targets(&["127.0.0.1".into(), "::1".into()]);
        let first = monitor.slots[0].feed.state.lock().unwrap();
        assert!(first.address.is_none());
        assert!(!first.is_current(first_generation));
        drop(first);
        monitor.set_targets(&[]);
        assert!(monitor.drain().is_empty());

        let at = Instant::now();
        let mut destination = Destination::new("127.0.0.1".into(), at);
        assert!(destination.sample(at, || false).is_none());
        assert!(destination.probe.is_none());
    }

    #[test]
    fn distinguishes_hostnames_from_ipv4_ipv6_and_scoped_literals() {
        assert_eq!(
            parse_literal("127.0.0.1"),
            Some("127.0.0.1:0".parse().unwrap())
        );
        assert_eq!(parse_literal("::1"), Some("[::1]:0".parse().unwrap()));
        assert_eq!(
            parse_literal("fe80::1%12"),
            Some("[fe80::1%12]:0".parse().unwrap())
        );
        assert!(valid_destination(parse_literal("fe80::1%12").unwrap()));
        assert!(!valid_destination(parse_literal("fe80::1").unwrap()));
        assert!(!valid_destination(
            parse_literal("255.255.255.255").unwrap()
        ));
        assert!(parse_literal("localhost").is_none());
    }
}
