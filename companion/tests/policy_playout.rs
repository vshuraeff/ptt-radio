//! real companion processes with the virtual audio device; run under the realtime lock.

use std::collections::HashMap;
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use opusic_c::{Application, Channels, Encoder, SampleRate};
use ptt_radio_companion::audio::OPUS_RATE;
use ptt_radio_companion::packet;
use ptt_radio_companion::transport::StreamId;
use ptt_radio_companion::transport::signaling::{
    self, Announce, Audience, IceCredentials, Policy, Signal, Suite,
};
use ptt_radio_companion::transport::wire::{self, GameMedia, Kind, PolicyGameMedia};
use ptt_radio_companion::unix_us;

#[path = "netlab/lab.rs"]
mod lab;
#[allow(dead_code)]
#[path = "netlab/scenario.rs"]
mod network;
#[path = "netlab/router.rs"]
mod router;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        // reap the bounded test process even when an assertion fails.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn start(dir: &Path, tone: &str) -> (Process, UdpSocket, SocketAddr) {
    start_with_args(dir, tone, "2.2", &["--game-only".into()])
}

fn start_with_args(
    dir: &Path,
    tone: &str,
    seconds: &str,
    transport_args: &[String],
) -> (Process, UdpSocket, SocketAddr) {
    fs::create_dir_all(dir).unwrap();
    let game = UdpSocket::bind("127.0.0.1:0").unwrap();
    game.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let log = fs::File::create(dir.join("companion.log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_ptt-radio-companion"))
        .args(["--port", "0", "--game-port"])
        .arg(game.local_addr().unwrap().port().to_string())
        .args(["--ptt-key", "none", "--aec", "off", "--tone", tone])
        .args(transport_args)
        .args(["--exit-after", seconds, "--speaker-wav"])
        .arg(dir.join("speaker.wav"))
        .arg("--trace")
        .arg(dir.join("companion.trace"))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let process = Process(child);
    let mut buf = [0; 2048];
    let (len, address) = game.recv_from(&mut buf).unwrap();
    assert_eq!(buf[2], packet::HELLO);
    assert!(String::from_utf8_lossy(&buf[5..len]).contains(" pol1 v2"));
    (process, game, address)
}

fn pair_scope(game: &UdpSocket, companion: SocketAddr, player: u16, revision: u64, joined: bool) {
    let mut peers = vec![3 - player];
    if joined {
        peers.push(3);
    }
    let policy = signaling::encode_policy(&Policy {
        audience: Audience {
            revision,
            send_to: peers.clone(),
            receive_from: peers,
        },
        tx_permitted: true,
        self_monitor: false,
    })
    .unwrap();
    game.send_to(&policy, companion).unwrap();
}

fn pair_roster(game: &UdpSocket, companion: SocketAddr, joined: bool) {
    let roster = if joined {
        &b"1\tA\t255,0,0\n2\tB\t0,255,0\n3\tC\t0,0,255"[..]
    } else {
        &b"1\tA\t255,0,0\n2\tB\t0,255,0"[..]
    };
    game.send_to(&packet::encode(packet::ROSTER, 0, roster), companion)
        .unwrap();
}

