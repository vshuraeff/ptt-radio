use ptt_radio_companion::policy::{InstallError, PolicyGate, PolicyUpdate};
use ptt_radio_companion::transport::StreamId;
use ptt_radio_companion::transport::signaling::{self, Audience, Policy};
use ptt_radio_companion::transport::wire::{self, *};

fn policy(revision: u64, receive_from: &[u16]) -> Policy {
    Policy {
        audience: Audience {
            revision,
            send_to: vec![2],
            receive_from: receive_from.to_vec(),
        },
        tx_permitted: true,
        self_monitor: true,
    }
}

#[test]
fn policy_exact_vector_and_strict_mod_origin() {
    let policy = policy(7, &[0, 2]);
    let bytes = signaling::encode_policy(&policy).unwrap();
    assert_eq!(
        bytes,
        [
            0x52, 1, 0x15, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 3, 0, 1, 0, 2, 0, 2, 0, 0, 0, 2
        ]
    );
    assert_eq!(signaling::decode_policy(&bytes).unwrap(), policy);
    for n in 0..bytes.len() {
        assert!(signaling::decode_policy(&bytes[..n]).is_err(), "prefix {n}");
    }
    for (index, value, error) in [
        (0, 0, CodecError::Invalid),
        (1, 2, CodecError::UnsupportedVersion),
        (2, AUDIENCE, CodecError::UnknownType),
        (4, 1, CodecError::Invalid),
        (5, 2, CodecError::UnsupportedVersion),
        (14, 4, CodecError::Invalid),
        (14, 2, CodecError::Invalid),
    ] {
        let mut malformed = bytes.clone();
        malformed[index] = value;
        assert_eq!(signaling::decode_policy(&malformed), Err(error));
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        signaling::decode_policy(&trailing),
        Err(CodecError::Trailing)
    );
    assert_eq!(
        signaling::decode_audience(&bytes),
        Err(CodecError::UnknownType)
    );
    assert!(signaling::decode_game(&bytes, true).is_err());
    assert!(signaling::decode(&bytes[5..]).is_err());
}

#[test]
fn policy_sets_remain_canonical_bounded_and_independent_of_tx() {
    for players in [vec![2, 2], vec![2, 1], (0..257).collect()] {
        let mut invalid = policy(1, &[]);
        invalid.audience.send_to = players.clone();
        assert!(signaling::encode_policy(&invalid).is_err());
        invalid.audience.send_to.clear();
        invalid.audience.receive_from = players;
        assert!(signaling::encode_policy(&invalid).is_err());
    }
    let maximum = policy(1, &(0..256).collect::<Vec<_>>());
    assert_eq!(
        signaling::decode_policy(&signaling::encode_policy(&maximum).unwrap()).unwrap(),
        maximum
    );
    let mut invalid = signaling::encode_policy(&policy(1, &[0, 2])).unwrap();
    invalid[23..25].copy_from_slice(&0u16.to_be_bytes());
    assert_eq!(signaling::decode_policy(&invalid), Err(CodecError::Invalid));
    invalid[19..21].copy_from_slice(&257u16.to_be_bytes());
    assert_eq!(
        signaling::decode_policy(&invalid),
        Err(CodecError::Oversize)
    );
    let mut empty = policy(1, &[0]);
    empty.audience.send_to.clear();
    assert!(
        signaling::decode_policy(&signaling::encode_policy(&empty).unwrap())
            .unwrap()
            .tx_permitted
    );
    empty.tx_permitted = false;
    assert_eq!(signaling::encode_policy(&empty), Err(CodecError::Invalid));
    empty.self_monitor = false;
    assert_eq!(
        signaling::decode_policy(&signaling::encode_policy(&empty).unwrap()).unwrap(),
        empty
    );
}

fn game(kind: Kind, opus: &[u8]) -> PolicyGameMedia<'_> {
    PolicyGameMedia {
        media: GameMedia {
            kind,
            stream: StreamId {
                player: 2,
                generation: 3,
            },
            sequence: 4,
            talkspurt: 5,
            opus,
        },
        revision: 7,
    }
}

