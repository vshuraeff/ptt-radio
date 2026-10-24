#[path = "netlab/router.rs"]
mod router;

use is::CandidateKind;
use ptt_radio_companion::connectivity::*;
use ptt_radio_companion::transport::signaling::{
    Announce, CAP_MEDIA_V2, CAP_UDP, PROTOCOL_VERSION, Signal, Suite,
};
use ptt_radio_companion::transport::wire::{BindChallenge, Datagram, encode_datagram};
use ptt_radio_companion::transport::{Path, StreamId};
use router::{Core, Edge, Filtering, Link, Mapping, NatConfig, Verdict};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}
fn v4(s: SocketAddr) -> SocketAddrV4 {
    match s {
        SocketAddr::V4(a) => a,
        _ => panic!("ipv4 lab"),
    }
}
fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn stream(player: u16) -> StreamId {
    StreamId {
        player,
        generation: u64::from(player) + 100,
    }
}
fn announce(peer: &PairConnectivity, player: u16) -> Signal {
    Signal::Announce(Announce {
        version: PROTOCOL_VERSION,
        capabilities: CAP_MEDIA_V2 | CAP_UDP,
        suites: vec![Suite::ChaChaPoly],
        stream_generation: stream(player).generation,
        static_key: [player as u8; 32],
        ice: peer.local_credentials(),
        candidates: peer.initial_candidates().to_vec(),
        candidates_complete: peer.initial_gathering_complete(),
    })
}

fn pair(
    local: u16,
    socket: SocketAddr,
    config: ConnectivityConfig,
    now: Instant,
) -> PairConnectivity {
    PairConnectivity::new(stream(local), stream(3 - local), vec![socket], config, now).unwrap()
}

// an ordinary RFC 5389 public-STUN success reply, with no MESSAGE-INTEGRITY.
fn reflection_reply(request: &[u8], mapped: SocketAddr) -> Vec<u8> {
    let mut value = vec![0, if mapped.is_ipv4() { 1 } else { 2 }];
    value.extend_from_slice(&(mapped.port() ^ 0x2112).to_be_bytes());
    match mapped.ip() {
        IpAddr::V4(ip) => value.extend_from_slice(&(u32::from(ip) ^ 0x2112_a442).to_be_bytes()),
        IpAddr::V6(ip) => {
            let mask = [0x21, 0x12, 0xa4, 0x42]
                .into_iter()
                .chain(request[8..20].iter().copied());
            value.extend(ip.octets().into_iter().zip(mask).map(|(a, b)| a ^ b));
        }
    }
    let mut reply = vec![1, 1];
    reply.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
    reply.extend_from_slice(&request[4..20]);
    reply.extend_from_slice(&[0, 0x20]);
    reply.extend_from_slice(&(value.len() as u16).to_be_bytes());
    reply.extend(value);
    reply
}

struct Lab {
    peers: [PairConnectivity; 2],
    sockets: [SocketAddr; 2],
    reflector: SocketAddr,
    core: Core,
    epoch: Instant,
    now: Duration,
    hide_reflection: bool,
    last_serial: [u32; 2],
    reordered: usize,
    last_payload: [Vec<u8>; 2],
    send_serial: [u32; 2],
}

impl Lab {
    fn new(
        mapping: [Mapping; 2],
        filtering: [Filtering; 2],
        impaired: bool,
        hide_reflection: bool,
    ) -> Self {
        let epoch = Instant::now();
        let sockets = [addr("10.1.0.1:5000"), addr("10.2.0.1:5000")];
        let reflector = addr("203.0.113.90:3478");
        let config = ConnectivityConfig {
            discovery: DiscoveryConfig {
                stun_servers: vec![reflector],
                ..Default::default()
            },
            ..Default::default()
        };
        let peers = std::array::from_fn(|i| pair(i as u16 + 1, sockets[i], config.clone(), epoch));
        let mut core = Core::new(713);
        for i in 0..2 {
            core.add_player(
                sockets[i],
                Ipv4Addr::new(198, 51, 100, i as u8 + 1),
                NatConfig {
                    id: i as u32,
                    mapping: mapping[i],
                    filtering: filtering[i],
                    timeout: Duration::from_secs(15),
                    ..Default::default()
                },
                Edge {
                    delay: ms(10),
                    ..Default::default()
                },
            );
        }
        core.add_server(reflector, v4(reflector), Edge::default());
        if impaired {
            for (a, b) in [(0, 1), (1, 0)] {
                core.set_link(
                    sockets[a],
                    sockets[b],
                    Link {
                        delay: ms(30),
                        jitter: ms(25),
                        loss: 0.12,
                        cuts: Vec::new(),
                    },
                );
            }
        }
        Self {
            peers,
            sockets,
            reflector,
            core,
            epoch,
            now: Duration::ZERO,
            hide_reflection,
            last_serial: [0; 2],
            reordered: 0,
            last_payload: [Vec::new(), Vec::new()],
            send_serial: [0; 2],
        }
    }

