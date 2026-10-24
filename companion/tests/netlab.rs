//! NAT and link router core of the p2p lab: mappings, filtering, hairpin, timeouts, delay, loss,
//! cuts and hole punching on injected time, plus one test through the real udp pump.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

#[path = "netlab/router.rs"]
mod router;

use router::{
    Core, Edge, Filtering, Link, MAX_DATAGRAM, Mapping, NatConfig, PREFIX, Pump, Verdict,
    decode_addr, encode_addr,
};

const MS: Duration = Duration::from_millis(1);
const FAR: Duration = Duration::from_secs(3600);

fn private(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9000 + n))
}

fn public(host: u8, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, host), port)
}

fn nat(id: u32, mapping: Mapping, filtering: Filtering) -> NatConfig {
    NatConfig {
        id,
        mapping,
        filtering,
        ..NatConfig::default()
    }
}

fn core() -> Core {
    Core::new(7)
}

// sends one datagram and reports whether it was delivered at `now`
fn delivered(core: &mut Core, now: Duration, from: SocketAddr, to: SocketAddrV4) -> bool {
    core.send(now, from, to, b"x");
    !core.due(now).is_empty()
}

// the public address the destination sees for `from`, like a stun reply
fn learn(core: &mut Core, now: Duration, from: SocketAddr, server: SocketAddrV4) -> SocketAddrV4 {
    core.send(now, from, server, b"hello");
    let got = core.due(now);
    assert_eq!(got.len(), 1);
    got[0].from
}

fn verdicts(core: &Core) -> Vec<Verdict> {
    core.trace().iter().map(|t| t.verdict).collect()
}

#[test]
fn envelope_round_trips() {
    let addr = public(9, 40123);
    assert_eq!(decode_addr(&encode_addr(addr)), Some(addr));
    assert_eq!(decode_addr(&[1, 2, 3]), None);
}

#[test]
fn malformed_and_unknown_sources_are_traced() {
    let mut core = core();
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        Edge::default(),
    );
    core.send_raw(Duration::ZERO, private(1), &[1, 2, 3]);
    core.send(Duration::ZERO, private(2), public(50, 1), b"x");
    assert_eq!(
        verdicts(&core),
        [Verdict::Malformed, Verdict::UnknownSource]
    );
    assert!(core.due(FAR).is_empty());
}

fn two_servers(core: &mut Core) -> (SocketAddrV4, SocketAddrV4) {
    let (s1, s2) = (public(100, 3478), public(101, 3478));
    core.add_server(private(101), s1, Edge::default());
    core.add_server(private(102), s2, Edge::default());
    (s1, s2)
}

#[test]
fn eim_reuses_one_public_port() {
    let mut core = core();
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        nat(
            1,
            Mapping::EndpointIndependent,
            Filtering::EndpointIndependent,
        ),
        Edge::default(),
    );
    let (s1, s2) = two_servers(&mut core);
    let a = learn(&mut core, Duration::ZERO, private(1), s1);
    let b = learn(&mut core, Duration::ZERO, private(1), s2);
    assert_eq!(a, b);
}

#[test]
fn edm_allocates_a_port_per_destination() {
    let mut core = core();
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        nat(
            1,
            Mapping::EndpointDependent,
            Filtering::EndpointIndependent,
        ),
        Edge::default(),
    );
    let (s1, s2) = two_servers(&mut core);
    let a = learn(&mut core, Duration::ZERO, private(1), s1);
    let b = learn(&mut core, Duration::ZERO, private(1), s2);
    let again = learn(&mut core, Duration::ZERO, private(1), s1);
    assert_eq!(a.ip(), b.ip());
    assert_ne!(a.port(), b.port());
    assert_eq!(a, again);
}

#[test]
fn filtering_modes_admit_exactly_the_right_remotes() {
    // (filtering, admits same addr+port, same addr other port, other addr)
    let cases = [
        (Filtering::EndpointIndependent, [true, true, true]),
        (Filtering::AddressDependent, [true, true, false]),
        (Filtering::AddressAndPortDependent, [true, false, false]),
    ];
    for (filtering, expect) in cases {
        let mut core = core();
        core.add_player(
            private(1),
            Ipv4Addr::new(203, 0, 113, 1),
            nat(1, Mapping::EndpointIndependent, filtering),
            Edge::default(),
        );
        let known = public(100, 3478);
        let same_ip = public(100, 3479);
        let other = public(101, 3478);
        core.add_server(private(101), known, Edge::default());
        core.add_server(private(102), same_ip, Edge::default());
        core.add_server(private(103), other, Edge::default());
        let mine = learn(&mut core, Duration::ZERO, private(1), known);
        let got = [private(101), private(102), private(103)]
            .map(|server| delivered(&mut core, MS, server, mine));
        assert_eq!(got, expect, "{filtering:?}");
    }
}