fn longest_zero(samples: &[f32]) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for sample in samples {
        if sample.abs() < 1e-4 {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn double_talk(mode: network::Mode, events: bool) -> [[usize; 2]; 2] {
    let dir = directory(&format!(
        "double-{mode:?}-{}",
        if events { "events" } else { "control" }
    ));
    let mut network = (mode != network::Mode::Game).then(|| network::Network::new(mode, 2, None));
    let mut players = Vec::new();
    for index in 0..2 {
        let args = network.as_ref().map_or_else(
            || vec!["--game-only".into()],
            |network| network.args(index, mode.hub()),
        );
        players.push(start_with_args(
            &dir.join(if index == 0 { "a" } else { "b" }),
            if index == 0 { "660" } else { "880" },
            "8.5",
            &args,
        ));
    }
    if mode.hub() {
        network
            .as_mut()
            .unwrap()
            .attach_hub(players[0].1.local_addr().unwrap().port());
    }
    for (index, (_, game, companion)) in players.iter().enumerate() {
        pair_roster(game, *companion, false);
        game.send_to(
            &packet::encode(packet::MIC_STATE, index as u16 + 1, &[1]),
            companion,
        )
        .unwrap();
        pair_scope(game, *companion, index as u16 + 1, 7, false);
        game.set_nonblocking(true).unwrap();
    }
    let began = Instant::now();
    let mut revision = 7;
    let mut joined = false;
    let mut event_us = [0; 2];
    let mut hub = None;
    let mut pending = Vec::new();
    let mut buf = [0; 2048];
    loop {
        if let Some(network) = &mut network {
            network.poll();
        }
        for (event, millis) in [6200, 7200].into_iter().enumerate() {
            if event_us[event] == 0 && began.elapsed() >= Duration::from_millis(millis) {
                event_us[event] = unix_us();
                if events {
                    revision += 1;
                    joined = event == 0;
                    for (index, (_, game, companion)) in players.iter().enumerate() {
                        if event == 0 {
                            pair_roster(game, *companion, true);
                        }
                        pair_scope(game, *companion, index as u16 + 1, revision, joined);
                    }
                }
            }
        }
        for (index, (_, game, companion)) in players.iter().enumerate() {
            while let Ok((len, from)) = game.recv_from(&mut buf) {
                let bytes = &buf[..len];
                if bytes[2] == packet::HELLO {
                    if from == *companion {
                        pair_roster(game, *companion, event_us[0] != 0 && events);
                        pair_scope(game, *companion, index as u16 + 1, revision, joined);
                    } else {
                        assert_eq!(index, 0);
                        assert_eq!(len, 5);
                        hub = Some(from);
                    }
                } else if bytes[2] == wire::SIGNAL_OUT {
                    let (target, signal) = signaling::decode_game(bytes, false).unwrap();
                    let sender = if Some(from) == hub {
                        0
                    } else {
                        index as u16 + 1
                    };
                    let forwarded = signaling::encode_game(&signal, sender, true).unwrap();
                    let destination = if target == 0 {
                        hub
                    } else {
                        players.get(target as usize - 1).map(|player| player.2)
                    };
                    if let Some(destination) = destination {
                        let delay = if matches!(signal, Signal::GameProbe { .. }) {
                            Duration::from_millis(80)
                        } else {
                            Duration::ZERO
                        };
                        pending.push((Instant::now() + delay, destination, forwarded));
                    }
                } else if matches!(
                    bytes[2],
                    wire::GAME_POLICY_START | wire::GAME_POLICY_VOICE | wire::GAME_POLICY_STOP
                ) {
                    let mut forwarded = bytes[..3].to_vec();
                    forwarded.extend_from_slice(&(index as u16 + 1).to_be_bytes());
                    forwarded.extend_from_slice(&bytes[3..]);
                    pending.push((
                        Instant::now() + Duration::from_millis(80),
                        players[1 - index].2,
                        forwarded,
                    ));
                }
            }
        }
        pending.retain(|(at, to, bytes)| {
            if Instant::now() < *at {
                true
            } else {
                players[0].1.send_to(bytes, to).unwrap();
                false
            }
        });
        let mut complete = true;
        for (process, _, _) in &mut players {
            if let Some(status) = process.0.try_wait().unwrap() {
                assert!(status.success(), "{}", dir.display());
            } else {
                complete = false;
            }
        }
        if complete {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(12),
            "{}",
            dir.display()
        );
        thread::sleep(Duration::from_millis(1));
    }
    let traces: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|player| fs::read_to_string(dir.join(player).join("companion.trace")).unwrap())
        .collect();
    let mut gaps = [[0; 2]; 2];
    for (index, trace) in traces.iter().enumerate() {
        let peer = (2 - index).to_string();
        let from = event_us[0] - 100_000;
        let until = event_us[1] + 300_000;
        let words: Vec<Vec<_>> = trace
            .lines()
            .map(|line| line.split_whitespace().collect())
            .collect();
        let selected = words.iter().rfind(|line| {
            line[1] == "switch" && line[2] == peer && line[0].parse::<u64>().unwrap() <= from
        });
        let route = mode.route();
        if route != "Game" || selected.is_some() {
            assert_eq!(
                selected.map(|line| line[3]),
                Some(route),
                "{mode:?}: {}",
                dir.display()
            );
        }
        let in_window = |line: &&Vec<&str>| {
            let at: u64 = line[0].parse().unwrap();
            at >= from && at < until
        };
        assert!(
            words
                .iter()
                .filter(in_window)
                .filter(|line| line[1] == "switch" && line[2] == peer)
                .all(|line| line[3] == route)
        );
        let sends = words
            .iter()
            .filter(in_window)
            .filter(|line| {
                line[1] == "route_tx"
                    && line[3] == route
                    && line[2] == if route == "Game" { "0" } else { &peer }
            })
            .count();
        assert!(
            sends > 10,
            "{mode:?} only {sends} actual sends, {}",
            dir.display()
        );
        let tx: HashMap<u16, u64> = traces[1 - index]
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields[1] == "tx")
                    .then(|| (fields[2].parse().unwrap(), fields[0].parse().unwrap()))
            })
            .collect();
        let mut delays: Vec<f64> = words
            .iter()
            .filter(in_window)
            .filter(|line| line[1] == "rx" && line[2] == peer)
            .filter_map(|line| {
                let at: u64 = line[0].parse().unwrap();
                tx.get(&line[3].parse::<u16>().unwrap())
                    .map(|sent| (at - sent) as f64 / 1000.0)
            })
            .collect();
        assert!(
            delays.len() > 10,
            "{mode:?} media not delivered, {}",
            dir.display()
        );
        delays.sort_by(f64::total_cmp);
        let median = delays[delays.len() / 2];
        assert!(
            network::delay_band(route).contains(&median),
            "{mode:?} actual median {median} ms, {}",
            dir.display()
        );
        let device_us: u64 = words.iter().find(|line| line[1] == "device").unwrap()[2]
            .parse()
            .unwrap();
        let mut wav = hound::WavReader::open(
            dir.join(if index == 0 { "a" } else { "b" })
                .join("speaker.wav"),
        )
        .unwrap();
        let samples: Vec<f32> = wav.samples::<f32>().collect::<Result<_, _>>().unwrap();
        for (event, at) in event_us.iter().enumerate() {
            if events {
                let revision = (8 + event).to_string();
                assert!(
                    words.iter().any(|line| {
                        line[1] == "policy"
                            && line[2] == revision
                            && (line[0].parse::<u64>().unwrap()).abs_diff(*at) < 100_000
                    }),
                    "{mode:?} player {} policy {revision} not installed, {}",
                    index + 1,
                    dir.display()
                );
            }
            let first = ((at - 100_000 - device_us) * u64::from(OPUS_RATE) / 1_000_000) as usize;
            let window = &samples[first..first + OPUS_RATE as usize * 2 / 5];
            let rms = (window.iter().map(|sample| sample * sample).sum::<f32>()
                / window.len() as f32)
                .sqrt();
            gaps[index][event] = longest_zero(window);
            assert!(
                rms > 0.01,
                "{mode:?} player {} unusably quiet window {event}: RMS {rms}, {}",
                index + 1,
                dir.display()
            );
            if !events {
                assert!(
                    gaps[index][event] <= OPUS_RATE as usize / 100,
                    "{mode:?} player {} unhealthy control window {event}: {} samples, {}",
                    index + 1,
                    gaps[index][event],
                    dir.display()
                );
            }
        }
        println!(
            "double-talk {mode:?} events={events} player={} selected={route} actual-sends={sends} tx-rx-median={median:.3}ms gaps={:?}samples artifacts={}",
            index + 1,
            gaps[index],
            dir.display()
        );
    }
    gaps
}

