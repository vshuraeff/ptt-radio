//! test-only NAT and link router: a pure core with injected time, plus a thin udp pump.
//! datagrams carry a 6-byte public address prefix (ipv4 + port, big-endian): the destination
//! when a companion sends, the source when the router delivers.
// shared with later lab units through #[path]; not every item is used by every includer
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const PREFIX: usize = 6;
// lab limit on a whole datagram (envelope prefix + payload); covers the 1200-byte media payload
pub const MAX_DATAGRAM: usize = PREFIX + 1500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    // one public port per private socket
    EndpointIndependent,
    // a new public port per destination
    EndpointDependent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtering {
    EndpointIndependent,
    AddressDependent,
    AddressAndPortDependent,
}

#[derive(Clone, Copy, Debug)]
pub struct NatConfig {
    // players with the same id sit behind one NAT and share its public ip
    pub id: u32,
    pub mapping: Mapping,
    pub filtering: Filtering,
    pub hairpin: bool,
    // a mapping expires after this long without outbound traffic
    pub timeout: Duration,
}

impl Default for NatConfig {
    fn default() -> Self {
        NatConfig {
            id: 0,
            mapping: Mapping::EndpointIndependent,
            filtering: Filtering::EndpointIndependent,
            hairpin: false,
            timeout: Duration::from_secs(30),
        }
    }
}

// per-player share of a link; a default link sums both ends
#[derive(Clone, Copy, Debug, Default)]
pub struct Edge {
    pub delay: Duration,
    pub jitter: Duration,
    pub loss: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Link {
    pub delay: Duration,
    // uniform within +-jitter, clamped at zero delay
    pub jitter: Duration,
    pub loss: f64,
    // hard cuts: everything sent in [from, until) is dropped
    pub cuts: Vec<(Duration, Duration)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Forwarded,
    Filtered,
    Lost,
    Cut,
    NoMapping,
    UnknownSource,
    Malformed,
    // datagram longer than MAX_DATAGRAM; decided and traced at send time, never forwarded
    Oversize,
}

#[derive(Clone, Copy, Debug)]
pub struct Trace {
    // send time, or arrival time for verdicts decided at the destination NAT
    pub at: Duration,
    pub from: Option<SocketAddrV4>,
    pub to: Option<SocketAddrV4>,
    pub verdict: Verdict,
}

#[derive(Clone, Debug)]
pub struct Delivery {
    pub to: SocketAddr,
    pub from: SocketAddrV4,
    pub payload: Vec<u8>,
}

impl Delivery {
    pub fn envelope(&self) -> Vec<u8> {
        let mut out = encode_addr(self.from).to_vec();
        out.extend_from_slice(&self.payload);
        out
    }
}

pub fn encode_addr(addr: SocketAddrV4) -> [u8; PREFIX] {
    let mut out = [0; PREFIX];
    out[..4].copy_from_slice(&addr.ip().octets());
    out[4..].copy_from_slice(&addr.port().to_be_bytes());
    out
}

pub fn decode_addr(bytes: &[u8]) -> Option<SocketAddrV4> {
    let head: &[u8; PREFIX] = bytes.get(..PREFIX)?.try_into().ok()?;
    let ip = Ipv4Addr::new(head[0], head[1], head[2], head[3]);
    Some(SocketAddrV4::new(
        ip,
        u16::from_be_bytes([head[4], head[5]]),
    ))
}

struct Endpoint {
    ip: Ipv4Addr,
    // none for a server, which owns a fixed public address and has no NAT
    nat: Option<NatConfig>,
    server: Option<SocketAddrV4>,
    edge: Edge,
}

struct Binding {
    private: SocketAddr,
    config: NatConfig,
    created: Duration,
    last_out: Duration,
    // remote -> time of the outbound datagram that first permitted it
    sent: HashMap<SocketAddrV4, Duration>,
}

struct Pending {
    at: Duration,
    seq: u64,
    to_public: SocketAddrV4,
    delivery: Delivery,
}

pub struct Core {
    endpoints: HashMap<SocketAddr, Endpoint>,
    servers: HashMap<SocketAddrV4, SocketAddr>,
    links: HashMap<(SocketAddr, SocketAddr), Link>,
    bindings: HashMap<SocketAddrV4, Binding>,
    by_private: HashMap<(SocketAddr, Option<SocketAddrV4>), SocketAddrV4>,
    next_port: HashMap<Ipv4Addr, u16>,
    pending: Vec<Pending>,
    seq: u64,
    rng: u64,
    trace: Vec<Trace>,
}

impl Core {
    pub fn new(seed: u64) -> Core {
        Core {
            endpoints: HashMap::new(),
            servers: HashMap::new(),
            links: HashMap::new(),
            bindings: HashMap::new(),
            by_private: HashMap::new(),
            next_port: HashMap::new(),
            pending: Vec::new(),
            seq: 0,
            rng: seed ^ 0x9E37_79B9_7F4A_7C15 | 1,
            trace: Vec::new(),
        }
    }

