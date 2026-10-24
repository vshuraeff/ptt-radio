//! production connection owner. time and datagrams are injected so the socket
//! pump and the network lab exercise exactly the same route and crypto policy.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::connectivity::{
    Action, ConnectivityConfig, DatagramTx, DiscoveryAction, HubReflection, PairConnectivity,
    ProbeEvent, ReceiveResult,
};
use crate::crypto::{self, Session, StaticKeypair};
use crate::packet;
use crate::transport::relay::{BindClient, CHALLENGE_MS, Limits, Relay, Reply, grant_frames};
use crate::transport::signaling::{self, Announce, Audience, RelayGrant, Signal, Suite};
use crate::transport::wire::{self, Bind, Datagram, GameMedia, Inner, Kind, ReceiverHandle};
use crate::transport::{Path, StreamId};

const RETRY: Duration = Duration::from_millis(500);
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);
const IMPROVE: Duration = Duration::from_millis(10);
const SUSTAIN: Duration = Duration::from_secs(3);
const REFRESH: Duration = Duration::from_millis(250);
// the game replies to each five-second hello with a policy, even if unchanged
const MIC_POLICY_LIFETIME: Duration = Duration::from_secs(60);

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Default)]
pub struct RuntimeConfig {
    pub connectivity: ConnectivityConfig,
    pub hub: Option<SocketAddr>,
    pub game_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_microphone_needs_a_current_permitted_policy_and_equal_refreshes_count() {
        let now = Instant::now();
        let mut runtime = Runtime::new(vec![], RuntimeConfig::default()).unwrap();
        assert!(
            runtime.capture_policy().0,
            "legacy sources keep their default"
        );
        assert!(!runtime.microphone_permitted(now));
        install_policy(&mut runtime, 7, &[], true, false, now);
        assert!(runtime.microphone_permitted(now));
        assert!(!runtime.microphone_permitted(now + MIC_POLICY_LIFETIME));
        install_policy(
            &mut runtime,
            7,
            &[],
            true,
            false,
            now + Duration::from_secs(30),
        );
        assert!(runtime.microphone_permitted(now + Duration::from_secs(75)));
        install_policy(
            &mut runtime,
            6,
            &[],
            true,
            false,
            now + Duration::from_secs(89),
        );
        install_policy(
            &mut runtime,
            7,
            &[],
            false,
            false,
            now + Duration::from_secs(89),
        );
        assert!(
            !runtime.microphone_permitted(now + Duration::from_secs(90)),
            "rollback and same-revision mutation do not refresh"
        );
        install_policy(
            &mut runtime,
            8,
            &[],
            false,
            false,
            now + Duration::from_secs(91),
        );
        assert!(!runtime.microphone_permitted(now + Duration::from_secs(91)));
        install_policy(
            &mut runtime,
            9,
            &[],
            true,
            false,
            now + Duration::from_secs(92),
        );
        assert!(runtime.microphone_permitted(now + Duration::from_secs(92)));
        runtime.policy = None;
        assert!(!runtime.microphone_permitted(now + Duration::from_secs(92)));
    }