#[test]
#[ignore = "real-time double-talk route measurement; requires the realtime lock"]
fn unrelated_join_and_retune_preserve_double_talk_on_direct_hub_and_game() {
    for mode in [
        network::Mode::Direct,
        network::Mode::Hub,
        network::Mode::Game,
    ] {
        let control = double_talk(mode, false);
        let events = double_talk(mode, true);
        for player in 0..2 {
            for event in 0..2 {
                assert!(
                    events[player][event] <= control[player][event] + OPUS_RATE as usize / 100,
                    "{mode:?} player {} event {event}: {} samples vs control {} (+10ms normal jitter)",
                    player + 1,
                    events[player][event],
                    control[player][event]
                );
            }
        }
    }
}

#[test]
#[ignore = "real Lua-produced channel groups; requires the realtime lock"]
fn lua_channel_groups_are_audible_internally_and_isolated_on_direct_and_game() {
    let lua = Command::new("lua")
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
        .args([
            "mod/tests/control_test.lua",
            "mod/ptt-radio/control.lua",
            "--groups",
        ])
        .output()
        .unwrap();
    assert!(
        lua.status.success(),
        "{}",
        String::from_utf8_lossy(&lua.stderr)
    );
    let mut policies = Vec::new();
    for line in String::from_utf8(lua.stdout).unwrap().lines() {
        if let Some(line) = line.strip_prefix("group-policy ") {
            let (player, hex) = line.split_once(' ').unwrap();
            assert_eq!(player.parse::<usize>().unwrap(), policies.len() + 1);
            let bytes: Vec<_> = hex
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            policies.push((signaling::decode_policy(&bytes).unwrap(), bytes));
        }
    }
    assert_eq!(policies.len(), 4);
    let tones = [660.0_f64, 880.0, 1170.0, 1490.0];
    let peers = [1, 0, 3, 2];
    for mode in [network::Mode::Direct, network::Mode::Game] {
        let dir = directory(&format!("channel-groups-{mode:?}"));
        let mut network =
            (mode == network::Mode::Direct).then(|| network::Network::new(mode, 4, None));
        let mut players = Vec::new();
        for (index, tone) in tones.iter().enumerate() {
            assert_eq!(
                policies[index].0.audience.send_to,
                [peers[index] as u16 + 1]
            );
            assert_eq!(
                policies[index].0.audience.receive_from,
                [peers[index] as u16 + 1]
            );
            let args = network.as_ref().map_or_else(
                || vec!["--game-only".into()],
                |network| network.args(index, false),
            );
            players.push(start_with_args(
                &dir.join((index + 1).to_string()),
                &tone.to_string(),
                "8.5",
                &args,
            ));
        }
        let refresh = |index: usize, players: &[(Process, UdpSocket, SocketAddr)]| {
            let (_, game, companion) = &players[index];
            game.send_to(
                &packet::encode(
                    packet::ROSTER,
                    0,
                    b"1\tA\t255,0,0\n2\tB\t0,255,0\n3\tC\t0,0,255\n4\tD\t255,255,0",
                ),
                companion,
            )
            .unwrap();
            game.send_to(
                &packet::encode(packet::MIC_STATE, index as u16 + 1, &[1]),
                companion,
            )
            .unwrap();
            game.send_to(&policies[index].1, companion).unwrap();
        };
        for (index, (_, game, _)) in players.iter().enumerate() {
            refresh(index, &players);
            game.set_nonblocking(true).unwrap();
        }
        let began = Instant::now();
        let origin_us = unix_us();
        let mut pending = Vec::new();
        let mut buf = [0; 2048];
        loop {
            if let Some(network) = &mut network {
                network.poll();
            }
            for (index, (_, game, _)) in players.iter().enumerate() {
                while let Ok((len, _)) = game.recv_from(&mut buf) {
                    let bytes = &buf[..len];
                    if bytes[2] == packet::HELLO {
                        refresh(index, &players);
                    } else if bytes[2] == wire::SIGNAL_OUT {
                        let (target, signal) = signaling::decode_game(bytes, false).unwrap();
                        if let Some(player) = target
                            .checked_sub(1)
                            .and_then(|target| players.get(target as usize))
                        {
                            let delay = if matches!(signal, Signal::GameProbe { .. }) {
                                Duration::from_millis(80)
                            } else {
                                Duration::ZERO
                            };
                            pending.push((
                                Instant::now() + delay,
                                player.2,
                                signaling::encode_game(&signal, index as u16 + 1, true).unwrap(),
                            ));
                        }
                    } else if matches!(
                        bytes[2],
                        wire::GAME_POLICY_START | wire::GAME_POLICY_VOICE | wire::GAME_POLICY_STOP
                    ) {
                        let mut forwarded = bytes[..3].to_vec();
                        forwarded.extend_from_slice(&(index as u16 + 1).to_be_bytes());
                        forwarded.extend_from_slice(&bytes[3..]);
                        let media = wire::decode_policy_game(&forwarded).unwrap();
                        assert_eq!(media.revision, policies[index].0.audience.revision);
                        for target in &policies[index].0.audience.send_to {
                            pending.push((
                                Instant::now() + Duration::from_millis(80),
                                players[*target as usize - 1].2,
                                forwarded.clone(),
                            ));
                        }
                    }
                }
            }
            pending.retain(|(at, to, bytes)| {
                if Instant::now() < *at {
                    true
                } else {
                    players[0].1.send_to(bytes, to).unwrap();
                    false
                }
            });
            let mut complete = true;
            for (process, _, _) in &mut players {
                if let Some(status) = process.0.try_wait().unwrap() {
                    assert!(status.success(), "{}", dir.display());
                } else {
                    complete = false;
                }
            }
            if complete {
                break;
            }
            assert!(
                began.elapsed() < Duration::from_secs(12),
                "{}",
                dir.display()
            );
            thread::sleep(Duration::from_millis(1));
        }
        let traces: Vec<String> = (1..=4)
            .map(|player| {
                fs::read_to_string(dir.join(player.to_string()).join("companion.trace")).unwrap()
            })
            .collect();
        for index in 0..4 {
            let from = origin_us + 6_200_000;
            let until = origin_us + 8_000_000;
            let peer = (peers[index] + 1).to_string();
            let words: Vec<Vec<_>> = traces[index]
                .lines()
                .map(|line| line.split_whitespace().collect())
                .collect();
            let in_window = |line: &&Vec<&str>| {
                let at: u64 = line[0].parse().unwrap();
                at >= from && at < until
            };
            let selected = words.iter().rfind(|line| {
                line[1] == "switch" && line[2] == peer && line[0].parse::<u64>().unwrap() <= from
            });
            if mode == network::Mode::Direct || selected.is_some() {
                assert_eq!(
                    selected.map(|line| line[3]),
                    Some(mode.route()),
                    "{}",
                    dir.display()
                );
            }
            assert!(
                words
                    .iter()
                    .filter(in_window)
                    .filter(|line| line[1] == "switch" && line[2] == peer)
                    .all(|line| line[3] == mode.route())
            );
            let sends = words
                .iter()
                .filter(in_window)
                .filter(|line| {
                    line[1] == "route_tx"
                        && line[3] == mode.route()
                        && line[2]
                            == if mode == network::Mode::Game {
                                "0"
                            } else {
                                &peer
                            }
                })
                .count();
            assert!(sends > 20, "{}", dir.display());
            let tx: HashMap<u16, u64> = traces[peers[index]]
                .lines()
                .filter_map(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    (fields[1] == "tx")
                        .then(|| (fields[2].parse().unwrap(), fields[0].parse().unwrap()))
                })
                .collect();
            let mut delays = Vec::new();
            for line in words
                .iter()
                .filter(in_window)
                .filter(|line| line[1] == "rx")
            {
                assert_eq!(line[2], peer, "cross-group receive, {}", dir.display());
                let at: u64 = line[0].parse().unwrap();
                if let Some(sent) = tx.get(&line[3].parse::<u16>().unwrap()) {
                    delays.push((at - sent) as f64 / 1000.0);
                }
            }
            assert!(delays.len() > 20, "no in-group delivery, {}", dir.display());
            delays.sort_by(f64::total_cmp);
            let median = delays[delays.len() / 2];
            assert!(
                network::delay_band(mode.route()).contains(&median),
                "{median}ms, {}",
                dir.display()
            );
            let device_us: u64 = words.iter().find(|line| line[1] == "device").unwrap()[2]
                .parse()
                .unwrap();
            let mut wav =
                hound::WavReader::open(dir.join((index + 1).to_string()).join("speaker.wav"))
                    .unwrap();
            let samples: Vec<f32> = wav.samples::<f32>().collect::<Result<_, _>>().unwrap();
            let first = ((from - device_us) * u64::from(OPUS_RATE) / 1_000_000) as usize;
            let end = ((until - device_us) * u64::from(OPUS_RATE) / 1_000_000) as usize;
            let samples = &samples[first..end];
            let amplitude = |hz: f64| {
                let (mut real, mut imaginary) = (0.0, 0.0);
                for (sample, value) in samples.iter().enumerate() {
                    let phase = std::f64::consts::TAU * hz * sample as f64 / f64::from(OPUS_RATE);
                    real += f64::from(*value) * phase.cos();
                    imaginary += f64::from(*value) * phase.sin();
                }
                2.0 * real.hypot(imaginary) / samples.len() as f64
            };
            let wanted = amplitude(tones[peers[index]]);
            assert!(
                wanted > 0.03 && longest_zero(samples) < OPUS_RATE as usize / 100,
                "inaudible in-group voice, {}",
                dir.display()
            );
            let forbidden = tones
                .iter()
                .enumerate()
                .filter(|(other, _)| *other / 2 != index / 2)
                .map(|(_, hz)| amplitude(*hz))
                .fold(0.0_f64, f64::max);
            let cross_db = 20.0 * (forbidden / wanted).max(1e-12).log10();
            assert!(
                cross_db < -40.0,
                "cross-group tone {cross_db}dB, {}",
                dir.display()
            );
            println!(
                "channel-groups {mode:?} player={} peer={} selected={} actual-sends={sends} tx-rx-median={median:.3}ms wanted-amplitude={wanted:.5} forbidden={cross_db:.2}dB artifacts={}",
                index + 1,
                peer,
                mode.route(),
                dir.display()
            );
        }
    }
}

