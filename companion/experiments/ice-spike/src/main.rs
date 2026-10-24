use is::stun::{StunMessage, StunPacket};
use is::{Candidate, CandidateKind, IceAgent, IceAgentEvent, IceCreds, Protocol};
use std::collections::{BTreeSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

const REFLECTOR: &str = "203.0.113.90:3478";
const DEADLINE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mapping {
    Independent,
    Endpoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filtering {
    Independent,
    Address,
    Endpoint,
}

struct Binding {
    inside: SocketAddr,
    outside: SocketAddr,
    mapping_remote: SocketAddr,
    contacted: BTreeSet<SocketAddr>,
}

struct Nat {
    ip: IpAddr,
    mapping: Mapping,
    filtering: Filtering,
    bindings: Vec<Binding>,
    next_port: u16,
}

impl Nat {
    fn new(peer: usize, mapping: Mapping, filtering: Filtering) -> Self {
        Self {
            ip: format!("198.51.100.{}", peer + 1).parse().unwrap(),
            mapping,
            filtering,
            bindings: Vec::new(),
            next_port: 40_000,
        }
    }

    fn outbound(&mut self, inside: SocketAddr, remote: SocketAddr) -> SocketAddr {
        let index = self.bindings.iter().position(|b| {
            b.inside == inside
                && (self.mapping == Mapping::Independent || b.mapping_remote == remote)
        });
        let index = index.unwrap_or_else(|| {
            let outside = SocketAddr::new(self.ip, self.next_port);
            self.next_port += 1;
            self.bindings.push(Binding {
                inside,
                outside,
                mapping_remote: remote,
                contacted: BTreeSet::new(),
            });
            self.bindings.len() - 1
        });
        self.bindings[index].contacted.insert(remote);
        self.bindings[index].outside
    }

    fn inbound(&self, source: SocketAddr, destination: SocketAddr) -> Option<SocketAddr> {
        self.bindings.iter().find_map(|b| {
            let allowed = match self.filtering {
                Filtering::Independent => true,
                Filtering::Address => b.contacted.iter().any(|c| c.ip() == source.ip()),
                Filtering::Endpoint => b.contacted.contains(&source),
            };
            (b.outside == destination && allowed).then_some(b.inside)
        })
    }

    fn gather(&mut self, inside: SocketAddr) -> SocketAddr {
        let reflector = REFLECTOR.parse().unwrap();
        let outside = self.outbound(inside, reflector);
        assert_eq!(self.inbound(reflector, outside), Some(inside));
        outside
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Path {
    source: SocketAddr,
    destination: SocketAddr,
}

struct Peer {
    agent: IceAgent,
    nat: Nat,
    nominated: Option<Path>,
    nomination_count: usize,
    discovered: BTreeSet<SocketAddr>,
    received_probe: bool,
}

struct Signal {
    to: usize,
    generation: u32,
    credentials: IceCreds,
    candidates: Vec<String>,
}

struct Datagram {
    from: usize,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    due: u64,
    serial: u64,
    probe: bool,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    mapping: [Mapping; 2],
    filtering: [Filtering; 2],
    hidden_reflexive: bool,
    impaired: bool,
    restart: bool,
    interfaces: usize,
    expect_connected: bool,
}

struct Lab {
    peers: [Peer; 2],
    signal: VecDeque<Signal>,
    packets: Vec<Datagram>,
    generation: u32,
    now: u64,
    epoch: Instant,
    emitted: u64,
    lost: u64,
    filtered: u64,
    reordered: u64,
    max_delivered_serial: [u64; 2],
    stale_signals: u64,
    rejected_stun: u64,
    config: Scenario,
}

fn credentials(peer: usize, generation: u32) -> IceCreds {
    // fixed fixture credentials keep the lab repeatable; production must supply csprng values.
    IceCreds {
        ufrag: format!("fixture-peer-{peer}-generation-{generation}"),
        pass: format!("fixture-password-peer-{peer}-generation-{generation}"),
    }
}

fn inside(peer: usize, interface: usize) -> SocketAddr {
    format!("10.{}.0.{}:5000", peer + 1, interface + 1)
        .parse()
        .unwrap()
}

impl Lab {
    fn new(config: Scenario) -> Self {
        let peers = std::array::from_fn(|peer| {
            let mut agent = IceAgent::new(credentials(peer, 0));
            agent.set_controlling(peer == 0);
            agent.set_control_tie_breaker(100 + peer as u64);
            agent.set_local_preference(|c: &Candidate, _| {
                if c.local().ip().to_string().ends_with(".1") {
                    65_000
                } else {
                    10_000
                }
            });
            let mut nat = Nat::new(peer, config.mapping[peer], config.filtering[peer]);
            for interface in 0..config.interfaces {
                let local = inside(peer, interface);
                agent.add_local_candidate(Candidate::host(local, "udp").unwrap());
                let mapped = nat.gather(local);
                if !(config.hidden_reflexive && peer == 0) {
                    agent.add_local_candidate(
                        Candidate::server_reflexive(mapped, local, "udp").unwrap(),
                    );
                }
            }
            assert!(!agent.ice_lite());
            Peer {
                agent,
                nat,
                nominated: None,
                nomination_count: 0,
                discovered: BTreeSet::new(),
                received_probe: false,
            }
        });
        let mut lab = Self {
            peers,
            signal: VecDeque::new(),
            packets: Vec::new(),
            generation: 0,
            now: 0,
            epoch: Instant::now(),
            emitted: 0,
            lost: 0,
            filtered: 0,
            reordered: 0,
            max_delivered_serial: [0; 2],
            stale_signals: 0,
            rejected_stun: 0,
            config,
        };
        lab.exchange();
        lab
    }

    fn exchange(&mut self) {
        for from in 0..2 {
            self.signal.push_back(Signal {
                to: 1 - from,
                generation: self.generation,
                credentials: self.peers[from].agent.local_credentials().clone(),
                candidates: self.peers[from]
                    .agent
                    .local_candidates()
                    .map(|c| c.to_sdp_string())
                    .collect(),
            });
        }
    }

    fn drain_signals(&mut self) {
        while let Some(signal) = self.signal.pop_front() {
            if signal.generation != self.generation {
                self.stale_signals += 1;
                continue;
            }
            let agent = &mut self.peers[signal.to].agent;
            agent.set_remote_credentials(signal.credentials);
            for candidate in signal.candidates {
                agent.add_remote_candidate(Candidate::from_sdp_string(&candidate).unwrap());
            }
        }
    }

    fn enqueue(&mut self, from: usize, path: Path, bytes: Vec<u8>, probe: bool) {
        assert!(
            self.peers[from]
                .agent
                .local_candidates()
                .any(|c| c.kind() == CandidateKind::Host && c.addr() == path.source)
        );
        let source = self.peers[from].nat.outbound(path.source, path.destination);
        self.emitted += 1;
        if !probe && self.config.impaired && self.emitted % 7 == 1 {
            self.lost += 1;
            return;
        }
        let interface_delay = if self.config.interfaces > 1 && path.source == inside(from, 0) {
            80
        } else {
            5
        };
        let jitter = if self.config.impaired && self.emitted % 3 == 0 {
            60
        } else {
            0
        };
        self.packets.push(Datagram {
            from,
            source,
            destination: path.destination,
            bytes,
            due: self.now + interface_delay + jitter,
            serial: self.emitted,
            probe,
        });
    }

    fn step(&mut self) {
        self.drain_signals();
        let now = self.epoch + Duration::from_millis(self.now);
        for from in 0..2 {
            let agent = &mut self.peers[from].agent;
            if agent.poll_timeout().is_none_or(|deadline| deadline <= now) {
                agent.handle_timeout(now);
            }
            while let Some(tx) = self.peers[from].agent.poll_transmit() {
                assert_eq!(tx.proto, Protocol::Udp);
                self.enqueue(
                    from,
                    Path {
                        source: tx.source,
                        destination: tx.destination,
                    },
                    tx.contents.to_vec(),
                    false,
                );
            }
        }
        self.packets.sort_by_key(|p| (p.due, p.serial));
        while self.packets.first().is_some_and(|p| p.due <= self.now) {
            let packet = self.packets.remove(0);
            let to = 1 - packet.from;
            let peer = &mut self.peers[to];
            let Some(destination) = peer.nat.inbound(packet.source, packet.destination) else {
                self.filtered += 1;
                continue;
            };
            if packet.serial < self.max_delivered_serial[to] {
                self.reordered += 1;
            }
            self.max_delivered_serial[to] = self.max_delivered_serial[to].max(packet.serial);
            if packet.probe {
                assert_eq!(packet.bytes, b"application-probe");
                peer.received_probe = true;
            } else {
                let message = StunMessage::parse(&packet.bytes).unwrap();
                if !peer.agent.handle_packet(
                    now,
                    StunPacket {
                        proto: Protocol::Udp,
                        source: packet.source,
                        destination,
                        message,
                    },
                ) {
                    self.rejected_stun += 1;
                }
            }
        }
        for peer in &mut self.peers {
            while let Some(event) = peer.agent.poll_event() {
                match event {
                    IceAgentEvent::NominatedSend {
                        proto,
                        source,
                        destination,
                    } => {
                        assert_eq!(proto, Protocol::Udp);
                        let path = Path {
                            source,
                            destination,
                        };
                        peer.nominated = Some(path);
                        peer.nomination_count += 1;
                    }
                    IceAgentEvent::DiscoveredRecv { source, .. } => {
                        peer.discovered.insert(source);
                    }
                    IceAgentEvent::IceRestart(creds) => {
                        assert_eq!(&creds, peer.agent.local_credentials());
                    }
                    IceAgentEvent::IceConnectionStateChange(_) => {}
                }
            }
        }
        self.now += 5;
    }

    fn run_for(&mut self, duration: u64) {
        let end = self.now + duration;
        while self.now < end {
            self.step();
        }
    }

    fn connected(&self) -> bool {
        self.peers
            .iter()
            .all(|p| p.agent.state().is_connected() && p.nominated.is_some())
    }

    fn connect(&mut self) -> bool {
        let end = self.now + DEADLINE_MS;
        while self.now < end && !self.connected() {
            self.step();
        }
        self.connected()
    }

    fn probe_nominated(&mut self) {
        for peer in &mut self.peers {
            peer.received_probe = false;
        }
        for from in 0..2 {
            self.enqueue(
                from,
                self.peers[from].nominated.unwrap(),
                b"application-probe".to_vec(),
                true,
            );
        }
        self.run_for(200);
        assert!(self.peers.iter().all(|p| p.received_probe));
        assert!(self.connected());
    }

    fn restart(&mut self) {
        self.generation += 1;
        for (index, peer) in self.peers.iter_mut().enumerate() {
            peer.agent
                .ice_restart(credentials(index, self.generation), false);
            assert!(peer.agent.remote_credentials().is_none());
            assert_eq!(peer.agent.remote_candidates().count(), 0);
            peer.nominated = None;
            peer.discovered.clear();
            peer.nat.bindings.clear();
            peer.nat.next_port += 1_000;
            let local = inside(index, 0);
            peer.agent
                .add_local_candidate(Candidate::host(local, "udp").unwrap());
            let mapped = peer.nat.gather(local);
            peer.agent
                .add_local_candidate(Candidate::server_reflexive(mapped, local, "udp").unwrap());
        }
        self.signal.push_back(Signal {
            to: 0,
            generation: self.generation - 1,
            credentials: credentials(1, self.generation - 1),
            candidates: Vec::new(),
        });
        self.exchange();
    }
}

fn scenarios() -> Vec<Scenario> {
    let baseline = Scenario {
        name: "eim-eif",
        mapping: [Mapping::Independent; 2],
        filtering: [Filtering::Independent; 2],
        hidden_reflexive: false,
        impaired: false,
        restart: false,
        interfaces: 1,
        expect_connected: true,
    };
    vec![
        baseline,
        Scenario {
            name: "eim-address-filter",
            filtering: [Filtering::Address; 2],
            ..baseline
        },
        Scenario {
            name: "eim-endpoint-filter",
            filtering: [Filtering::Endpoint; 2],
            ..baseline
        },
        Scenario {
            name: "peer-reflexive-hidden-a",
            filtering: [Filtering::Endpoint, Filtering::Independent],
            hidden_reflexive: true,
            ..baseline
        },
        Scenario {
            name: "hidden-a-endpoint-filter-both-blocked",
            filtering: [Filtering::Endpoint; 2],
            hidden_reflexive: true,
            expect_connected: false,
            ..baseline
        },
        Scenario {
            name: "loss-reorder-restart",
            filtering: [Filtering::Endpoint; 2],
            impaired: true,
            restart: true,
            ..baseline
        },
        Scenario {
            name: "edm-endpoint-with-eim-eif",
            mapping: [Mapping::Independent, Mapping::Endpoint],
            filtering: [Filtering::Independent, Filtering::Endpoint],
            ..baseline
        },
        Scenario {
            name: "edm-endpoint-both-blocked",
            mapping: [Mapping::Endpoint; 2],
            filtering: [Filtering::Endpoint; 2],
            expect_connected: false,
            ..baseline
        },
        Scenario {
            name: "two-interfaces-priority-vs-rtt",
            filtering: [Filtering::Endpoint; 2],
            interfaces: 2,
            ..baseline
        },
    ]
}

fn exercise(config: Scenario) {
    let mut lab = Lab::new(config);
    assert_eq!(lab.connect(), config.expect_connected, "{}", config.name);
    if config.expect_connected {
        lab.run_for(2_000);
        lab.probe_nominated();
        if config.hidden_reflexive {
            assert!(
                lab.peers[0]
                    .agent
                    .local_candidates()
                    .any(|c| c.kind() == CandidateKind::PeerReflexive)
            );
            assert!(
                lab.peers[1]
                    .agent
                    .remote_candidates()
                    .any(|c| c.kind() == CandidateKind::PeerReflexive)
            );
        }
        if config.restart {
            let old_paths: Vec<_> = lab.peers.iter().map(|p| p.nominated.unwrap()).collect();
            lab.restart();
            assert!(lab.connect(), "restart deadline");
            lab.probe_nominated();
            assert_eq!(lab.stale_signals, 1);
            for (index, (peer, old_path)) in lab.peers.iter().zip(old_paths).enumerate() {
                assert_ne!(peer.nominated.unwrap(), old_path);
                assert!(peer.nomination_count >= 2);
                assert_eq!(
                    peer.agent.remote_credentials().unwrap().ufrag,
                    credentials(1 - index, lab.generation).ufrag
                );
            }
        }
        if config.impaired {
            assert!(lab.lost > 0);
            assert!(lab.reordered > 0);
        }
        if config.interfaces > 1 {
            for (index, peer) in lab.peers.iter().enumerate() {
                let remote_nat = &lab.peers[1 - index].nat;
                let fast_remote = remote_nat.bindings[1].outside;
                let slow_remote = remote_nat.bindings[0].outside;
                assert!(peer.agent.has_viable_remote_candidate(fast_remote));
                assert!(peer.agent.has_viable_remote_candidate(slow_remote));
                assert_eq!(peer.nominated.unwrap().source, inside(index, 0));
                let nominated_rtt = peer.agent.nominated_pair_rtt().unwrap();
                assert!(nominated_rtt >= Duration::from_millis(80));
                let candidate = peer
                    .agent
                    .remote_candidates()
                    .find(|c| c.addr() == fast_remote)
                    .unwrap();
                let pair = peer
                    .agent
                    .find_pair_for_equivalent_remote_candidate(&candidate)
                    .unwrap();
                let pair_rtt = pair.last_successful_rtt().unwrap();
                assert!(pair_rtt < nominated_rtt);
                assert!(!pair.is_nominated());
                assert!(pair.responses_received() > 1);
                println!(
                    "  peer {index}: first pair to fast remote {fast_remote}: state={:?} rtt={pair_rtt:?} responses={} total_rtt={:?}; nominated rtt={nominated_rtt:?}",
                    pair.state(),
                    pair.responses_received(),
                    pair.total_round_trip_time()
                );
            }
        }
    } else {
        assert_eq!(lab.filtered, lab.emitted);
        assert_eq!(lab.rejected_stun, 0);
        assert!(lab.peers.iter().all(|p| p.nominated.is_none()));
    }
    println!(
        "{}: t={}ms generation={} sent={} loss={} filtered={} reordered={} stale_signal={} rejected_stun={}",
        config.name,
        lab.now,
        lab.generation,
        lab.emitted,
        lab.lost,
        lab.filtered,
        lab.reordered,
        lab.stale_signals,
        lab.rejected_stun
    );
    for (index, peer) in lab.peers.iter().enumerate() {
        println!(
            "  peer {index}: state={:?} nominated={:?} rtt={:?} discovered={} nominations={} prflx_local={} prflx_remote={}",
            peer.agent.state(),
            peer.nominated,
            peer.agent.nominated_pair_rtt(),
            peer.discovered.len(),
            peer.nomination_count,
            peer.agent
                .local_candidates()
                .filter(|c| c.kind() == CandidateKind::PeerReflexive)
                .count(),
            peer.agent
                .remote_candidates()
                .filter(|c| c.kind() == CandidateKind::PeerReflexive)
                .count()
        );
    }
}

fn main() {
    for scenario in scenarios() {
        exercise(scenario);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_matrix() {
        for scenario in scenarios() {
            exercise(scenario);
        }
    }

    #[test]
    fn restart_rejects_old_authenticated_stun() {
        let mut lab = Lab::new(scenarios()[0]);
        lab.drain_signals();
        lab.peers[0].agent.handle_timeout(lab.epoch);
        let tx = lab.peers[0].agent.poll_transmit().unwrap();
        let message = StunMessage::parse(&tx.contents).unwrap();
        assert!(message.is_binding_request());
        assert!(lab.peers[1].agent.accepts_message(&message));
        lab.restart();
        lab.drain_signals();
        assert!(!lab.peers[1].agent.accepts_message(&message));
    }

    #[test]
    fn mapping_and_filtering_are_independent() {
        let local = inside(0, 0);
        let r1: SocketAddr = "203.0.113.1:1000".parse().unwrap();
        let r2: SocketAddr = "203.0.113.1:2000".parse().unwrap();
        let stranger = "203.0.113.2:1000".parse().unwrap();
        for mapping in [Mapping::Independent, Mapping::Endpoint] {
            for filtering in [
                Filtering::Independent,
                Filtering::Address,
                Filtering::Endpoint,
            ] {
                let mut nat = Nat::new(0, mapping, filtering);
                let a = nat.outbound(local, r1);
                assert_eq!(nat.inbound(r1, a), Some(local));
                assert_eq!(
                    nat.inbound(r2, a).is_some(),
                    filtering != Filtering::Endpoint
                );
                assert_eq!(
                    nat.inbound(stranger, a).is_some(),
                    filtering == Filtering::Independent
                );
                let b = nat.outbound(local, r2);
                assert_eq!(a == b, mapping == Mapping::Independent);
                assert_eq!(nat.inbound(r2, b), Some(local));
                assert!(nat.inbound(r1, SocketAddr::new(nat.ip, 60_000)).is_none());
            }
        }
    }
}
