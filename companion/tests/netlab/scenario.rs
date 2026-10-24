use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use ptt_radio_companion::transport::hub::{Hub, registration};
use ptt_radio_companion::transport::relay::{Limits, Reply};
use ptt_radio_companion::unix_us;

use super::router::{self, Core, Edge, Filtering, Link, Mapping, NatConfig, Pump};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Game,
    Direct,
    Hub,
    Player,
    Fallback,
    HubDown,
    CutHub,
    CutGame,
}

impl Mode {
    pub fn warmup(self, scenario: &str) -> f64 {
        match self {
            Self::Game => 0.0,
            // preserve the double-talk baseline, including the known hub gain flake.
            _ if scenario == "double-talk" => 6.0,
            // these windows still see startup jitter-buffer margin with less preparation.
            Self::Direct if matches!(scenario, "echo" | "latency" | "stall") => 6.0,
            Self::Hub | Self::Player if matches!(scenario, "stall" | "latency") => 6.0,
            // traces select both directions within 1.5s; retain settling before speech.
            Self::Direct | Self::Hub => 2.0,
            // player relay settles near 2.7s; echo also checks the 10s hello refresh.
            Self::Player if scenario == "echo" => 4.5,
            Self::Player => 3.0,
            _ => 6.0,
        }
    }
    pub fn route(self) -> &'static str {
        match self {
            Self::Game | Self::Fallback => "Game",
            Self::Direct | Self::CutHub | Self::CutGame => "Direct",
            Self::Hub | Self::HubDown => "Hub",
            Self::Player => "PlayerRelay",
        }
    }

    pub fn hub(self) -> bool {
        matches!(self, Self::Hub | Self::HubDown | Self::CutHub)
    }
}

pub fn delay_band(route: &str) -> std::ops::Range<f64> {
    match route {
        "Game" => 65.0..120.0,
        "Direct" => 5.0..20.0,
        "Hub" => 30.0..50.0,
        "PlayerRelay" => 20.0..40.0,
        _ => panic!("unknown route {route}"),
    }
}

pub struct Network {
    pub pump: Pump,
    pub origin_us: u64,
    pub locals: Vec<SocketAddr>,
    server: UdpSocket,
    public: SocketAddrV4,
    game: Option<UdpSocket>,
    hub: Hub,
    next_hello: u64,
    pub game_received: usize,
    pub game_sent: usize,
}

impl Network {
    pub fn new(mode: Mode, players: usize, stall: Option<(Duration, Duration)>) -> Self {
        let mut core = Core::new(21);
        let locals: Vec<_> = (0..players)
            .map(|_| {
                UdpSocket::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
            })
            .collect();
        for (i, &local) in locals.iter().enumerate() {
            let edm = matches!(mode, Mode::Hub | Mode::Fallback | Mode::HubDown);
            core.add_player(
                local,
                super::lab::player_ip(i),
                NatConfig {
                    id: i as u32 + 1,
                    mapping: if edm {
                        Mapping::EndpointDependent
                    } else {
                        Mapping::EndpointIndependent
                    },
                    filtering: if edm {
                        Filtering::AddressAndPortDependent
                    } else {
                        Filtering::EndpointIndependent
                    },
                    ..NatConfig::default()
                },
                Edge {
                    delay: Duration::from_millis(5),
                    ..Edge::default()
                },
            );
        }
        for (a, b) in [(0, 1), (1, 0)] {
            let cuts = match mode {
                Mode::Player => vec![(Duration::ZERO, Duration::from_secs(60))],
                Mode::CutHub | Mode::CutGame => {
                    vec![(Duration::from_secs(10), Duration::from_secs(60))]
                }
                _ => Vec::new(),
            };
            core.set_link(
                locals[a],
                locals[b],
                Link {
                    delay: Duration::from_millis(5),
                    cuts,
                    ..Link::default()
                },
            );
        }
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let public = "198.51.100.1:3478".parse().unwrap();
        let server_local = server.local_addr().unwrap();
        core.add_server(
            server_local,
            public,
            Edge {
                delay: Duration::from_millis(10),
                ..Edge::default()
            },
        );
        if mode == Mode::HubDown {
            for &local in &locals {
                for (a, b) in [(local, server_local), (server_local, local)] {
                    core.set_link(
                        a,
                        b,
                        Link {
                            delay: Duration::from_millis(15),
                            cuts: vec![(Duration::from_secs(10), Duration::from_secs(60))],
                            ..Link::default()
                        },
                    );
                }
            }
        }
        let stalls = stall.into_iter().map(|(at, len)| (at, at + len)).collect();
        Self {
            origin_us: unix_us(),
            pump: Pump::start_with_stalls(core, stalls).unwrap(),
            locals,
            server,
            public,
            game: None,
            hub: Hub::new(21, Limits::default()),
            next_hello: 0,
            game_received: 0,
            game_sent: 0,
        }
    }

    pub fn attach_hub(&mut self, game: u16) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .connect(SocketAddr::from(([127, 0, 0, 1], game)))
            .unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
            .send(&registration(socket.local_addr().unwrap().port()))
            .unwrap();
        self.game = Some(socket);
    }

    pub fn args(&self, i: usize, hub: bool) -> Vec<String> {
        let mut args = vec![
            "--netlab".into(),
            self.pump.addr.to_string(),
            "--netlab-bind".into(),
            self.locals[i].to_string(),
            "--stun-server".into(),
            self.public.to_string(),
        ];
        if hub {
            args.extend(["--hub".into(), self.public.to_string()]);
        }
        args
    }

    pub fn poll(&mut self) {
        let now = unix_us() / 1000;
        let mut buf = [0u8; 2048];
        if let Some(game) = &self.game {
            if now >= self.next_hello {
                game.send(&registration(game.local_addr().unwrap().port()))
                    .unwrap();
                self.next_hello = now + 1000;
            }
            while let Ok(len) = game.recv(&mut buf) {
                self.game_received += 1;
                for frame in self.hub.on_game(now, &buf[..len]) {
                    self.game_sent += 1;
                    game.send(&frame.frame).unwrap();
                }
            }
            for frame in self.hub.tick(now) {
                game.send(&frame.frame).unwrap();
            }
        }
        while let Ok((len, _)) = self.server.recv_from(&mut buf) {
            let from = router::decode_addr(&buf[..len]).unwrap();
            let data = &buf[router::PREFIX..len];
            let response = if data.len() >= 20 && data[..2] == [0, 1] {
                let mut reply = vec![1, 1, 0, 12];
                reply.extend_from_slice(&data[4..20]);
                reply.extend_from_slice(&[0, 0x20, 0, 8, 0, 1]);
                reply.extend_from_slice(&(from.port() ^ 0x2112).to_be_bytes());
                reply.extend_from_slice(&(u32::from(*from.ip()) ^ 0x2112_a442).to_be_bytes());
                Some((SocketAddr::V4(from), reply))
            } else if self.game.is_some() {
                match self.hub.relay.handle(now, SocketAddr::V4(from), data) {
                    Ok(Reply::Forward(to)) => Some((to, data.to_vec())),
                    Ok(Reply::Send(to, bytes)) => Some((to, bytes)),
                    Err(_) => None,
                }
            } else {
                None
            };
            if let Some((SocketAddr::V4(to), data)) = response {
                let mut reply = router::encode_addr(to).to_vec();
                reply.extend_from_slice(&data);
                self.server.send_to(&reply, self.pump.addr).unwrap();
            }
        }
    }
}