fn scope(revision: u64, receive: &[u16], tx: bool) -> Vec<u8> {
    signaling::encode_policy(&Policy {
        audience: Audience {
            revision,
            send_to: vec![],
            receive_from: receive.to_vec(),
        },
        tx_permitted: tx,
        self_monitor: false,
    })
    .unwrap()
}

fn announce() -> Vec<u8> {
    signaling::encode_game(
        &Signal::Announce(Announce {
            version: signaling::PROTOCOL_VERSION,
            capabilities: signaling::CAP_MEDIA_V2,
            suites: vec![Suite::Aes256Gcm],
            stream_generation: 42,
            static_key: [1; 32],
            ice: IceCredentials {
                restart_generation: 1,
                ufrag: "policy-test".into(),
                password: "test-only-ice".into(),
            },
            candidates: vec![],
            candidates_complete: true,
        }),
        1,
        true,
    )
    .unwrap()
}

fn setup(game: &UdpSocket, companion: SocketAddr, receive: &[u16], open: bool) {
    for bytes in [
        packet::encode(
            packet::ROSTER,
            0,
            b"1\tsource\t255,0,0\n2\tlistener\t0,255,0\n3\tsilent\t0,0,255",
        ),
        packet::encode(packet::MIC_STATE, 2, &[u8::from(open)]),
        scope(7, receive, true),
        announce(),
    ] {
        game.send_to(&bytes, companion).unwrap();
    }
}