    fn step(&mut self) {
        let now = self.epoch + self.now;
        let mut signals = Vec::new();
        for i in 0..2 {
            self.peers[i].handle_timeout(now).unwrap();
            while let Some(action) = self.peers[i].poll_action() {
                match action {
                    Action::Send(tx) | Action::Discovery(DiscoveryAction::Send(tx)) => self
                        .core
                        .send(self.now, tx.source, v4(tx.destination), &tx.contents),
                    Action::PublishDescription | Action::PublishCandidates => {
                        let updates = if action == Action::PublishDescription {
                            vec![announce(&self.peers[i], i as u16 + 1)]
                        } else {
                            self.peers[i].candidate_signals().unwrap()
                        };
                        for mut signal in updates {
                            if self.hide_reflection && i == 0 {
                                if let Signal::Announce(a) = &mut signal {
                                    a.candidates.retain(|s| !s.contains("typ srflx"));
                                }
                                if let Signal::IceCandidates { candidates, .. } = &mut signal {
                                    candidates.retain(|s| !s.contains("typ srflx"));
                                }
                            }
                            signals.push((i, signal));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (from, signal) in signals {
            self.peers[1 - from]
                .handle_signal(from as u16 + 1, &signal, now)
                .unwrap();
        }
        for delivery in self.core.due(self.now) {
            if delivery.to == self.reflector {
                if self.hide_reflection && *delivery.from.ip() == Ipv4Addr::new(198, 51, 100, 1) {
                    continue;
                }
                let reply = reflection_reply(&delivery.payload, delivery.from.into());
                self.core
                    .send(self.now, self.reflector, delivery.from, &reply);
            } else if let Some(i) = self.sockets.iter().position(|s| *s == delivery.to) {
                let admission = self.peers[i]
                    .receive(delivery.from.into(), delivery.to, &delivery.payload, now)
                    .unwrap();
                if delivery.payload.first() == Some(&8)
                    && delivery.payload.len() == 5
                    && matches!(admission, ReceiveResult::UntrustedApplication(_))
                {
                    let serial = u32::from_be_bytes(delivery.payload[1..5].try_into().unwrap());
                    if serial < self.last_serial[i] {
                        self.reordered += 1;
                    }
                    self.last_serial[i] = self.last_serial[i].max(serial);
                    self.last_payload[i] = delivery.payload.clone();
                }
            }
        }
        self.now += ms(5);
    }

    fn run(&mut self, duration: Duration) {
        let until = self.now + duration;
        while self.now < until {
            self.step();
        }
    }
    fn connected(&self) -> bool {
        self.peers.iter().all(|p| p.direct().is_some())
    }
    fn connect(&mut self) -> bool {
        let until = self.now + Duration::from_secs(12);
        while self.now < until && !self.connected() {
            self.step();
        }
        self.connected()
    }
    fn send_application(&mut self) {
        for i in 0..2 {
            let route = self.peers[i].direct().unwrap();
            self.send_serial[i] += 1;
            let mut bytes = vec![8];
            bytes.extend_from_slice(&self.send_serial[i].to_be_bytes());
            self.core
                .send(self.now, route.source, v4(route.destination), &bytes);
        }
    }
}

#[test]
fn ice_through_real_nat_core_mapping_and_filtering() {
    for filtering in [
        Filtering::EndpointIndependent,
        Filtering::AddressDependent,
        Filtering::AddressAndPortDependent,
    ] {
        let mut lab = Lab::new(
            [Mapping::EndpointIndependent; 2],
            [filtering; 2],
            false,
            false,
        );
        assert!(lab.connect(), "{filtering:?}");
        for peer in &lab.peers {
            assert!(peer.candidate_count(true, CandidateKind::ServerReflexive) > 0);
        }
        lab.send_application();
        lab.run(ms(100));
        assert!(lab.last_payload.iter().all(|p| !p.is_empty()));
        assert!(
            lab.core
                .trace()
                .iter()
                .any(|t| t.verdict == Verdict::Forwarded)
        );
    }
    let mut lab = Lab::new(
        [Mapping::EndpointIndependent, Mapping::EndpointDependent],
        [
            Filtering::EndpointIndependent,
            Filtering::AddressAndPortDependent,
        ],
        false,
        false,
    );
    assert!(lab.connect());
    let mut blocked = Lab::new(
        [Mapping::EndpointDependent; 2],
        [Filtering::AddressAndPortDependent; 2],
        false,
        false,
    );
    assert!(!blocked.connect());
    assert!(
        blocked
            .core
            .trace()
            .iter()
            .any(|t| t.verdict == Verdict::Filtered)
    );
}

#[test]
fn real_peer_reflexive_learning_without_signaled_reflexive_candidate() {
    let mut lab = Lab::new(
        [Mapping::EndpointIndependent; 2],
        [
            Filtering::AddressAndPortDependent,
            Filtering::EndpointIndependent,
        ],
        false,
        true,
    );
    assert!(lab.connect());
    assert!(lab.peers[0].candidate_count(true, CandidateKind::PeerReflexive) > 0);
    assert!(lab.peers[1].candidate_count(false, CandidateKind::PeerReflexive) > 0);
    lab.send_application();
    lab.run(ms(100));
    assert!(lab.last_payload.iter().all(|p| !p.is_empty()));
}

#[test]
fn ice_survives_loss_reorder_and_one_sided_restart() {
    let mut lab = Lab::new(
        [Mapping::EndpointIndependent; 2],
        [Filtering::AddressAndPortDependent; 2],
        true,
        false,
    );
    assert!(lab.connect());
    for _ in 0..120 {
        lab.send_application();
        lab.run(ms(5));
    }
    lab.run(ms(100));
    assert!(lab.reordered > 0);
    assert!(lab.core.trace().iter().any(|t| t.verdict == Verdict::Lost));
    let previous = announce(&lab.peers[0], 1);
    let old_route = lab.peers[0].direct().unwrap();
    let old_credentials = lab.peers[0].local_credentials();
    lab.peers[0].restart(lab.epoch + lab.now).unwrap();
    assert!(lab.peers[0].direct().is_none());
    assert!(lab.peers[0].local_credentials().ufrag != old_credentials.ufrag);
    assert!(lab.peers[0].local_credentials().password != old_credentials.password);
    assert!(lab.connect());
    assert_ne!(lab.peers[0].direct().unwrap().id, old_route.id);
    let previous_serial = lab.last_serial;
    for _ in 0..10 {
        lab.send_application();
        lab.run(ms(20));
    }
    lab.run(ms(100));
    assert!(
        lab.last_serial
            .iter()
            .zip(previous_serial)
            .all(|(&new, old)| new > old)
    );
    assert_eq!(
        lab.peers[1]
            .handle_signal(1, &previous, lab.epoch + lab.now)
            .unwrap(),
        SignalResult::Stale
    );
    assert!(
        lab.peers[0]
            .probes_mut()
            .authenticated_pong(old_route.id, 7, lab.epoch + lab.now)
            .is_empty()
    );
}

#[test]
fn ipv6_full_ice_and_public_stun_reflection() {
    let now = Instant::now();
    let sockets = [addr("[2001:db8::1]:5000"), addr("[2001:db8::2]:5000")];
    let server = addr("[2001:db8::90]:3478");
    let mut peers = std::array::from_fn::<_, 2, _>(|i| {
        pair(
            i as u16 + 1,
            sockets[i],
            ConnectivityConfig {
                discovery: DiscoveryConfig {
                    stun_servers: vec![server],
                    ..Default::default()
                },
                ..Default::default()
            },
            now,
        )
    });
    for tick in 0..1200 {
        let time = now + ms(tick * 5);
        let mut signals = Vec::new();
        let mut packets = Vec::new();
        for (i, peer) in peers.iter_mut().enumerate() {
            peer.handle_timeout(time).unwrap();
            while let Some(action) = peer.poll_action() {
                match action {
                    Action::PublishDescription => signals.push((i, announce(peer, i as u16 + 1))),
                    Action::PublishCandidates => signals.extend(
                        peer.candidate_signals()
                            .unwrap()
                            .into_iter()
                            .map(|signal| (i, signal)),
                    ),
                    Action::Send(tx) => packets.push((i, tx)),
                    Action::Discovery(DiscoveryAction::Send(tx)) => {
                        let mapped = SocketAddr::new(tx.source.ip(), 6000);
                        let reply = reflection_reply(&tx.contents, mapped);
                        assert_eq!(
                            peer.receive(server, tx.source, &reply, time).unwrap(),
                            ReceiveResult::Reflection
                        );
                    }
                    _ => {}
                }
            }
        }
        for (i, signal) in signals {
            peers[1 - i]
                .handle_signal(i as u16 + 1, &signal, time)
                .unwrap();
        }
        for (i, tx) in packets {
            if tx.destination == sockets[1 - i] {
                peers[1 - i]
                    .receive(tx.source, tx.destination, &tx.contents, time)
                    .unwrap();
            }
        }
        if peers.iter().all(|p| p.direct().is_some()) {
            break;
        }
    }
    assert!(peers.iter().all(|p| p.direct().is_some()));
    assert!(
        peers
            .iter()
            .all(|p| p.candidate_count(true, CandidateKind::ServerReflexive) > 0)
    );
}

#[test]
fn public_stun_parallel_queries_spoof_and_deadline() {
    let now = Instant::now();
    let local = addr("10.1.0.1:5000");
    let servers = [addr("203.0.113.1:3478"), addr("203.0.113.2:3478")];
    let mut peer = pair(
        1,
        local,
        ConnectivityConfig {
            discovery: DiscoveryConfig {
                stun_servers: servers.to_vec(),
                timeout: ms(300),
                ..Default::default()
            },
            ..Default::default()
        },
        now,
    );
    let original = announce(&peer, 1);
    let mut requests = Vec::new();
    while let Some(action) = peer.poll_action() {
        if let Action::Discovery(DiscoveryAction::Send(tx)) = action {
            requests.push(tx);
        }
    }
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|tx| tx.source == local));
    assert_eq!(requests[0].contents.len(), 20);
    let mapped = addr("198.51.100.1:40000");
    let reply = reflection_reply(&requests[0].contents, mapped);
    assert_eq!(
        peer.receive(servers[1], local, &reply, now + ms(10))
            .unwrap(),
        ReceiveResult::Dropped
    );
    let mut foreign = reply.clone();
    foreign[8] ^= 1;
    assert_eq!(
        peer.receive(servers[0], local, &foreign, now + ms(10))
            .unwrap(),
        ReceiveResult::Dropped
    );
    let mut malformed = reply.clone();
    malformed[23] = 100;
    assert_eq!(
        peer.receive(servers[0], local, &malformed, now + ms(10))
            .unwrap(),
        ReceiveResult::Dropped
    );
    assert_eq!(
        peer.receive(servers[0], local, &reply, now + ms(10))
            .unwrap(),
        ReceiveResult::Reflection
    );
    assert!(announce(&peer, 1) == original);
    let Signal::Announce(frozen) = original else {
        unreachable!()
    };
    assert_eq!(frozen.candidates.len(), 1);
    let Signal::IceCandidates {
        candidates,
        complete,
        ..
    } = peer.candidate_signals().unwrap().remove(0)
    else {
        unreachable!()
    };
    assert_eq!(candidates.len(), 2);
    assert!(!complete);
    assert_eq!(
        peer.candidate_count(true, CandidateKind::ServerReflexive),
        1
    );
    assert!(!peer.gathering_complete());
    peer.handle_timeout(now + ms(300)).unwrap();
    assert!(peer.gathering_complete());
    let late = reflection_reply(&requests[1].contents, addr("198.51.100.1:40001"));
    assert_eq!(
        peer.receive(servers[1], local, &late, now + ms(300))
            .unwrap(),
        ReceiveResult::Dropped
    );
    assert_eq!(
        peer.candidate_count(true, CandidateKind::ServerReflexive),
        1
    );
}

#[test]
fn hub_is_preferred_and_only_verified_matching_bind_transcript_reflects() {
    let now = Instant::now();
    let local = addr("10.1.0.1:5000");
    let hub = addr("203.0.113.1:4000");
    let mut peer = pair(
        1,
        local,
        ConnectivityConfig {
            discovery: DiscoveryConfig {
                hub: Some(HubReflection {
                    endpoint: hub,
                    session: 4,
                    hub_generation: 9,
                }),
                stun_servers: vec![addr("203.0.113.2:3478")],
                ..Default::default()
            },
            ..Default::default()
        },
        now,
    );
    let mut binding = None;
    while let Some(action) = peer.poll_action() {
        match action {
            Action::Discovery(DiscoveryAction::BeginHubBinding {
                source,
                hub: endpoint,
                request,
            }) => {
                assert_eq!(source, local);
                assert_eq!(endpoint, hub);
                assert_eq!(request.player, 1);
                binding = Some(request);
            }
            Action::Discovery(DiscoveryAction::Send(_)) => panic!("hub must suppress public STUN"),
            _ => {}
        }
    }
    let request = binding.unwrap();
    let challenge = BindChallenge {
        request,
        endpoint: addr("198.51.100.1:40000"),
        challenge: [7; 32],
        expires_at_ms: 10000,
    };
    let raw_ack = encode_datagram(Datagram::BindAck {
        challenge,
        proof: [0; 32],
    })
    .unwrap();
    assert_eq!(
        peer.receive(hub, local, &raw_ack, now).unwrap(),
        ReceiveResult::Dropped
    );
    assert_eq!(
        peer.candidate_count(true, CandidateKind::ServerReflexive),
        0
    );
    let mut wrong = challenge;
    wrong.request.attempt += 1;
    assert!(
        !peer
            .authenticated_hub_reflection(local, hub, wrong, now)
            .unwrap()
    );
    assert!(
        !peer
            .authenticated_hub_reflection(local, addr("203.0.113.2:4000"), challenge, now)
            .unwrap()
    );
    assert!(
        peer.authenticated_hub_reflection(local, hub, challenge, now)
            .unwrap()
    );
    assert!(
        !peer
            .authenticated_hub_reflection(local, hub, challenge, now)
            .unwrap()
    );
    assert!(peer.gathering_complete());
    assert_eq!(
        peer.candidate_count(true, CandidateKind::ServerReflexive),
        1
    );
}

#[test]
fn signal_generations_credentials_and_trickle_are_scoped_atomically() {
    let now = Instant::now();
    let mut a = pair(
        1,
        addr("192.0.2.1:5000"),
        ConnectivityConfig::default(),
        now,
    );
    let mut b = pair(
        2,
        addr("192.0.2.2:5000"),
        ConnectivityConfig::default(),
        now,
    );
    let signal = announce(&b, 2);
    assert_eq!(
        a.handle_signal(7, &signal, now).unwrap(),
        SignalResult::WrongStream
    );
    assert_eq!(
        a.handle_signal(2, &signal, now).unwrap(),
        SignalResult::Applied
    );
    assert_eq!(
        a.handle_signal(2, &signal, now).unwrap(),
        SignalResult::Duplicate
    );
    let Signal::Announce(mut mutated) = signal.clone() else {
        unreachable!()
    };
    let extra = is::Candidate::host(addr("192.0.2.3:5000"), "udp")
        .unwrap()
        .to_sdp_string();
    mutated.candidates.push(extra.clone());
    assert!(a.handle_signal(2, &Signal::Announce(mutated), now).is_err());
    assert_eq!(a.candidate_count(false, CandidateKind::Host), 1);
    let trickle = Signal::IceCandidates {
        stream_generation: stream(2).generation,
        restart_generation: 0,
        candidates: vec![extra],
        complete: true,
    };
    assert_eq!(
        a.handle_signal(2, &trickle, now).unwrap(),
        SignalResult::Applied
    );
    assert_eq!(a.candidate_count(false, CandidateKind::Host), 2);
    assert_eq!(
        a.handle_signal(2, &signal, now).unwrap(),
        SignalResult::Duplicate
    );
    let Signal::Announce(mut wrong) = signal.clone() else {
        unreachable!()
    };
    wrong.ice.password.push('a');
    assert!(a.handle_signal(2, &Signal::Announce(wrong), now).is_err());
    let stale_trickle = Signal::IceCandidates {
        stream_generation: stream(2).generation,
        restart_generation: 1,
        candidates: b.local_candidates(),
        complete: true,
    };
    assert_eq!(
        a.handle_signal(2, &stale_trickle, now).unwrap(),
        SignalResult::Stale
    );
    b.restart(now).unwrap();
    assert_eq!(
        a.handle_signal(2, &announce(&b, 2), now).unwrap(),
        SignalResult::Applied
    );
    assert_eq!(
        a.handle_signal(2, &signal, now).unwrap(),
        SignalResult::Stale
    );
    let mut current = stream(2);
    current.generation += 1;
    a.select_remote(current, now).unwrap();
    assert_eq!(
        a.handle_signal(2, &announce(&b, 2), now).unwrap(),
        SignalResult::WrongStream
    );
    let Signal::Announce(mut invalid) = announce(&b, 2) else {
        unreachable!()
    };
    invalid.stream_generation = current.generation;
    invalid.candidates.push("not a candidate".into());
    assert!(a.handle_signal(2, &Signal::Announce(invalid), now).is_err());
    assert_eq!(a.candidate_count(false, CandidateKind::Host), 0);
}

#[test]
fn stale_stun_and_endpoint_media_are_not_peer_authentication() {
    let mut lab = Lab::new(
        [Mapping::EndpointIndependent; 2],
        [Filtering::AddressAndPortDependent; 2],
        false,
        false,
    );
    assert!(lab.connect());
    let now = lab.epoch + lab.now;
    let route = lab.peers[0].direct().unwrap();
    assert_eq!(
        lab.peers[0]
            .receive(route.destination, route.source, &[4, 0, 0], now)
            .unwrap(),
        ReceiveResult::UntrustedApplication(route)
    );
    assert_eq!(
        lab.peers[0]
            .receive(addr("198.51.100.9:40000"), route.source, &[4, 0, 0], now)
            .unwrap(),
        ReceiveResult::Dropped
    );
    assert_eq!(
        lab.peers[0]
            .receive(route.destination, addr("10.1.0.9:5000"), &[4, 0, 0], now)
            .unwrap(),
        ReceiveResult::Dropped
    );
    // capture a real authenticated request from this ICE generation.
    let mut old_request = None;
    for tick in 1..400 {
        lab.peers[1].handle_timeout(now + ms(tick * 5)).unwrap();
        while let Some(action) = lab.peers[1].poll_action() {
            if let Action::Send(tx) = action
                && tx.contents.get(..2) == Some(&[0, 1])
            {
                old_request = Some(tx);
                break;
            }
        }
        if old_request.is_some() {
            break;
        }
    }
    let old_request = old_request.expect("ICE consent request");
    let later = now + ms(2500);
    lab.peers[0].restart(later).unwrap();
    assert_eq!(
        lab.peers[0]
            .receive(
                route.destination,
                route.source,
                &old_request.contents,
                later
            )
            .unwrap(),
        ReceiveResult::Dropped
    );
    assert_eq!(
        lab.peers[0]
            .receive(route.destination, route.source, &[4, 0, 0], later)
            .unwrap(),
        ReceiveResult::Dropped
    );
}

#[test]
fn simultaneous_restart_and_lost_initial_signal_recover() {
    let mut lab = Lab::new(
        [Mapping::EndpointIndependent; 2],
        [Filtering::AddressAndPortDependent; 2],
        false,
        false,
    );
    // drop both initial descriptions; periodic immutable retries restore them.
    for peer in &mut lab.peers {
        let mut discovery = Vec::new();
        while let Some(action) = peer.poll_action() {
            if let Action::Discovery(DiscoveryAction::Send(tx)) = action {
                discovery.push(tx);
            }
        }
        for tx in discovery {
            lab.core
                .send(Duration::ZERO, tx.source, v4(tx.destination), &tx.contents);
        }
    }
    assert!(lab.connect());
    assert!(lab.now >= Duration::from_secs(1));
    let now = lab.epoch + lab.now;
    for peer in &mut lab.peers {
        peer.restart(now).unwrap();
    }
    assert!(lab.connect());
    lab.send_application();
    lab.run(ms(100));
    assert!(lab.last_payload.iter().all(|p| !p.is_empty()));
}

#[test]
fn timer_deadlines_signal_retry_and_bounded_fallback() {
    let now = Instant::now();
    let mut peer = pair(
        1,
        addr("192.0.2.1:5000"),
        ConnectivityConfig {
            attempt_timeout: ms(1500),
            ..Default::default()
        },
        now,
    );
    while peer.poll_action().is_some() {}
    peer.handle_timeout(now + ms(999)).unwrap();
    let mut publish = 0;
    while let Some(action) = peer.poll_action() {
        if action == Action::PublishDescription {
            publish += 1;
        }
    }
    assert_eq!(publish, 0);
    peer.handle_timeout(now + ms(1000)).unwrap();
    assert!(std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::PublishDescription));
    assert!(peer.deadline().is_some_and(|d| d <= now + ms(1500)));
    peer.handle_timeout(now + ms(1500)).unwrap();
    assert!(std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::AttemptTimedOut));
    peer.handle_timeout(now + ms(5000)).unwrap();
    assert!(!std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::AttemptTimedOut));
}