    pub fn add_player(
        &mut self,
        private: SocketAddr,
        public_ip: Ipv4Addr,
        nat: NatConfig,
        edge: Edge,
    ) {
        let endpoint = Endpoint {
            ip: public_ip,
            nat: Some(nat),
            server: None,
            edge,
        };
        self.endpoints.insert(private, endpoint);
    }

    pub fn add_server(&mut self, private: SocketAddr, public: SocketAddrV4, edge: Edge) {
        let endpoint = Endpoint {
            ip: *public.ip(),
            nat: None,
            server: Some(public),
            edge,
        };
        self.endpoints.insert(private, endpoint);
        self.servers.insert(public, private);
    }

    pub fn set_link(&mut self, from: SocketAddr, to: SocketAddr, link: Link) {
        self.links.insert((from, to), link);
    }

    // the explicit link, or the sum of both ends' edges
    pub fn link(&self, from: SocketAddr, to: SocketAddr) -> Link {
        if let Some(link) = self.links.get(&(from, to)) {
            return link.clone();
        }
        let a = self
            .endpoints
            .get(&from)
            .map(|e| e.edge)
            .unwrap_or_default();
        let b = self.endpoints.get(&to).map(|e| e.edge).unwrap_or_default();
        Link {
            delay: a.delay + b.delay,
            jitter: a.jitter + b.jitter,
            loss: 1.0 - (1.0 - a.loss) * (1.0 - b.loss),
            cuts: Vec::new(),
        }
    }

    pub fn trace(&self) -> &[Trace] {
        &self.trace
    }

    // parses the envelope of a raw datagram from a companion's real socket
    pub fn send_raw(&mut self, now: Duration, from_private: SocketAddr, datagram: &[u8]) {
        match decode_addr(datagram) {
            // send enforces the size limit
            Some(to) => self.send(now, from_private, to, &datagram[PREFIX..]),
            None => self.record(now, None, None, Verdict::Malformed),
        }
    }

    pub fn send(
        &mut self,
        now: Duration,
        from_private: SocketAddr,
        to_public: SocketAddrV4,
        payload: &[u8],
    ) {
        if PREFIX + payload.len() > MAX_DATAGRAM {
            return self.record(now, None, Some(to_public), Verdict::Oversize);
        }
        let Some(ep) = self.endpoints.get(&from_private) else {
            return self.record(now, None, Some(to_public), Verdict::UnknownSource);
        };
        let (ep_ip, ep_nat, ep_server) = (ep.ip, ep.nat, ep.server);
        let from_public = match (ep_server, ep_nat) {
            (Some(public), _) => public,
            (None, Some(nat)) => {
                if *to_public.ip() == ep_ip && !nat.hairpin {
                    let unmapped = SocketAddrV4::new(ep_ip, 0);
                    return self.record(now, Some(unmapped), Some(to_public), Verdict::Filtered);
                }
                self.map_out(now, from_private, ep_ip, nat, to_public)
            }
            (None, None) => unreachable!("a player always has a nat"),
        };
        // the destination endpoint, if the address belongs to one, picks the link
        let to_private = self
            .servers
            .get(&to_public)
            .copied()
            .or_else(|| self.bindings.get(&to_public).map(|b| b.private))
            .or_else(|| {
                // no binding yet: any endpoint on that ip, lowest address for determinism
                let same_ip = self
                    .endpoints
                    .iter()
                    .filter(|(_, e)| e.ip == *to_public.ip());
                same_ip.map(|(private, _)| *private).min()
            });
        let link = match to_private {
            Some(to) => self.link(from_private, to),
            None => self.link(from_private, SocketAddr::from(([0, 0, 0, 0], 0))),
        };
        if link
            .cuts
            .iter()
            .any(|&(from, until)| now >= from && now < until)
        {
            return self.record(now, Some(from_public), Some(to_public), Verdict::Cut);
        }
        if link.loss > 0.0 && self.unit() < link.loss {
            return self.record(now, Some(from_public), Some(to_public), Verdict::Lost);
        }
        let jitter = link.jitter.as_secs_f64();
        let offset = (self.unit() * 2.0 - 1.0) * jitter;
        let delay = (link.delay.as_secs_f64() + offset).max(0.0);
        self.seq += 1;
        self.pending.push(Pending {
            at: now + Duration::from_secs_f64(delay),
            seq: self.seq,
            to_public,
            delivery: Delivery {
                to: to_private.unwrap_or(from_private),
                from: from_public,
                payload: payload.to_vec(),
            },
        });
    }

