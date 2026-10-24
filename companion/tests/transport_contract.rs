use ptt_radio_companion::packet;
use ptt_radio_companion::transport::signaling::{self as signal, *};
use ptt_radio_companion::transport::wire::{self, *};
use ptt_radio_companion::transport::{Path, StreamId, extend_sequence};

fn media<'a>(kind: Kind, opus: &'a [u8]) -> GameMedia<'a> {
    GameMedia {
        kind,
        stream: StreamId {
            player: 513,
            generation: 0x0102030405060708,
        },
        sequence: 65535,
        talkspurt: 42,
        opus,
    }
}

fn header(kind: Kind) -> Header {
    Header {
        kind,
        phase: true,
        receiver: ReceiverHandle(0x01020304),
        counter: 0x05060708,
    }
}

#[test]
fn exact_big_endian_game_layout_and_legacy_isolation() {
    let voice = media(Kind::Voice, &[9, 8]);
    let own = wire::encode_game(voice, false).unwrap();
    assert_eq!(
        own,
        [
            0x52, 1, GAME_VOICE, 255, 255, 1, 2, 3, 4, 5, 6, 7, 8, 0, 42, 9, 8
        ]
    );
    let forwarded = wire::encode_game(voice, true).unwrap();
    let mut mod_fanout = vec![packet::MAGIC, packet::VERSION, own[2], 2, 1];
    mod_fanout.extend_from_slice(&own[3..]);
    assert_eq!(forwarded, mod_fanout);
    assert_eq!(wire::decode_game(&forwarded).unwrap(), voice);
    assert!(packet::parse(&forwarded).is_none());
    assert_eq!(extend_sequence(voice.sequence, 65536), Some(65535));
    // legacy START/STOP ignore tails; legacy VOICE would include the added bytes.
    let mut legacy_start = forwarded.clone();
    legacy_start[2] = packet::START;
    assert!(matches!(
        packet::parse(&legacy_start),
        Some(packet::Incoming::Start { .. })
    ));
    let mut legacy_voice = forwarded;
    legacy_voice[2] = packet::VOICE;
    let Some(packet::Incoming::Voice { frame, .. }) = packet::parse(&legacy_voice) else {
        panic!("legacy voice")
    };
    assert_eq!(frame.len(), 12);
    assert_eq!(
        wire::decode_game(&[0x52, 1, packet::STOP, 2, 1, 0, 0]),
        Err(CodecError::UnknownType)
    );
}