#[test]
fn policy_game_layout_retains_sender_sequence_generation_and_talkspurt() {
    for (kind, game_kind, opus) in [
        (Kind::PolicyStart, 11, &[][..]),
        (Kind::PolicyVoice, 12, &[99][..]),
        (Kind::PolicyStop, 13, &[][..]),
    ] {
        let packet = game(kind, opus);
        let outbound = wire::encode_policy_game(packet, false).unwrap();
        let mut expected = vec![
            0x52, 1, game_kind, 0, 4, 0, 0, 0, 0, 0, 0, 0, 3, 0, 5, 0, 0, 0, 0, 0, 0, 0, 7,
        ];
        expected.extend_from_slice(opus);
        assert_eq!(outbound, expected);
        let forwarded = wire::encode_policy_game(packet, true).unwrap();
        let mut fanout = outbound;
        fanout.splice(3..3, [0, 2]);
        assert_eq!(forwarded, fanout);
        assert_eq!(wire::decode_policy_game(&forwarded).unwrap(), packet);
        assert_eq!(wire::decode_game(&forwarded), Err(CodecError::UnknownType));
        for n in 0..forwarded.len() {
            assert!(
                wire::decode_policy_game(&forwarded[..n]).is_err(),
                "prefix {n} kind {game_kind}"
            );
        }
        if kind != Kind::PolicyVoice {
            let mut trailing = forwarded;
            trailing.push(0);
            assert!(wire::decode_policy_game(&trailing).is_err());
        }
    }
    assert!(wire::encode_policy_game(game(Kind::Voice, &[1]), true).is_err());
    assert!(wire::encode_policy_game(game(Kind::PolicyVoice, &[]), true).is_err());
    assert!(wire::encode_policy_game(game(Kind::PolicyStart, &[1]), true).is_err());
}

#[test]
fn policy_udp_authenticated_kinds_and_relay_envelopes() {
    for (kind, opus) in [
        (Kind::PolicyVoice, &[99][..]),
        (Kind::PolicyStart, &[][..]),
        (Kind::PolicyStop, &[][..]),
    ] {
        let media = PolicyInner {
            kind,
            sequence: 4,
            talkspurt: 5,
            revision: 7,
            opus,
        };
        let inner = encode_policy_inner(media).unwrap();
        let mut expected = vec![kind as u8, 0, 4, 0, 5, 0, 0, 0, 0, 0, 0, 0, 7];
        expected.extend_from_slice(opus);
        assert_eq!(inner, expected);
        assert_eq!(decode_policy_inner(&inner, kind).unwrap(), media);
        assert!(decode_policy_inner(&inner, Kind::Voice).is_err());
        let mut ciphertext = inner;
        ciphertext.extend_from_slice(&[0; TAG]);
        let header = Header {
            kind,
            phase: false,
            receiver: ReceiverHandle(1),
            counter: 2,
        };
        let udp = encode_datagram(Datagram::Encrypted {
            header,
            ciphertext: &ciphertext,
        })
        .unwrap();
        assert_eq!(
            decode_datagram(&udp).unwrap(),
            Datagram::Encrypted {
                header,
                ciphertext: &ciphertext
            }
        );
        let relay = encode_datagram(Datagram::Relay {
            session: 3,
            datagram: &udp,
        })
        .unwrap();
        assert_eq!(
            decode_datagram(&relay).unwrap(),
            Datagram::Relay {
                session: 3,
                datagram: &udp
            }
        );
    }
    let legacy = encode_inner(Inner {
        kind: Kind::Ping,
        sequence: 0,
        talkspurt: 0,
        body: &[0; 8],
    })
    .unwrap();
    assert!(decode_policy_inner(&legacy, Kind::Ping).is_err());
}

