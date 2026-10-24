//! game relay emulator: stands in for Factorio and the ptt-radio mod so
//! companions can be tested without the game. it speaks the exact wire format
//! of mod/ptt-radio/control.lua:
//! - each player has a game port (its --enable-lua-udp port, where its
//!   companion sends) and a companion port (where its companion listens);
//! - HELLO is answered with ROSTER (sender 0), MIC_STATE and HALF_DUPLEX (the
//!   player's own index) and the authoritative AUDIENCE to that player's companion;
//! - START/VOICE/STOP are forwarded to every other player's companion, and to
//!   the sender's own one when its echo setting is on, with the sender's
//!   player index inserted before the sequence. v2 media follows the same rule,
//!   but each listener gets one representation: a companion whose HELLO body
//!   ends with " v2" gets v2 frames and never a v1 copy of such a sender, a
//!   legacy one gets v2 frames converted to v1 and the synthetic timeout STOP.
//! - opaque SIGNAL follows the roster independently of force-scoped media;
//!   optional player 0 registers its server-only companion port with HELLO.
//!
//! transport model, after the measurements in udp-probe/README.md: every packet
//! takes `delay` plus a uniform random part of `jitter`, is delivered on the
//! next 60 Hz game tick, never overtakes an earlier packet and is never lost.
//! a stall (autosave, pause) stops the ticks; what queued up meanwhile is
//! delivered in one burst on the first tick after it. chat messages and GUI
//! rendering are not emulated.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::packet::{
    self, ECHO_STATE, HALF_DUPLEX, HELLO, MAGIC, MIC_STATE, ROSTER, START, STOP, VERSION, VOICE,
};
use crate::transport::signaling::{self, Audience};
use crate::transport::wire::{self, GAME_START, GAME_STOP, GAME_VOICE, SIGNAL_IN, SIGNAL_OUT};

pub const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);

pub struct Player {
    pub name: String,
    pub companion_port: u16,
    /// the mod's per-player setting ptt-radio-echo
    pub echo: bool,
    /// what the mod's open-mic button holds; sent as MIC_STATE on every HELLO
    pub open_mic: bool,
    /// the mod's per-player setting ptt-radio-half-duplex; sent as HALF_DUPLEX on every HELLO
    pub half_duplex: bool,
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
    pub echo_states: Vec<EchoState>,
    pub hellos: u64,
    pub ignored: u64,
    /// wall-clock (start, end) of each stall that happened
    pub stalls: Vec<(u64, u64)>,
}

pub struct EchoState {
    pub sender: u16,
    pub engaged: bool,
    pub received_us: u64,
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
    members: Vec<Member>,
    force_scope: bool,
    revision: u64,
    membership_changed: bool,
}

struct Member {
    index: u16,
    connected: bool,
    force: u16,
    seen: Option<u64>,
    /// the last HELLO announced v2 game media
    v2: bool,
    speaking: Option<u64>,
    talk: Option<Talk>,
}

struct Talk {
    generation: u64,
    retired: Vec<u64>,
    talkspurt: u16,
    closed: bool,
    last_tick: u64,
}

impl Member {
    fn accept_v2(&mut self, data: &[u8], tick: u64) -> bool {
        let forwarded = packet::encode(data[2], 0, &data[3..]);
        let Ok(media) = wire::decode_game(&forwarded) else {
            return false;
        };
        let generation = media.stream.generation;
        if self
            .talk
            .as_ref()
            .is_none_or(|t| t.generation != generation)
        {
            if media.kind == wire::Kind::Stop
                || self
                    .talk
                    .as_ref()
                    .is_some_and(|t| t.retired.contains(&generation) || t.retired.len() >= 16)
            {
                return false;
            }
            let mut retired = Vec::new();
            if let Some(previous) = self.talk.take() {
                retired = previous.retired;
                retired.push(previous.generation);
            }
            self.talk = Some(Talk {
                generation,
                retired,
                talkspurt: media.talkspurt,
                closed: false,
                last_tick: tick,
            });
        } else {
            let talk = self.talk.as_mut().expect("generation selected");
            let delta = media.talkspurt.wrapping_sub(talk.talkspurt);
            if delta >= 32768
                || (delta == 0 && talk.closed)
                || (media.kind == wire::Kind::Stop && delta != 0)
            {
                return false;
            }
            talk.talkspurt = media.talkspurt;
        }
        let talk = self.talk.as_mut().expect("generation selected");
        talk.last_tick = tick;
        talk.closed = media.kind == wire::Kind::Stop;
        self.speaking = (!talk.closed).then_some(tick);
        true
    }
}

