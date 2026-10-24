//! harness side of the p2p lab: builds a router core for a few players, starts the pump and
//! hands each companion its `--netlab` flags. included through #[path] after router.rs.
#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use ptt_radio_companion::transport::lab::label;

use super::router::{Core, Edge, NatConfig, Pump, Trace, Verdict};

pub struct LabPlayer {
    pub nat: NatConfig,
    pub edge: Edge,
}

pub struct Lab {
    pub pump: Pump,
    // private address of each player's companion, as registered with the router
    pub locals: Vec<SocketAddr>,
    // a public stun-like server; `server` is its socket, behind the same router
    pub server: UdpSocket,
    pub server_public: SocketAddrV4,
}

// the public ip of player `i`
pub fn player_ip(i: usize) -> Ipv4Addr {
    Ipv4Addr::new(203, 0, 113, i as u8 + 1)
}

// a port nobody holds right now; the companion binds it later
fn reserve() -> SocketAddr {
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .unwrap()
}

impl Lab {
    pub fn start(seed: u64, players: Vec<LabPlayer>) -> Lab {
        let mut core = Core::new(seed);
        let mut locals = Vec::new();
        for (i, player) in players.into_iter().enumerate() {
            let local = reserve();
            core.add_player(local, player_ip(i), player.nat, player.edge);
            locals.push(local);
        }
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_public = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 1), 3478);
        core.add_server(server.local_addr().unwrap(), server_public, Edge::default());
        let pump = Pump::start(core).unwrap();
        Lab {
            pump,
            locals,
            server,
            server_public,
        }
    }

    // the flags that point player `i`'s companion at the router
    pub fn companion_args(&self, i: usize) -> Vec<String> {
        vec![
            "--netlab".into(),
            self.pump.addr.to_string(),
            "--netlab-bind".into(),
            self.locals[i].to_string(),
        ]
    }

    // the trace as (u13 datagram type, verdict) pairs
    pub fn labelled(&self) -> Vec<(&'static str, Verdict)> {
        labelled(&self.pump.trace())
    }
}

pub fn labelled(trace: &[Trace]) -> Vec<(&'static str, Verdict)> {
    trace.iter().map(|t| (label(t.first), t.verdict)).collect()
}
