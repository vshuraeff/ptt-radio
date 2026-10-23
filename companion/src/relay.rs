//! game relay emulator: stands in for Factorio and the ptt-radio mod so
//! companions can be tested without the game. it speaks the exact wire format
//! of mod/ptt-radio/control.lua:
//! - each player has a game port (its --enable-lua-udp port, where its
//!   companion sends) and a companion port (where its companion listens);
//! - HELLO is answered with ROSTER (sender 0) and MIC_STATE (the player's own
//!   index) to that player's companion;
//! - START/VOICE/STOP are forwarded to every other player's companion, and to
//!   the sender's own one when its echo setting is on, with the sender's
//!   player index in place of the first two bytes of the companion header.
//!
//! transport model, after the measurements in udp-probe/README.md: every packet
//! takes `delay` plus a uniform random part of `jitter`, is delivered on the
//! next 60 Hz game tick, never overtakes an earlier packet and is never lost.
//! a stall (autosave, pause) stops the ticks; what queued up meanwhile is
//! delivered in one burst on the first tick after it. not emulated: the mod's
//! synthetic STOP for a dead companion, the force scope, chat messages.

use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::packet::{self, HELLO, MAGIC, MIC_STATE, ROSTER, START, STOP, VERSION, VOICE};

pub const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);

pub struct Player {
    pub name: String,
    pub companion_port: u16,
    /// the mod's per-player setting ptt-radio-echo
    pub echo: bool,
    /// what the mod's open-mic button holds; sent as MIC_STATE on every HELLO
    pub open_mic: bool,
}

pub struct Config {
    pub delay: Duration,
    pub jitter: Duration,
    pub stall: Option<Stall>,
}

impl Default for Config {
    /// client -> server -> client p50 ~85 ms, p95 <= ~100 ms, as measured on the stand
    fn default() -> Self {
        Config {
            delay: Duration::from_millis(72),
            jitter: Duration::from_millis(10),
            stall: None,
        }
    }
}

/// stalls of `len`, the first `at` after the relay starts, then every `every`
#[derive(Clone, Copy)]
pub struct Stall {
    pub at: Duration,
    pub len: Duration,
    pub every: Option<Duration>,
}

impl Stall {
    fn covers(&self, elapsed: Duration) -> bool {
        let Some(mut since) = elapsed.checked_sub(self.at) else {
            return false;
        };
        if let Some(every) = self.every.filter(|every| !every.is_zero()) {
            since = Duration::from_nanos((since.as_nanos() % every.as_nanos()) as u64);
        }
        since < self.len
    }
}

/// one forwarded START/VOICE/STOP, for the scenarios' per-stage timing
pub struct Delivery {
    pub sender: u16,
    pub kind: u8,
    pub seq: u16,
    pub received_us: u64,
    pub delivered_us: u64,
}

#[derive(Default)]
pub struct Report {
    pub deliveries: Vec<Delivery>,
    pub hellos: u64,
    pub ignored: u64,
    /// wall-clock (start, end) of each stall that happened
    pub stalls: Vec<(u64, u64)>,
}

struct Pending {
    player: usize,
    payload: Vec<u8>,
    received_us: u64,
    due: Instant,
}

struct State {
    queue: VecDeque<Pending>,
    last_due: Instant,
    rng: u64,
    report: Report,
}

pub struct Relay {
    game_ports: Vec<u16>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
}