#[test]
fn demux_requires_cookie_exact_aligned_length_and_header() {
    let request = [
        0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    assert_eq!(classify_datagram(&request), DatagramClass::Stun);
    for length in 0..20 {
        assert_eq!(
            classify_datagram(&request[..length]),
            DatagramClass::Invalid
        );
    }
    for index in [0, 2, 3, 4, 5, 6, 7] {
        let mut bad = request;
        bad[index] ^= 3;
        if index != 0 {
            assert_eq!(classify_datagram(&bad), DatagramClass::Invalid);
        }
    }
    let mut extra = request.to_vec();
    extra.extend_from_slice(&[0; 4]);
    assert_eq!(classify_datagram(&extra), DatagramClass::Invalid);
    for kind in 4..=8 {
        assert_eq!(classify_datagram(&[kind]), DatagramClass::Application);
    }
}

fn ping(events: Vec<ProbeEvent>, path: Path) -> (RouteId, u64) {
    events
        .into_iter()
        .find_map(|e| match e {
            ProbeEvent::SendPing { route, token } if route.path == path => Some((route, token)),
            _ => None,
        })
        .unwrap()
}

#[test]
fn authenticated_probes_have_separate_rtt_and_precise_active_backup_deadlines() {
    let now = Instant::now();
    let mut probes = ProbeScheduler::default();
    let direct = probes.register(Path::Direct, now).unwrap();
    let hub = probes.register(Path::Hub { session: 8 }, now).unwrap();
    probes.set_activity(Path::Direct, true, now);
    let events = probes.poll(now).unwrap();
    let (_, token) = ping(events.clone(), Path::Direct);
    let (_, backup_token) = ping(events, hub.path);
    assert_eq!(probes.deadline(), Some(now + TALK_INTERVAL));
    let events = probes.authenticated_pong(direct, token, now + ms(20));
    assert!(events.contains(&ProbeEvent::NetworkRtt {
        route: direct,
        token,
        rtt: ms(20)
    }));
    assert!(probes.is_live(direct));
    probes.authenticated_pong(hub, backup_token, now + ms(20));
    assert!(probes.poll(now + ms(249)).unwrap().is_empty());
    assert_eq!(
        ping(probes.poll(now + ms(250)).unwrap(), Path::Direct).0,
        direct
    );
    assert!(
        !probes
            .poll(now + ms(1019))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route: direct })
    );
    assert!(
        probes
            .poll(now + ms(1020))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route: direct })
    );
    assert!(!probes.is_live(direct));
    assert_eq!(probes.rtt(direct), None);
    assert!(probes.is_live(hub));
    assert!(
        probes
            .poll(now + ms(3020))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route: hub })
    );
}