#[test]
fn inner_game_and_udp_roundtrips_share_logical_media_fields() {
    for (kind, body) in [
        (Kind::Voice, &[9, 8][..]),
        (Kind::Start, &[][..]),
        (Kind::Stop, &[][..]),
        (Kind::Ping, &[0; 8][..]),
        (Kind::Pong, &[1; 8][..]),
    ] {
        let inner = Inner {
            kind,
            sequence: 65535,
            talkspurt: 42,
            body,
        };
        let encoded = encode_inner(inner).unwrap();
        assert_eq!(decode_inner(&encoded, kind).unwrap(), inner);
        let mut ciphertext = encoded;
        ciphertext.extend_from_slice(&[0; TAG]);
        let udp = encode_datagram(Datagram::Encrypted {
            header: header(kind),
            ciphertext: &ciphertext,
        })
        .unwrap();
        assert_eq!(&udp[..UDP_HEADER], &[kind as u8, 1, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            decode_datagram(&udp).unwrap(),
            Datagram::Encrypted {
                header: header(kind),
                ciphertext: &ciphertext
            }
        );
        let relay = encode_datagram(Datagram::Relay {
            session: 0x01020304,
            datagram: &udp,
        })
        .unwrap();
        assert_eq!(&relay[..5], &[0x10, 1, 2, 3, 4]);
        assert_eq!(
            decode_datagram(&relay).unwrap(),
            Datagram::Relay {
                session: 0x01020304,
                datagram: &udp
            }
        );
        if matches!(kind, Kind::Voice | Kind::Start | Kind::Stop) {
            let game = media(kind, body);
            assert_eq!(
                wire::decode_game(&wire::encode_game(game, true).unwrap()).unwrap(),
                game
            );
            assert_eq!(
                (game.sequence, game.talkspurt, game.opus),
                (inner.sequence, inner.talkspurt, inner.body)
            );
        }
    }
    let context = SessionContext {
        sender: media(Kind::Start, &[]).stream,
        receiver: StreamId {
            player: 2,
            generation: 99,
        },
        key_epoch: 3,
        announce_hash: [0; 32],
    };
    assert_eq!(context.nonce(7), (3u64 << 32) | 7);
}

fn challenge(endpoint: &str) -> BindChallenge {
    BindChallenge {
        request: BindRequest {
            session: 1,
            hub_generation: 2,
            player: 2,
            attempt: 3,
            client_nonce: [4; 32],
        },
        endpoint: endpoint.parse().unwrap(),
        challenge: [5; 32],
        expires_at_ms: 6,
    }
}

#[test]
fn bind_stages_roundtrip_and_mac_input_covers_freshness_endpoint_and_player() {
    let grant = relay_grant();
    for endpoint in ["127.0.0.1:1234", "[::1]:5678", "[fe80::1%3]:5678"] {
        let c = challenge(endpoint);
        let packets = [
            Datagram::Bind(Bind::Request(c.request)),
            Datagram::Bind(Bind::Challenge(c)),
            Datagram::Bind(Bind::Commit {
                challenge: c,
                proof: [7; 32],
            }),
            Datagram::BindAck {
                challenge: c,
                proof: [8; 32],
            },
        ];
        for packet in packets {
            let bytes = encode_datagram(packet).unwrap();
            assert_eq!(decode_datagram(&bytes).unwrap(), packet);
            for n in 0..bytes.len() {
                assert!(decode_datagram(&bytes[..n]).is_err(), "prefix {n}");
            }
            let mut trailing = bytes;
            trailing.push(0);
            assert_eq!(decode_datagram(&trailing), Err(CodecError::Trailing));
        }
        let transcript = bind_mac_input(&grant, c, false);
        assert_ne!(transcript, bind_mac_input(&grant, c, true));
        for changed in [
            BindChallenge {
                expires_at_ms: 7,
                ..c
            },
            BindChallenge {
                challenge: [6; 32],
                ..c
            },
            BindChallenge {
                endpoint: "127.0.0.2:1234".parse().unwrap(),
                ..c
            },
            BindChallenge {
                request: BindRequest {
                    attempt: 4,
                    ..c.request
                },
                ..c
            },
            BindChallenge {
                request: BindRequest {
                    player: 3,
                    ..c.request
                },
                ..c
            },
            BindChallenge {
                request: BindRequest {
                    session: 2,
                    ..c.request
                },
                ..c
            },
            BindChallenge {
                request: BindRequest {
                    hub_generation: 3,
                    ..c.request
                },
                ..c
            },
            BindChallenge {
                request: BindRequest {
                    client_nonce: [9; 32],
                    ..c.request
                },
                ..c
            },
        ] {
            assert_ne!(transcript, bind_mac_input(&grant, changed, false));
        }
    }
}

#[test]
fn strict_media_bounds_unknown_reserved_and_malformed_packets() {
    assert_eq!(MEDIA_OVERHEAD, 31);
    assert_eq!(RELAY_MEDIA_OVERHEAD, 36);
    let opus = vec![9; MAX_OPUS];
    let mut ciphertext = encode_inner(Inner {
        kind: Kind::Voice,
        sequence: 0,
        talkspurt: 1,
        body: &opus,
    })
    .unwrap();
    ciphertext.extend_from_slice(&[0; TAG]);
    let udp = encode_datagram(Datagram::Encrypted {
        header: header(Kind::Voice),
        ciphertext: &ciphertext,
    })
    .unwrap();
    let relay = encode_datagram(Datagram::Relay {
        session: 1,
        datagram: &udp,
    })
    .unwrap();
    assert_eq!(relay.len(), MAX_DATAGRAM);
    assert!(decode_datagram(&relay).is_ok());
    let mut extra = udp.clone();
    extra.push(0);
    assert_eq!(
        encode_datagram(Datagram::Relay {
            session: 1,
            datagram: &extra
        }),
        Err(CodecError::Oversize)
    );
    let mut flags = udp.clone();
    flags[1] = 2;
    assert_eq!(decode_datagram(&flags), Err(CodecError::Invalid));
    assert_eq!(decode_datagram(&[255]), Err(CodecError::UnknownType));
    assert_eq!(
        decode_datagram(&vec![0; MAX_DATAGRAM + 1]),
        Err(CodecError::Oversize)
    );
    assert_eq!(
        decode_datagram(&[0x10, 0, 0, 0, 1, 0x10]),
        Err(CodecError::Invalid)
    );
    for n in 0..UDP_HEADER + INNER_HEADER + TAG {
        assert!(decode_datagram(&udp[..n]).is_err());
    }
    assert_eq!(
        decode_inner(&[99, 0, 0, 0, 0], Kind::Voice),
        Err(CodecError::UnknownType)
    );
    assert_eq!(
        decode_inner(&[5, 0, 0, 0, 0], Kind::Voice),
        Err(CodecError::Invalid)
    );
    assert!(decode_inner(&[4, 0, 0, 0, 0], Kind::Voice).is_err());
    assert!(decode_inner(&[5, 0, 0, 0, 0, 1], Kind::Start).is_err());
    assert!(decode_inner(&[7, 0, 0, 0, 0], Kind::Ping).is_err());
    let start = wire::encode_game(media(Kind::Start, &[]), true).unwrap();
    for n in 0..start.len() {
        assert!(wire::decode_game(&start[..n]).is_err());
    }
    let mut trailing = start;
    trailing.push(0);
    assert!(wire::decode_game(&trailing).is_err());
    assert!(
        encode_inner(Inner {
            kind: Kind::Voice,
            sequence: 0,
            talkspurt: 0,
            body: &vec![0; MAX_OPUS + 1]
        })
        .is_err()
    );
}

fn announce() -> Announce {
    Announce {
        version: PROTOCOL_VERSION,
        capabilities: CAP_MEDIA_V2 | CAP_UDP,
        suites: vec![Suite::Aes256Gcm, Suite::ChaChaPoly],
        stream_generation: 0x0102030405060708,
        static_key: [7; 32],
        ice: IceCredentials {
            restart_generation: 1,
            ufrag: "test-ufrag".into(),
            password: "test-only-password".into(),
        },
        candidates: vec!["candidate:1 1 UDP 2122260223 192.0.2.1 1234 typ host".into()],
        candidates_complete: true,
    }
}

#[test]
fn signaling_roundtrips_every_kind_and_all_path_variants() {
    let handshake = Handshake {
        attempt: 0x01020304,
        session_generation: 0x0102030405060708,
        receive_handle: ReceiverHandle(0x090a0b0c),
        noise: vec![7; 48],
    };
    let mut signals = vec![
        Signal::Announce(announce()),
        Signal::Noise1(handshake.clone()),
        Signal::Noise2(handshake),
        Signal::KeyConfirmation {
            attempt: 1,
            session_generation: 2,
            token: 3,
        },
        Signal::RelayGrant(relay_grant()),
        Signal::IceCandidates {
            stream_generation: 0x0102030405060708,
            restart_generation: 3,
            candidates: vec!["candidate:2 1 UDP 1 2001:db8::1 1234 typ host".into()],
            complete: false,
        },
        Signal::RelayClose {
            relay: 1,
            session: 2,
            hub_generation: 3,
        },
        Signal::RekeyRequest {
            session_generation: 0x0102030405060708,
        },
    ];
    for path in [
        Path::Game,
        Path::Direct,
        Path::Hub { session: 1 },
        Path::PlayerRelay {
            player: 3,
            session: 4,
        },
    ] {
        signals.push(Signal::Rtt {
            probe: 7,
            path,
            rtt_us: 1234,
        });
    }
    for signal in signals {
        let bytes = signal::encode(&signal).unwrap();
        assert!(signal::decode(&bytes).unwrap() == signal);
        for n in 0..bytes.len() {
            assert!(
                signal::decode(&bytes[..n]).is_err(),
                "prefix {n} kind {}",
                bytes[1]
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(
            signal::decode(&trailing),
            Err(CodecError::Trailing)
        ));
        for inbound in [false, true] {
            let game = signal::encode_game(&signal, 513, inbound).unwrap();
            assert_eq!(
                &game[..5],
                &[0x52, 1, if inbound { SIGNAL_IN } else { SIGNAL_OUT }, 2, 1]
            );
            let (peer, decoded) = signal::decode_game(&game, inbound).unwrap();
            assert_eq!(peer, 513);
            assert!(decoded == signal);
            assert!(signal::decode_game(&game, !inbound).is_err());
        }
    }
    let noise = signal::encode(&Signal::Noise1(Handshake {
        attempt: 0x01020304,
        session_generation: 0x0102030405060708,
        receive_handle: ReceiverHandle(0x090a0b0c),
        noise: vec![9],
    }))
    .unwrap();
    assert_eq!(
        noise,
        [
            2, 2, 1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0, 1, 9
        ]
    );
}

#[test]
fn signaling_rejects_bounds_versions_unknown_kinds_and_noncanonical_sets() {
    assert!(matches!(
        signal::decode(&[2, 255]),
        Err(CodecError::UnknownType)
    ));
    assert!(matches!(
        signal::decode(&[1, 1]),
        Err(CodecError::UnsupportedVersion)
    ));
    assert!(matches!(
        signal::decode(&vec![0; 1201]),
        Err(CodecError::Oversize)
    ));
    assert!(
        signal::encode_audience(&Audience {
            revision: 1,
            send_to: vec![2, 1],
            receive_from: vec![]
        })
        .is_err()
    );
    assert!(
        signal::encode_audience(&Audience {
            revision: 1,
            send_to: vec![1, 1],
            receive_from: vec![]
        })
        .is_err()
    );
    let mut a = announce();
    a.ice.password = "x".repeat(MAX_CREDENTIAL_BYTES + 1);
    assert!(signal::encode(&Signal::Announce(a)).is_err());
    let mut a = announce();
    a.suites = vec![Suite::ChaChaPoly, Suite::ChaChaPoly];
    assert!(signal::encode(&Signal::Announce(a)).is_err());
    let mut a = announce();
    a.candidates = vec!["x".repeat(MAX_CANDIDATE_BYTES); MAX_CANDIDATES];
    assert!(matches!(
        signal::encode(&Signal::Announce(a)),
        Err(CodecError::Oversize)
    ));
    let mut bytes = signal::encode(&Signal::Announce(announce())).unwrap();
    bytes[9] = 255;
    assert!(matches!(
        signal::decode(&bytes),
        Err(CodecError::UnknownType)
    ));
    let oversized_noise = Signal::Noise1(Handshake {
        attempt: 0,
        session_generation: 0,
        receive_handle: ReceiverHandle(0),
        noise: vec![0; MAX_NOISE_BYTES + 1],
    });
    assert!(signal::encode(&oversized_noise).is_err());
    assert_eq!(
        select_suite(
            &[Suite::ChaChaPoly, Suite::Aes256Gcm],
            &[Suite::Aes256Gcm, Suite::ChaChaPoly]
        ),
        Some(Suite::Aes256Gcm)
    );
    assert_eq!(
        select_suite(&[Suite::Aes256Gcm], &[Suite::ChaChaPoly]),
        None
    );
    assert!(is_initiator(1, 2));
    assert!(!is_initiator(2, 1));
}

#[test]
fn malformed_signaling_fields_fail_before_becoming_runtime_state() {
    let mut audience = signal::encode_audience(&Audience {
        revision: 1,
        send_to: vec![1, 2],
        receive_from: vec![],
    })
    .unwrap();
    audience[15..19].copy_from_slice(&[0, 2, 0, 1]);
    assert!(matches!(
        signal::decode_audience(&audience),
        Err(CodecError::Invalid)
    ));
    audience[13..15].copy_from_slice(&257u16.to_be_bytes());
    assert!(matches!(
        signal::decode_audience(&audience),
        Err(CodecError::Oversize)
    ));

    let mut a = signal::encode(&Signal::Announce(announce())).unwrap();
    a[57] = 255;
    assert!(matches!(signal::decode(&a), Err(CodecError::Invalid)));
    a[55..57].copy_from_slice(&257u16.to_be_bytes());
    assert!(matches!(signal::decode(&a), Err(CodecError::Oversize)));

    assert!(matches!(
        signal::decode(&[2, 8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 2, 0]),
        Err(CodecError::Invalid)
    ));
    assert!(matches!(
        signal::decode(&[2, 8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 9]),
        Err(CodecError::Oversize)
    ));
    assert_eq!(decode_datagram(&[0x11, 3]), Err(CodecError::UnknownType));

    let mut noise = signal::encode(&Signal::Noise1(Handshake {
        attempt: 1,
        session_generation: 2,
        receive_handle: ReceiverHandle(3),
        noise: vec![4],
    }))
    .unwrap();
    noise[18..20].copy_from_slice(&1025u16.to_be_bytes());
    assert!(matches!(signal::decode(&noise), Err(CodecError::Oversize)));
}

fn relay_grant() -> RelayGrant {
    RelayGrant {
        session: 1,
        relay: 0,
        sender: 2,
        recipient: 3,
        hub_generation: 2,
        expires_at_ms: 5,
        secret: [6; 16],
    }
}

#[test]
fn bind_mac_input_has_one_public_field_order_and_separate_domains() {
    let grant = relay_grant();
    let c = challenge("127.0.0.1:1234");
    let mut expected = b"ptt-radio-v2 bind commit\0".to_vec();
    // grant public fields: session, relay, sender, recipient, hub generation, expiry.
    expected.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2, 0, 3]);
    expected.extend_from_slice(&2u64.to_be_bytes());
    expected.extend_from_slice(&5u64.to_be_bytes());
    // challenge: request, observed endpoint, relay nonce and expiry.
    expected.extend_from_slice(&1u32.to_be_bytes());
    expected.extend_from_slice(&2u64.to_be_bytes());
    expected.extend_from_slice(&2u16.to_be_bytes());
    expected.extend_from_slice(&3u64.to_be_bytes());
    expected.extend_from_slice(&[4; 32]);
    expected.extend_from_slice(&[4, 127, 0, 0, 1, 4, 210]);
    expected.extend_from_slice(&[5; 32]);
    expected.extend_from_slice(&6u64.to_be_bytes());
    let input = bind_mac_input(&grant, c, false);
    assert_eq!(input, expected);
    let ack = bind_mac_input(&grant, c, true);
    assert!(ack.starts_with(b"ptt-radio-v2 bind ack\0"));
    assert_eq!(
        &ack[b"ptt-radio-v2 bind ack\0".len()..],
        &input[b"ptt-radio-v2 bind commit\0".len()..]
    );
    for altered in [
        RelayGrant {
            session: 2,
            ..grant.clone()
        },
        RelayGrant {
            relay: 1,
            ..grant.clone()
        },
        RelayGrant {
            sender: 4,
            ..grant.clone()
        },
        RelayGrant {
            recipient: 4,
            ..grant.clone()
        },
        RelayGrant {
            hub_generation: 3,
            ..grant.clone()
        },
        RelayGrant {
            expires_at_ms: 6,
            ..grant.clone()
        },
    ] {
        assert_ne!(input, bind_mac_input(&altered, c, false));
    }
    assert_eq!(
        input,
        bind_mac_input(
            &RelayGrant {
                secret: [7; 16],
                ..grant
            },
            c,
            false
        )
    );
}