fn directory(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ptt-radio-policy-{name}-{}-{}",
        std::process::id(),
        unix_us()
    ))
}

#[test]
#[ignore = "real-time virtual-device measurement; requires the realtime lock"]
fn revoked_audio_stops_within_one_virtual_device_period_and_silent_retune_has_no_gap() {
    for phase in [0, 3, 7] {
        let dir = directory(&format!("tail-{phase}"));
        let (mut process, game, companion) = start(&dir, "0");
        setup(&game, companion, &[1, 3], false);
        let mut encoder =
            Encoder::new(Channels::Mono, SampleRate::Hz48000, Application::Voip).unwrap();
        let start = Instant::now() + Duration::from_millis(80 + phase);
        let mut revision = 7;
        let mut silent_retune = 0;
        let mut skipped_epoch = 0;
        for frame in 0..65 {
            thread::sleep(
                (start + Duration::from_millis(frame * 20))
                    .saturating_duration_since(Instant::now()),
            );
            if frame == 15 {
                revision = 8;
                silent_retune = unix_us();
                game.send_to(&scope(8, &[1], true), companion).unwrap();
            }
            if frame == 25 {
                revision = 10;
                skipped_epoch = unix_us();
                game.send_to(&scope(10, &[1], true), companion).unwrap();
            }
            if frame == 45 {
                game.send_to(&scope(11, &[], true), companion).unwrap();
            }
            let pcm: Vec<_> = (0..960)
                .map(|sample| {
                    let time = (frame * 960 + sample) as f32 / OPUS_RATE as f32;
                    0.3 * (std::f32::consts::TAU * 880.0 * time).sin()
                })
                .collect();
            let mut opus = [0; wire::MAX_POLICY_OPUS];
            let len = encoder.encode_float_to_slice(&pcm, &mut opus).unwrap();
            let bytes = wire::encode_policy_game(
                PolicyGameMedia {
                    media: GameMedia {
                        stream: StreamId {
                            player: 1,
                            generation: 42,
                        },
                        kind: Kind::PolicyVoice,
                        sequence: frame as u16 + 1,
                        talkspurt: 1,
                        opus: &opus[..len],
                    },
                    revision,
                },
                true,
            )
            .unwrap();
            game.send_to(&bytes, companion).unwrap();
        }
        assert!(process.0.wait().unwrap().success(), "{}", dir.display());
        let trace = fs::read_to_string(dir.join("companion.trace")).unwrap();
        let device_us: u64 = trace
            .lines()
            .find_map(|line| {
                let words: Vec<_> = line.split_whitespace().collect();
                (words.get(1) == Some(&"device")).then(|| words[2].parse().unwrap())
            })
            .unwrap();
        let fence: Vec<_> = trace
            .lines()
            .rfind(|line| line.split_whitespace().nth(1) == Some("policy_purge"))
            .unwrap()
            .split_whitespace()
            .collect();
        let enforced_us: u64 = fence[2].parse().unwrap();
        let dropped: usize = fence[4].parse().unwrap();
        let mut reader = hound::WavReader::open(dir.join("speaker.wav")).unwrap();
        let samples: Vec<f32> = reader.samples::<f32>().collect::<Result<_, _>>().unwrap();
        let last = samples
            .iter()
            .rposition(|sample| sample.abs() > 1e-4)
            .expect("audible source before revocation");
        let last_us = device_us as f64 + (last + 1) as f64 * 1e6 / OPUS_RATE as f64;
        let tail_ms = ((last_us - enforced_us as f64) / 1000.0).max(0.0);
        assert!(tail_ms <= 12.0, "tail {tail_ms:.3} ms, {}", dir.display());
        let first = ((silent_retune.saturating_sub(device_us) as f64 / 1e6 - 0.080)
            * OPUS_RATE as f64) as usize;
        let end = first + OPUS_RATE as usize / 5;
        let mut longest = 0;
        let mut silence = 0;
        for sample in &samples[first..end] {
            if sample.abs() < 1e-4 {
                silence += 1;
                longest = longest.max(silence);
            } else {
                silence = 0;
            }
        }
        assert!(
            longest < OPUS_RATE as usize / 500,
            "silent retune gap {} ms, {}",
            longest as f64 * 1000.0 / OPUS_RATE as f64,
            dir.display()
        );
        let first = ((skipped_epoch.saturating_sub(device_us) as f64 / 1e6 - 0.020)
            * OPUS_RATE as f64) as usize;
        let mut longest_gap = 0;
        let mut gap = 0;
        for sample in &samples[first..first + OPUS_RATE as usize / 5] {
            if sample.abs() < 1e-4 {
                gap += 1;
                longest_gap = longest_gap.max(gap);
            } else {
                gap = 0;
            }
        }
        let gap_ms = longest_gap as f64 * 1000.0 / OPUS_RATE as f64;
        assert!(
            gap_ms < 150.0,
            "skipped-epoch recovery {gap_ms:.3} ms, {}",
            dir.display()
        );
        println!(
            "phase={phase}ms tail={tail_ms:.3}ms dropped={dropped} samples silent-retune-longest-zero={longest} samples skipped-epoch-gap={gap_ms:.3}ms artifacts={}",
            dir.display()
        );
    }
}