#[test]
fn hairpin_works_on_and_fails_off() {
    for hairpin in [true, false] {
        let mut core = core();
        let config = NatConfig {
            hairpin,
            ..nat(
                1,
                Mapping::EndpointIndependent,
                Filtering::EndpointIndependent,
            )
        };
        let ip = Ipv4Addr::new(203, 0, 113, 1);
        core.add_player(private(1), ip, config, Edge::default());
        core.add_player(private(2), ip, config, Edge::default());
        let server = public(100, 3478);
        core.add_server(private(101), server, Edge::default());
        let a = learn(&mut core, Duration::ZERO, private(1), server);
        core.send(MS, private(2), a, b"x");
        let got = core.due(MS);
        assert_eq!(!got.is_empty(), hairpin);
        if hairpin {
            assert_eq!(got[0].to, private(1));
            assert_eq!(*got[0].from.ip(), ip);
        } else {
            assert_eq!(core.trace().last().unwrap().verdict, Verdict::Filtered);
        }
    }
}

#[test]
fn mapping_expires_without_outbound_traffic() {
    let mut core = core();
    let config = NatConfig {
        timeout: Duration::from_secs(10),
        ..NatConfig::default()
    };
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        config,
        Edge::default(),
    );
    let server = public(100, 3478);
    core.add_server(private(101), server, Edge::default());
    let s = Duration::from_secs;
    let mine = learn(&mut core, Duration::ZERO, private(1), server);
    assert!(delivered(&mut core, s(5), private(101), mine));
    // outbound at 8 s refreshes; the inbound at 5 s did not
    core.send(s(8), private(1), server, b"keepalive");
    assert_eq!(core.due(s(8)).len(), 1);
    assert!(delivered(&mut core, s(17), private(101), mine));
    assert!(!delivered(&mut core, s(19), private(101), mine));
    assert_eq!(core.trace().last().unwrap().verdict, Verdict::NoMapping);
    // a new outbound datagram builds a fresh mapping
    let fresh = learn(&mut core, s(20), private(1), server);
    assert_ne!(fresh, mine);
}

fn jittery(seed: u64) -> Core {
    let mut core = Core::new(seed);
    let edge = |delay, jitter| Edge {
        delay: Duration::from_millis(delay),
        jitter: Duration::from_millis(jitter),
        loss: 0.0,
    };
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        edge(20, 5),
    );
    core.add_server(private(101), public(100, 3478), edge(30, 5));
    core
}

#[test]
fn delay_and_jitter_stay_in_bounds_and_are_seeded() {
    let run = |seed| {
        let mut core = jittery(seed);
        for i in 0..200u32 {
            core.send(MS * i, private(1), public(100, 3478), &i.to_be_bytes());
        }
        let got = core.due(FAR);
        let at: Vec<Duration> = core.trace().iter().map(|t| t.at).collect();
        (got, at)
    };
    let (got, at) = run(3);
    assert_eq!(got.len(), 200);
    // delivery times are non-decreasing, and each lies within 50 ms +- 10 ms of its send time
    assert!(at.windows(2).all(|w| w[0] <= w[1]));
    for (i, delivery) in got.iter().enumerate() {
        let id = u32::from_be_bytes(delivery.payload[..4].try_into().unwrap());
        assert!(id < 200, "payload {i}");
        let arrived = at[i];
        let sent = MS * id;
        assert!(
            arrived >= sent + 40 * MS && arrived <= sent + 60 * MS,
            "{id}: {arrived:?}"
        );
    }
    let (_, again) = run(3);
    let (_, other) = run(4);
    assert_eq!(at, again);
    assert_ne!(at, other);
}

#[test]
fn due_holds_datagrams_until_their_time() {
    let mut core = jittery(1);
    core.send(Duration::ZERO, private(1), public(100, 3478), b"x");
    assert!(core.due(39 * MS).is_empty());
    assert_eq!(core.due(61 * MS).len(), 1);
}

#[test]
fn loss_rate_matches_the_configured_probability() {
    let mut core = core();
    let edge = Edge {
        loss: 0.2,
        ..Edge::default()
    };
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        edge,
    );
    core.add_server(private(101), public(100, 3478), Edge::default());
    for i in 0..10_000u32 {
        core.send(MS * i, private(1), public(100, 3478), b"x");
    }
    let lost = verdicts(&core)
        .iter()
        .filter(|v| **v == Verdict::Lost)
        .count();
    let rate = lost as f64 / 10_000.0;
    assert!((rate - 0.2).abs() < 0.02, "rate {rate}");
    assert_eq!(core.due(FAR).len(), 10_000 - lost);
}