    #[test]
    fn policy_close_flushes_wire_stops_under_the_retiring_permission() {
        let now = Instant::now();
        let mut runtime = Runtime::new(vec![], RuntimeConfig::default()).unwrap();
        install_policy(&mut runtime, 7, &[], true, false, now);
        runtime.transmit(&packet::encode(packet::START, 41, &[]), now);
        while runtime.poll().is_some() {}
        let denied = signaling::encode_policy(&signaling::Policy {
            audience: Audience {
                revision: 8,
                send_to: vec![1, 2],
                receive_from: vec![],
            },
            tx_permitted: false,
            self_monitor: false,
        })
        .unwrap();
        let mut closed = false;
        runtime
            .game_with_capture_close(&denied, now, |runtime, permitted| {
                assert!(!permitted);
                assert!(runtime.capture_policy().0);
                assert_eq!(runtime.capture_policy().1, Some(7));
                runtime.close_transmit();
                let stops: Vec<_> = std::iter::from_fn(|| runtime.poll())
                    .filter_map(|output| {
                        if let Output::Game(bytes) = output
                            && bytes[2] == wire::GAME_POLICY_STOP
                        {
                            let mut forwarded = bytes[..3].to_vec();
                            forwarded.extend_from_slice(&1u16.to_be_bytes());
                            forwarded.extend_from_slice(&bytes[3..]);
                            wire::decode_policy_game(&forwarded).ok().map(|media| {
                                (media.media.kind, media.revision, media.media.sequence)
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(
                    stops,
                    [(Kind::PolicyStop, 7, 41), (Kind::PolicyStop, 7, 41)]
                );
                closed = true;
            })
            .unwrap();
        assert!(closed);
        assert!(!runtime.capture_policy().0);
        runtime
            .game_with_capture_close(&denied, now, |_, _| panic!("equal policy closes no talk"))
            .unwrap();
        install_policy(&mut runtime, 9, &[], true, false, now);
        runtime
            .game_with_capture_close(&denied, now, |_, _| {
                panic!("rejected rollback closes no talk")
            })
            .unwrap();
    }

    #[test]
    fn malformed_policy_does_not_consume_the_roster_epoch_reset() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.identify(1, now).unwrap();
        install_policy(&mut runtime, 7, &[2], true, false, now);
        let generation = runtime.generation;
        runtime.roster(vec![1, 2], now).unwrap();
        let valid = signaling::encode_policy(&signaling::Policy {
            audience: Audience {
                revision: 2,
                send_to: vec![2],
                receive_from: vec![2],
            },
            tx_permitted: true,
            self_monitor: false,
        })
        .unwrap();
        let mut malformed = valid.clone();
        malformed[14] = 4;
        assert!(signaling::decode_policy(&malformed).is_err());
        runtime
            .game_with_capture_close(&malformed, now, |_, _| {
                panic!("malformed policy closes no talk")
            })
            .unwrap();
        assert!(runtime.audience_reset);
        assert_eq!(runtime.generation, generation);
        runtime.game(&valid, now).unwrap();
        assert_ne!(runtime.generation, generation);
        assert_eq!(runtime.capture_policy().1, Some(2));
        assert!(runtime.microphone_permitted(now));
    }

    fn install_policy(
        runtime: &mut Runtime,
        revision: u64,
        receive: &[u16],
        tx: bool,
        own: bool,
        now: Instant,
    ) {
        let policy = signaling::Policy {
            audience: Audience {
                revision,
                send_to: vec![1, 2],
                receive_from: receive.to_vec(),
            },
            tx_permitted: tx,
            self_monitor: own,
        };
        runtime
            .game(&signaling::encode_policy(&policy).unwrap(), now)
            .unwrap();
    }

    fn policy_arrival(pair: &mut Pair, path: Path, revision: u64, sequence: u16) -> usize {
        let generation = pair.peers[0].generation;
        if path == Path::Game {
            let bytes = wire::encode_policy_game(
                wire::PolicyGameMedia {
                    media: GameMedia {
                        kind: Kind::PolicyVoice,
                        stream: StreamId {
                            player: 1,
                            generation,
                        },
                        sequence,
                        talkspurt: 11,
                        opus: &[1, 2],
                    },
                    revision,
                },
                true,
            )
            .unwrap();
            pair.peers[1].game(&bytes, pair.now).unwrap();
        } else {
            let mut body = revision.to_be_bytes().to_vec();
            body.extend_from_slice(&[1, 2]);
            let bytes = pair.peers[0]
                .peers
                .get_mut(&2)
                .unwrap()
                .session
                .as_mut()
                .unwrap()
                .encrypt(Inner {
                    kind: Kind::PolicyVoice,
                    sequence,
                    talkspurt: 11,
                    body: &body,
                })
                .unwrap();
            let destination = pair.peers[1].sockets[0];
            let (source, bytes) = if path == Path::Direct {
                (pair.peers[0].sockets[0], bytes)
            } else {
                let session = match path {
                    Path::Hub { session } | Path::PlayerRelay { session, .. } => session,
                    _ => unreachable!(),
                };
                (
                    "127.0.0.1:45000".parse().unwrap(),
                    wire::encode_datagram(Datagram::Relay {
                        session,
                        datagram: &bytes,
                    })
                    .unwrap(),
                )
            };
            pair.peers[1]
                .udp(source, destination, &bytes, pair.now)
                .unwrap();
        }
        std::iter::from_fn(|| pair.peers[1].poll())
            .filter(|output| matches!(output, Output::Media(_) | Output::UdpMedia(_)))
            .count()
    }

    #[test]
    fn policy_revocation_continuity_and_unseen_old_talkspurt_on_every_ingress() {
        for route in 0..4 {
            let mut pair = Pair::new();
            pair.advance(2500);
            let path = match route {
                0 => Path::Game,
                1 => Path::Direct,
                _ => {
                    add_hub(&mut pair, 1);
                    if route == 2 {
                        Path::Hub { session: 9 }
                    } else {
                        let peer = pair.peers[1].peers.get_mut(&1).unwrap();
                        let mut binding = peer.bindings.remove(&(0, 9)).unwrap();
                        binding.grant.relay = 3;
                        peer.bindings.insert((3, 9), binding);
                        Path::PlayerRelay {
                            player: 3,
                            session: 9,
                        }
                    }
                }
            };
            pair.peers[1].audience = None;
            assert_eq!(policy_arrival(&mut pair, path, 7, 1), 0, "cold {path:?}");
            install_policy(&mut pair.peers[1], 7, &[1], true, false, pair.now);
            assert_eq!(policy_arrival(&mut pair, path, 7, 2), 1, "current {path:?}");
            let reset = pair.peers[1].capture_reset;
            install_policy(&mut pair.peers[1], 8, &[1], true, false, pair.now);
            assert_eq!(pair.peers[1].capture_reset, reset);
            assert_eq!(
                policy_arrival(&mut pair, path, 7, 3),
                1,
                "previous {path:?}"
            );
            assert_eq!(
                policy_arrival(&mut pair, path, 8, 4),
                1,
                "continuing {path:?}"
            );
            assert_eq!(policy_arrival(&mut pair, path, 9, 5), 0, "future {path:?}");
            install_policy(&mut pair.peers[1], 9, &[], false, false, pair.now);
            assert_eq!(policy_arrival(&mut pair, path, 8, 6), 0, "revoked {path:?}");
            assert_eq!(policy_arrival(&mut pair, path, 9, 7), 0);
            install_policy(&mut pair.peers[1], 10, &[1], true, false, pair.now);
            assert_eq!(
                policy_arrival(&mut pair, path, 9, 8),
                0,
                "new eligibility {path:?}"
            );
            assert_eq!(policy_arrival(&mut pair, path, 10, 9), 1);
            install_policy(&mut pair.peers[1], 12, &[1], true, false, pair.now);
            assert_eq!(
                policy_arrival(&mut pair, path, 10, 10),
                0,
                "skipped {path:?}"
            );
            assert_eq!(policy_arrival(&mut pair, path, 12, 11), 1);
            let playout_reset = pair.peers[1].playout_reset;
            install_policy(&mut pair.peers[1], 12, &[1], true, false, pair.now);
            assert_eq!(pair.peers[1].playout_reset, playout_reset);
            let legacy = wire::encode_game(
                GameMedia {
                    stream: StreamId {
                        player: 1,
                        generation: pair.peers[0].generation,
                    },
                    kind: Kind::Voice,
                    sequence: 20,
                    talkspurt: 12,
                    opus: &[1],
                },
                true,
            )
            .unwrap();
            pair.peers[1].game(&legacy, pair.now).unwrap();
            assert!(
                !std::iter::from_fn(|| pair.peers[1].poll()).any(|o| matches!(o, Output::Media(_)))
            );
        }
    }

    #[test]
    fn policy_transmit_stamps_each_packet_without_restarting_unaffected_talkspurts() {
        let mut pair = Pair::new();
        pair.advance(2500);
        install_policy(&mut pair.peers[0], 7, &[2], true, false, pair.now);
        pair.peers[0].transmit(&packet::encode(packet::START, 0, &[]), pair.now);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 1, &[1]), pair.now);
        let talkspurt = pair.peers[0].talkspurt;
        install_policy(&mut pair.peers[0], 8, &[2], true, false, pair.now);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 2, &[2]), pair.now);
        let packets: Vec<_> = std::iter::from_fn(|| pair.peers[0].poll())
            .filter_map(|o| match o {
                Output::Game(bytes) if bytes[2] == wire::GAME_POLICY_VOICE => Some(bytes),
                _ => None,
            })
            .collect();
        let mut stamped = Vec::new();
        for bytes in packets {
            let mut forwarded = bytes[..3].to_vec();
            forwarded.extend_from_slice(&1u16.to_be_bytes());
            forwarded.extend_from_slice(&bytes[3..]);
            let media = wire::decode_policy_game(&forwarded).unwrap();
            stamped.push((media.revision, media.media.sequence, media.media.talkspurt));
        }
        assert_eq!(stamped, [(7, 1, talkspurt), (8, 2, talkspurt)]);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 3, &[3]), pair.now);
        install_policy(&mut pair.peers[0], 9, &[2], false, false, pair.now);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 4, &[4]), pair.now);
        install_policy(&mut pair.peers[0], 10, &[2], true, false, pair.now);
        assert!(!std::iter::from_fn(|| pair.peers[0].poll()).any(|o| outgoing_media(&o)));
        pair.peers[0].transmit(&packet::encode(packet::START, 4, &[]), pair.now);
        assert_ne!(pair.peers[0].talkspurt, talkspurt);
    }

    #[test]
    fn policy_rollback_retires_streams_and_keys_only_at_a_fresh_roster_boundary() {
        let mut pair = Pair::new();
        pair.advance(2500);
        let old_generation = pair.peers[0].generation;
        install_policy(&mut pair.peers[0], 7, &[2], true, false, pair.now);
        install_policy(&mut pair.peers[1], 7, &[1], true, false, pair.now);
        let old_receiver_generation = pair.peers[1].generation;
        let mut body = 1u64.to_be_bytes().to_vec();
        body.push(1);
        let delayed_udp = pair.peers[0]
            .peers
            .get_mut(&2)
            .unwrap()
            .session
            .as_mut()
            .unwrap()
            .encrypt(Inner {
                kind: Kind::PolicyVoice,
                sequence: 8,
                talkspurt: 1,
                body: &body,
            })
            .unwrap();
        install_policy(&mut pair.peers[1], 1, &[1], true, false, pair.now);
        assert_eq!(pair.peers[1].generation, old_receiver_generation);
        pair.peers[1].roster(vec![1, 2], pair.now).unwrap();
        install_policy(&mut pair.peers[1], 1, &[1], true, false, pair.now);
        assert_ne!(pair.peers[1].generation, old_receiver_generation);
        assert!(pair.peers[1].peers[&1].retired.contains(&old_generation));
        assert_eq!(policy_arrival(&mut pair, Path::Game, 1, 9), 0);
        let source = pair.peers[0].sockets[0];
        let destination = pair.peers[1].sockets[0];
        pair.peers[1]
            .udp(source, destination, &delayed_udp, pair.now)
            .unwrap();
        assert!(
            !std::iter::from_fn(|| pair.peers[1].poll())
                .any(|o| matches!(o, Output::Media(_) | Output::UdpMedia(_)))
        );
        pair.peers[0].roster(vec![1, 2], pair.now).unwrap();
        install_policy(&mut pair.peers[0], 1, &[2], true, false, pair.now);
        pair.advance(3000);
        assert_eq!(policy_arrival(&mut pair, Path::Direct, 1, 10), 1);
    }

    #[test]
    fn policy_start_keeps_its_playout_clock_through_enforcement_before_voice() {
        let mut pair = Pair::new();
        pair.advance(2500);
        let now = pair.now;
        let stream = StreamId {
            player: 1,
            generation: pair.peers[0].generation,
        };
        let receiver = &mut pair.peers[1];
        install_policy(receiver, 7, &[1], true, false, now);
        let mut mixer = crate::mix::Mixer::default();
        mixer.select(stream, now);
        for (kind, sequence, opus, delay) in [
            (Kind::PolicyStart, 0, &[][..], 0),
            (Kind::PolicyVoice, 1, &[0xf8, 0xff, 0xfe][..], 50),
        ] {
            let at = now + Duration::from_millis(delay);
            let packet = wire::encode_policy_game(
                wire::PolicyGameMedia {
                    media: GameMedia {
                        stream,
                        kind,
                        sequence,
                        talkspurt: 1,
                        opus,
                    },
                    revision: 7,
                },
                true,
            )
            .unwrap();
            receiver.game(&packet, at).unwrap();
            let media = std::iter::from_fn(|| receiver.poll())
                .find_map(|output| match output {
                    Output::Media(media) => Some(media),
                    _ => None,
                })
                .expect("policy media admitted");
            assert!(receiver.accepts_media(1, media.revision));
            assert_eq!(
                mixer.receive_game_at(media.borrowed(), media.revision, at),
                crate::mix::MediaOutcome::Accepted
            );
            if kind == Kind::PolicyStart {
                assert!(
                    mixer
                        .tick_checked(
                            now + Duration::from_millis(20),
                            crate::mix::SLICE_SAMPLES,
                            |player, revision| receiver.accepts_media(player, revision),
                        )
                        .is_none()
                );
            }
        }
        assert!(
            mixer
                .tick_checked(
                    now + crate::jitter::START_WAIT,
                    crate::mix::SLICE_SAMPLES,
                    |player, revision| receiver.accepts_media(player, revision),
                )
                .is_some(),
            "the START clock must not move to the first voice"
        );
        assert_eq!(mixer.pulled[0].slice, Some((1, 0)));
    }

    #[test]
    fn roster_equal_policy_reset_rotates_only_when_contents_change() {
        let mut pair = Pair::new();
        pair.advance(2500);
        install_policy(&mut pair.peers[0], 7, &[2], true, false, pair.now);
        install_policy(&mut pair.peers[1], 6, &[1], true, false, pair.now);
        install_policy(&mut pair.peers[1], 7, &[1], true, false, pair.now);
        let generation = pair.peers[1].generation;
        let capture_reset = pair.peers[1].capture_reset;
        while pair.peers[1].poll().is_some() {}
        pair.peers[1].roster(vec![1, 2], pair.now).unwrap();
        install_policy(&mut pair.peers[1], 7, &[1], true, false, pair.now);
        assert_eq!(pair.peers[1].generation, generation);
        assert_eq!(pair.peers[1].capture_reset, capture_reset);
        assert!(
            !std::iter::from_fn(|| pair.peers[1].poll())
                .any(|output| { matches!(output, Output::Purge { .. } | Output::ResetStreams) })
        );
        install_policy(&mut pair.peers[1], 7, &[], true, false, pair.now);
        assert!(pair.peers[1].accepts_media(1, Some(7)));
        assert_eq!(pair.peers[1].generation, generation);
        pair.peers[1].roster(vec![1, 2], pair.now).unwrap();
        install_policy(&mut pair.peers[1], 7, &[1, 3], true, false, pair.now);
        assert_ne!(pair.peers[1].generation, generation);
        assert!(!pair.peers[1].accepts_media(1, Some(6)));
        assert!(pair.peers[1].accepts_media(3, Some(7)));
        let outputs: Vec<_> = std::iter::from_fn(|| pair.peers[1].poll()).collect();
        assert!(
            outputs
                .iter()
                .any(|output| matches!(output, Output::ResetStreams))
        );
        assert!(
            outputs
                .iter()
                .any(|output| matches!(output, Output::Purge { all: true, .. }))
        );
        assert_eq!(policy_arrival(&mut pair, Path::Game, 7, 10), 0);
        assert_eq!(policy_arrival(&mut pair, Path::Direct, 7, 11), 0);
        pair.peers[0].roster(vec![1, 2], pair.now).unwrap();
        install_policy(&mut pair.peers[0], 7, &[2, 3], true, false, pair.now);
        pair.advance(3000);
        assert_eq!(policy_arrival(&mut pair, Path::Direct, 7, 12), 1);
    }

    #[test]
    fn legacy_own_game_echo_is_compatible_but_never_bypasses_installed_policy() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:0".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.identify(1, now).unwrap();
        assert!(!runtime.allows_receive(2));
        let media = GameMedia {
            stream: StreamId {
                player: 1,
                generation: runtime.generation,
            },
            kind: Kind::Voice,
            sequence: 1,
            talkspurt: 1,
            opus: &[1],
        };
        let legacy = wire::encode_game(media, true).unwrap();
        runtime.game(&legacy, now).unwrap();
        assert!(
            std::iter::from_fn(|| runtime.poll()).any(|output| matches!(output, Output::Media(_)))
        );
        install_policy(&mut runtime, 7, &[], true, true, now);
        runtime.game(&legacy, now).unwrap();
        assert!(
            !std::iter::from_fn(|| runtime.poll()).any(|output| matches!(output, Output::Media(_)))
        );
        let own = |revision| {
            wire::encode_policy_game(
                wire::PolicyGameMedia {
                    media: GameMedia {
                        kind: Kind::PolicyVoice,
                        ..media
                    },
                    revision,
                },
                true,
            )
            .unwrap()
        };
        runtime.game(&own(7), now).unwrap();
        assert!(
            std::iter::from_fn(|| runtime.poll()).any(|output| matches!(output, Output::Media(_)))
        );
        install_policy(&mut runtime, 8, &[], false, false, now);
        for bytes in [&legacy, &own(7), &own(8)] {
            runtime.game(bytes, now).unwrap();
            assert!(
                !std::iter::from_fn(|| runtime.poll())
                    .any(|output| matches!(output, Output::Media(_)))
            );
        }
    }

    #[test]
    fn tx_muted_player_relay_forwards_opaque_third_party_media_but_cancels_its_own_queue() {
        let now = Instant::now();
        let local: SocketAddr = "127.0.0.1:41003".parse().unwrap();
        let endpoints: [SocketAddr; 2] = [
            "127.0.0.1:41001".parse().unwrap(),
            "127.0.0.1:41002".parse().unwrap(),
        ];
        let mut runtime = Runtime::new(vec![local], RuntimeConfig::default()).unwrap();
        runtime.identify(3, now).unwrap();
        install_policy(&mut runtime, 7, &[], true, false, now);
        runtime.transmit(&packet::encode(packet::VOICE, 1, &[1]), now);
        install_policy(&mut runtime, 8, &[], false, false, now);
        assert!(!std::iter::from_fn(|| runtime.poll()).any(|output| outgoing_media(&output)));
        let now_ms = runtime.now_ms(now);
        let frames = runtime.relay.as_mut().unwrap().offer(1, 2, now_ms).unwrap();
        let (_, Signal::RelayGrant(grant)) =
            signaling::decode_game(&frames[0].frame, false).unwrap()
        else {
            panic!("relay grant");
        };
        for (index, endpoint) in endpoints.iter().enumerate() {
            let mut client = BindClient::new(grant.clone(), index as u16 + 1, 1).unwrap();
            runtime
                .udp(*endpoint, local, &client.request(), now)
                .unwrap();
            let challenge = std::iter::from_fn(|| runtime.poll())
                .find_map(|output| match output {
                    Output::Udp(tx) => match wire::decode_datagram(&tx.contents) {
                        Ok(Datagram::Bind(Bind::Challenge(challenge))) => Some(challenge),
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap();
            runtime
                .udp(
                    *endpoint,
                    local,
                    &client.on_challenge(challenge).unwrap(),
                    now,
                )
                .unwrap();
            assert!(std::iter::from_fn(|| runtime.poll()).any(|output| matches!(output, Output::Udp(tx) if tx.destination == *endpoint && matches!(wire::decode_datagram(&tx.contents), Ok(Datagram::BindAck { .. })))));
        }
        let ciphertext = vec![0; wire::INNER_HEADER + wire::TAG + 9];
        let encrypted = wire::encode_datagram(Datagram::Encrypted {
            header: wire::Header {
                kind: Kind::PolicyVoice,
                phase: false,
                receiver: ReceiverHandle(11),
                counter: 1,
            },
            ciphertext: &ciphertext,
        })
        .unwrap();
        let wrapped = wire::encode_datagram(Datagram::Relay {
            session: grant.session,
            datagram: &encrypted,
        })
        .unwrap();
        runtime.udp(endpoints[0], local, &wrapped, now).unwrap();
        let forwarded: Vec<_> = std::iter::from_fn(|| runtime.poll())
            .filter_map(|output| match output {
                Output::Udp(tx) => Some((tx.destination, tx.contents)),
                _ => None,
            })
            .collect();
        assert_eq!(forwarded, [(endpoints[1], wrapped)]);
    }

    #[test]
    fn bound_hub_rediscovery_renews_attempt_reflection_and_route_without_rekey() {
        let mut pair = Pair::new();
        pair.advance(2500);
        let runtime = &mut pair.peers[0];
        let now = pair.now;
        let endpoint = "127.0.0.1:45000".parse().unwrap();
        runtime.config.hub = Some(endpoint);
        let mut hub = Relay::new(0, 77, Limits::default());
        let frames = hub.offer(1, 2, runtime.now_ms(now)).unwrap();
        let (_, Signal::RelayGrant(grant)) =
            signaling::decode_game(&frames[0].frame, false).unwrap()
        else {
            panic!("grant")
        };
        let key = (0, grant.session);
        runtime.grant(0, grant.clone(), now).unwrap();
        let pump = |runtime: &mut Runtime, hub: &mut Relay| {
            for _ in 0..4 {
                runtime.tick(now, true).unwrap();
                while let Some(output) = runtime.poll() {
                    if let Output::Udp(tx) = output
                        && tx.destination == endpoint
                        && matches!(wire::decode_datagram(&tx.contents), Ok(Datagram::Bind(_)))
                    {
                        let Reply::Send(to, bytes) = hub
                            .handle(runtime.now_ms(now), tx.source, &tx.contents)
                            .unwrap()
                        else {
                            panic!("bind reply")
                        };
                        runtime.udp(endpoint, to, &bytes, now).unwrap();
                    }
                }
            }
        };
        pump(runtime, &mut hub);
        let peer = &runtime.peers[&2];
        assert!(peer.bindings[&key].bound);
        let previous_attempt = peer.bindings[&key].attempt;
        let path = peer.bindings[&key].path();
        let previous_route = peer.ice.probes().route(path).unwrap();
        let epoch = peer.session.as_ref().unwrap().active_epoch();
        let generation = peer.session.as_ref().unwrap().generation();
        let peer = runtime.peers.get_mut(&2).unwrap();
        let before = peer
            .session
            .as_mut()
            .unwrap()
            .encrypt(Inner {
                kind: Kind::Voice,
                sequence: 8,
                talkspurt: 1,
                body: &[1],
            })
            .unwrap();
        peer.ice
            .discover_hub(
                HubReflection {
                    endpoint,
                    session: grant.session,
                    hub_generation: grant.hub_generation,
                },
                now,
            )
            .unwrap();
        runtime.tick(now, true).unwrap();
        assert!(runtime.peers[&2].bindings[&key].attempt > previous_attempt);
        assert!(!runtime.peers[&2].bindings[&key].bound);
        pump(runtime, &mut hub);
        let peer = runtime.peers.get_mut(&2).unwrap();
        assert!(peer.bindings[&key].bound);
        assert!(peer.ice.gathering_complete());
        assert!(peer.ice.probes().route(path).unwrap().revision > previous_route.revision);
        assert_eq!(peer.session.as_ref().unwrap().active_epoch(), epoch);
        assert_eq!(peer.session.as_ref().unwrap().generation(), generation);
        let after = peer
            .session
            .as_mut()
            .unwrap()
            .encrypt(Inner {
                kind: Kind::Voice,
                sequence: 9,
                talkspurt: 1,
                body: &[1],
            })
            .unwrap();
        let Datagram::Encrypted { header: before, .. } = wire::decode_datagram(&before).unwrap()
        else {
            panic!("media")
        };
        let Datagram::Encrypted { header: after, .. } = wire::decode_datagram(&after).unwrap()
        else {
            panic!("media")
        };
        assert_eq!(before.phase, after.phase);
        assert!(after.counter > before.counter);
    }

    #[test]
    fn late_dns_preserves_pending_hub_and_skips_successfully_reflected_socket() {
        let now = Instant::now();
        let source = "127.0.0.1:45001".parse().unwrap();
        let hub = "127.0.0.1:45000".parse().unwrap();
        let server = "127.0.0.1:3478".parse().unwrap();
        let mut config = ConnectivityConfig::default();
        config.discovery.hub = Some(HubReflection {
            endpoint: hub,
            session: 1,
            hub_generation: 2,
        });
        let mut ice = PairConnectivity::new(
            StreamId {
                player: 1,
                generation: 1,
            },
            StreamId {
                player: 2,
                generation: 2,
            },
            vec![source],
            config,
            now,
        )
        .unwrap();
        let request = std::iter::from_fn(|| ice.poll_action())
            .find_map(|a| match a {
                Action::Discovery(DiscoveryAction::BeginHubBinding { request, .. }) => {
                    Some(request)
                }
                _ => None,
            })
            .unwrap();
        while ice.poll_action().is_some() {}
        ice.discover_stun(vec![server], now).unwrap();
        assert!(
            !std::iter::from_fn(|| ice.poll_action())
                .any(|a| matches!(a, Action::Discovery(DiscoveryAction::Send(_))))
        );
        assert!(
            ice.authenticated_hub_reflection(
                source,
                hub,
                wire::BindChallenge {
                    request,
                    endpoint: "127.0.0.1:46001".parse().unwrap(),
                    challenge: [1; 32],
                    expires_at_ms: u64::MAX,
                },
                now
            )
            .unwrap()
        );
        while ice.poll_action().is_some() {}
        ice.discover_stun(vec![server], now).unwrap();
        assert!(
            !std::iter::from_fn(|| ice.poll_action())
                .any(|a| matches!(a, Action::Discovery(DiscoveryAction::Send(_))))
        );
    }

    #[test]
    fn late_dns_without_a_new_server_changes_nothing() {
        let now = Instant::now();
        let source = "127.0.0.1:45001".parse().unwrap();
        let server = "127.0.0.1:3478".parse().unwrap();
        for servers in [Vec::new(), vec![server]] {
            let mut config = ConnectivityConfig::default();
            config.discovery.stun_servers = servers.clone();
            let mut ice = PairConnectivity::new(
                StreamId {
                    player: 1,
                    generation: 1,
                },
                StreamId {
                    player: 2,
                    generation: 2,
                },
                vec![source],
                config,
                now,
            )
            .unwrap();
            while ice.poll_action().is_some() {}
            for late in [Vec::new(), servers.clone()] {
                ice.discover_stun(late, now + ms(1)).unwrap();
                assert!(ice.poll_action().is_none());
            }
        }
    }

    #[test]
    fn rejected_late_stun_result_preserves_config_hub_and_future_roster() {
        let mut pair = Pair::new();
        add_hub(&mut pair, 0);
        let runtime = &mut pair.peers[0];
        let credentials = runtime.peers[&2].ice.local_credentials();
        let packet = runtime.peers[&2].bindings[&(0, 9)].packet.clone();
        assert!(
            runtime
                .stun_servers(vec!["0.0.0.0:3478".parse().unwrap()], pair.now)
                .is_err()
        );
        assert!(
            runtime
                .config
                .connectivity
                .discovery
                .stun_servers
                .is_empty()
        );
        assert_eq!(runtime.peers[&2].bindings[&(0, 9)].packet, packet);
        assert!(runtime.peers[&2].ice.local_credentials() == credentials);
        runtime.roster(vec![1, 2, 3], pair.now).unwrap();
        assert!(runtime.peers.contains_key(&3));
    }

    #[test]
    fn review_regression_audience_rollback_needs_one_fresh_roster() {
        for original in [vec![], vec![2]] {
            let mut pair = Pair::new();
            let runtime = &mut pair.peers[0];
            let rollback = if original.is_empty() { vec![2] } else { vec![] };
            let update = |revision, scope: Vec<u16>| {
                signaling::encode_audience(&Audience {
                    revision,
                    send_to: scope.clone(),
                    receive_from: scope,
                })
                .unwrap()
            };
            runtime
                .game(&update(500, original.clone()), pair.now)
                .unwrap();
            runtime
                .game(&update(3, rollback.clone()), pair.now)
                .unwrap();
            assert!(!runtime.allows_receive(2));
            runtime.game(&update(501, vec![2]), pair.now).unwrap();
            assert!(!runtime.allows_receive(2));
            runtime.roster(vec![1, 2], pair.now).unwrap();
            runtime
                .game(&update(3, rollback.clone()), pair.now)
                .unwrap();
            assert_eq!(runtime.allows_receive(2), rollback.contains(&2));
            runtime.game(&update(3, vec![]), pair.now).unwrap();
            assert!(!runtime.allows_receive(2));
            runtime.roster(vec![1, 2], pair.now).unwrap();
            runtime.game(&update(2, vec![]), pair.now).unwrap();
            assert!(!runtime.allows_receive(2));
            runtime.game(&update(2, vec![2]), pair.now).unwrap();
            assert!(runtime.allows_receive(2));
        }
    }

    #[test]
    fn identify_alone_does_not_authorize_an_audience_reset() {
        let now = Instant::now();
        let update = |revision, scope: Vec<u16>| {
            signaling::encode_audience(&Audience {
                revision,
                send_to: scope.clone(),
                receive_from: scope,
            })
            .unwrap()
        };
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.game(&update(500, vec![]), now).unwrap();
        runtime.identify(1, now).unwrap();
        runtime.game(&update(3, vec![2]), now).unwrap();
        assert!(!runtime.allows_receive(2));
        runtime.roster(vec![1, 2], now).unwrap();
        runtime.game(&update(3, vec![2]), now).unwrap();
        assert!(runtime.allows_receive(2));
    }

    #[test]
    fn startup_roster_then_audience_is_accepted_after_identify() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.roster(vec![1, 2], now).unwrap();
        runtime.identify(1, now).unwrap();
        let audience = signaling::encode_audience(&Audience {
            revision: 1,
            send_to: vec![2],
            receive_from: vec![2],
        })
        .unwrap();
        runtime.game(&audience, now).unwrap();
        assert!(runtime.allows_receive(2));
    }

    #[test]
    fn review_regression_retired_game_generation_never_reaches_hold() {
        let mut pair = Pair::new();
        pair.peers[0]
            .peers
            .get_mut(&2)
            .unwrap()
            .retired
            .push_back(777);
        while pair.peers[0].poll().is_some() {}
        let bytes = wire::encode_game(
            GameMedia {
                stream: StreamId {
                    player: 2,
                    generation: 777,
                },
                kind: Kind::Voice,
                sequence: 1,
                talkspurt: 1,
                opus: &[1],
            },
            true,
        )
        .unwrap();
        pair.peers[0].game(&bytes, pair.now).unwrap();
        assert!(
            !std::iter::from_fn(|| pair.peers[0].poll()).any(|o| matches!(o, Output::Media(_)))
        );
    }

    #[test]
    fn review_regression_tick_error_keeps_advancing_other_peers() {
        let mut pair = Pair::new();
        pair.peers[0].roster(vec![1, 2, 3], pair.now).unwrap();
        pair.peers[0].next_handle = u32::MAX;
        assert!(pair.peers[0].tick(pair.now, true).is_err());
        assert!(pair.peers[0].peers[&3].next_retry > pair.now);
        assert_eq!(pair.peers[0].peers.len(), 2);
    }

    #[test]
    fn review_regression_grant_error_retains_peer() {
        let mut pair = Pair::new();
        add_hub(&mut pair, 0);
        let runtime = &mut pair.peers[0];
        runtime.config.hub = Some("127.0.0.1:45000".parse().unwrap());
        let binding = runtime
            .peers
            .get_mut(&2)
            .unwrap()
            .bindings
            .get_mut(&(0, 9))
            .unwrap();
        binding.attempt = u64::MAX;
        binding.bound = false;
        binding.deadline = pair.now;
        let grant = binding.grant.clone();
        assert!(runtime.grant(0, grant, pair.now).is_err());
        assert!(runtime.peers.contains_key(&2));
    }

    #[test]
    fn review_regression_unchanged_routes_do_not_repeat_reorder_output() {
        let mut pair = Pair::new();
        pair.advance(2500);
        pair.peers[0].tick(pair.now, true).unwrap();
        while pair.peers[0].poll().is_some() {}
        pair.peers[0].tick(pair.now + ms(1), true).unwrap();
        assert!(
            !std::iter::from_fn(|| pair.peers[0].poll())
                .any(|o| matches!(o, Output::Reorder(_, _)))
        );
    }

    #[test]
    fn review_regression_no_stun_fallback_publishes_completion_once() {
        let now = Instant::now();
        let mut config = ConnectivityConfig::default();
        config.discovery.hub = Some(HubReflection {
            endpoint: "127.0.0.1:45000".parse().unwrap(),
            session: 1,
            hub_generation: 2,
        });
        let mut ice = PairConnectivity::new(
            StreamId {
                player: 1,
                generation: 1,
            },
            StreamId {
                player: 2,
                generation: 2,
            },
            vec!["127.0.0.1:45001".parse().unwrap()],
            config,
            now,
        )
        .unwrap();
        while ice.poll_action().is_some() {}
        ice.handle_timeout(now + ms(800)).unwrap();
        let actions: Vec<_> = std::iter::from_fn(|| ice.poll_action()).collect();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::DiscoveryComplete))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::PublishCandidates))
        );
        ice.handle_timeout(now + ms(801)).unwrap();
        assert!(
            !std::iter::from_fn(|| ice.poll_action())
                .any(|a| matches!(a, Action::DiscoveryComplete))
        );
    }

    #[test]
    fn hub_attempt_exhaustion_keeps_the_binding_and_peer() {
        let mut pair = Pair::new();
        add_hub(&mut pair, 0);
        let runtime = &mut pair.peers[0];
        let binding = runtime
            .peers
            .get_mut(&2)
            .unwrap()
            .bindings
            .get_mut(&(0, 9))
            .unwrap();
        binding.bound = false;
        binding.attempt = u64::MAX;
        let grant = binding.grant.clone();
        let endpoint = binding.endpoint;
        runtime
            .peers
            .get_mut(&2)
            .unwrap()
            .ice
            .discover_hub(
                HubReflection {
                    endpoint,
                    session: grant.session,
                    hub_generation: grant.hub_generation,
                },
                pair.now,
            )
            .unwrap();
        assert!(runtime.tick(pair.now, true).is_err());
        assert_eq!(runtime.peers[&2].bindings[&(0, 9)].attempt, u64::MAX);
    }

    #[test]
    fn late_stun_results_reach_discovery_without_changing_stream_identity() {
        let mut pair = Pair::new();
        let generation = pair.peers[0].generation;
        while pair.peers[0].poll().is_some() {}
        let server = "127.0.0.1:3478".parse().unwrap();
        pair.peers[0].stun_servers(vec![server], pair.now).unwrap();
        pair.peers[0].tick(pair.now, false).unwrap();
        assert_eq!(pair.peers[0].generation, generation);
        assert!(
            std::iter::from_fn(|| pair.peers[0].poll())
                .any(|o| matches!(o, Output::Udp(tx) if tx.destination == server))
        );
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    struct Pair {
        peers: [Runtime; 2],
        now: Instant,
        media: Vec<(usize, Media)>,
        datagrams: Vec<(usize, Vec<u8>)>,
        traces: Vec<String>,
        lose_pongs: bool,
        cut_udp: bool,
        lose_return: bool,
        cut_game: bool,
        cut_direct: bool,
        hub: Option<crate::transport::hub::Hub>,
    }

    impl Pair {
        fn new() -> Self {
            let now = Instant::now();
            let mut peers = std::array::from_fn(|i| {
                let socket = SocketAddr::from(([127, 0, 0, 1], 41001 + i as u16));
                Runtime::new(vec![socket], RuntimeConfig::default()).unwrap()
            });
            for (i, peer) in peers.iter_mut().enumerate() {
                peer.roster(vec![1, 2], now).unwrap();
                peer.identify(i as u16 + 1, now).unwrap();
                let audience = signaling::encode_audience(&Audience {
                    revision: 1,
                    send_to: vec![2 - i as u16],
                    receive_from: vec![2 - i as u16],
                })
                .unwrap();
                peer.game(&audience, now).unwrap();
            }
            Self {
                peers,
                now,
                media: Vec::new(),
                datagrams: Vec::new(),
                traces: Vec::new(),
                lose_pongs: false,
                cut_udp: false,
                lose_return: false,
                cut_game: false,
                cut_direct: false,
                hub: None,
            }
        }

        fn hub_frames(&mut self, frames: Vec<crate::transport::relay::Outgoing>) {
            if self.cut_game {
                return;
            }
            for frame in frames {
                let (to, signal) = signaling::decode_game(&frame.frame, false).unwrap();
                self.peers[to as usize - 1]
                    .game(&signaling::encode_game(&signal, 0, true).unwrap(), self.now)
                    .unwrap();
            }
        }

        fn flush(&mut self) {
            for _ in 0..100 {
                let mut events = Vec::new();
                for (i, peer) in self.peers.iter_mut().enumerate() {
                    while let Some(output) = peer.poll() {
                        events.push((i, output));
                    }
                }
                if events.is_empty() {
                    return;
                }
                for (i, output) in events {
                    match output {
                        Output::Game(bytes) => {
                            if self.cut_game {
                                continue;
                            }
                            if let Ok((to, signal)) = signaling::decode_game(&bytes, false) {
                                if to == 0 {
                                    let now_ms = self.peers[i].now_ms(self.now);
                                    if let Some(hub) = &mut self.hub {
                                        let inbound =
                                            signaling::encode_game(&signal, i as u16 + 1, true)
                                                .unwrap();
                                        let frames = hub.on_game(now_ms, &inbound);
                                        self.hub_frames(frames);
                                    }
                                    continue;
                                }
                                let inbound =
                                    signaling::encode_game(&signal, i as u16 + 1, true).unwrap();
                                self.peers[to as usize - 1]
                                    .game(&inbound, self.now)
                                    .unwrap();
                            } else if matches!(
                                bytes[2],
                                wire::GAME_VOICE | wire::GAME_START | wire::GAME_STOP
                            ) {
                                let mut inbound = bytes[..3].to_vec();
                                inbound.extend_from_slice(&(i as u16 + 1).to_be_bytes());
                                inbound.extend_from_slice(&bytes[3..]);
                                self.peers[1 - i].game(&inbound, self.now).unwrap();
                            }
                        }
                        Output::Udp(tx) => {
                            self.datagrams.push((i, tx.contents.clone()));
                            if self.cut_udp {
                                continue;
                            }
                            if self.lose_return
                                && i == 1
                                && matches!(
                                    wire::decode_datagram(&tx.contents),
                                    Ok(Datagram::Encrypted { .. })
                                )
                            {
                                continue;
                            }
                            if self.lose_pongs
                                && matches!(wire::decode_datagram(&tx.contents), Ok(Datagram::Encrypted { header, .. }) if header.kind == Kind::Pong)
                            {
                                continue;
                            }
                            if tx.destination == self.peers[1 - i].sockets[0] {
                                if self.cut_direct {
                                    continue;
                                }
                                self.peers[1 - i]
                                    .udp(tx.source, tx.destination, &tx.contents, self.now)
                                    .unwrap();
                            } else if let Some(hub) = &mut self.hub {
                                let now_ms = self.peers[i].now_ms(self.now);
                                let reply = hub.relay.handle(now_ms, tx.source, &tx.contents);
                                let delivered = match reply {
                                    Ok(crate::transport::relay::Reply::Send(to, bytes)) => {
                                        Some((to, bytes))
                                    }
                                    Ok(crate::transport::relay::Reply::Forward(to)) => {
                                        Some((to, tx.contents))
                                    }
                                    Err(_) => None,
                                };
                                if let Some((to, bytes)) = delivered {
                                    let peer = self
                                        .peers
                                        .iter_mut()
                                        .find(|p| p.sockets.contains(&to))
                                        .unwrap();
                                    peer.udp(tx.destination, to, &bytes, self.now).unwrap();
                                }
                            }
                        }
                        Output::Media(media) | Output::UdpMedia(media) => {
                            self.media.push((i, media))
                        }
                        Output::Trace(text) => self.traces.push(text),
                        _ => {}
                    }
                }
            }
            panic!("runtime action loop did not quiesce");
        }

        fn advance(&mut self, millis: u64) {
            for _ in 0..millis / 10 {
                self.now += ms(10);
                let now_ms = self.peers[0].now_ms(self.now);
                if let Some(hub) = &mut self.hub {
                    let frames = hub.tick(now_ms);
                    self.hub_frames(frames);
                }
                for peer in &mut self.peers {
                    peer.tick(self.now, true).unwrap();
                }
                self.flush();
            }
        }

        fn active(&self, i: usize) -> Path {
            self.peers[i].peers[&(2 - i as u16)]
                .choice
                .active
                .unwrap_or(Path::Game)
        }
    }

    #[test]
    fn direct_and_hub_voice_survive_a_join_freeze_across_the_old_grant_expiry() {
        for through_hub in [false, true] {
            let mut pair = Pair::new();
            if through_hub {
                let endpoint = "127.0.0.1:45000".parse().unwrap();
                for runtime in &mut pair.peers {
                    runtime.config.hub = Some(endpoint);
                }
                pair.cut_direct = true;
                pair.hub = Some(crate::transport::hub::Hub::new(77, Limits::default()));
            }
            pair.advance(5000);
            if through_hub {
                let now_ms = pair.peers[0].now_ms(pair.now);
                let first_expiry = pair
                    .hub
                    .as_ref()
                    .unwrap()
                    .relay
                    .live(now_ms)
                    .map(|g| g.expires_at_ms)
                    .min()
                    .unwrap();
                // a healthy pair already received and bound the early renewal. the game
                // freezes ten seconds before the original grant expires
                pair.advance(first_expiry - now_ms - 10_000);
                for (i, runtime) in pair.peers.iter().enumerate() {
                    let bindings = &runtime.peers[&(2 - i as u16)].bindings;
                    assert!(bindings.values().filter(|b| b.bound).count() >= 2);
                    assert!(
                        bindings
                            .values()
                            .any(|b| b.bound && b.grant.expires_at_ms > first_expiry + 60_000)
                    );
                    assert!(matches!(pair.active(i), Path::Hub { .. }));
                }
            } else {
                assert_eq!(pair.active(0), Path::Direct);
                assert_eq!(pair.active(1), Path::Direct);
            }
            for runtime in &mut pair.peers {
                install_policy(runtime, 7, &[1, 2], true, false, pair.now);
                runtime.transmit(&packet::encode(packet::START, 0, &[]), pair.now);
            }
            pair.cut_game = true;
            pair.flush();
            let freeze_start = pair.now;
            for sequence in 1u16..=2250 {
                pair.advance(20);
                let before = pair.media.len();
                for (i, runtime) in pair.peers.iter_mut().enumerate() {
                    assert!(runtime.microphone_permitted(pair.now));
                    runtime.transmit(
                        &packet::encode(packet::VOICE, sequence, &[i as u8]),
                        pair.now,
                    );
                }
                pair.flush();
                let voices: Vec<_> = pair.media[before..]
                    .iter()
                    .filter(|(_, m)| m.kind == Kind::Voice)
                    .collect();
                assert_eq!(
                    voices.len(),
                    2,
                    "both directions, hub={through_hub}, sequence={sequence}"
                );
                for (listener, media) in voices {
                    assert_eq!(media.sequence, sequence);
                    assert_eq!(media.opus, [1 - *listener as u8]);
                    assert_eq!(media.revision, Some(7));
                }
            }
            assert_eq!(
                pair.now.duration_since(freeze_start),
                Duration::from_secs(45)
            );
            for runtime in &pair.peers {
                assert!(runtime.microphone_permitted(pair.now));
                assert!(!runtime.microphone_permitted(freeze_start + MIC_POLICY_LIFETIME));
            }
        }
    }

    #[test]
    fn direct_runtime_negotiates_confirms_probes_and_emits_game_backup() {
        let mut pair = Pair::new();
        pair.advance(2500);
        assert_eq!(pair.active(0), Path::Direct, "{:?}", pair.traces);
        assert_eq!(pair.active(1), Path::Direct);
        pair.peers[0].transmit(&packet::encode(packet::START, 0, &[]), pair.now);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 1, &[1, 2, 3]), pair.now);
        pair.flush();
        assert_eq!(
            pair.media
                .iter()
                .filter(|(i, m)| *i == 1 && m.kind == Kind::Voice)
                .count(),
            2
        );
        assert!(pair.datagrams.iter().any(|(_,d)| matches!(wire::decode_datagram(d),Ok(Datagram::Encrypted {header,..}) if header.kind==Kind::Voice)));
        pair.cut_udp = true;
        pair.advance(1100);
        assert_eq!(pair.active(0), Path::Game);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 2, &[4]), pair.now);
        pair.flush();
        assert!(
            pair.media
                .iter()
                .any(|(_, m)| m.sequence == 2 && m.kind == Kind::Voice)
        );
    }

    #[test]
    fn exactly_one_pong_returns_on_the_authenticated_arrival_route() {
        let mut pair = Pair::new();
        pair.advance(2500);
        pair.datagrams.clear();
        let peer = pair.peers[0].peers.get_mut(&2).unwrap();
        let ping = peer
            .session
            .as_mut()
            .unwrap()
            .encrypt(Inner {
                kind: Kind::Ping,
                sequence: 0,
                talkspurt: 0,
                body: &17u64.to_be_bytes(),
            })
            .unwrap();
        let from = pair.peers[0].sockets[0];
        let to = pair.peers[1].sockets[0];
        pair.peers[1].udp(from, to, &ping, pair.now).unwrap();
        pair.flush();
        let pongs=pair.datagrams.iter().filter(|(_,d)| matches!(wire::decode_datagram(d),Ok(Datagram::Encrypted {header,..}) if header.kind==Kind::Pong)).count();
        assert_eq!(pongs, 1);
        pair.peers[1].udp(from, to, &ping, pair.now).unwrap();
        pair.flush();
        assert_eq!(pair.datagrams.iter().filter(|(_,d)| matches!(wire::decode_datagram(d),Ok(Datagram::Encrypted {header,..}) if header.kind==Kind::Pong)).count(),1);
    }

    #[test]
    fn no_audience_blocks_udp_media_but_does_not_block_signaling() {
        let mut pair = Pair::new();
        pair.advance(2500);
        pair.peers[0].audience = None;
        pair.datagrams.clear();
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 1, &[1]), pair.now);
        pair.flush();
        assert!(!pair.datagrams.iter().any(|(_,d)| matches!(wire::decode_datagram(d),Ok(Datagram::Encrypted {header,..}) if header.kind==Kind::Voice)));
        pair.peers[0].audience = Some(Audience {
            revision: 2,
            send_to: vec![2],
            receive_from: vec![],
        });
        pair.peers[1].audience = Some(Audience {
            revision: 2,
            send_to: vec![1],
            receive_from: vec![],
        });
        pair.media.clear();
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 2, &[1]), pair.now);
        pair.flush();
        assert!(pair.media.is_empty());
        assert!(pair.traces.iter().any(|s| s.contains("audience-udp")));
    }

    #[test]
    fn all_pongs_lost_has_bounded_retries_and_coordinated_fresh_snapshots() {
        let mut pair = Pair::new();
        pair.lose_pongs = true;
        pair.lose_return = true;
        let generations = [pair.peers[0].generation, pair.peers[1].generation];
        pair.advance(12500);
        assert!(
            pair.traces.iter().any(|t| t.contains("reset-required")),
            "{:?}",
            pair.traces
        );
        for (i, peer) in pair.peers.iter().enumerate() {
            let other = 2 - i as u16;
            assert!(peer.peers[&other].local.ice.restart_generation > 0);
            assert!(
                peer.peers[&other]
                    .remote
                    .as_ref()
                    .unwrap()
                    .ice
                    .restart_generation
                    > 0
            );
            assert_eq!(peer.generation, generations[i]);
        }
        assert!(
            pair.datagrams.len() < 200,
            "retry flood: {}",
            pair.datagrams.len()
        );
        pair.lose_pongs = false;
        pair.lose_return = false;
        pair.advance(3000);
        assert_eq!(pair.active(0), Path::Direct);
        assert_eq!(pair.active(1), Path::Direct);
    }

    #[test]
    fn game_probe_binds_sender_route_revision_and_token_without_reply_loops() {
        let mut pair = Pair::new();
        pair.advance(2500);
        let unknown = signaling::encode_game(
            &Signal::GameProbe {
                probe: 7,
                reply: false,
            },
            9,
            true,
        )
        .unwrap();
        pair.peers[0].game(&unknown, pair.now).unwrap();
        assert!(pair.peers[0].poll().is_none());
        let forged = signaling::encode_game(
            &Signal::GameProbe {
                probe: 7,
                reply: true,
            },
            2,
            true,
        )
        .unwrap();
        pair.peers[0].game(&forged, pair.now).unwrap();
        assert!(pair.peers[0].poll().is_none());
        let peer = pair.peers[0].peers.get_mut(&2).unwrap();
        peer.ice.probes_mut().register(Path::Game, pair.now);
        let events = peer.ice.probes_mut().poll(pair.now).unwrap();
        let token = events
            .into_iter()
            .find_map(|e| {
                if let ProbeEvent::SendPing { route, token } = e {
                    (route.path == Path::Game).then_some(token)
                } else {
                    None
                }
            })
            .unwrap();
        peer.ice.probes_mut().register(Path::Game, pair.now);
        let stale = signaling::encode_game(
            &Signal::GameProbe {
                probe: token,
                reply: true,
            },
            2,
            true,
        )
        .unwrap();
        pair.peers[0].game(&stale, pair.now + ms(1)).unwrap();
        assert!(pair.peers[0].poll().is_none());
    }

    #[test]
    fn route_optimization_requires_ten_ms_for_three_seconds_and_failure_is_immediate() {
        let now = Instant::now();
        let mut choice = Choice::default();
        assert_eq!(choice.update(&[], now).0, Path::Game);
        let hub = Path::Hub { session: 1 };
        assert_eq!(
            choice
                .update(&[(Path::Game, ms(100)), (hub, ms(40))], now)
                .0,
            hub
        );
        assert_eq!(
            choice
                .update(&[(hub, ms(40)), (Path::Direct, ms(31))], now + ms(4000))
                .0,
            hub
        );
        assert_eq!(
            choice
                .update(&[(hub, ms(40)), (Path::Direct, ms(30))], now + ms(5000))
                .0,
            hub
        );
        assert_eq!(
            choice
                .update(&[(hub, ms(40)), (Path::Direct, ms(30))], now + ms(7999))
                .0,
            hub
        );
        assert_eq!(
            choice
                .update(&[(hub, ms(40)), (Path::Direct, ms(30))], now + ms(8000))
                .0,
            Path::Direct
        );
        assert_eq!(choice.update(&[(hub, ms(40))], now + ms(8001)).0, hub);
    }

    #[test]
    fn optimization_to_faster_game_does_not_bounce_back_and_backup_prefers_udp() {
        let now = Instant::now();
        let mut choice = Choice::default();
        let hub = Path::Hub { session: 1 };
        assert_eq!(
            choice.update(
                &[
                    (Path::Direct, ms(100)),
                    (Path::Game, ms(50)),
                    (hub, ms(110))
                ],
                now
            ),
            (Path::Direct, Some(hub))
        );
        choice.update(
            &[(Path::Direct, ms(100)), (Path::Game, ms(50))],
            now + ms(1),
        );
        assert_eq!(
            choice
                .update(
                    &[(Path::Direct, ms(100)), (Path::Game, ms(50))],
                    now + ms(3001)
                )
                .0,
            Path::Game
        );
        for n in 3002..3010 {
            assert_eq!(
                choice
                    .update(
                        &[(Path::Direct, ms(100)), (Path::Game, ms(50))],
                        now + ms(n)
                    )
                    .0,
                Path::Game
            );
        }
    }

    fn add_hub(pair: &mut Pair, index: usize) -> Path {
        let local = index as u16 + 1;
        let other = 2 - index as u16;
        let grant = RelayGrant {
            session: 9,
            relay: 0,
            sender: 1,
            recipient: 2,
            hub_generation: 77,
            expires_at_ms: pair.peers[index].now_ms(pair.now) + 100_000,
            secret: [9; 16],
        };
        let path = Path::Hub { session: 9 };
        let client = BindClient::new(grant.clone(), local, 1).unwrap();
        let source = pair.peers[index].sockets[0];
        let peer = pair.peers[index].peers.get_mut(&other).unwrap();
        peer.bindings.insert(
            (0, 9),
            Binding {
                grant,
                source,
                endpoint: "127.0.0.1:45000".parse().unwrap(),
                packet: client.request(),
                client,
                attempt: 1,
                started: pair.now,
                deadline: pair.now + HANDSHAKE_LIMIT,
                next: pair.now,
                bound: true,
            },
        );
        let id = peer.ice.probes_mut().register(path, pair.now).unwrap();
        let token = peer
            .ice
            .probes_mut()
            .poll(pair.now)
            .unwrap()
            .into_iter()
            .find_map(|e| match e {
                ProbeEvent::SendPing { route, token } if route == id => Some(token),
                _ => None,
            })
            .unwrap();
        peer.ice
            .probes_mut()
            .authenticated_pong(id, token, pair.now + ms(2));
        peer.backup = Some(path);
        path
    }

    #[test]
    fn encrypts_once_for_two_udp_routes_and_sends_one_game_copy() {
        let mut pair = Pair::new();
        pair.advance(2500);
        add_hub(&mut pair, 0);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 7, &[1, 2]), pair.now);
        let mut direct = None;
        let mut hub = None;
        let mut game = 0;
        while let Some(output) = pair.peers[0].poll() {
            match output {
                Output::Udp(tx) => match wire::decode_datagram(&tx.contents).unwrap() {
                    Datagram::Encrypted { .. } => direct = Some(tx.contents.clone()),
                    Datagram::Relay { datagram, .. } => hub = Some(datagram.to_vec()),
                    _ => {}
                },
                Output::Game(bytes) if bytes[2] == wire::GAME_VOICE => game += 1,
                _ => {}
            }
        }
        assert_eq!(direct, hub);
        assert!(direct.is_some());
        assert_eq!(game, 1);
    }

    #[test]
    fn relay_pong_preserves_the_complete_arrival_route() {
        let mut pair = Pair::new();
        pair.advance(2500);
        add_hub(&mut pair, 1);
        let peer = pair.peers[0].peers.get_mut(&2).unwrap();
        let ping = peer
            .session
            .as_mut()
            .unwrap()
            .encrypt(Inner {
                kind: Kind::Ping,
                sequence: 0,
                talkspurt: 0,
                body: &21u64.to_be_bytes(),
            })
            .unwrap();
        let wrapped = wire::encode_datagram(Datagram::Relay {
            session: 9,
            datagram: &ping,
        })
        .unwrap();
        let to = pair.peers[1].sockets[0];
        pair.peers[1]
            .udp("127.0.0.1:45000".parse().unwrap(), to, &wrapped, pair.now)
            .unwrap();
        let packets: Vec<_> = std::iter::from_fn(|| pair.peers[1].poll())
            .filter_map(|o| {
                if let Output::Udp(tx) = o {
                    Some(tx)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].destination, "127.0.0.1:45000".parse().unwrap());
        assert!(
            matches!(wire::decode_datagram(&packets[0].contents),Ok(Datagram::Relay {session:9,datagram}) if matches!(wire::decode_datagram(datagram),Ok(Datagram::Encrypted {header,..}) if header.kind==Kind::Pong))
        );
    }

    #[test]
    fn authenticated_non_pong_activates_pending_key_when_all_pongs_are_lost() {
        let mut pair = Pair::new();
        pair.lose_pongs = true;
        pair.advance(5500);
        for (i, peer) in pair.peers.iter().enumerate() {
            assert!(
                peer.peers[&(2 - i as u16)]
                    .session
                    .as_ref()
                    .unwrap()
                    .active_epoch()
                    .is_some()
            );
        }
        assert!(!pair.traces.iter().any(|t| t.contains("reset-required")));
    }

    #[test]
    fn phase_overlap_rekey_is_retained_and_retried_after_old_key_expires() {
        let mut pair = Pair::new();
        pair.advance(2500);
        let handle = pair.peers[0].handle().unwrap();
        let mut peer = pair.peers[0].peers.remove(&2).unwrap();
        let actions = peer
            .session
            .as_mut()
            .unwrap()
            .request_rekey(handle, pair.now)
            .unwrap();
        pair.peers[0].crypto_actions(2, &mut peer, actions, None, pair.now);
        pair.peers[0].peers.insert(2, peer);
        pair.flush();
        let generation = pair.peers[0].peers[&2]
            .session
            .as_ref()
            .unwrap()
            .generation();
        let request = signaling::encode_game(
            &Signal::RekeyRequest {
                session_generation: generation,
            },
            2,
            true,
        )
        .unwrap();
        pair.peers[0].game(&request, pair.now).unwrap();
        assert!(pair.peers[0].peers[&2].deferred_rekey);
        pair.advance(5500);
        assert!(
            pair.peers[0].peers[&2]
                .session
                .as_ref()
                .unwrap()
                .generation()
                > generation
        );
    }

    #[test]
    fn bind_retry_uses_higher_attempt_after_challenge_lifetime_and_stops_at_deadline() {
        let mut pair = Pair::new();
        pair.advance(2500);
        pair.peers[0].config.hub = Some("127.0.0.1:45000".parse().unwrap());
        let grant = RelayGrant {
            session: 10,
            relay: 0,
            sender: 1,
            recipient: 2,
            hub_generation: 77,
            expires_at_ms: pair.peers[0].now_ms(pair.now) + 100_000,
            secret: [9; 16],
        };
        pair.peers[0].grant(9, grant.clone(), pair.now).unwrap();
        assert!(pair.peers[0].peers[&2].bindings.is_empty());
        pair.peers[0].grant(0, grant, pair.now).unwrap();
        pair.peers[0].tick(pair.now, true).unwrap();
        let attempt = pair.peers[0].peers[&2].bindings[&(0, 10)].attempt;
        while pair.peers[0].poll().is_some() {}
        pair.peers[0].tick(pair.now + ms(5100), true).unwrap();
        assert!(pair.peers[0].peers[&2].bindings[&(0, 10)].attempt > attempt);
        while pair.peers[0].poll().is_some() {}
        pair.peers[0].tick(pair.now + ms(10100), true).unwrap();
        assert!(!std::iter::from_fn(||pair.peers[0].poll()).any(|o|matches!(o,Output::Udp(tx) if matches!(wire::decode_datagram(&tx.contents),Ok(Datagram::Bind(_))))));
    }

    #[test]
    fn first_hub_announce_is_not_postponed_by_ticks_before_any_peer() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig {
                hub: Some("127.0.0.1:45000".parse().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let hub_announces = |runtime: &mut Runtime| {
            std::iter::from_fn(|| runtime.poll())
                .filter(|o| match o {
                    Output::Game(bytes) => matches!(
                        signaling::decode_game(bytes, false),
                        Ok((0, Signal::Announce(_)))
                    ),
                    _ => false,
                })
                .count()
        };
        runtime.tick(now, false).unwrap();
        assert_eq!(hub_announces(&mut runtime), 0);
        let later = now + Duration::from_secs(1);
        runtime.roster(vec![1, 2], later).unwrap();
        runtime.identify(1, later).unwrap();
        runtime.tick(later, false).unwrap();
        assert_eq!(hub_announces(&mut runtime), 1);
        runtime.tick(later + Duration::from_secs(1), false).unwrap();
        assert_eq!(hub_announces(&mut runtime), 0);
        runtime.tick(later + Duration::from_secs(5), false).unwrap();
        assert_eq!(hub_announces(&mut runtime), 1);
    }

    #[test]
    fn game_only_ignores_relay_grants_and_emits_no_udp() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig {
                hub: Some("127.0.0.1:45000".parse().unwrap()),
                game_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        runtime.roster(vec![1, 2], now).unwrap();
        runtime.identify(1, now).unwrap();
        let grant = RelayGrant {
            session: 10,
            relay: 0,
            sender: 1,
            recipient: 2,
            hub_generation: 77,
            expires_at_ms: runtime.now_ms(now) + 100_000,
            secret: [9; 16],
        };
        runtime.grant(0, grant, now).unwrap();
        runtime.tick(now, true).unwrap();
        assert!(runtime.peers[&2].bindings.is_empty());
        assert!(!std::iter::from_fn(|| runtime.poll()).any(|o| matches!(o, Output::Udp(_))));
    }

    #[test]
    fn hub_generation_replacement_rejects_retired_grants() {
        let mut pair = Pair::new();
        pair.advance(2500);
        pair.peers[0].config.hub = Some("127.0.0.1:45000".parse().unwrap());
        let old = RelayGrant {
            session: 10,
            relay: 0,
            sender: 1,
            recipient: 2,
            hub_generation: 77,
            expires_at_ms: pair.peers[0].now_ms(pair.now) + 100_000,
            secret: [9; 16],
        };
        pair.peers[0].grant(0, old.clone(), pair.now).unwrap();
        let mut new = old.clone();
        new.session = 11;
        new.hub_generation = 78;
        pair.peers[0].grant(0, new, pair.now).unwrap();
        pair.peers[0].grant(0, old, pair.now).unwrap();
        assert_eq!(
            pair.peers[0].peers[&2]
                .bindings
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![(0, 11)]
        );
    }

    #[test]
    fn player_relay_reoffers_closed_or_expired_pairs_and_renews_live_grants() {
        let mut pair = Pair::new();
        pair.advance(2500);
        let runtime = &mut pair.peers[0];
        runtime.roster(vec![1, 2, 3], pair.now).unwrap();
        let third = runtime.peers.get_mut(&3).unwrap();
        let mut remote = third.local.clone();
        remote.stream_generation = 333;
        third.remote = Some(remote);
        let id = third
            .ice
            .probes_mut()
            .register(Path::Direct, pair.now)
            .unwrap();
        let token = third
            .ice
            .probes_mut()
            .poll(pair.now)
            .unwrap()
            .into_iter()
            .find_map(|e| match e {
                ProbeEvent::SendPing { route, token } if route == id => Some(token),
                _ => None,
            })
            .unwrap();
        third
            .ice
            .probes_mut()
            .authenticated_pong(id, token, pair.now + ms(1));
        runtime.relay = Some(Relay::new(
            1,
            runtime.generation,
            Limits {
                grant_ms: 1000,
                ..Default::default()
            },
        ));
        runtime.next_relay_check = None;
        runtime.player_relays(pair.now);
        let first = runtime.offers[&(2, 3)].0;
        runtime.relay.as_mut().unwrap().withdraw(first);
        runtime.player_relays(pair.now + ms(250));
        let second = runtime.offers[&(2, 3)].0;
        assert_ne!(first, second);
        runtime.player_relays(pair.now + ms(1001));
        let third = runtime.offers[&(2, 3)].0;
        assert_ne!(second, third);
        runtime.player_relays(pair.now + ms(2100));
        let fourth = runtime.offers[&(2, 3)].0;
        assert_ne!(third, fourth);
    }

    #[test]
    fn first_game_frame_is_v2_before_identification_or_peer_capability() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.transmit(&packet::encode(packet::START, 0, &[]), now);
        runtime.transmit(&packet::encode(packet::VOICE, 1, &[1]), now);
        runtime.transmit(&packet::encode(packet::STOP, 1, &[]), now);
        let packets: Vec<_> = std::iter::from_fn(|| runtime.poll())
            .filter_map(|o| {
                if let Output::Game(bytes) = o {
                    Some(bytes)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            packets.iter().map(|p| p[2]).collect::<Vec<_>>(),
            vec![wire::GAME_START, wire::GAME_VOICE, wire::GAME_STOP]
        );
        for bytes in packets {
            assert_eq!(&bytes[5..13], &runtime.generation.to_be_bytes());
        }
    }

    #[test]
    fn self_game_echo_uses_its_authoritative_local_stream() {
        let now = Instant::now();
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.identify(7, now).unwrap();
        runtime.transmit(&packet::encode(packet::START, 0, &[]), now);
        runtime.transmit(&packet::encode(packet::VOICE, 1, &[0xf8, 0xff, 0xfe]), now);
        let mut mixer = crate::mix::Mixer::default();
        let mut voice = 0;
        while let Some(output) = runtime.poll() {
            match output {
                Output::Select(stream) => {
                    assert!(mixer.select(stream, now).is_empty());
                }
                Output::Game(bytes) => {
                    let mut forwarded = bytes[..3].to_vec();
                    forwarded.extend_from_slice(&7u16.to_be_bytes());
                    forwarded.extend_from_slice(&bytes[3..]);
                    let media = wire::decode_game(&forwarded).unwrap();
                    if mixer.game(media, now) && media.kind == Kind::Voice {
                        voice += 1;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(voice, 1);
        assert_eq!(mixer.stats.decoded, 1);
        assert!(mixer.tick(now + ms(60)).is_some());
    }

    #[test]
    fn game_media_and_announce_reach_the_mixer_in_either_arrival_order() {
        for media_first in [false, true] {
            let mut pair = Pair::new();
            let now = pair.now;
            let mut mixer = crate::mix::Mixer::default();
            let mut received = Vec::new();
            let drain = |runtime: &mut Runtime,
                         mixer: &mut crate::mix::Mixer,
                         received: &mut Vec<(u16, u16)>| {
                while let Some(output) = runtime.poll() {
                    match output {
                        Output::Select(stream) => received.extend(mixer.select(stream, now)),
                        Output::Media(media)
                            if mixer.game(media.borrowed(), now) && media.kind == Kind::Voice =>
                        {
                            received.push((media.stream.player, media.sequence));
                        }
                        _ => {}
                    }
                }
            };
            drain(&mut pair.peers[1], &mut mixer, &mut received);
            let announce = signaling::encode_game(
                &Signal::Announce(pair.peers[0].peers[&2].local.clone()),
                1,
                true,
            )
            .unwrap();
            let media = wire::encode_game(
                GameMedia {
                    stream: StreamId {
                        player: 1,
                        generation: pair.peers[0].generation,
                    },
                    kind: Kind::Voice,
                    sequence: 1,
                    talkspurt: 1,
                    opus: &[0xf8, 0xff, 0xfe],
                },
                true,
            )
            .unwrap();
            let packets = if media_first {
                [&media, &announce]
            } else {
                [&announce, &media]
            };
            for (index, bytes) in packets.into_iter().enumerate() {
                pair.peers[1].game(bytes, now).unwrap();
                drain(&mut pair.peers[1], &mut mixer, &mut received);
                if index == 0 {
                    assert_eq!(mixer.stats.decoded, 0);
                    assert!(received.is_empty());
                }
            }
            assert_eq!(received, vec![(1, 1)]);
            assert_eq!(mixer.stats.decoded, 1);
            pair.peers[1].game(&media, now).unwrap();
            drain(&mut pair.peers[1], &mut mixer, &mut received);
            assert_eq!(received, vec![(1, 1)]);
            assert_eq!(mixer.stats.decoded, 1);
            assert!(mixer.tick(now + ms(60)).is_some());
        }
    }

    #[test]
    fn game_send_is_v2_only_even_with_unannounced_or_legacy_listeners() {
        let mut pair = Pair::new();
        pair.advance(2500);
        pair.peers[0].audience.as_mut().unwrap().send_to.push(3);
        pair.peers[0].transmit(&packet::encode(packet::START, 0, &[]), pair.now);
        let mut kinds = Vec::new();
        while let Some(output) = pair.peers[0].poll() {
            if let Output::Game(bytes) = output {
                kinds.push(bytes[2]);
            }
        }
        assert!(!kinds.contains(&packet::START));
        assert_eq!(kinds.iter().filter(|&&k| k == wire::GAME_START).count(), 1);
        pair.peers[0]
            .audience
            .as_mut()
            .unwrap()
            .send_to
            .retain(|&p| p != 3);
        pair.peers[0].transmit(&packet::encode(packet::VOICE, 1, &[1]), pair.now);
        let mut kinds = Vec::new();
        while let Some(output) = pair.peers[0].poll() {
            if let Output::Game(bytes) = output {
                kinds.push(bytes[2]);
            }
        }
        assert!(!kinds.contains(&packet::VOICE));
        assert_eq!(kinds.iter().filter(|&&k| k == wire::GAME_VOICE).count(), 1);
        pair.peers[0].tick(pair.now + REFRESH, true).unwrap();
        assert!(
            std::iter::from_fn(|| pair.peers[0].poll())
                .any(|o| matches!(o,Output::Game(bytes) if bytes[2]==wire::GAME_START))
        );
    }

    #[test]
    fn review_regression_pair_recreated_after_roster_removal_reconverges_on_udp() {
        // the mod sends ROSTER only to connected players: the side that rebuilds the pair
        // is the one still in the game, and the player who left keeps its old snapshot
        for (rebuilder, stale_first) in [(0, false), (0, true), (1, false), (1, true)] {
            let holder = 1 - rebuilder;
            let rebuilder_player = rebuilder as u16 + 1;
            let mut pair = Pair::new();
            pair.advance(2500);
            assert_eq!(pair.active(holder), Path::Direct);
            pair.peers[rebuilder]
                .roster(vec![rebuilder_player], pair.now)
                .unwrap();
            pair.advance(3000);
            assert_eq!(pair.active(holder), Path::Game);
            pair.peers[rebuilder].roster(vec![1, 2], pair.now).unwrap();
            if stale_first {
                // the holder's republished pre-removal snapshot overtakes the new pair's
                let stale = signaling::encode_game(
                    &Signal::Announce(pair.peers[holder].peers[&rebuilder_player].local.clone()),
                    holder as u16 + 1,
                    true,
                )
                .unwrap();
                pair.peers[rebuilder].game(&stale, pair.now).unwrap();
            }
            pair.advance(5000);
            let case = format!("rebuilder {rebuilder} stale_first {stale_first}");
            for i in 0..2 {
                let peer = &pair.peers[i].peers[&(2 - i as u16)];
                assert!(
                    peer.session
                        .as_ref()
                        .is_some_and(|s| s.active_epoch().is_some()),
                    "{case}: {:?}",
                    pair.traces
                );
                assert_ne!(pair.active(i), Path::Game, "{case}");
            }
            pair.datagrams.clear();
            pair.media.clear();
            pair.peers[holder].transmit(&packet::encode(packet::VOICE, 9, &[7]), pair.now);
            pair.flush();
            assert!(pair.datagrams.iter().any(|(i, d)| *i == holder && matches!(wire::decode_datagram(d), Ok(Datagram::Encrypted { header, .. }) if header.kind == Kind::Voice)), "{case}");
            assert!(
                pair.media
                    .iter()
                    .any(|(i, m)| *i == rebuilder && m.sequence == 9 && m.kind == Kind::Voice),
                "{case}"
            );
        }
    }
}

fn ordinary_kind(kind: Kind) -> Kind {
    match kind {
        Kind::PolicyVoice => Kind::Voice,
        Kind::PolicyStart => Kind::Start,
        Kind::PolicyStop => Kind::Stop,
        kind => kind,
    }
}

fn policy_kind(kind: Kind) -> Kind {
    match kind {
        Kind::Voice => Kind::PolicyVoice,
        Kind::Start => Kind::PolicyStart,
        Kind::Stop => Kind::PolicyStop,
        kind => kind,
    }
}

pub struct Media {
    pub stream: StreamId,
    pub kind: Kind,
    pub sequence: u16,
    pub talkspurt: u16,
    pub opus: Vec<u8>,
    pub revision: Option<u64>,
}

impl Media {
    pub fn borrowed(&self) -> GameMedia<'_> {
        GameMedia {
            stream: self.stream,
            kind: self.kind,
            sequence: self.sequence,
            talkspurt: self.talkspurt,
            opus: &self.opus,
        }
    }
}

pub enum Output {
    Game(Vec<u8>),
    Udp(DatagramTx),
    Select(StreamId),
    Media(Media),
    UdpMedia(Media),
    Reorder(u16, Duration),
    Purge { all: bool, players: Vec<u16> },
    ResetStreams,
    Trace(String),
}

struct PendingOutput {
    output: Output,
    local_media: bool,
}

impl From<Output> for PendingOutput {
    fn from(output: Output) -> Self {
        Self {
            output,
            local_media: false,
        }
    }
}

#[cfg(test)]
fn outgoing_media(output: &Output) -> bool {
    match output {
        Output::Game(bytes) => matches!(
            bytes.get(2).copied(),
            Some(
                wire::GAME_START
                    | wire::GAME_VOICE
                    | wire::GAME_STOP
                    | wire::GAME_POLICY_START
                    | wire::GAME_POLICY_VOICE
                    | wire::GAME_POLICY_STOP
            )
        ),
        Output::Udp(tx) => {
            let datagram = match wire::decode_datagram(&tx.contents) {
                Ok(Datagram::Relay { datagram, .. }) => wire::decode_datagram(datagram),
                packet => packet,
            };
            matches!(datagram, Ok(Datagram::Encrypted { header, .. }) if !matches!(header.kind, Kind::Ping | Kind::Pong))
        }
        _ => false,
    }
}

#[derive(Default)]
struct Choice {
    active: Option<Path>,
    improving: Option<(Path, Instant)>,
    udp_ready: bool,
}

fn rank(path: Path) -> (u8, u16, u32) {
    match path {
        Path::Direct => (0, 0, 0),
        Path::Hub { session } => (1, 0, session),
        Path::PlayerRelay { player, session } => (2, player, session),
        Path::Game => (3, 0, 0),
    }
}

impl Choice {
    fn update(&mut self, routes: &[(Path, Duration)], now: Instant) -> (Path, Option<Path>) {
        let mut live = routes.to_vec();
        live.sort_by_key(|(p, rtt)| (*rtt, rank(*p)));
        let candidate = live.first().map_or(Path::Game, |r| r.0);
        let current = self.active.unwrap_or(Path::Game);
        let current_rtt = live.iter().find(|r| r.0 == current).map(|r| r.1);
        let udp = live.iter().find(|r| r.0 != Path::Game).map(|r| r.0);
        let immediate = if current == Path::Game && udp.is_some() && !self.udp_ready {
            udp
        } else if current_rtt.is_none() {
            Some(candidate)
        } else {
            None
        };
        let next = if let Some(next) = immediate {
            self.improving = None;
            next
        } else if candidate != current && current_rtt.is_some_and(|rtt| rtt >= live[0].1 + IMPROVE)
        {
            let (_, since) = *self.improving.get_or_insert((candidate, now));
            if self.improving.is_some_and(|(p, _)| p != candidate) {
                self.improving = Some((candidate, now));
                current
            } else if now.saturating_duration_since(since) >= SUSTAIN {
                self.improving = None;
                candidate
            } else {
                current
            }
        } else {
            self.improving = None;
            current
        };
        self.active = Some(next);
        self.udp_ready = udp.is_some();
        let backup = live
            .iter()
            .find(|(p, _)| *p != next && *p != Path::Game)
            .map(|r| r.0)
            .or_else(|| (next != Path::Game).then_some(Path::Game));
        (next, backup)
    }
}

struct Binding {
    grant: RelayGrant,
    source: SocketAddr,
    endpoint: SocketAddr,
    client: BindClient,
    attempt: u64,
    started: Instant,
    deadline: Instant,
    next: Instant,
    packet: Vec<u8>,
    bound: bool,
}

impl Binding {
    fn new_attempt(&mut self, player: u16, now: Instant) -> Result<()> {
        let attempt = self
            .attempt
            .checked_add(1)
            .ok_or("bind attempts exhausted")?;
        let client =
            BindClient::new(self.grant.clone(), player, attempt).ok_or("bind random source")?;
        self.attempt = attempt;
        self.client = client;
        self.packet = self.client.request();
        self.started = now;
        self.next = now;
        self.bound = false;
        Ok(())
    }

    fn path(&self) -> Path {
        if self.grant.relay == 0 {
            Path::Hub {
                session: self.grant.session,
            }
        } else {
            Path::PlayerRelay {
                player: self.grant.relay,
                session: self.grant.session,
            }
        }
    }
}

struct Peer {
    ice: PairConnectivity,
    local: Announce,
    remote: Option<Announce>,
    retired: VecDeque<u64>,
    session: Option<Session>,
    retry_until: Option<Instant>,
    next_retry: Instant,
    waiting_reset: bool,
    /// no handshake message has verified against this pair's snapshots yet. a newer
    /// remote snapshot then answers our own announce, so it does not restart ours:
    /// restarting would answer an answer, and both sides would restart without end.
    unconfirmed: bool,
    deferred_rekey: bool,
    bindings: BTreeMap<(u16, u32), Binding>,
    choice: Choice,
    routes: Vec<(Path, Duration)>,
    backup: Option<Path>,
    reorder_wait: Option<Duration>,
}

pub struct Runtime {
    sockets: Vec<SocketAddr>,
    config: RuntimeConfig,
    key: StaticKeypair,
    generation: u64,
    player: Option<u16>,
    peers: BTreeMap<u16, Peer>,
    roster: Vec<u16>,
    audience: Option<Audience>,
    policy: Option<crate::policy::PolicyGate>,
    policy_seen: Option<Instant>,
    capture_reset: u64,
    playout_reset: u64,
    audience_reset: bool,
    audience_blocked: bool,
    relay: Option<Relay>,
    offers: BTreeMap<(u16, u16), (u32, u64, u64)>,
    /// above every restart generation a removed pair announced. the mod sends ROSTER
    /// only to connected players, so a player who left keeps our old snapshot; a
    /// rebuilt pair starting here is a restart to it, not a mutation it drops.
    restart_floor: u32,
    next_handle: u32,
    out: VecDeque<PendingOutput>,
    talkspurt: u16,
    sequence: u16,
    on_air: bool,
    next_start: Option<Instant>,
    next_hub_announce: Option<Instant>,
    epoch: Instant,
    unix_ms: u64,
    relay_generations: BTreeMap<u16, (u64, Vec<u64>)>,
    next_relay_check: Option<Instant>,
    next_offer_publish: Option<Instant>,
}

impl Runtime {
    pub fn selected_path(&self, player: u16) -> Option<Path> {
        self.peers.get(&player).and_then(|peer| peer.choice.active)
    }

    pub fn new(sockets: Vec<SocketAddr>, config: RuntimeConfig) -> Result<Self> {
        let mut generation = [0; 8];
        getrandom::fill(&mut generation)?;
        Ok(Self {
            sockets,
            config,
            key: crypto::generate_static_keypair()?,
            generation: u64::from_be_bytes(generation).max(1),
            player: None,
            peers: BTreeMap::new(),
            roster: Vec::new(),
            audience: None,
            policy: None,
            policy_seen: None,
            capture_reset: 0,
            playout_reset: 0,
            audience_reset: false,
            audience_blocked: false,
            relay: None,
            offers: BTreeMap::new(),
            restart_floor: 0,
            next_handle: 0,
            out: VecDeque::new(),
            talkspurt: 0,
            sequence: 0,
            on_air: false,
            next_start: None,
            next_hub_announce: None,
            epoch: Instant::now(),
            unix_ms: crate::unix_us() / 1000,
            relay_generations: BTreeMap::new(),
            next_relay_check: None,
            next_offer_publish: None,
        })
    }

    pub fn poll(&mut self) -> Option<Output> {
        while let Some(PendingOutput {
            output,
            local_media,
        }) = self.out.pop_front()
        {
            if local_media && !self.capture_policy().0 {
                continue;
            }
            if let Output::Media(media) | Output::UdpMedia(media) = &output
                && !self.accepts_media(media.stream.player, media.revision)
            {
                continue;
            }
            return Some(output);
        }
        None
    }

    fn now_ms(&self, now: Instant) -> u64 {
        self.unix_ms
            .saturating_add(now.saturating_duration_since(self.epoch).as_millis() as u64)
    }

    fn trace(&mut self, text: String) {
        self.out.push_back(PendingOutput::from(Output::Trace(text)));
    }

    fn handle(&mut self) -> Result<ReceiverHandle> {
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .ok_or("receive handles exhausted")?;
        Ok(ReceiverHandle(self.next_handle))
    }

    fn signal(&mut self, peer: u16, signal: &Signal) {
        match signaling::encode_game(signal, peer, false) {
            Ok(bytes) => self.out.push_back(PendingOutput::from(Output::Game(bytes))),
            Err(_) => self.trace(format!("drop {peer} signal-encoding")),
        }
    }

    fn description(&self, ice: &PairConnectivity) -> Announce {
        Announce {
            version: signaling::PROTOCOL_VERSION,
            capabilities: signaling::CAP_MEDIA_V2
                | if self.config.game_only {
                    0
                } else {
                    signaling::CAP_UDP
                },
            suites: vec![Suite::Aes256Gcm, Suite::ChaChaPoly],
            stream_generation: self.generation,
            static_key: self.key.public,
            ice: ice.local_credentials(),
            candidates: ice.initial_candidates().to_vec(),
            candidates_complete: ice.initial_gathering_complete(),
        }
    }

    pub fn identify(&mut self, player: u16, now: Instant) -> Result<()> {
        if self.player == Some(player) {
            return Ok(());
        }
        if self.player.is_some() {
            return Err("game changed local identity".into());
        }
        self.player = Some(player);
        if let Some(policy) = &mut self.policy {
            policy.identify(player);
            self.purge(true, Vec::new());
        }
        self.out
            .push_back(PendingOutput::from(Output::Select(StreamId {
                player,
                generation: self.generation,
            })));
        self.relay = Some(Relay::new(player, self.generation, Limits::default()));
        self.trace(format!("generation {player} {}", self.generation));
        self.apply_roster(self.roster.clone(), now)
    }

    /// the trusted ROSTER signal; only it authorizes the one-shot AUDIENCE reset.
    pub fn roster(&mut self, players: Vec<u16>, now: Instant) -> Result<()> {
        self.audience_reset = true;
        self.apply_roster(players, now)
    }

    fn apply_roster(&mut self, players: Vec<u16>, now: Instant) -> Result<()> {
        self.roster = players;
        let Some(local) = self.player else {
            return Ok(());
        };
        let removed: Vec<_> = self
            .peers
            .keys()
            .copied()
            .filter(|p| !self.roster.contains(p))
            .collect();
        for player in removed {
            if let Some(peer) = self.peers.remove(&player) {
                self.restart_floor = self
                    .restart_floor
                    .max(peer.local.ice.restart_generation.saturating_add(1));
            }
        }
        for remote in self.roster.clone() {
            if remote == local || remote == 0 || self.peers.contains_key(&remote) {
                continue;
            }
            if self.peers.len() >= 256 {
                break;
            }
            let mut config = self.config.connectivity.clone();
            config.discovery.hub = self.config.hub.map(|endpoint| HubReflection {
                endpoint,
                session: 0,
                hub_generation: 0,
            });
            if self.config.game_only {
                config.discovery = Default::default();
            }
            let mut ice = PairConnectivity::new(
                StreamId {
                    player: local,
                    generation: self.generation,
                },
                StreamId {
                    player: remote,
                    generation: 0,
                },
                self.sockets.clone(),
                config,
                now,
            )?;
            ice.probes_mut().register(Path::Game, now);
            ice.start_at_restart_generation(self.restart_floor);
            let description = self.description(&ice);
            self.signal(remote, &Signal::Announce(description.clone()));
            self.peers.insert(
                remote,
                Peer {
                    ice,
                    local: description,
                    remote: None,
                    retired: VecDeque::new(),
                    session: None,
                    retry_until: None,
                    next_retry: now,
                    waiting_reset: false,
                    unconfirmed: true,
                    deferred_rekey: false,
                    bindings: BTreeMap::new(),
                    choice: Choice::default(),
                    routes: Vec::new(),
                    backup: None,
                    reorder_wait: None,
                },
            );
        }
        Ok(())
    }

    pub fn allows_receive(&self, sender: u16) -> bool {
        self.accepts_media(sender, None)
    }

    pub fn accepts_media(&self, sender: u16, revision: Option<u64>) -> bool {
        if let Some(policy) = &self.policy {
            self.player.is_some()
                && revision.is_some_and(|revision| policy.accepts(sender, revision))
        } else {
            revision.is_none()
                && (self.player == Some(sender)
                    || self
                        .audience
                        .as_ref()
                        .is_some_and(|a| a.receive_from.contains(&sender)))
        }
    }

    pub fn capture_policy(&self) -> (bool, Option<u64>, u64) {
        let current = self.policy.as_ref().and_then(|gate| gate.current());
        (
            self.policy.is_none() || current.is_some_and(|p| p.tx_permitted),
            current.map(|p| p.audience.revision),
            self.capture_reset,
        )
    }

    /// real devices require an explicit, recently refreshed transmit permission;
    /// legacy synthetic sources retain `capture_policy`'s pre-policy behaviour
    pub fn microphone_permitted(&self, now: Instant) -> bool {
        self.policy_seen
            .is_some_and(|seen| now.saturating_duration_since(seen) < MIC_POLICY_LIFETIME)
            && self
                .policy
                .as_ref()
                .and_then(|gate| gate.current())
                .is_some_and(|policy| policy.tx_permitted)
    }

    /// ends the retiring talk while its policy and routes are still installed.
    /// the socket owner must drain these outputs before revoking that policy
    pub fn close_transmit(&mut self) {
        if self.on_air {
            self.send_media(Kind::Stop, &[]);
            self.send_media(Kind::Stop, &[]);
            self.on_air = false;
            self.next_start = None;
        }
    }

    pub fn playout_reset(&self) -> u64 {
        self.playout_reset
    }

    fn purge(&mut self, all: bool, players: Vec<u16>) {
        if all || !players.is_empty() {
            self.playout_reset = self.playout_reset.wrapping_add(1);
            self.out
                .push_back(PendingOutput::from(Output::Purge { all, players }));
        }
    }

    fn reset_policy_epoch(&mut self, now: Instant) -> Result<()> {
        let retired: BTreeMap<_, _> = self
            .peers
            .iter()
            .map(|(player, peer)| {
                let mut retired = peer.retired.clone();
                if let Some(remote) = &peer.remote {
                    retired.push_back(remote.stream_generation);
                }
                (*player, retired)
            })
            .collect();
        let mut fresh = Self::new(self.sockets.clone(), self.config.clone())?;
        fresh.capture_reset = self.capture_reset;
        fresh.playout_reset = self.playout_reset;
        fresh.policy = Some(crate::policy::PolicyGate::new(self.player.unwrap_or(0)));
        fresh
            .out
            .push_back(PendingOutput::from(Output::ResetStreams));
        fresh.roster = self.roster.clone();
        if let Some(player) = self.player {
            fresh.identify(player, now)?;
        }
        for (player, peer) in &mut fresh.peers {
            if let Some(retired) = retired.get(player) {
                peer.retired = retired.clone();
            }
        }
        *self = fresh;
        Ok(())
    }

    pub fn stun_servers(&mut self, servers: Vec<SocketAddr>, now: Instant) -> Result<()> {
        if self.config.game_only {
            return Ok(());
        }
        if servers
            .iter()
            .any(|s| !crate::connectivity::valid_socket(*s))
        {
            return Err(crate::connectivity::ConnectivityError::Configuration.into());
        }
        for server in servers {
            if self.config.connectivity.discovery.stun_servers.len() < 4
                && !self
                    .config
                    .connectivity
                    .discovery
                    .stun_servers
                    .contains(&server)
            {
                self.config.connectivity.discovery.stun_servers.push(server);
            }
        }
        let servers = self.config.connectivity.discovery.stun_servers.clone();
        let mut failure = None;
        for peer in self.peers.values_mut() {
            if let Err(err) = peer.ice.discover_stun(servers.clone(), now) {
                failure.get_or_insert(err);
            }
        }
        failure.map_or(Ok(()), |err| Err(err.into()))
    }

    pub fn game(&mut self, bytes: &[u8], now: Instant) -> Result<bool> {
        self.game_with_capture_close(bytes, now, |_, _| {})
    }

    /// the device owner can flush a retiring talk before an accepted policy
    /// change takes effect. the callback receives the new transmit permission;
    /// rejected and identical policies never close it
    pub fn game_with_capture_close(
        &mut self,
        bytes: &[u8],
        now: Instant,
        mut close: impl FnMut(&mut Self, bool),
    ) -> Result<bool> {
        if let Ok(policy) = signaling::decode_policy(bytes) {
            let permitted = policy.tx_permitted;
            let reset = self.audience_reset
                && self
                    .policy
                    .as_ref()
                    .and_then(|gate| gate.current())
                    .is_some_and(|p| {
                        policy.audience.revision < p.audience.revision
                            || (policy.audience.revision == p.audience.revision && policy != *p)
                    });
            let mut gate = if reset {
                crate::policy::PolicyGate::new(self.player.unwrap_or(0))
            } else {
                self.policy
                    .clone()
                    .unwrap_or_else(|| crate::policy::PolicyGate::new(self.player.unwrap_or(0)))
            };
            let first = gate.current().is_none();
            let update = gate.install(policy);
            self.audience_reset = false;
            match update {
                Ok(crate::policy::PolicyUpdate::Unchanged) => self.policy_seen = Some(now),
                Ok(crate::policy::PolicyUpdate::Installed {
                    purge_all,
                    removed_receivers,
                    tx_changed,
                    self_monitor_changed,
                }) => {
                    if first || reset || tx_changed || self_monitor_changed {
                        close(self, permitted);
                        if reset {
                            self.reset_policy_epoch(now)?;
                        }
                        self.out.retain(|pending| !pending.local_media);
                        self.capture_reset = self.capture_reset.wrapping_add(1);
                        self.on_air = false;
                        self.next_start = None;
                    }
                    self.policy = Some(gate);
                    self.policy_seen = Some(now);
                    self.purge(purge_all, removed_receivers);
                    self.trace(format!("policy {}", self.capture_policy().1.unwrap()));
                }
                Err(error) => self.trace(format!("drop 0 policy-{error:?}")),
            }
            return Ok(true);
        }
        if let Ok(audience) = signaling::decode_audience(bytes) {
            if self.policy.is_some() {
                return Ok(true);
            }
            let removed = self.audience.as_ref().map_or_else(Vec::new, |old| {
                old.receive_from
                    .iter()
                    .copied()
                    .filter(|p| !audience.receive_from.contains(p))
                    .collect()
            });
            let first = self.audience.is_none();
            let rollback = self
                .audience
                .as_ref()
                .is_some_and(|a| audience.revision < a.revision);
            if self.audience_reset || (!self.audience_blocked && !rollback) {
                self.audience = Some(audience);
                self.audience_reset = false;
                self.audience_blocked = false;
                self.purge(first || rollback, removed);
            } else if rollback {
                if let Some(scope) = &mut self.audience {
                    scope.send_to.clear();
                    scope.receive_from.clear();
                }
                self.audience_blocked = true;
                self.purge(true, Vec::new());
                self.trace("drop 0 audience-rollback".to_string());
            }
            return Ok(true);
        }
        let decoded_media = wire::decode_policy_game(bytes)
            .map(|packet| (packet.media, Some(packet.revision)))
            .or_else(|_| wire::decode_game(bytes).map(|media| (media, None)));
        if let Ok((media, revision)) = decoded_media {
            if self
                .peers
                .get(&media.stream.player)
                .is_some_and(|p| p.retired.contains(&media.stream.generation))
            {
                self.trace(format!("drop {} retired-game", media.stream.player));
            } else if self.accepts_media(media.stream.player, revision) {
                self.out.push_back(PendingOutput::from(Output::Media(Media {
                    stream: media.stream,
                    kind: ordinary_kind(media.kind),
                    sequence: media.sequence,
                    talkspurt: media.talkspurt,
                    opus: media.opus.to_vec(),
                    revision,
                })));
            } else {
                self.trace(format!("drop {} audience-game", media.stream.player));
            }
            return Ok(true);
        }
        let Ok((sender, signal)) = signaling::decode_game(bytes, true) else {
            return Ok(false);
        };
        if let Some(relay) = &mut self.relay {
            self.out.extend(
                relay
                    .on_signal(sender, &signal)
                    .into_iter()
                    .map(|f| PendingOutput::from(Output::Game(f.frame))),
            );
        }
        if let Signal::RelayGrant(grant) = signal {
            self.grant(sender, grant, now)?;
            return Ok(true);
        }
        if let Signal::RelayClose {
            relay,
            session,
            hub_generation,
        } = &signal
        {
            if sender == *relay {
                for peer in self.peers.values_mut() {
                    if peer
                        .bindings
                        .get(&(*relay, *session))
                        .is_some_and(|b| b.grant.hub_generation == *hub_generation)
                        && let Some(binding) = peer.bindings.remove(&(*relay, *session))
                    {
                        peer.ice.probes_mut().remove(binding.path());
                    }
                }
            }
            return Ok(true);
        }
        let Some(mut peer) = self.peers.remove(&sender) else {
            return Ok(true);
        };
        let result = self.peer_signal(sender, &mut peer, &signal, now);
        self.peers.insert(sender, peer);
        result?;
        Ok(true)
    }

    fn peer_signal(
        &mut self,
        sender: u16,
        peer: &mut Peer,
        signal: &Signal,
        now: Instant,
    ) -> Result<()> {
        if let Signal::GameProbe { probe, reply } = signal {
            if peer.remote.is_none() {
                return Ok(());
            }
            if *reply {
                if let Some(route) = peer.ice.probes().route(Path::Game) {
                    for event in peer.ice.probes_mut().authenticated_pong(route, *probe, now) {
                        self.probe(sender, peer, event, now);
                    }
                }
            } else {
                self.signal(
                    sender,
                    &Signal::GameProbe {
                        probe: *probe,
                        reply: true,
                    },
                );
            }
            return Ok(());
        }
        if let Signal::Announce(remote) = signal {
            if remote.capabilities & signaling::CAP_MEDIA_V2 == 0 {
                return Ok(());
            }
            if peer.retired.contains(&remote.stream_generation) {
                return Ok(());
            }
            if let Some(old) = &peer.remote {
                if old.stream_generation == remote.stream_generation {
                    if remote.ice.restart_generation < old.ice.restart_generation {
                        return Ok(());
                    }
                    if remote.ice.restart_generation == old.ice.restart_generation {
                        if remote != old {
                            self.trace(format!("drop {sender} announce-mutation"));
                        }
                        return Ok(());
                    }
                }
                if old.stream_generation != remote.stream_generation {
                    if peer.retired.len() >= 32 {
                        return Ok(());
                    }
                    peer.retired.push_back(old.stream_generation);
                }
                if !peer.waiting_reset && !peer.unconfirmed {
                    peer.ice.restart(now)?;
                    peer.local = self.description(&peer.ice);
                }
                peer.waiting_reset = false;
                peer.session = None;
                peer.retry_until = None;
                peer.deferred_rekey = false;
                peer.choice = Choice::default();
                for binding in peer.bindings.values() {
                    peer.ice.probes_mut().remove(binding.path());
                    if binding.bound {
                        peer.ice.probes_mut().register(binding.path(), now);
                    }
                }
            }
            let stream = StreamId {
                player: sender,
                generation: remote.stream_generation,
            };
            peer.ice.select_remote(stream, now)?;
            peer.ice.handle_signal(sender, signal, now)?;
            if peer.ice.probes().route(Path::Game).is_none() {
                peer.ice.probes_mut().register(Path::Game, now);
            }
            peer.remote = Some(remote.clone());
            self.out
                .push_back(PendingOutput::from(Output::Select(stream)));
            self.trace(format!("generation {sender} {}", stream.generation));
            self.signal(sender, &Signal::Announce(peer.local.clone()));
            if !self.config.game_only && remote.capabilities & signaling::CAP_UDP != 0 {
                peer.session = Some(Session::new(
                    self.player.unwrap(),
                    &peer.local,
                    self.key.private.clone(),
                    sender,
                    remote,
                )?);
                peer.next_retry = now;
            }
            return Ok(());
        }
        if matches!(signal, Signal::IceCandidates { .. }) {
            peer.ice.handle_signal(sender, signal, now)?;
        } else if matches!(
            signal,
            Signal::Noise1(_)
                | Signal::Noise2(_)
                | Signal::KeyConfirmation { .. }
                | Signal::RekeyRequest { .. }
        ) {
            let handle = self.handle()?;
            if let Some(session) = &mut peer.session {
                match session.on_signal(sender, signal, handle, now) {
                    Ok(actions) => {
                        // a KK message only verifies when both sides hold the same snapshots
                        peer.unconfirmed = false;
                        self.crypto_actions(sender, peer, actions, None, now)
                    }
                    Err(crypto::Error::PhaseInUse) => {
                        peer.next_retry = now + RETRY;
                        peer.deferred_rekey = true;
                    }
                    Err(crypto::Error::ResetRequired) => self.reset(sender, peer, now)?,
                    Err(_) => self.trace(format!("drop {sender} handshake")),
                }
            }
        }
        Ok(())
    }

    fn crypto_actions(
        &mut self,
        sender: u16,
        peer: &mut Peer,
        actions: crypto::Actions,
        arrival: Option<Path>,
        now: Instant,
    ) {
        if actions.activated {
            peer.retry_until = None;
            self.trace(format!("key {sender} activated"));
        } else if !actions.signals.is_empty() && peer.retry_until.is_none() {
            peer.retry_until = Some(now + HANDSHAKE_LIMIT);
        }
        for signal in actions.signals {
            self.signal(sender, &signal);
        }
        for datagram in actions.datagrams {
            if let Some(path) = arrival {
                self.send_udp(peer, path, &datagram, false);
            } else {
                if peer.ice.direct().is_some() {
                    self.send_udp(peer, Path::Direct, &datagram, false);
                }
                let paths: Vec<_> = peer
                    .bindings
                    .values()
                    .filter(|b| b.bound)
                    .map(Binding::path)
                    .collect();
                for path in paths {
                    self.send_udp(peer, path, &datagram, false);
                }
            }
        }
    }

    fn reset(&mut self, sender: u16, peer: &mut Peer, now: Instant) -> Result<()> {
        peer.session = None;
        peer.retry_until = None;
        peer.deferred_rekey = false;
        peer.waiting_reset = true;
        peer.choice = Choice::default();
        peer.routes.clear();
        peer.ice.restart(now)?;
        peer.local = self.description(&peer.ice);
        self.signal(sender, &Signal::Announce(peer.local.clone()));
        self.trace(format!("switch {sender} game reset-required"));
        Ok(())
    }

    fn send_udp(&mut self, peer: &Peer, path: Path, bytes: &[u8], local_media: bool) {
        let route = match path {
            Path::Direct => peer
                .ice
                .direct()
                .map(|r| (r.source, r.destination, bytes.to_vec())),
            Path::Hub { .. } | Path::PlayerRelay { .. } => peer
                .bindings
                .values()
                .find(|b| b.path() == path && b.bound)
                .and_then(|b| {
                    wire::encode_datagram(Datagram::Relay {
                        session: b.grant.session,
                        datagram: bytes,
                    })
                    .ok()
                    .map(|wrapped| (b.source, b.endpoint, wrapped))
                }),
            Path::Game => None,
        };
        if let Some((source, destination, contents)) = route {
            self.out.push_back(PendingOutput {
                output: Output::Udp(DatagramTx {
                    source,
                    destination,
                    contents,
                }),
                local_media,
            });
        }
    }

    fn probe(&mut self, sender: u16, peer: &mut Peer, event: ProbeEvent, now: Instant) {
        match event {
            ProbeEvent::SendPing { route, token } => {
                if route.path == Path::Game {
                    if peer.remote.is_some() {
                        self.signal(
                            sender,
                            &Signal::GameProbe {
                                probe: token,
                                reply: false,
                            },
                        );
                    }
                } else if let Some(session) = &mut peer.session
                    && let Ok(bytes) = session.encrypt(Inner {
                        kind: Kind::Ping,
                        sequence: 0,
                        talkspurt: 0,
                        body: &token.to_be_bytes(),
                    })
                {
                    self.send_udp(peer, route.path, &bytes, false);
                }
            }
            ProbeEvent::NetworkRtt { route, rtt, .. } => {
                self.trace(format!(
                    "network_rtt {sender} {:?} {} {}",
                    route.path,
                    route.revision,
                    rtt.as_micros()
                ));
            }
            ProbeEvent::Available { route } => self.trace(format!(
                "route {sender} {:?} {} available",
                route.path, route.revision
            )),
            ProbeEvent::Unavailable { route } => {
                self.trace(format!(
                    "route {sender} {:?} {} unavailable",
                    route.path, route.revision
                ));
                if let Some(binding) = peer
                    .bindings
                    .values_mut()
                    .find(|b| b.path() == route.path && b.bound)
                    && binding.new_attempt(self.player.unwrap(), now).is_ok()
                {
                    binding.deadline = now + HANDSHAKE_LIMIT;
                    peer.ice.probes_mut().remove(route.path);
                }
            }
            ProbeEvent::SendPong { .. } => {}
        }
    }

    fn grant(&mut self, sender: u16, grant: RelayGrant, now: Instant) -> Result<()> {
        if self.config.game_only {
            return Ok(());
        }
        let Some(local) = self.player else {
            return Ok(());
        };
        let now_ms = self.now_ms(now);
        if sender != grant.relay
            || grant.expires_at_ms <= now_ms
            || grant.sender == grant.recipient
            || grant.sender == grant.relay
            || grant.recipient == grant.relay
        {
            return Ok(());
        }
        let remote = if grant.sender == local {
            grant.recipient
        } else if grant.recipient == local {
            grant.sender
        } else {
            return Ok(());
        };
        let endpoint = if sender == 0 {
            self.config.hub
        } else {
            self.peers
                .get(&sender)
                .filter(|p| {
                    p.remote
                        .as_ref()
                        .is_some_and(|a| a.stream_generation == grant.hub_generation)
                })
                .and_then(|p| p.ice.direct())
                .map(|r| r.destination)
        };
        let Some(endpoint) = endpoint else {
            return Ok(());
        };
        if !self.peers.contains_key(&remote) {
            return Ok(());
        }
        if let Some((current, retired)) = self.relay_generations.get_mut(&sender) {
            if grant.hub_generation != *current {
                if retired.contains(&grant.hub_generation) || retired.len() >= 16 {
                    return Ok(());
                }
                retired.push(*current);
                *current = grant.hub_generation;
                for peer in self.peers.values_mut() {
                    let old: Vec<_> = peer
                        .bindings
                        .keys()
                        .filter(|(relay, _)| *relay == sender)
                        .copied()
                        .collect();
                    for key in old {
                        let binding = peer.bindings.remove(&key).unwrap();
                        peer.ice.probes_mut().remove(binding.path());
                    }
                }
            }
        } else {
            self.relay_generations
                .insert(sender, (grant.hub_generation, Vec::new()));
        }
        let Some(mut peer) = self.peers.remove(&remote) else {
            return Ok(());
        };
        let result = (|| -> Result<()> {
            let key = (sender, grant.session);
            if let Some(binding) = peer.bindings.get_mut(&key)
                && binding.grant == grant
                && !binding.bound
                && now >= binding.deadline
            {
                binding.new_attempt(local, now)?;
                binding.deadline = now + HANDSHAKE_LIMIT;
            }
            if peer.bindings.len() < 16
                && !peer.bindings.contains_key(&key)
                && let Some(&source) = self
                    .sockets
                    .iter()
                    .find(|s| s.is_ipv4() == endpoint.is_ipv4())
            {
                let client =
                    BindClient::new(grant.clone(), local, 1).ok_or("bind random source")?;
                let packet = client.request();
                peer.bindings.insert(
                    key,
                    Binding {
                        grant: grant.clone(),
                        source,
                        endpoint,
                        client,
                        attempt: 1,
                        started: now,
                        deadline: now + HANDSHAKE_LIMIT,
                        next: now,
                        packet,
                        bound: false,
                    },
                );
                if sender == 0 {
                    peer.ice.discover_hub(
                        HubReflection {
                            endpoint,
                            session: grant.session,
                            hub_generation: grant.hub_generation,
                        },
                        now,
                    )?;
                }
            }
            Ok(())
        })();
        self.peers.insert(remote, peer);
        result
    }

    pub fn udp(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
        now: Instant,
    ) -> Result<()> {
        let now_ms = self.now_ms(now);
        if let Some(relay) = &mut self.relay
            && relay.claims(source, bytes)
        {
            if let Ok(reply) = relay.handle(now_ms, source, bytes) {
                let (to, contents) = match reply {
                    Reply::Send(to, contents) => (to, contents),
                    Reply::Forward(to) => (to, bytes.to_vec()),
                };
                self.out
                    .push_back(PendingOutput::from(Output::Udp(DatagramTx {
                        source: destination,
                        destination: to,
                        contents,
                    })));
            }
            return Ok(());
        }
        let ids: Vec<_> = self.peers.keys().copied().collect();
        for sender in ids {
            let mut peer = self.peers.remove(&sender).unwrap();
            let result = self.peer_udp(sender, &mut peer, source, destination, bytes, now);
            self.peers.insert(sender, peer);
            if result? {
                break;
            }
        }
        Ok(())
    }

    fn peer_udp(
        &mut self,
        sender: u16,
        peer: &mut Peer,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
        now: Instant,
    ) -> Result<bool> {
        let now_ms = self.now_ms(now);
        let (path, encrypted) = match wire::decode_datagram(bytes) {
            Ok(Datagram::Bind(Bind::Challenge(challenge)))
            | Ok(Datagram::BindAck { challenge, .. }) => {
                let key = peer
                    .bindings
                    .iter()
                    .find(|(_, b)| {
                        b.endpoint == source
                            && b.source == destination
                            && b.grant.session == challenge.request.session
                            && b.grant.hub_generation == challenge.request.hub_generation
                    })
                    .map(|(k, _)| *k);
                let Some(key) = key else {
                    return Ok(false);
                };
                let binding = peer.bindings.get_mut(&key).unwrap();
                if now_ms >= challenge.expires_at_ms || now_ms >= binding.grant.expires_at_ms {
                    return Ok(true);
                }
                match wire::decode_datagram(bytes)? {
                    Datagram::Bind(Bind::Challenge(c)) => {
                        if let Some(packet) = binding.client.on_challenge(c) {
                            binding.packet = packet.clone();
                            binding.next = now + RETRY;
                            self.out
                                .push_back(PendingOutput::from(Output::Udp(DatagramTx {
                                    source: destination,
                                    destination: source,
                                    contents: packet,
                                })));
                        }
                    }
                    Datagram::BindAck { challenge, proof }
                        if binding.client.on_ack(challenge, &proof).is_some() && !binding.bound =>
                    {
                        binding.bound = true;
                        let path = binding.path();
                        peer.ice.probes_mut().register(path, now);
                        if key.0 == 0 {
                            peer.ice.authenticated_hub_reflection(
                                destination,
                                source,
                                challenge,
                                now,
                            )?;
                        }
                    }
                    _ => {}
                }
                return Ok(true);
            }
            Ok(Datagram::Relay { session, datagram }) => {
                let Some(binding) = peer.bindings.values().find(|b| {
                    b.bound
                        && b.grant.session == session
                        && b.endpoint == source
                        && b.source == destination
                        && b.grant.expires_at_ms > now_ms
                }) else {
                    return Ok(false);
                };
                (binding.path(), datagram)
            }
            _ => match peer.ice.receive(source, destination, bytes, now)? {
                ReceiveResult::UntrustedApplication(route) => (route.id.path, bytes),
                ReceiveResult::Dropped => return Ok(false),
                _ => return Ok(true),
            },
        };
        let Some(session) = &mut peer.session else {
            return Ok(false);
        };
        let reception = match session.receive(encrypted, now) {
            Ok(reception) => reception,
            Err(crypto::Error::WrongReceiver | crypto::Error::Crypto) => return Ok(false),
            Err(crypto::Error::Replay) => {
                self.trace(format!("duplicate {sender} {:?} encrypted", path));
                return Ok(true);
            }
            Err(_) => {
                self.trace(format!("drop {sender} crypto"));
                return Ok(true);
            }
        };
        self.crypto_actions(sender, peer, reception.actions, Some(path), now);
        let packet = reception.packet;
        if packet.kind == Kind::Pong {
            if let Some(route) = peer.ice.probes().route(path) {
                let token = u64::from_be_bytes(packet.body.as_slice().try_into().unwrap());
                for event in peer.ice.probes_mut().authenticated_pong(route, token, now) {
                    self.probe(sender, peer, event, now);
                }
            }
        } else if packet.kind != Kind::Ping {
            let revision = if matches!(
                packet.kind,
                Kind::PolicyVoice | Kind::PolicyStart | Kind::PolicyStop
            ) {
                Some(u64::from_be_bytes(packet.body[..8].try_into().unwrap()))
            } else {
                None
            };
            if self.accepts_media(sender, revision) {
                self.out
                    .push_back(PendingOutput::from(Output::UdpMedia(Media {
                        stream: packet.context.sender,
                        kind: ordinary_kind(packet.kind),
                        sequence: packet.sequence,
                        talkspurt: packet.talkspurt,
                        opus: if revision.is_some() {
                            packet.body[8..].to_vec()
                        } else {
                            packet.body
                        },
                        revision,
                    })));
            } else {
                self.trace(format!("drop {sender} audience-udp"));
            }
        }
        Ok(true)
    }

    pub fn transmit(&mut self, packet: &[u8], now: Instant) {
        if packet.len() < 5 || !self.capture_policy().0 {
            return;
        }
        let kind = match packet[2] {
            packet::START => Kind::Start,
            packet::VOICE => Kind::Voice,
            packet::STOP => Kind::Stop,
            _ => return,
        };
        self.sequence = u16::from_be_bytes([packet[3], packet[4]]);
        if kind == Kind::Start {
            self.talkspurt = self.talkspurt.wrapping_add(1);
            self.on_air = true;
            self.next_start = Some(now + REFRESH);
        }
        self.send_media(kind, &packet[5..]);
        if kind == Kind::Stop {
            self.on_air = false;
            self.next_start = None;
        }
    }

    fn send_media(&mut self, kind: Kind, opus: &[u8]) {
        let (allowed, revision, _) = self.capture_policy();
        if !allowed {
            return;
        }
        let kind = if revision.is_some() {
            policy_kind(kind)
        } else {
            kind
        };
        let mut policy_body = Vec::new();
        if let Some(revision) = revision {
            policy_body.extend_from_slice(&revision.to_be_bytes());
            policy_body.extend_from_slice(opus);
        }
        let body = if revision.is_some() {
            &policy_body[..]
        } else {
            opus
        };
        let player = self.player.unwrap_or(0);
        let listeners = self
            .policy
            .as_ref()
            .and_then(|p| p.current())
            .map(|p| &p.audience)
            .or(self.audience.as_ref())
            .map_or_else(Vec::new, |a| a.send_to.clone());
        for listener in listeners {
            if listener == player {
                continue;
            }
            let Some(peer) = self.peers.get(&listener) else {
                continue;
            };
            let primary = peer.choice.active.unwrap_or(Path::Game);
            let paths: Vec<_> = [Some(primary), peer.backup]
                .into_iter()
                .flatten()
                .filter(|p| *p != Path::Game)
                .collect();
            if paths.is_empty() {
                continue;
            }
            let mut peer = self.peers.remove(&listener).unwrap();
            if let Some(session) = &mut peer.session {
                match session.encrypt(Inner {
                    kind,
                    sequence: self.sequence,
                    talkspurt: self.talkspurt,
                    body,
                }) {
                    Ok(bytes) => {
                        for path in paths {
                            self.send_udp(&peer, path, &bytes, true);
                            self.trace(format!(
                                "route_tx {listener} {:?} {} {}",
                                path, self.sequence, self.talkspurt
                            ));
                        }
                    }
                    Err(_) => self.trace(format!("drop {listener} udp-encryption")),
                }
            }
            self.peers.insert(listener, peer);
        }
        let media = GameMedia {
            stream: StreamId {
                player,
                generation: self.generation,
            },
            kind,
            sequence: self.sequence,
            talkspurt: self.talkspurt,
            opus,
        };
        let encoded = match revision {
            Some(revision) => {
                wire::encode_policy_game(wire::PolicyGameMedia { media, revision }, false)
            }
            None => wire::encode_game(media, false),
        };
        match encoded {
            Ok(bytes) => {
                self.out.push_back(PendingOutput {
                    output: Output::Game(bytes),
                    local_media: true,
                });
                self.trace(format!(
                    "route_tx 0 Game {} {}",
                    self.sequence, self.talkspurt
                ));
            }
            Err(_) => self.trace(format!("drop {player} game-encoding")),
        }
    }

    pub fn tick(&mut self, now: Instant, talking: bool) -> Result<()> {
        if self.on_air && self.next_start.is_some_and(|at| now >= at) {
            self.send_media(Kind::Start, &[]);
            self.next_start = Some(now + REFRESH);
        }
        let ids: Vec<_> = self.peers.keys().copied().collect();
        let mut failure = None;
        for sender in ids {
            let mut peer = self.peers.remove(&sender).unwrap();
            let result = self.tick_peer(sender, &mut peer, now, talking);
            self.peers.insert(sender, peer);
            if let Err(err) = result {
                self.trace(format!("drop {sender} peer-tick"));
                if failure.is_none() {
                    failure = Some(err);
                }
            }
        }
        self.player_relays(now);
        // the refresh period starts at the first announcement, not at a tick with no peers yet
        if self.next_hub_announce.is_none_or(|at| now >= at)
            && let Some(description) = self.peers.values().next().map(|p| p.local.clone())
        {
            self.signal(0, &Signal::Announce(description));
            self.next_hub_announce = Some(now + Duration::from_secs(5));
        }
        failure.map_or(Ok(()), Err)
    }

    fn tick_peer(
        &mut self,
        sender: u16,
        peer: &mut Peer,
        now: Instant,
        talking: bool,
    ) -> Result<()> {
        peer.ice.handle_timeout(now)?;
        while let Some(action) = peer.ice.poll_action() {
            match action {
                Action::Send(tx) | Action::Discovery(DiscoveryAction::Send(tx))
                    if !self.config.game_only =>
                {
                    self.out.push_back(PendingOutput::from(Output::Udp(tx)))
                }
                Action::Discovery(DiscoveryAction::BeginHubBinding {
                    source,
                    hub,
                    request,
                }) => {
                    if let Some(binding) = peer.bindings.get_mut(&(0, request.session))
                        && binding.source == source
                        && binding.endpoint == hub
                        && let Some(request) = peer.ice.hub_binding_request(
                            source,
                            hub,
                            request,
                            binding
                                .attempt
                                .checked_add(1)
                                .ok_or("bind attempts exhausted")?,
                        )
                        && let Some(client) =
                            BindClient::from_request(binding.grant.clone(), request)
                    {
                        binding.client = client;
                        binding.packet = binding.client.request();
                        binding.attempt = request.attempt;
                        binding.bound = false;
                        binding.started = now;
                        binding.deadline = now + HANDSHAKE_LIMIT;
                        binding.next = now;
                    }
                }
                Action::PublishDescription => {
                    self.signal(sender, &Signal::Announce(peer.local.clone()))
                }
                Action::PublishCandidates => {
                    for signal in peer.ice.candidate_signals()? {
                        self.signal(sender, &signal);
                    }
                }
                Action::Probe(event) => self.probe(sender, peer, event, now),
                Action::DirectNominated(route) => self.trace(format!(
                    "route {sender} Direct {} nominated",
                    route.id.revision
                )),
                Action::DirectInvalidated(route) => self.trace(format!(
                    "route {sender} Direct {} invalidated",
                    route.revision
                )),
                _ => {}
            }
        }
        let now_ms = self.now_ms(now);
        let expired: Vec<_> = peer
            .bindings
            .iter()
            .filter(|(_, b)| now_ms >= b.grant.expires_at_ms)
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            let b = peer.bindings.remove(&key).unwrap();
            peer.ice.probes_mut().remove(b.path());
        }
        for binding in peer
            .bindings
            .values_mut()
            .filter(|b| !b.bound && now >= b.next && now < b.deadline)
        {
            if now.saturating_duration_since(binding.started) >= Duration::from_millis(CHALLENGE_MS)
            {
                binding.new_attempt(self.player.unwrap(), now)?;
            }
            self.out
                .push_back(PendingOutput::from(Output::Udp(DatagramTx {
                    source: binding.source,
                    destination: binding.endpoint,
                    contents: binding.packet.clone(),
                })));
            binding.next = now + RETRY;
        }
        if peer.retry_until.is_some_and(|at| now >= at) {
            peer.retry_until = None;
            if let Some(session) = &mut peer.session
                && matches!(session.abort_pending(), Err(crypto::Error::ResetRequired))
            {
                self.reset(sender, peer, now)?;
            }
            peer.next_retry = now + Duration::from_secs(1);
        } else if now >= peer.next_retry && !peer.waiting_reset && !self.config.game_only {
            peer.next_retry = now + RETRY;
            let handle = self.handle()?;
            if let Some(session) = &mut peer.session {
                session.expire(now);
                let actions = if peer.retry_until.is_some() {
                    session.retry()
                } else if session.active_epoch().is_none() && self.player.unwrap() < sender {
                    session.begin(
                        session
                            .generation()
                            .checked_add(1)
                            .ok_or("session generation exhausted")?,
                        handle,
                        now,
                    )
                } else if session.needs_rekey() || peer.deferred_rekey {
                    session.request_rekey(handle, now)
                } else {
                    Ok(crypto::Actions::default())
                };
                match actions {
                    Ok(actions) => {
                        peer.deferred_rekey = false;
                        self.crypto_actions(sender, peer, actions, None, now);
                    }
                    Err(crypto::Error::PhaseInUse) => {}
                    Err(crypto::Error::ResetRequired) => self.reset(sender, peer, now)?,
                    Err(_) => self.trace(format!("drop {sender} handshake-retry")),
                }
            }
        }
        let mut paths = vec![Path::Game, Path::Direct];
        paths.extend(
            peer.bindings
                .values()
                .filter(|b| b.bound)
                .map(Binding::path),
        );
        peer.routes = paths
            .into_iter()
            .filter_map(|path| {
                let route = peer.ice.probes().route(path)?;
                if !peer.ice.probes().is_live(route) {
                    return None;
                }
                if path != Path::Game
                    && peer
                        .session
                        .as_ref()
                        .is_none_or(|s| s.active_epoch().is_none())
                {
                    return None;
                }
                Some((path, peer.ice.probes().rtt(route)?))
            })
            .collect();
        peer.routes.sort_by_key(|(p, rtt)| (*rtt, rank(*p)));
        let previous = peer.choice.active;
        let (active, backup) = peer.choice.update(&peer.routes, now);
        peer.backup = backup;
        if previous != Some(active) {
            self.trace(format!("switch {sender} {:?} selected", active));
        }
        peer.ice.probes_mut().set_activity(active, talking, now);
        let primary_rtt = peer
            .routes
            .iter()
            .find(|r| r.0 == active)
            .map_or(Duration::ZERO, |r| r.1);
        let secondary_rtt = peer
            .routes
            .iter()
            .find(|r| Some(r.0) == backup)
            .map_or(primary_rtt, |r| r.1);
        let wait = secondary_rtt.saturating_sub(primary_rtt) / 2 + Duration::from_millis(10);
        let wait = wait.min(Duration::from_millis(200));
        if peer.reorder_wait != Some(wait) {
            peer.reorder_wait = Some(wait);
            self.out
                .push_back(PendingOutput::from(Output::Reorder(sender, wait)));
        }
        Ok(())
    }

    fn player_relays(&mut self, now: Instant) {
        if self.config.game_only {
            return;
        }
        if self.next_relay_check.is_some_and(|at| now < at) {
            return;
        }
        self.next_relay_check = Some(now + Duration::from_millis(250));
        let publish = self.next_offer_publish.is_none_or(|at| now >= at);
        if publish {
            self.next_offer_publish = Some(now + Duration::from_secs(5));
        }
        let now_ms = self.now_ms(now);
        let eligible: Vec<_> = self
            .peers
            .iter()
            .filter_map(|(&p, peer)| {
                let route = peer.ice.probes().route(Path::Direct)?;
                peer.ice
                    .probes()
                    .is_live(route)
                    .then(|| (p, peer.remote.as_ref().unwrap().stream_generation))
            })
            .collect();
        let Some(relay) = &mut self.relay else {
            return;
        };
        relay.expire(now_ms);
        let live: BTreeMap<_, _> = relay.live(now_ms).map(|g| (g.session, g.clone())).collect();
        self.offers
            .retain(|_, (session, _, _)| live.contains_key(session));
        let stale: Vec<_> = self
            .offers
            .iter()
            .filter(|((a, b), (_, ag, bg))| {
                !eligible.contains(&(*a, *ag)) || !eligible.contains(&(*b, *bg))
            })
            .map(|(pair, entry)| (*pair, entry.0))
            .collect();
        for (pair, session) in stale {
            self.out.extend(
                relay
                    .withdraw(session)
                    .into_iter()
                    .map(|f| PendingOutput::from(Output::Game(f.frame))),
            );
            self.offers.remove(&pair);
        }
        for (i, &(a, ag)) in eligible.iter().enumerate() {
            for &(b, bg) in &eligible[i + 1..] {
                let grant = if let Some((session, _, _)) = self.offers.get(&(a, b)) {
                    let current = &live[session];
                    if current.expires_at_ms.saturating_sub(now_ms) >= relay.limits().grant_ms / 4 {
                        continue;
                    }
                    relay.renew(*session, now_ms)
                } else {
                    relay.issue(a, b, now_ms)
                };
                if let Ok(grant) = grant {
                    self.offers.insert((a, b), (grant.session, ag, bg));
                    self.out.extend(
                        grant_frames(grant)
                            .into_iter()
                            .map(|f| PendingOutput::from(Output::Game(f.frame))),
                    );
                }
            }
        }
        if publish {
            for (session, _, _) in self.offers.values() {
                if let Some(grant) = relay.live(now_ms).find(|g| g.session == *session) {
                    self.out.extend(
                        grant_frames(grant.clone())
                            .into_iter()
                            .map(|f| PendingOutput::from(Output::Game(f.frame))),
                    );
                }
            }
        }
    }
}