#[test]
#[ignore = "real companion process; requires the realtime lock"]
fn open_mic_mute_and_reenable_uses_a_fresh_talkspurt_with_an_empty_send_set() {
    let dir = directory("open-mic");
    let (mut process, game, companion) = start(&dir, "660");
    setup(&game, companion, &[], true);
    game.set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let start = Instant::now();
    let mut muted = false;
    let mut enabled = false;
    let mut voice = Vec::new();
    let mut buf = [0; 2048];
    while start.elapsed() < Duration::from_millis(1500) {
        if !muted && start.elapsed() >= Duration::from_millis(400) {
            game.send_to(&scope(8, &[], false), companion).unwrap();
            muted = true;
        }
        if !enabled && start.elapsed() >= Duration::from_millis(800) {
            game.send_to(&scope(9, &[], true), companion).unwrap();
            enabled = true;
        }
        let Ok((len, _)) = game.recv_from(&mut buf) else {
            continue;
        };
        if buf[2] == wire::GAME_POLICY_VOICE {
            let mut forwarded = buf[..3].to_vec();
            forwarded.extend_from_slice(&2u16.to_be_bytes());
            forwarded.extend_from_slice(&buf[3..len]);
            let packet = wire::decode_policy_game(&forwarded).unwrap();
            voice.push((packet.revision, packet.media.talkspurt));
        }
    }
    assert!(process.0.wait().unwrap().success());
    let before: Vec<_> = voice
        .iter()
        .filter(|(revision, _)| *revision == 7)
        .collect();
    let after: Vec<_> = voice
        .iter()
        .filter(|(revision, _)| *revision == 9)
        .collect();
    assert!(
        before.len() >= 10 && after.len() >= 10,
        "{voice:?}, {}",
        dir.display()
    );
    assert!(!voice.iter().any(|(revision, _)| *revision == 8));
    assert_ne!(before[0].1, after[0].1);
    assert!(after.iter().all(|frame| frame.1 == after[0].1));
    println!(
        "open-mic voices before={} after={} talkspurts {} -> {}, artifacts={}",
        before.len(),
        after.len(),
        before[0].1,
        after[0].1,
        dir.display()
    );
}