#[test]
fn scheduled_cut_drops_exactly_its_interval() {
    let mut core = core();
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        Edge::default(),
    );
    let server = public(100, 3478);
    core.add_server(private(101), server, Edge::default());
    let link = Link {
        cuts: vec![(Duration::from_secs(1), Duration::from_secs(2))],
        ..Link::default()
    };
    core.set_link(private(1), private(101), link);
    for ms in [500, 1000, 1999, 2000, 2500] {
        core.send(Duration::from_millis(ms), private(1), server, b"x");
    }
    use Verdict::{Cut, Forwarded};
    core.due(FAR);
    // cuts are decided at send time; forwarded verdicts land later in the trace
    let cuts: Vec<Verdict> = core.trace().iter().map(|t| t.verdict).collect();
    assert_eq!(cuts, [Cut, Cut, Forwarded, Forwarded, Forwarded]);
}

#[test]
fn hole_punch_succeeds_for_eim_with_open_filtering() {
    let mut core = core();
    let open = nat(
        1,
        Mapping::EndpointIndependent,
        Filtering::EndpointIndependent,
    );
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        open,
        Edge::default(),
    );
    core.add_player(
        private(2),
        Ipv4Addr::new(203, 0, 113, 2),
        NatConfig { id: 2, ..open },
        Edge::default(),
    );
    let server = public(100, 3478);
    core.add_server(private(101), server, Edge::default());
    let a = learn(&mut core, Duration::ZERO, private(1), server);
    let b = learn(&mut core, Duration::ZERO, private(2), server);
    let t = 10 * MS;
    core.send(t, private(1), b, b"to b");
    core.send(t, private(2), a, b"to a");
    let got = core.due(t);
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|d| d.to == private(2) && d.from == a));
    assert!(got.iter().any(|d| d.to == private(1) && d.from == b));
}

#[test]
fn hole_punch_fails_for_edm_with_port_dependent_filtering() {
    let mut core = core();
    let strict = nat(
        1,
        Mapping::EndpointDependent,
        Filtering::AddressAndPortDependent,
    );
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        strict,
        Edge::default(),
    );
    core.add_player(
        private(2),
        Ipv4Addr::new(203, 0, 113, 2),
        NatConfig { id: 2, ..strict },
        Edge::default(),
    );
    let server = public(100, 3478);
    core.add_server(private(101), server, Edge::default());
    let a = learn(&mut core, Duration::ZERO, private(1), server);
    let b = learn(&mut core, Duration::ZERO, private(2), server);
    let t = 10 * MS;
    core.send(t, private(1), b, b"to b");
    core.send(t, private(2), a, b"to a");
    assert!(core.due(t).is_empty());
    let last = verdicts(&core);
    assert_eq!(
        &last[last.len() - 2..],
        [Verdict::Filtered, Verdict::Filtered]
    );
}

#[test]
fn filtering_ignores_permissions_opened_after_the_arrival() {
    let mut core = core();
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        nat(
            1,
            Mapping::EndpointIndependent,
            Filtering::AddressAndPortDependent,
        ),
        Edge::default(),
    );
    let (stun, other) = (public(100, 3478), public(101, 3478));
    core.add_server(private(101), stun, Edge::default());
    core.add_server(private(102), other, Edge::default());
    core.set_link(
        private(102),
        private(1),
        Link {
            delay: MS,
            ..Link::default()
        },
    );
    let mine = learn(&mut core, Duration::ZERO, private(1), stun);
    // unsolicited from `other`, arriving at 1 ms
    core.send(Duration::ZERO, private(102), mine, b"early");
    // the player contacts `other` only at 2 ms, before the arrival is released
    core.send(2 * MS, private(1), other, b"late");
    let got = core.due(2 * MS);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].to, private(102));
    let early = core
        .trace()
        .iter()
        .find(|t| t.from == Some(other) && t.to == Some(mine))
        .unwrap();
    assert_eq!((early.at, early.verdict), (MS, Verdict::Filtered));
}