#[test]
fn policy_media_maximum_still_fits_wrapped_udp_and_rejects_empty_or_extra_control_data() {
    let opus = vec![1; MAX_POLICY_OPUS];
    assert_eq!(MAX_POLICY_OPUS + 8, MAX_OPUS);
    let media = PolicyInner {
        kind: Kind::PolicyVoice,
        sequence: 0,
        talkspurt: 1,
        revision: u64::MAX,
        opus: &opus,
    };
    let mut ciphertext = encode_policy_inner(media).unwrap();
    ciphertext.extend_from_slice(&[0; TAG]);
    let udp = encode_datagram(Datagram::Encrypted {
        header: Header {
            kind: media.kind,
            phase: false,
            receiver: ReceiverHandle(1),
            counter: 2,
        },
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
    let oversized = vec![1; MAX_POLICY_OPUS + 1];
    assert!(
        encode_policy_inner(PolicyInner {
            opus: &oversized,
            ..media
        })
        .is_err()
    );
    assert!(wire::encode_policy_game(game(Kind::PolicyVoice, &oversized), true).is_err());
    for (kind, body) in [
        (Kind::PolicyVoice, &[0; 8][..]),
        (Kind::PolicyStart, &[0; 7][..]),
        (Kind::PolicyStop, &[0; 9][..]),
    ] {
        assert!(
            encode_inner(Inner {
                kind,
                sequence: 0,
                talkspurt: 1,
                body
            })
            .is_err()
        );
        let mut malformed = vec![kind as u8, 0, 0, 0, 1];
        malformed.extend_from_slice(body);
        assert!(decode_policy_inner(&malformed, kind).is_err());
    }
}

#[test]
fn cold_join_retransmission_mutation_and_rollback_fail_closed() {
    let mut gate = PolicyGate::new(1);
    assert!(!gate.accepts(2, 7));
    let installed = policy(7, &[0, 2]);
    assert!(matches!(
        gate.install(installed.clone()).unwrap(),
        PolicyUpdate::Installed {
            purge_all: true,
            ..
        }
    ));
    assert!(gate.accepts(0, 7));
    assert!(gate.accepts(1, 7));
    assert!(gate.accepts(2, 7));
    assert!(!gate.accepts(2, 6));
    assert!(!gate.accepts(2, 8));
    assert_eq!(gate.install(installed.clone()), Ok(PolicyUpdate::Unchanged));
    let mut mutation = installed.clone();
    mutation.audience.send_to.clear();
    assert_eq!(gate.install(mutation), Err(InstallError::Mutation));
    assert_eq!(
        gate.install(policy(6, &[0, 2])),
        Err(InstallError::Rollback)
    );
    assert_eq!(gate.current(), Some(&installed));
    let mut invalid = policy(8, &[0, 2]);
    invalid.tx_permitted = false;
    assert_eq!(
        gate.install(invalid),
        Err(InstallError::Invalid(CodecError::Invalid))
    );
    assert_eq!(gate.current(), Some(&installed));
}

#[test]
fn adjacent_window_intersects_eligibility_and_purges_only_revoked_receivers() {
    let mut gate = PolicyGate::new(1);
    gate.install(policy(7, &[0, 2, 3])).unwrap();
    assert_eq!(
        gate.install(policy(8, &[0, 2, 4])),
        Ok(PolicyUpdate::Installed {
            purge_all: false,
            removed_receivers: vec![3],
            tx_changed: false,
            self_monitor_changed: false,
        })
    );
    assert!(gate.accepts(2, 7));
    assert!(gate.accepts(2, 8));
    assert!(!gate.accepts(3, 7));
    assert!(!gate.accepts(3, 8));
    assert!(!gate.accepts(4, 7));
    assert!(gate.accepts(4, 8));
    assert!(!gate.accepts(4, 9));
    assert_eq!(
        gate.install(policy(8, &[0, 2, 4])),
        Ok(PolicyUpdate::Unchanged)
    );
    assert!(gate.accepts(2, 7));
    gate.install(policy(9, &[0, 2, 4])).unwrap();
    assert!(!gate.accepts(2, 7));
    assert!(gate.accepts(2, 8));
}

#[test]
fn skipped_revisions_cannot_infer_continuity_and_trusted_reset_accepts_new_epoch() {
    let mut gate = PolicyGate::new(1);
    gate.install(policy(7, &[2])).unwrap();
    assert!(matches!(
        gate.install(policy(9, &[2])).unwrap(),
        PolicyUpdate::Installed {
            purge_all: true,
            ..
        }
    ));
    assert!(!gate.accepts(2, 7));
    assert!(!gate.accepts(2, 8));
    assert!(gate.accepts(2, 9));
    gate.install(policy(10, &[2])).unwrap();
    assert!(gate.accepts(2, 9));
    gate.reset(5);
    assert!(!gate.accepts(2, 10));
    gate.install(policy(0, &[2])).unwrap();
    assert!(gate.accepts(2, 0));
    assert!(gate.accepts(5, 0));
    assert!(!gate.accepts(1, 0));
}

#[test]
fn off_mute_and_self_flags_apply_under_both_adjacent_policies_including_player_zero() {
    let mut gate = PolicyGate::new(1);
    gate.install(policy(7, &[0, 1, 2])).unwrap();
    let mut muted = policy(8, &[0, 1, 2]);
    muted.tx_permitted = false;
    muted.self_monitor = false;
    assert_eq!(
        gate.install(muted),
        Ok(PolicyUpdate::Installed {
            purge_all: false,
            removed_receivers: vec![1],
            tx_changed: true,
            self_monitor_changed: true,
        })
    );
    assert!(gate.accepts(0, 7));
    assert!(gate.accepts(2, 7));
    assert!(!gate.accepts(1, 7));
    assert!(!gate.accepts(1, 8));
    gate.install(policy(9, &[0, 1, 2])).unwrap();
    assert!(!gate.accepts(1, 8));
    assert!(gate.accepts(1, 9));
    let off = Policy {
        audience: Audience {
            revision: 10,
            send_to: vec![],
            receive_from: vec![],
        },
        tx_permitted: false,
        self_monitor: false,
    };
    assert_eq!(
        gate.install(off),
        Ok(PolicyUpdate::Installed {
            purge_all: false,
            removed_receivers: vec![0, 1, 2],
            tx_changed: true,
            self_monitor_changed: true,
        })
    );
    for player in [0, 1, 2] {
        assert!(!gate.accepts(player, 9));
        assert!(!gate.accepts(player, 10));
    }
}

#[test]
fn revision_exhaustion_never_wraps_into_a_previous_window() {
    let mut gate = PolicyGate::new(1);
    gate.install(policy(u64::MAX - 1, &[2])).unwrap();
    gate.install(policy(u64::MAX, &[2])).unwrap();
    assert!(gate.accepts(2, u64::MAX - 1));
    assert_eq!(gate.install(policy(0, &[2])), Err(InstallError::Rollback));
    assert!(!gate.accepts(2, 0));
}