#[test]
fn probes_reject_unmatched_cross_route_stale_replayed_and_unvalidated_responses() {
    let now = Instant::now();
    let mut probes = ProbeScheduler::default();
    let old = probes.register(Path::Direct, now).unwrap();
    let hub = probes.register(Path::Hub { session: 1 }, now).unwrap();
    let (_, token) = ping(probes.poll(now).unwrap(), Path::Direct);
    assert!(
        probes
            .authenticated_pong(old, token ^ 1, now + ms(10))
            .is_empty()
    );
    assert!(
        probes
            .authenticated_pong(hub, token, now + ms(10))
            .is_empty()
    );
    assert!(probes.authenticated_ping(old, token).is_some());
    assert!(!probes.is_live(old));
    assert_eq!(probes.rtt(old), None);
    assert_eq!(probes.authenticated_pong(old, token, now + ms(10)).len(), 2);
    assert!(
        probes
            .authenticated_pong(old, token, now + ms(11))
            .is_empty()
    );
    let new = probes.register(Path::Direct, now + ms(20)).unwrap();
    assert_ne!(new, old);
    assert!(
        probes
            .authenticated_pong(old, token, now + ms(21))
            .is_empty()
    );
    let (_, fresh) = ping(probes.poll(now + ms(20)).unwrap(), Path::Direct);
    assert!(
        probes
            .authenticated_pong(new, fresh, now + ms(3020))
            .is_empty()
    );
    assert!(!probes.is_live(new));
}