#[test]
fn hole_punch_works_between_eim_players_with_strict_filtering() {
    let mut core = core();
    let strict = nat(
        1,
        Mapping::EndpointIndependent,
        Filtering::AddressAndPortDependent,
    );
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        strict,
        Edge::default(),
    );
    core.add_player(
        private(2),
        Ipv4Addr::new(203, 0, 113, 2),
        NatConfig { id: 2, ..strict },
        Edge::default(),
    );
    let slow = Link {
        delay: 5 * MS,
        ..Link::default()
    };
    core.set_link(private(1), private(2), slow.clone());
    core.set_link(private(2), private(1), slow);
    let server = public(100, 3478);
    core.add_server(private(101), server, Edge::default());
    let a = learn(&mut core, Duration::ZERO, private(1), server);
    let b = learn(&mut core, Duration::ZERO, private(2), server);
    // both punch at 10 ms; each packet arrives after the other side's outbound permission
    core.send(10 * MS, private(1), b, b"to b");
    core.send(10 * MS, private(2), a, b"to a");
    assert_eq!(core.due(20 * MS).len(), 2);
}

#[test]
fn oversize_is_traced_not_forwarded_and_the_limit_is_exact() {
    let mut core = core();
    core.add_player(
        private(1),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        Edge::default(),
    );
    let server = public(100, 3478);
    core.add_server(private(101), server, Edge::default());
    core.send(
        Duration::ZERO,
        private(1),
        server,
        &[0; MAX_DATAGRAM - PREFIX],
    );
    core.send(
        Duration::ZERO,
        private(1),
        server,
        &[0; MAX_DATAGRAM - PREFIX + 1],
    );
    let mut raw = encode_addr(server).to_vec();
    raw.resize(MAX_DATAGRAM + 1, 0);
    core.send_raw(Duration::ZERO, private(1), &raw);
    assert_eq!(core.due(FAR).len(), 1);
    assert_eq!(
        verdicts(&core),
        [Verdict::Oversize, Verdict::Oversize, Verdict::Forwarded]
    );
}

#[test]
fn pump_rejects_oversize_and_delivers_the_largest_datagram_intact() {
    let companion = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hub = UdpSocket::bind("127.0.0.1:0").unwrap();
    hub.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let hub_public = public(100, 3478);
    let mut core = core();
    core.add_player(
        companion.local_addr().unwrap(),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        Edge::default(),
    );
    core.add_server(hub.local_addr().unwrap(), hub_public, Edge::default());
    let pump = Pump::start(core).unwrap();
    let envelope = |len: usize| {
        let mut out = encode_addr(hub_public).to_vec();
        out.extend((0..len - PREFIX).map(|i| i as u8));
        out
    };

    // oversize first: nothing may arrive, not even a truncated copy
    companion
        .send_to(&envelope(PREFIX + 3000), pump.addr)
        .unwrap();
    let mut buf = vec![0u8; 4096];
    assert!(hub.recv_from(&mut buf).is_err());

    let max = envelope(MAX_DATAGRAM);
    companion.send_to(&max, pump.addr).unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (len, _) = hub.recv_from(&mut buf).unwrap();
    // the hub sees the source prefix instead of the destination one; payloads match
    assert_eq!(len, MAX_DATAGRAM);
    assert_eq!(&buf[PREFIX..len], &max[PREFIX..]);
    let verdicts: Vec<Verdict> = pump.trace().iter().map(|t| t.verdict).collect();
    assert_eq!(verdicts, [Verdict::Oversize, Verdict::Forwarded]);
}

#[test]
fn pump_relays_through_real_sockets() {
    let companion = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hub = UdpSocket::bind("127.0.0.1:0").unwrap();
    for socket in [&companion, &hub] {
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
    }
    let hub_public = public(100, 3478);
    let mut core = core();
    core.add_player(
        companion.local_addr().unwrap(),
        Ipv4Addr::new(203, 0, 113, 1),
        NatConfig::default(),
        Edge::default(),
    );
    core.add_server(hub.local_addr().unwrap(), hub_public, Edge::default());
    let pump = Pump::start(core).unwrap();

    let mut out = encode_addr(hub_public).to_vec();
    out.extend_from_slice(b"ping");
    companion.send_to(&out, pump.addr).unwrap();
    let mut buf = [0u8; 64];
    let (len, from) = hub.recv_from(&mut buf).unwrap();
    assert_eq!(from, pump.addr);
    assert_eq!(&buf[PREFIX..len], b"ping");
    let seen = decode_addr(&buf[..len]).unwrap();
    assert_eq!(*seen.ip(), Ipv4Addr::new(203, 0, 113, 1));

    let mut back = encode_addr(seen).to_vec();
    back.extend_from_slice(b"pong");
    hub.send_to(&back, pump.addr).unwrap();
    let (len, _) = companion.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[PREFIX..len], b"pong");
    assert_eq!(decode_addr(&buf[..len]), Some(hub_public));

    let forwarded = pump
        .trace()
        .iter()
        .filter(|t| t.verdict == Verdict::Forwarded)
        .count();
    assert_eq!(forwarded, 2);
}