#[cfg(unix)]
#[test]
fn launch_mode_runs_the_game_and_exits_with_its_status_after_a_clean_shutdown() {
    let dir = directory("launch");
    fs::create_dir_all(&dir).unwrap();
    let game = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = game.local_addr().unwrap().port().to_string();
    let argv = dir.join("argv");
    let script = format!(
        "printf '%s\\n' \"$@\" > '{}'; sleep 1; exit 3",
        argv.display()
    );
    let log = fs::File::create(dir.join("companion.log")).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_ptt-radio-companion"))
            .args(["--port", "0", "--game-port", &port])
            .args([
                "--ptt-key",
                "none",
                "--aec",
                "off",
                "--tone",
                "660",
                "--game-only",
            ])
            .args(["--exit-after", "20", "--speaker-wav"])
            .arg(dir.join("speaker.wav"))
            .args([
                "--launch",
                "/bin/sh",
                "-c",
                &script,
                "game",
                "--mp-connect",
                "host",
            ])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let began = Instant::now();
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{}",
            dir.display()
        );
        thread::sleep(Duration::from_millis(20));
    };
    let log = fs::read_to_string(dir.join("companion.log")).unwrap();
    assert_eq!(status.code(), Some(3), "{log}");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "ends with the game, {log}"
    );
    assert_eq!(
        fs::read_to_string(&argv).unwrap(),
        format!("--mp-connect\nhost\n--enable-lua-udp\n{port}\n")
    );
    assert!(
        log.contains("game exited") && log.contains("stats: "),
        "{log}"
    );
}

#[cfg(unix)]
#[test]
fn launch_mode_starts_the_game_alone_when_the_companion_port_is_taken() {
    let dir = directory("launch-alone");
    fs::create_dir_all(&dir).unwrap();
    // held for the whole test: the companion's own bind is its first setup step
    let taken = UdpSocket::bind("127.0.0.1:0").unwrap();
    let game = UdpSocket::bind("127.0.0.1:0").unwrap();
    let game_port = game.local_addr().unwrap().port().to_string();
    let argv = dir.join("argv");
    let script = format!("printf '%s\\n' \"$@\" > '{}'; exit 3", argv.display());
    let log = fs::File::create(dir.join("companion.log")).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_ptt-radio-companion"))
            .arg("--port")
            .arg(taken.local_addr().unwrap().port().to_string())
            .args(["--game-port", &game_port])
            .args(["--ptt-key", "none", "--aec", "off", "--tone", "660"])
            .args(["--game-only", "--exit-after", "20", "--speaker-wav"])
            .arg(dir.join("speaker.wav"))
            .args(["--launch", "/bin/sh", "-c", &script, "game"])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let began = Instant::now();
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{}",
            dir.display()
        );
        thread::sleep(Duration::from_millis(20));
    };
    let log = fs::read_to_string(dir.join("companion.log")).unwrap();
    assert_eq!(status.code(), Some(3), "{log}");
    assert!(
        log.contains("cannot bind") && log.contains("without the companion"),
        "{log}"
    );
    assert!(!log.contains("game started"), "{log}");
    assert_eq!(
        fs::read_to_string(&argv).unwrap(),
        format!("--enable-lua-udp\n{game_port}\n")
    );
}

fn self_monitor(revision: u64, tx: bool) -> Vec<u8> {
    signaling::encode_policy(&Policy {
        audience: Audience {
            revision,
            send_to: vec![],
            receive_from: vec![],
        },
        tx_permitted: tx,
        self_monitor: tx,
    })
    .unwrap()
}

/// plays the game for player 1 until the RESULT of `id`: its policy media go back to it
/// 60 ms later when `echo` is set. returns the result and the talkspurt of each echoed VOICE
fn self_test_result(
    game: &UdpSocket,
    companion: SocketAddr,
    id: u16,
    echo: bool,
) -> (packet::SelfTestResult, Vec<u16>) {
    game.send_to(
        &[
            packet::MAGIC,
            packet::VERSION,
            packet::SELF_TEST,
            0,
            0,
            (id >> 8) as u8,
            id as u8,
        ],
        companion,
    )
    .unwrap();
    game.set_read_timeout(Some(Duration::from_millis(1)))
        .unwrap();
    let began = Instant::now();
    let mut pending: Vec<(Instant, Vec<u8>)> = Vec::new();
    let mut talkspurts = Vec::new();
    let mut buf = [0; 2048];
    loop {
        assert!(began.elapsed() < Duration::from_secs(15), "no RESULT {id}");
        pending.retain(|(at, bytes)| {
            if Instant::now() < *at {
                return true;
            }
            game.send_to(bytes, companion).unwrap();
            false
        });
        let Ok((len, _)) = game.recv_from(&mut buf) else {
            continue;
        };
        let bytes = &buf[..len];
        if let Some(result) = packet::decode_self_test_result(bytes) {
            assert_eq!(result.test_id, id);
            return (result, talkspurts);
        }
        if echo
            && matches!(
                bytes[2],
                wire::GAME_POLICY_START | wire::GAME_POLICY_VOICE | wire::GAME_POLICY_STOP
            )
        {
            let mut forwarded = bytes[..3].to_vec();
            forwarded.extend_from_slice(&1u16.to_be_bytes());
            forwarded.extend_from_slice(&bytes[3..]);
            if bytes[2] == wire::GAME_POLICY_VOICE {
                talkspurts.push(
                    wire::decode_policy_game(&forwarded)
                        .unwrap()
                        .media
                        .talkspurt,
                );
            }
            pending.push((Instant::now() + Duration::from_millis(60), forwarded));
        }
    }
}