    // releases datagrams that arrived by `now`, in time order, after the destination NAT checks.
    // contract: callers may send at a later time before calling `due` for an earlier arrival;
    // filtering is evaluated at the arrival time, so such sends open no permission retroactively.
    // `send` times must be non-decreasing per binding.
    pub fn due(&mut self, now: Duration) -> Vec<Delivery> {
        let mut ready = Vec::new();
        let mut rest = Vec::new();
        for p in self.pending.drain(..) {
            if p.at <= now {
                ready.push(p);
            } else {
                rest.push(p);
            }
        }
        self.pending = rest;
        ready.sort_by_key(|p| (p.at, p.seq));
        let mut out = Vec::new();
        for p in ready {
            let from = p.delivery.from;
            let verdict = self.admit(p.at, p.to_public, from);
            self.record(p.at, Some(from), Some(p.to_public), verdict);
            if verdict == Verdict::Forwarded {
                let mut delivery = p.delivery;
                if let Some(private) = self.servers.get(&p.to_public) {
                    delivery.to = *private;
                } else if let Some(b) = self.bindings.get(&p.to_public) {
                    delivery.to = b.private;
                }
                out.push(delivery);
            }
        }
        out
    }

    fn admit(&mut self, at: Duration, to_public: SocketAddrV4, from: SocketAddrV4) -> Verdict {
        if self.servers.contains_key(&to_public) {
            return Verdict::Forwarded;
        }
        let Some(binding) = self.bindings.get(&to_public) else {
            return Verdict::NoMapping;
        };
        if at < binding.created {
            return Verdict::NoMapping;
        }
        if at.saturating_sub(binding.last_out) > binding.config.timeout {
            self.bindings.remove(&to_public);
            return Verdict::NoMapping;
        }
        // only permissions that existed at the arrival time count, never later sends
        let allowed = match binding.config.filtering {
            Filtering::EndpointIndependent => true,
            Filtering::AddressDependent => binding
                .sent
                .iter()
                .any(|(s, &since)| s.ip() == from.ip() && since <= at),
            Filtering::AddressAndPortDependent => {
                binding.sent.get(&from).is_some_and(|&since| since <= at)
            }
        };
        if allowed {
            Verdict::Forwarded
        } else {
            Verdict::Filtered
        }
    }

    // finds or creates the binding for an outbound datagram and returns its public address
    fn map_out(
        &mut self,
        now: Duration,
        private: SocketAddr,
        ip: Ipv4Addr,
        nat: NatConfig,
        to: SocketAddrV4,
    ) -> SocketAddrV4 {
        let key = match nat.mapping {
            Mapping::EndpointIndependent => (private, None),
            Mapping::EndpointDependent => (private, Some(to)),
        };
        let live = self.by_private.get(&key).copied().filter(|public| {
            self.bindings
                .get(public)
                .is_some_and(|b| now.saturating_sub(b.last_out) <= b.config.timeout)
        });
        let public = live.unwrap_or_else(|| {
            let port = self.next_port.entry(ip).or_insert(40000);
            let public = SocketAddrV4::new(ip, *port);
            *port += 1;
            self.by_private.insert(key, public);
            self.bindings.insert(
                public,
                Binding {
                    private,
                    config: nat,
                    created: now,
                    last_out: now,
                    sent: HashMap::new(),
                },
            );
            public
        });
        let binding = self
            .bindings
            .get_mut(&public)
            .expect("binding just ensured");
        binding.last_out = now;
        binding.sent.entry(to).or_insert(now);
        public
    }

    fn record(
        &mut self,
        at: Duration,
        from: Option<SocketAddrV4>,
        to: Option<SocketAddrV4>,
        verdict: Verdict,
    ) {
        self.trace.push(Trace {
            at,
            from,
            to,
            verdict,
        });
    }

    // xorshift64, uniform in [0, 1)
    fn unit(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

// owns the inside socket on a thread; stops on `stop` or on drop
pub struct Pump {
    pub addr: SocketAddr,
    core: Arc<Mutex<Core>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pump {
    pub fn start(core: Core) -> std::io::Result<Pump> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(1)))?;
        let addr = socket.local_addr()?;
        let core = Arc::new(Mutex::new(core));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (core, stop) = (Arc::clone(&core), Arc::clone(&stop));
            thread::spawn(move || run(socket, core, stop))
        };
        Ok(Pump {
            addr,
            core,
            stop,
            thread: Some(thread),
        })
    }

    pub fn trace(&self) -> Vec<Trace> {
        self.core.lock().unwrap().trace().to_vec()
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(socket: UdpSocket, core: Arc<Mutex<Core>>, stop: Arc<AtomicBool>) {
    let start = Instant::now();
    // one byte more than the limit, so an oversize datagram is seen rather than truncated
    let mut buf = [0u8; MAX_DATAGRAM + 1];
    while !stop.load(Ordering::Relaxed) {
        let received = match socket.recv_from(&mut buf) {
            Ok((len, from)) => Some((len, from)),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => None,
            Err(_) => break,
        };
        let now = start.elapsed();
        let mut core = core.lock().unwrap();
        if let Some((len, from)) = received {
            core.send_raw(now, from, &buf[..len]);
        }
        for delivery in core.due(now) {
            let _ = socket.send_to(&delivery.envelope(), delivery.to);
        }
    }
}