#[test]
fn idle_keepalive_recovers_dead_route_and_activity_speeds_up_deadline() {
    let now = Instant::now();
    let mut probes = ProbeScheduler::default();
    let route = probes.register(Path::Game, now).unwrap();
    for second in 0..20 {
        let events = probes.poll(now + Duration::from_secs(second)).unwrap();
        let (id, token) = ping(events, Path::Game);
        assert_eq!(id, route);
        if second == 19 {
            assert!(
                !probes
                    .authenticated_pong(route, token, now + Duration::from_secs(second) + ms(10))
                    .is_empty()
            );
        }
    }
    assert!(probes.is_live(route));
    probes.set_activity(Path::Game, true, now + ms(19020));
    assert!(probes.deadline().is_some_and(|d| d <= now + ms(19270)));
}

#[test]
fn selecting_new_peer_stream_clears_route_tokens_without_reusing_revisions() {
    let now = Instant::now();
    let mut peer = pair(
        1,
        addr("192.0.2.1:5000"),
        ConnectivityConfig::default(),
        now,
    );
    let path = Path::PlayerRelay {
        player: 3,
        session: 8,
    };
    let old = peer.probes_mut().register(path, now).unwrap();
    let (_, token) = ping(peer.probes_mut().poll(now).unwrap(), path);
    peer.probes_mut()
        .authenticated_pong(old, token, now + ms(10));
    assert!(peer.probes().is_live(old));
    let mut remote = stream(2);
    remote.generation += 1;
    peer.select_remote(remote, now + ms(20)).unwrap();
    assert!(!peer.probes().is_live(old));
    assert!(
        std::iter::from_fn(|| peer.poll_action())
            .any(|a| a == Action::Probe(ProbeEvent::Unavailable { route: old }))
    );
    let new = peer.probes_mut().register(path, now + ms(20)).unwrap();
    assert!(new.revision > old.revision);
    assert!(
        peer.probes_mut()
            .authenticated_pong(old, token, now + ms(21))
            .is_empty()
    );
}