pub struct Relay {
    game_ports: Vec<u16>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
    hub: Option<usize>,
}

impl Relay {
    /// binds 127.0.0.1:<game_port> per player (0 picks a free port) and starts relaying;
    /// players get indices 1, 2, ... in order
    pub fn start(config: Config, players: Vec<(u16, Player)>) -> io::Result<Relay> {
        Self::start_with_hub(config, players, None)
    }

    /// adds a server Lua-UDP endpoint; its five-byte HELLO registers the output port.
    pub fn start_with_hub(
        config: Config,
        mut players: Vec<(u16, Player)>,
        hub_port: Option<u16>,
    ) -> io::Result<Relay> {
        if players.len() >= u16::MAX as usize {
            return Err(io::Error::new(ErrorKind::InvalidInput, "too many players"));
        }
        let hub = hub_port.map(|port| {
            let slot = players.len();
            players.push((
                port,
                Player {
                    name: "server".into(),
                    companion_port: 34555,
                    echo: false,
                    open_mic: false,
                    half_duplex: false,
                },
            ));
            slot
        });
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
            members: players
                .iter()
                .enumerate()
                .map(|(i, _)| Member {
                    index: if Some(i) == hub { 0 } else { i as u16 + 1 },
                    connected: true,
                    force: 1,
                    seen: None,
                    v2: false,
                    speaking: None,
                    talk: None,
                })
                .collect(),
            force_scope: false,
            revision: 0,
            membership_changed: false,
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
            hub,
        })
    }

    /// the game port of player `index` (1-based, as the mod numbers players)
    pub fn game_port(&self, index: u16) -> u16 {
        self.game_ports[if index == 0 {
            self.hub.expect("hub endpoint configured")
        } else {
            index as usize - 1
        }]
    }

    /// updates the game-owned force policy; applied on the next non-stalled tick.
    pub fn set_force_scope(&self, force_scope: bool) {
        let mut state = self.state.lock().unwrap();
        state.force_scope = force_scope;
        state.revision += 1;
        state.membership_changed = true;
    }

    /// changes a roster player's force, like on_player_changed_force/forces_merged.
    pub fn set_force(&self, index: u16, force: u16) {
        let mut state = self.state.lock().unwrap();
        state.members[index as usize - 1].force = force;
        state.revision += 1;
        state.membership_changed = true;
    }

    /// join/leave without rebinding the machine's UDP port.
    pub fn set_connected(&self, index: u16, connected: bool) {
        let mut state = self.state.lock().unwrap();
        let member = &mut state.members[index as usize - 1];
        member.connected = connected;
        if !connected {
            member.talk = None;
            member.speaking = None;
            member.seen = None;
            member.v2 = false;
        }
        state.revision += 1;
        state.membership_changed = true;
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
    let hub = state
        .lock()
        .unwrap()
        .members
        .iter()
        .position(|m| m.index == 0);
    let hub_port = Cell::new(34555);
    // the game sends from the receiving player's own udp port to that player's companion
    let send = |player: usize, data: &[u8]| {
        let port = if Some(player) == hub {
            hub_port.get()
        } else {
            players[player].companion_port
        };
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        if let Err(err) = sockets[player].send_to(data, to) {
            eprintln!("relay: send to companion of player {}: {err}", player + 1);
        }
    };
    let mut stalled_since: Option<u64> = None;
    let mut game_tick = 0u64;
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
        game_tick += 1;
        let in_scope = |sender: usize, listener: usize, state: &State| {
            state.members[sender].index == 0
                || !state.force_scope
                || state.members[sender].force == state.members[listener].force
        };
        // one representation per listener: `legacy` to legacy companions, `out`
        // to v2 companions only when `to_v2`
        let fan_out = |state: &State, sender: usize, out: &[u8], legacy: &[u8], to_v2: bool| {
            for (listener, player) in players.iter().enumerate() {
                let member = &state.members[listener];
                if member.connected
                    && member.index != 0
                    && (listener != sender || player.echo)
                    && in_scope(sender, listener, state)
                {
                    if !member.v2 {
                        send(listener, legacy);
                    } else if to_v2 {
                        send(listener, out);
                    }
                }
            }
        };
        let audience = |listener: usize, state: &State| {
            let peers: Vec<u16> = state
                .members
                .iter()
                .enumerate()
                .filter(|(i, member)| {
                    member.connected
                        && member.index != 0
                        && *i != listener
                        && in_scope(listener, *i, state)
                })
                .map(|(_, member)| member.index)
                .collect();
            let mut receive_from = if state.members[listener].index == 0 {
                Vec::new()
            } else {
                vec![0]
            };
            if state.members[listener].index != 0 {
                receive_from.extend_from_slice(&peers);
            }
            let (send_to, receive_from) = if receive_from.len() > 256 || peers.len() > 256 {
                (Vec::new(), Vec::new())
            } else {
                (peers, receive_from)
            };
            signaling::encode_audience(&Audience {
                revision: state.revision,
                send_to,
                receive_from,
            })
            .expect("bounded canonical roster")
        };
        let roster = |state: &State| {
            let mut lines = vec!["0\tserver\t204,204,204".to_string()];
            for (i, player) in players.iter().enumerate() {
                let member = &state.members[i];
                if !member.connected || member.index == 0 {
                    continue;
                }
                let (r, g, b) = COLORS[i % COLORS.len()];
                lines.push(format!("{}\t{}\t{r},{g},{b}", member.index, player.name));
            }
            packet::encode(ROSTER, 0, lines.join("\n").as_bytes())
        };
        if state.membership_changed {
            for (listener, member) in state.members.iter().enumerate() {
                if member.connected && (member.index != 0 || member.seen.is_some()) {
                    send(listener, &roster(&state));
                    send(listener, &audience(listener, &state));
                }
            }
            state.membership_changed = false;
        }
        while state.queue.front().is_some_and(|p| p.due <= now) {
            let p = state.queue.pop_front().expect("checked above");
            let sender_index = state.members[p.player].index;
            if !state.members[p.player].connected {
                state.report.ignored += 1;
                continue;
            }
            match p.payload[2] {
                ECHO_STATE
                    if sender_index != 0
                        && state.members[p.player].seen.is_some()
                        && matches!(p.payload.as_slice(), [_, _, _, _, _, 0 | 1]) =>
                {
                    state.report.echo_states.push(EchoState {
                        sender: sender_index,
                        engaged: p.payload[5] == 1,
                        received_us: p.received_us,
                    });
                }
                HELLO => {
                    if sender_index == 0 {
                        if p.payload.len() != 5 {
                            state.report.ignored += 1;
                            continue;
                        }
                        let port = u16::from_be_bytes([p.payload[3], p.payload[4]]);
                        if port != 0 && port < 1024 {
                            state.report.ignored += 1;
                            continue;
                        }
                        hub_port.set(if port == 0 { 34555 } else { port });
                    }
                    state.members[p.player].seen = Some(game_tick);
                    state.report.hellos += 1;
                    send(p.player, &roster(&state));
                    if sender_index == 0 {
                        send(p.player, &audience(p.player, &state));
                        continue;
                    }
                    // the body is the companion version, then " v2" when it speaks v2
                    state.members[p.player].v2 = p
                        .payload
                        .get(5..)
                        .is_some_and(|body| body.ends_with(b" v2"));
                    let open = players[p.player].open_mic as u8;
                    send(p.player, &packet::encode(MIC_STATE, sender_index, &[open]));
                    let half = players[p.player].half_duplex as u8;
                    send(
                        p.player,
                        &packet::encode(HALF_DUPLEX, sender_index, &[half]),
                    );
                    send(p.player, &audience(p.player, &state));
                }
                SIGNAL_OUT if (5..=1205).contains(&p.payload.len()) => {
                    let target = u16::from_be_bytes([p.payload[3], p.payload[4]]);
                    let out = packet::encode(SIGNAL_IN, sender_index, &p.payload[5..]);
                    for (listener, member) in state.members.iter().enumerate() {
                        if member.connected
                            && (member.index != 0 || member.seen.is_some())
                            && (target == u16::MAX || target == member.index)
                        {
                            send(listener, &out);
                        }
                    }
                }
                // legacy framing remains permissive; v2 is validated before forwarding
                kind @ (START | VOICE | STOP | GAME_START | GAME_VOICE | GAME_STOP) => {
                    let member = &mut state.members[p.player];
                    if matches!(kind, GAME_START | GAME_VOICE | GAME_STOP) {
                        if !member.accept_v2(&p.payload, game_tick) {
                            state.report.ignored += 1;
                            continue;
                        }
                    } else if member.talk.is_none() {
                        member.speaking = (kind != STOP).then_some(game_tick);
                    }
                    let out = packet::encode(kind, sender_index, &p.payload[3..]);
                    // a legacy listener gets v2 as v1: sequence and opus kept,
                    // generation and talkspurt dropped
                    let legacy_kind = match kind {
                        GAME_START => Some(START),
                        GAME_VOICE => Some(VOICE),
                        GAME_STOP => Some(STOP),
                        _ => None,
                    };
                    let legacy = legacy_kind.map(|legacy_kind| {
                        let body = [&p.payload[3..5], &p.payload[15..]].concat();
                        packet::encode(legacy_kind, sender_index, &body)
                    });
                    let to_v2 = legacy_kind.is_some() || !state.members[p.player].v2;
                    fan_out(
                        &state,
                        p.player,
                        &out,
                        legacy.as_deref().unwrap_or(&out),
                        to_v2,
                    );
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
        let mut synthetic = Vec::new();
        for (sender, member) in state.members.iter_mut().enumerate() {
            if game_tick.is_multiple_of(15)
                && member.speaking.is_some_and(|last| game_tick - last > 30)
            {
                member.speaking = None;
                synthetic.push((sender, packet::encode(STOP, member.index, &[0, 0])));
            }
            if game_tick.is_multiple_of(60) {
                if member
                    .talk
                    .as_ref()
                    .is_some_and(|talk| game_tick - talk.last_tick > 3600)
                {
                    member.talk = None;
                }
                if member.seen.is_some_and(|last| game_tick - last > 900) {
                    member.seen = None;
                    member.v2 = false;
                    member.talk = None;
                }
            }
        }
        // legacy listeners need a timeout stop; v2 listeners are not sent it
        for (sender, out) in synthetic {
            fan_out(&state, sender, &out, &out, false);
        }
    }
}

// player colors in the roster: red, blue, green, orange
const COLORS: [(u8, u8, u8); 4] = [(255, 0, 10), (40, 120, 255), (60, 200, 60), (255, 160, 0)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Incoming;
    use crate::transport::StreamId;
    use crate::transport::wire::{self, GameMedia, Kind};

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
            half_duplex: open_mic,
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
        assert_eq!(
            packet::parse(&recv(&b)),
            Some(Incoming::HalfDuplex { on: true })
        );
        assert_eq!(
            signaling::decode_audience(&recv(&b)).unwrap(),
            Audience {
                revision: 0,
                send_to: vec![1],
                receive_from: vec![0, 1],
            }
        );
        a.send_to(&packet::encode(HELLO, 0, &[]), game(1)).unwrap();
        for _ in 0..4 {
            recv(&a);
        }

        // alice has echo off: only bob hears her; bob has echo on: both hear him
        b.send_to(&packet::encode(ECHO_STATE, 0, &[1]), game(2))
            .unwrap();
        a.send_to(&packet::encode(ECHO_STATE, 0, &[0]), game(1))
            .unwrap();
        a.send_to(&packet::encode(ECHO_STATE, 0, &[2]), game(1))
            .unwrap();
        let voice = |sender, seq, frame| Incoming::Voice { sender, seq, frame };
        a.send_to(&packet::encode(VOICE, 7, &[1, 0, 2]), game(1))
            .unwrap();
        assert_eq!(packet::parse(&recv(&b)), Some(voice(1, 7, &[1, 0, 2])));
        b.send_to(&packet::encode(VOICE, 9, &[3]), game(2)).unwrap();
        assert_eq!(packet::parse(&recv(&b)), Some(voice(2, 9, &[3])));
        assert_eq!(packet::parse(&recv(&a)), Some(voice(2, 9, &[3])));
        // v2: the game puts the sender's index before seq, generation and talkspurt
        let v2 = |player, kind, opus| GameMedia {
            kind,
            stream: StreamId {
                player,
                generation: 0x0102_0304_0506_0708,
            },
            sequence: 10,
            talkspurt: 3,
            opus,
        };
        // bob's HELLO did not say v2: he gets the legacy kind with seq and opus
        for (kind, legacy, opus) in [
            (Kind::Start, START, &[][..]),
            (Kind::Voice, VOICE, &[4, 0]),
            (Kind::Stop, STOP, &[]),
        ] {
            a.send_to(
                &wire::encode_game(v2(0, kind, opus), false).unwrap(),
                game(1),
            )
            .unwrap();
            assert_eq!(
                recv(&b),
                packet::encode(legacy, 1, &[&[0, 10][..], opus].concat())
            );
        }

        let report = relay.stop();
        assert_eq!(report.hellos, 2);
        assert_eq!(report.echo_states.len(), 2);
        assert!(
            report
                .echo_states
                .iter()
                .any(|s| s.sender == 2 && s.engaged)
        );
        assert!(
            report
                .echo_states
                .iter()
                .any(|s| s.sender == 1 && !s.engaged)
        );
        assert_eq!(report.ignored, 1);
        assert_eq!(
            report
                .deliveries
                .iter()
                .map(|d| (d.sender, d.kind, d.seq))
                .collect::<Vec<_>>(),
            [
                (1, VOICE, 7),
                (2, VOICE, 9),
                (1, GAME_START, 10),
                (1, GAME_VOICE, 10),
                (1, GAME_STOP, 10)
            ]
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

    fn quiet(socket: &UdpSocket) {
        socket
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        let mut buf = [0; 2048];
        assert!(matches!(
            socket.recv(&mut buf).unwrap_err().kind(),
            ErrorKind::TimedOut | ErrorKind::WouldBlock
        ));
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
    }

    fn test_player(socket: &UdpSocket) -> (u16, Player) {
        (
            0,
            Player {
                name: "test".into(),
                companion_port: socket.local_addr().unwrap().port(),
                echo: false,
                open_mic: false,
                half_duplex: false,
            },
        )
    }

    fn fast_config() -> Config {
        Config {
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            stall: None,
        }
    }

    fn snapshot(socket: &UdpSocket) -> Audience {
        assert!(matches!(
            packet::parse(&recv(socket)),
            Some(Incoming::Roster(_))
        ));
        signaling::decode_audience(&recv(socket)).unwrap()
    }

    #[test]
    fn udp_signal_is_roster_routed_and_cannot_author_audience() {
        let (a, b, c, hub) = (companion(), companion(), companion(), companion());
        let relay = Relay::start_with_hub(
            fast_config(),
            vec![test_player(&a), test_player(&b), test_player(&c)],
            Some(0),
        )
        .unwrap();
        let send = |socket: &UdpSocket, index, data: &[u8]| {
            socket
                .send_to(data, ("127.0.0.1", relay.game_port(index)))
                .unwrap();
        };
        send(
            &hub,
            0,
            &packet::encode(HELLO, hub.local_addr().unwrap().port(), &[]),
        );
        assert_eq!(
            snapshot(&hub),
            Audience {
                revision: 0,
                send_to: vec![1, 2, 3],
                receive_from: vec![]
            }
        );
        relay.set_force(3, 2);
        for socket in [&a, &b, &c, &hub] {
            snapshot(socket);
        }
        relay.set_force_scope(true);
        let aa = snapshot(&a);
        assert_eq!(aa.send_to, [2]);
        assert_eq!(aa.receive_from, [0, 2]);
        snapshot(&b);
        assert_eq!(snapshot(&c).send_to, []);
        assert_eq!(snapshot(&hub).send_to, [1, 2, 3]);

        let body = vec![0x14; 1200];
        send(&a, 1, &packet::encode(SIGNAL_OUT, 3, &body));
        assert_eq!(recv(&c), packet::encode(SIGNAL_IN, 1, &body));
        quiet(&b);
        send(&a, 1, &packet::encode(VOICE, 1, &[9]));
        assert_eq!(recv(&b), packet::encode(VOICE, 1, &[0, 1, 9]));
        quiet(&c);
        send(&a, 1, &packet::encode(STOP, 2, &[]));
        assert_eq!(recv(&b), packet::encode(STOP, 1, &[0, 2]));
        for data in [
            packet::encode(wire::AUDIENCE, 0, b"forged"),
            packet::encode(SIGNAL_IN, 0, b"forged"),
            packet::encode(SIGNAL_OUT, 3, &vec![0; 1201]),
            vec![MAGIC, VERSION, SIGNAL_OUT, 0],
        ] {
            send(&a, 1, &data);
        }
        let nested = packet::encode(wire::AUDIENCE, 0, b"nested");
        send(&a, 1, &packet::encode(SIGNAL_OUT, 3, &nested));
        assert_eq!(recv(&c), packet::encode(SIGNAL_IN, 1, &nested));
        quiet(&b);
        send(&b, 2, &packet::encode(SIGNAL_OUT, u16::MAX, b"broadcast"));
        for socket in [&a, &b, &c, &hub] {
            assert_eq!(recv(socket), packet::encode(SIGNAL_IN, 2, b"broadcast"));
        }
        send(&a, 1, &packet::encode(SIGNAL_OUT, 0, b"hub only"));
        assert_eq!(recv(&hub), packet::encode(SIGNAL_IN, 1, b"hub only"));
        send(&hub, 0, &packet::encode(SIGNAL_OUT, 1, b"server sender"));
        assert_eq!(recv(&a), packet::encode(SIGNAL_IN, 0, b"server sender"));
        relay.set_connected(2, false);
        assert_eq!(snapshot(&a).send_to, []);
        snapshot(&c);
        assert_eq!(snapshot(&hub).send_to, [1, 3]);
        quiet(&b);
        send(&a, 1, &packet::encode(SIGNAL_OUT, 2, b"departed"));
        quiet(&b);
        relay.set_connected(2, true);
        assert_eq!(snapshot(&a).send_to, [2]);
        snapshot(&b);
        snapshot(&c);
        snapshot(&hub);
        let report = relay.stop();
        assert_eq!(report.hellos, 1);
        assert_eq!(report.ignored, 4);
    }

    #[test]
    fn udp_v2_identity_bounds_mixed_legacy_and_badge_recovery() {
        use crate::transport::{
            StreamId,
            wire::{GameMedia, Kind},
        };
        let (a, b) = (companion(), companion());
        let relay = Relay::start(fast_config(), vec![test_player(&a), test_player(&b)]).unwrap();
        let send = |data: &[u8]| {
            a.send_to(data, ("127.0.0.1", relay.game_port(1))).unwrap();
        };
        // b listens as v2; a, which never says HELLO, stays a legacy sender
        b.send_to(
            &packet::encode(HELLO, 0, b"test v2"),
            ("127.0.0.1", relay.game_port(2)),
        )
        .unwrap();
        for _ in 0..4 {
            recv(&b);
        }
        let media = |kind, generation, talkspurt, sequence, opus| {
            wire::encode_game(
                GameMedia {
                    kind,
                    stream: StreamId {
                        player: 65534,
                        generation,
                    },
                    sequence,
                    talkspurt,
                    opus,
                },
                false,
            )
            .unwrap()
        };
        let generation = 0xffff_ffff_ffff_fffe;
        let start = media(Kind::Start, generation, 1, 10, &[]);
        for bad in [
            start[..14].to_vec(),
            [start.as_slice(), &[0]].concat(),
            media(Kind::Stop, generation, 1, 10, &[])[..14].to_vec(),
            media(Kind::Voice, generation, 1, 10, &[1])[..15].to_vec(),
            media(Kind::Stop, generation, 1, 10, &[]),
        ] {
            send(&bad);
        }
        send(&start);
        let forwarded = |data: &[u8]| packet::encode(data[2], 1, &data[3..]);
        assert_eq!(recv(&b), forwarded(&start));
        send(&start);
        assert_eq!(recv(&b), forwarded(&start));
        let mixed = packet::encode(VOICE, 10, &[5]);
        send(&mixed);
        assert_eq!(recv(&b), forwarded(&mixed));
        // the badge times out; the synthetic STOP is for legacy listeners only
        thread::sleep(Duration::from_millis(750));
        quiet(&b);
        send(&start);
        assert_eq!(recv(&b), forwarded(&start));
        let voice = media(Kind::Voice, generation, 1, 11, &[1, 0, 2]);
        send(&voice);
        assert_eq!(
            wire::decode_game(&recv(&b)).unwrap().stream,
            StreamId {
                player: 1,
                generation
            }
        );
        let newer = media(Kind::Start, generation, 2, 12, &[]);
        send(&newer);
        assert_eq!(recv(&b), forwarded(&newer));
        send(&media(Kind::Stop, generation, 1, 11, &[]));
        send(&start);
        let legacy = packet::encode(VOICE, 12, &[5]);
        send(&legacy);
        assert_eq!(recv(&b), forwarded(&legacy));
        let stop = media(Kind::Stop, generation, 2, 13, &[]);
        send(&stop);
        assert_eq!(recv(&b), forwarded(&stop));
        send(&newer);
        let restart = media(Kind::Start, generation + 1, 1, 0, &[]);
        send(&restart);
        assert_eq!(recv(&b), forwarded(&restart));
        send(&media(Kind::Voice, generation, 3, 14, &[5]));
        let opus = vec![5; wire::MAX_OPUS];
        let max = media(Kind::Voice, generation + 1, 1, 1, &opus);
        send(&[max.as_slice(), &[0]].concat());
        send(&max);
        assert_eq!(recv(&b), forwarded(&max));
        let before_wrap = media(Kind::Start, 0, 65535, 2, &[]);
        send(&before_wrap);
        assert_eq!(recv(&b), forwarded(&before_wrap));
        let wrapped = media(Kind::Start, 0, 0, 3, &[]);
        send(&wrapped);
        assert_eq!(recv(&b), forwarded(&wrapped));
        send(&media(Kind::Stop, 0, 65535, 2, &[]));
        quiet(&b);
        let report = relay.stop();
        assert_eq!(report.ignored, 11);
    }
}