fn amplitude(samples: &[f32], hz: f64) -> f64 {
    let (mut real, mut imaginary) = (0.0, 0.0);
    for (index, value) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * hz * index as f64 / f64::from(OPUS_RATE);
        real += f64::from(*value) * phase.cos();
        imaginary += f64::from(*value) * phase.sin();
    }
    2.0 * real.hypot(imaginary) / samples.len() as f64
}

#[test]
#[ignore = "real companion process with the virtual device; requires the realtime lock"]
fn self_test_plays_its_tone_and_the_returned_recording_through_half_duplex() {
    let dir = directory("self-test");
    let (mut process, game, companion) =
        start_with_args(&dir, "660", "16", &["--game-only".into()]);
    for bytes in [
        packet::encode(packet::ROSTER, 0, b"1\tself\t255,0,0"),
        packet::encode(packet::MIC_STATE, 1, &[1]),
        packet::encode(packet::HALF_DUPLEX, 1, &[1]),
        self_monitor(7, true),
    ] {
        game.send_to(&bytes, companion).unwrap();
    }
    let (busy, _) = self_test_result(&game, companion, 1, false);
    assert_eq!(busy.outcome, packet::Outcome::Busy, "open mic is on");
    game.send_to(&packet::encode(packet::MIC_STATE, 1, &[0]), companion)
        .unwrap();
    game.send_to(&self_monitor(8, false), companion).unwrap();
    let (denied, _) = self_test_result(&game, companion, 2, false);
    assert_eq!(denied.outcome, packet::Outcome::TxNotPermitted);
    game.send_to(&self_monitor(9, true), companion).unwrap();
    // the POLICY is installed before the request: both travel the same socket in order
    let started_us = unix_us();
    let (result, talkspurts) = self_test_result(&game, companion, 0x1234, true);
    let finished_us = unix_us();
    assert!(process.0.wait().unwrap().success());
    println!(
        "self-test {result:?} in {} ms, artifacts={}",
        (finished_us - started_us) / 1000,
        dir.display()
    );
    assert_eq!(result.outcome, packet::Outcome::Ok, "{}", dir.display());
    assert_eq!(result.tone, 1);
    // --tone plays at 0.3, about -10 dBFS
    assert!((-12..=-9).contains(&result.mic_peak_dbfs), "{result:?}");
    assert_eq!(result.frames_sent, 150, "3 s of 20 ms frames, no pre-roll");
    assert_eq!(talkspurts.len(), 150);
    assert!(
        talkspurts.iter().all(|t| *t == talkspurts[0]),
        "one talkspurt"
    );
    assert_eq!(result.frames_returned, result.frames_sent);
    assert!((60..300).contains(&result.first_return_ms), "{result:?}");
    // the RESULT follows the last frame by 2 s: tone, capture, send and wait
    let elapsed_ms = (finished_us - started_us) / 1000;
    assert!((8_400..10_000).contains(&elapsed_ms), "{elapsed_ms} ms");

    let trace = fs::read_to_string(dir.join("companion.trace")).unwrap();
    let device_us: u64 = trace
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .nth(1)
                .filter(|w| *w == "device")
                .map(|_| line)
        })
        .and_then(|line| line.split_whitespace().nth(2))
        .unwrap()
        .parse()
        .unwrap();
    let mut wav = hound::WavReader::open(dir.join("speaker.wav")).unwrap();
    let samples: Vec<f32> = wav.samples::<f32>().collect::<Result<_, _>>().unwrap();
    let at = |us: u64| ((us - device_us) * u64::from(OPUS_RATE) / 1_000_000) as usize;
    let tone = &samples[at(started_us + 50_000)..at(started_us + 450_000)];
    assert!(
        amplitude(tone, 440.0) > 0.2,
        "local tone {}",
        amplitude(tone, 440.0)
    );
    assert!(
        amplitude(tone, 660.0) < 0.01,
        "the input is not played before it is sent"
    );
    // the returned copy plays while half-duplex is on: the test never holds PTT
    let echo = &samples[at(started_us + 4_000_000)..at(started_us + 6_500_000)];
    assert!(
        amplitude(echo, 660.0) > 0.1,
        "returned copy {}",
        amplitude(echo, 660.0)
    );
    assert!(amplitude(echo, 440.0) < 0.01, "the tone is never sent");
}