#[test]
fn signaling_retry_backoff_caps_at_eight_seconds_and_restart_resets_it() {
    let now = Instant::now();
    let mut peer = pair(
        1,
        addr("192.0.2.1:5000"),
        ConnectivityConfig::default(),
        now,
    );
    while peer.poll_action().is_some() {}
    for retry_at in [1000, 3000, 7000, 15000, 23000] {
        peer.handle_timeout(now + ms(retry_at - 1)).unwrap();
        assert!(
            !std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::PublishDescription)
        );
        peer.handle_timeout(now + ms(retry_at)).unwrap();
        assert!(std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::PublishDescription));
    }
    peer.restart(now + ms(23010)).unwrap();
    assert!(std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::PublishDescription));
    peer.handle_timeout(now + ms(24009)).unwrap();
    assert!(!std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::PublishDescription));
    peer.handle_timeout(now + ms(24010)).unwrap();
    assert!(std::iter::from_fn(|| peer.poll_action()).any(|a| a == Action::PublishDescription));
}

#[test]
fn four_ipv6_sockets_publish_reflections_within_signal_envelope_limit() {
    let now = Instant::now();
    let sockets: Vec<_> = (1..=4)
        .map(|i| {
            addr(&format!(
                "[2001:db8:ffff:ffff:ffff:ffff:ffff:{i:04x}]:65535"
            ))
        })
        .collect();
    let server = addr("[2001:db8::90]:3478");
    let mut peer = PairConnectivity::new(
        stream(1),
        stream(2),
        sockets,
        ConnectivityConfig {
            discovery: DiscoveryConfig {
                stun_servers: vec![server],
                ..Default::default()
            },
            ..Default::default()
        },
        now,
    )
    .unwrap();
    let initial = announce(&peer, 1);
    ptt_radio_companion::transport::signaling::encode_game(&initial, 2, false)
        .expect("initial host candidates fit the ANNOUNCE envelope");
    let mut remote = pair(
        2,
        addr("[2001:db8::2]:5000"),
        ConnectivityConfig::default(),
        now,
    );
    remote.handle_signal(1, &initial, now).unwrap();
    let mut count = 0;
    while let Some(action) = peer.poll_action() {
        if let Action::Discovery(DiscoveryAction::Send(tx)) = action {
            count += 1;
            let mapped = addr(&format!(
                "[2001:db8:eeee:eeee:eeee:eeee:eeee:{count:04x}]:65534"
            ));
            let reply = reflection_reply(&tx.contents, mapped);
            assert_eq!(
                peer.receive(server, tx.source, &reply, now).unwrap(),
                ReceiveResult::Reflection
            );
        }
    }
    assert_eq!(count, 4);
    assert_eq!(peer.local_candidates().len(), 8);
    let updates = peer.candidate_signals().unwrap();
    assert!(updates.len() > 1);
    let mut advertised = 0;
    for (index, update) in updates.iter().enumerate() {
        let bytes = ptt_radio_companion::transport::signaling::encode_game(update, 2, false)
            .expect("candidate update fits the game envelope");
        assert!(bytes.len() <= 1200);
        let Signal::IceCandidates {
            candidates,
            complete,
            ..
        } = update
        else {
            unreachable!()
        };
        advertised += candidates.len();
        assert_eq!(*complete, index + 1 == updates.len());
        remote.handle_signal(1, update, now).unwrap();
    }
    assert_eq!(advertised, 8);
    assert_eq!(remote.candidate_count(false, CandidateKind::Host), 4);
    assert_eq!(
        remote.candidate_count(false, CandidateKind::ServerReflexive),
        4
    );
}