impl Relay {
    /// binds 127.0.0.1:<game_port> per player (0 picks a free port) and starts relaying;
    /// players get indices 1, 2, ... in order
    pub fn start(config: Config, players: Vec<(u16, Player)>) -> io::Result<Relay> {
        let mut sockets = Vec::new();
        for (port, _) in &players {
            let socket = UdpSocket::bind(("127.0.0.1", *port))?;
            socket.set_read_timeout(Some(Duration::from_millis(20)))?;
            sockets.push(Arc::new(socket));
        }
        let game_ports = sockets
            .iter()
            .map(|s| s.local_addr().map(|a| a.port()))
            .collect::<io::Result<_>>()?;
        let players: Arc<Vec<Player>> = Arc::new(players.into_iter().map(|(_, p)| p).collect());
        let started = Instant::now();
        let state = Arc::new(Mutex::new(State {
            queue: VecDeque::new(),
            last_due: started,
            rng: 0x9e37_79b9_7f4a_7c15,
            report: Report::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for (player, socket) in sockets.iter().enumerate() {
            let (socket, state, stop) = (socket.clone(), state.clone(), stop.clone());
            let (delay, jitter) = (config.delay, config.jitter);
            threads.push(thread::spawn(move || {
                receive(player, &socket, delay, jitter, &state, &stop)
            }));
        }
        {
            let (state, stop) = (state.clone(), stop.clone());
            threads.push(thread::spawn(move || {
                tick(started, config.stall, &sockets, &players, &state, &stop)
            }));
        }
        Ok(Relay {
            game_ports,
            stop,
            threads,
            state,
        })
    }

    /// the game port of player `index` (1-based, as the mod numbers players)
    pub fn game_port(&self, index: u16) -> u16 {
        self.game_ports[index as usize - 1]
    }

    pub fn stop(self) -> Report {
        self.stop.store(true, Relaxed);
        for thread in self.threads {
            thread.join().expect("relay thread");
        }
        std::mem::take(&mut self.state.lock().unwrap().report)
    }
}

fn receive(
    player: usize,
    socket: &UdpSocket,
    delay: Duration,
    jitter: Duration,
    state: &Mutex<State>,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 65536];
    while !stop.load(Relaxed) {
        let len = match socket.recv(&mut buf) {
            Ok(len) => len,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(err) => {
                eprintln!("relay: receive failed: {err}");
                continue;
            }
        };
        let now = Instant::now();
        let mut state = state.lock().unwrap();
        // the mod drops anything that is not R, version 1
        if !matches!(buf[..len], [MAGIC, VERSION, _, ..]) {
            state.report.ignored += 1;
            continue;
        }
        // xorshift: a fixed seed keeps runs comparable
        state.rng ^= state.rng << 13;
        state.rng ^= state.rng >> 7;
        state.rng ^= state.rng << 17;
        let extra = jitter.mul_f64((state.rng >> 11) as f64 / (1u64 << 53) as f64);
        // lockstep input actions arrive in order
        let due = (now + delay + extra).max(state.last_due);
        state.last_due = due;
        state.queue.push_back(Pending {
            player,
            payload: buf[..len].to_vec(),
            received_us: crate::unix_us(),
            due,
        });
    }
}

fn tick(
    started: Instant,
    stall: Option<Stall>,
    sockets: &[Arc<UdpSocket>],
    players: &[Player],
    state: &Mutex<State>,
    stop: &AtomicBool,
) {
    // the game sends from the receiving player's own udp port to that player's companion
    let send = |player: usize, data: &[u8]| {
        let to = SocketAddr::from(([127, 0, 0, 1], players[player].companion_port));
        if let Err(err) = sockets[player].send_to(data, to) {
            eprintln!("relay: send to companion of player {}: {err}", player + 1);
        }
    };
    let mut stalled_since: Option<u64> = None;
    for n in 1u32.. {
        thread::sleep((started + TICK * n).saturating_duration_since(Instant::now()));
        if stop.load(Relaxed) {
            break;
        }
        let now = Instant::now();
        let mut state = state.lock().unwrap();
        if stall.is_some_and(|s| s.covers(now - started)) {
            stalled_since.get_or_insert_with(crate::unix_us);
            continue;
        }
        if let Some(since) = stalled_since.take() {
            state.report.stalls.push((since, crate::unix_us()));
        }
        while state.queue.front().is_some_and(|p| p.due <= now) {
            let p = state.queue.pop_front().expect("checked above");
            let sender_index = p.player as u16 + 1;
            match p.payload[2] {
                HELLO => {
                    state.report.hellos += 1;
                    let mut roster = vec!["0\tserver\t204,204,204".to_string()];
                    for (i, player) in players.iter().enumerate() {
                        let (r, g, b) = COLORS[i % COLORS.len()];
                        roster.push(format!("{}\t{}\t{r},{g},{b}", i + 1, player.name));
                    }
                    send(
                        p.player,
                        &packet::encode(ROSTER, 0, roster.join("\n").as_bytes()),
                    );
                    let open = players[p.player].open_mic as u8;
                    send(p.player, &packet::encode(MIC_STATE, sender_index, &[open]));
                }
                // like the mod, forward whatever follows the 3-byte header, however short
                kind @ (START | VOICE | STOP) => {
                    let out = packet::encode(kind, sender_index, &p.payload[3..]);
                    for (listener, player) in players.iter().enumerate() {
                        if listener != p.player || player.echo {
                            send(listener, &out);
                        }
                    }
                    // only a packet with a sequence number is a frame the report can time
                    if let [_, _, _, hi, lo, ..] = p.payload[..] {
                        state.report.deliveries.push(Delivery {
                            sender: sender_index,
                            kind,
                            seq: u16::from_be_bytes([hi, lo]),
                            received_us: p.received_us,
                            delivered_us: crate::unix_us(),
                        });
                    }
                }
                _ => state.report.ignored += 1,
            }
        }
    }
}

// player colors in the roster: red, blue, green, orange
const COLORS: [(u8, u8, u8); 4] = [(255, 0, 10), (40, 120, 255), (60, 200, 60), (255, 160, 0)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Incoming;

    fn companion() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
    }

    #[test]
    fn a_zero_stall_period_does_not_panic() {
        let stall = Stall {
            at: Duration::ZERO,
            len: Duration::from_millis(5),
            every: Some(Duration::ZERO),
        };
        assert!(stall.covers(Duration::from_millis(1)));
        assert!(!stall.covers(Duration::from_millis(9)));
    }

    fn recv(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let len = socket.recv(&mut buf).unwrap();
        buf[..len].to_vec()
    }

    #[test]
    fn hello_roster_and_fan_out_like_the_mod() {
        let (a, b) = (companion(), companion());
        let player = |name: &str, socket: &UdpSocket, echo, open_mic| Player {
            name: name.into(),
            companion_port: socket.local_addr().unwrap().port(),
            echo,
            open_mic,
        };
        let config = Config {
            delay: Duration::from_millis(5),
            jitter: Duration::ZERO,
            stall: None,
        };
        let relay = Relay::start(
            config,
            vec![
                (0, player("alice", &a, false, false)),
                (0, player("bob", &b, true, true)),
            ],
        )
        .unwrap();
        let game = |index| SocketAddr::from(([127, 0, 0, 1], relay.game_port(index)));

        b.send_to(&packet::encode(HELLO, 0, b"test"), game(2))
            .unwrap();
        let Some(Incoming::Roster(roster)) = packet::parse(&recv(&b)) else {
            panic!("no roster")
        };
        assert_eq!(
            roster
                .iter()
                .map(|p| (p.index, p.name.as_str()))
                .collect::<Vec<_>>(),
            [(0, "server"), (1, "alice"), (2, "bob")]
        );
        assert_eq!(
            packet::parse(&recv(&b)),
            Some(Incoming::MicState {
                player: 2,
                open: true
            })
        );

        // alice has echo off: only bob hears her; bob has echo on: both hear him
        let voice = |sender, seq, frame| Incoming::Voice { sender, seq, frame };
        a.send_to(&packet::encode(VOICE, 7, &[1, 0, 2]), game(1))
            .unwrap();
        assert_eq!(packet::parse(&recv(&b)), Some(voice(1, 7, &[1, 0, 2])));
        b.send_to(&packet::encode(VOICE, 9, &[3]), game(2)).unwrap();
        assert_eq!(packet::parse(&recv(&b)), Some(voice(2, 9, &[3])));
        assert_eq!(packet::parse(&recv(&a)), Some(voice(2, 9, &[3])));

        let report = relay.stop();
        assert_eq!(report.hellos, 1);
        assert_eq!(
            report
                .deliveries
                .iter()
                .map(|d| (d.sender, d.kind, d.seq))
                .collect::<Vec<_>>(),
            [(1, VOICE, 7), (2, VOICE, 9)]
        );
        assert!(report.deliveries[0].delivered_us >= report.deliveries[0].received_us + 5_000);
    }

    #[test]
    fn stall_windows() {
        let ms = Duration::from_millis;
        let once = Stall {
            at: ms(1000),
            len: ms(500),
            every: None,
        };
        assert!(!once.covers(ms(999)) && once.covers(ms(1000)) && once.covers(ms(1499)));
        assert!(!once.covers(ms(1500)) && !once.covers(ms(3000)));
        let periodic = Stall {
            every: Some(ms(2000)),
            ..once
        };
        assert!(periodic.covers(ms(3200)) && !periodic.covers(ms(3600)));
    }
}