#[test]
fn audience_is_mod_game_state_and_cannot_be_injected_via_player_signal() {
    let audience = Audience {
        revision: 0x0102030405060708,
        send_to: vec![0, 1, 255],
        receive_from: vec![2, 513],
    };
    let encoded = signal::encode_audience(&audience).unwrap();
    assert_eq!(&encoded[..5], &[0x52, 1, AUDIENCE, 0, 0]);
    assert_eq!(signal::decode_audience(&encoded).unwrap(), audience);
    for n in 0..encoded.len() {
        assert!(signal::decode_audience(&encoded[..n]).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert_eq!(
        signal::decode_audience(&trailing),
        Err(CodecError::Trailing)
    );
    let mut forged_body = vec![2, 7];
    forged_body.extend_from_slice(&encoded[5..]);
    assert!(matches!(
        signal::decode(&forged_body),
        Err(CodecError::UnknownType)
    ));
    for peer in [0u16, 513] {
        for inbound in [false, true] {
            let mut forged = vec![0x52, 1, if inbound { SIGNAL_IN } else { SIGNAL_OUT }];
            forged.extend_from_slice(&peer.to_be_bytes());
            forged.extend_from_slice(&forged_body);
            assert!(matches!(
                signal::decode_game(&forged, inbound),
                Err(CodecError::UnknownType)
            ));
            assert_eq!(
                signal::decode_audience(&forged),
                Err(CodecError::UnknownType)
            );
            forged.truncate(5);
            forged.extend_from_slice(&encoded);
            assert!(signal::decode_game(&forged, inbound).is_err());
        }
    }
    assert!(matches!(
        signal::decode_game(&encoded, true),
        Err(CodecError::UnknownType)
    ));
    assert!(signal::decode_audience(&vec![0; MAX_DATAGRAM + 1]).is_err());
}

#[test]
fn media_types_leave_stun_range_and_empty_fields_are_invalid() {
    for first in 0..=3 {
        assert_eq!(decode_datagram(&[first]), Err(CodecError::UnknownType));
        assert_eq!(Kind::try_from(first), Err(CodecError::UnknownType));
    }
    assert_eq!(
        [
            Kind::Voice as u8,
            Kind::Start as u8,
            Kind::Stop as u8,
            Kind::Ping as u8,
            Kind::Pong as u8
        ],
        [4, 5, 6, 7, 8]
    );
    let mut a = announce();
    a.ice.ufrag.clear();
    assert!(matches!(
        signal::encode(&Signal::Announce(a)),
        Err(CodecError::Invalid)
    ));
    let mut encoded = signal::encode(&Signal::Announce(announce())).unwrap();
    encoded[55..57].copy_from_slice(&0u16.to_be_bytes());
    assert!(matches!(signal::decode(&encoded), Err(CodecError::Invalid)));
    let mut noise = signal::encode(&Signal::Noise1(Handshake {
        attempt: 1,
        session_generation: 2,
        receive_handle: ReceiverHandle(3),
        noise: vec![4],
    }))
    .unwrap();
    noise[18..20].copy_from_slice(&0u16.to_be_bytes());
    assert!(matches!(signal::decode(&noise), Err(CodecError::Invalid)));
}