fn live_idle_probe(now: Instant, path: Path) -> (ProbeScheduler, RouteId) {
    let mut probes = ProbeScheduler::default();
    let route = probes.register(path, now).unwrap();
    let (_, token) = ping(probes.poll(now).unwrap(), path);
    probes.authenticated_pong(route, token, now + ms(20));
    assert!(probes.is_live(route));
    (probes, route)
}

#[test]
fn probe_activity_transition_with_jitter_sends_immediately_without_flapping() {
    let now = Instant::now();
    let (mut probes, route) = live_idle_probe(now, Path::Direct);
    probes.set_activity(Path::Direct, true, now + ms(900));
    assert_eq!(probes.deadline(), Some(now + ms(900)));
    let events = probes.poll(now + ms(900)).unwrap();
    assert!(!events.contains(&ProbeEvent::Unavailable { route }));
    let (_, token) = ping(events, route.path);
    assert!(
        !probes
            .poll(now + ms(1020))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route })
    );
    let events = probes.authenticated_pong(route, token, now + ms(1030));
    assert!(!events.contains(&ProbeEvent::Available { route }));
    assert_eq!(probes.rtt(route), Some(ms(130)));
    assert!(probes.is_live(route));
}

#[test]
fn probe_activity_transition_after_missing_idle_pong_gets_one_talk_second() {
    let now = Instant::now();
    let (mut probes, route) = live_idle_probe(now, Path::Direct);
    // the one-second idle probe is lost; its route is still healthy under 3 s.
    ping(probes.poll(now + ms(1000)).unwrap(), route.path);
    probes.set_activity(Path::Direct, true, now + ms(1100));
    assert_eq!(probes.deadline(), Some(now + ms(1100)));
    let events = probes.poll(now + ms(1100)).unwrap();
    assert!(!events.contains(&ProbeEvent::Unavailable { route }));
    ping(events, route.path);
    assert!(
        !probes
            .poll(now + ms(2099))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route })
    );
    assert!(
        probes
            .poll(now + ms(2100))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route })
    );
    assert!(!probes.is_live(route));
}

#[test]
fn probe_activity_transition_to_healthy_backup_preserves_its_liveness() {
    let now = Instant::now();
    let path = Path::Hub { session: 8 };
    let (mut probes, backup) = live_idle_probe(now, path);
    let direct = probes.register(Path::Direct, now + ms(20)).unwrap();
    probes.set_activity(Path::Direct, true, now + ms(20));
    ping(probes.poll(now + ms(20)).unwrap(), Path::Direct);
    assert!(
        probes
            .poll(now + ms(1020))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route: direct })
    );
    assert!(probes.is_live(backup));
    probes.set_activity(path, true, now + ms(1220));
    let events = probes.poll(now + ms(1220)).unwrap();
    assert!(!events.contains(&ProbeEvent::Unavailable { route: backup }));
    let (_, token) = ping(events, path);
    let events = probes.authenticated_pong(backup, token, now + ms(1250));
    assert!(!events.contains(&ProbeEvent::Available { route: backup }));
    assert!(probes.is_live(backup));
}

#[test]
fn probe_activity_transition_repeated_identical_calls_cannot_defer_death() {
    let now = Instant::now();
    let (mut probes, route) = live_idle_probe(now, Path::Direct);
    probes.set_activity(Path::Direct, true, now + ms(500));
    let mut unavailable = Vec::new();
    for tick in 500..=1500 {
        probes.set_activity(Path::Direct, true, now + ms(tick));
        for event in probes.poll(now + ms(tick)).unwrap() {
            assert!(!matches!(event, ProbeEvent::Available { .. }));
            if event == (ProbeEvent::Unavailable { route }) {
                unavailable.push(tick);
            }
        }
    }
    assert_eq!(unavailable, vec![1500]);
    assert!(!probes.is_live(route));
}

#[test]
fn probe_activity_transition_does_not_revive_an_already_dead_route() {
    let now = Instant::now();
    let (mut probes, route) = live_idle_probe(now, Path::Direct);
    assert!(
        probes
            .poll(now + ms(3020))
            .unwrap()
            .contains(&ProbeEvent::Unavailable { route })
    );
    for (at, talking) in [(3100, true), (3200, false), (3300, true)] {
        probes.set_activity(Path::Direct, talking, now + ms(at));
        assert!(!probes.is_live(route));
        assert!(
            !probes
                .poll(now + ms(at))
                .unwrap()
                .iter()
                .any(|event| matches!(event, ProbeEvent::Available { .. }))
        );
    }
}

#[test]
fn probe_activity_transition_does_not_grant_grace_after_idle_deadline_expired() {
    let now = Instant::now();
    let (mut probes, route) = live_idle_probe(now, Path::Direct);
    // the owner missed the idle deadline; switching modes must not hide it.
    probes.set_activity(Path::Direct, true, now + ms(4000));
    let events = probes.poll(now + ms(4000)).unwrap();
    assert!(events.contains(&ProbeEvent::Unavailable { route }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProbeEvent::Available { .. }))
    );
    assert!(!probes.is_live(route));
}
