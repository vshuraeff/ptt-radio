use std::time::{Duration, Instant};

use ptt_radio_companion::crypto::{self, Actions, Error, Session};
use ptt_radio_companion::transport::signaling::{
    Announce, CAP_MEDIA_V2, CAP_UDP, IceCredentials, PROTOCOL_VERSION, Signal, Suite,
};
use ptt_radio_companion::transport::wire::{self, Datagram, Inner, Kind, ReceiverHandle};

fn announce(suites: Vec<Suite>, generation: u64) -> (Announce, Vec<u8>) {
    let keys = crypto::generate_static_keypair().unwrap();
    (
        Announce {
            version: PROTOCOL_VERSION,
            capabilities: CAP_MEDIA_V2 | CAP_UDP,
            suites,
            stream_generation: generation,
            static_key: keys.public,
            ice: IceCredentials {
                restart_generation: 1,
                ufrag: "test".into(),
                password: "test-password".into(),
            },
            candidates: vec![],
            candidates_complete: true,
        },
        keys.private,
    )
}

fn pair(suite: Suite) -> (Session, Session) {
    let (a, ak) = announce(vec![suite], 1 << 40);
    let (b, bk) = announce(vec![suite], (1 << 40) + 1);
    (
        Session::new(10, &a, ak, 20, &b).unwrap(),
        Session::new(20, &b, bk, 10, &a).unwrap(),
    )
}

fn connect(a: &mut Session, b: &mut Session, generation: u64, handle: u32, now: Instant) {
    let first = a.begin(generation, ReceiverHandle(handle), now).unwrap();
    let second = b
        .on_signal(10, &first.signals[0], ReceiverHandle(handle + 1), now)
        .unwrap();
    let confirmation = a
        .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
        .unwrap();
    let ack = b.receive(&confirmation.datagrams[0], now).unwrap();
    assert!(ack.actions.activated);
    assert!(
        a.receive(&ack.actions.datagrams[0], now)
            .unwrap()
            .actions
            .activated
    );
    assert_eq!(a.active_epoch(), b.active_epoch());
}

fn voice(a: &mut Session, sequence: u16) -> Vec<u8> {
    a.encrypt(Inner {
        kind: Kind::Voice,
        sequence,
        talkspurt: 7,
        body: &[1, 2, 3],
    })
    .unwrap()
}

fn counter(bytes: &[u8]) -> u32 {
    let Datagram::Encrypted { header, .. } = wire::decode_datagram(bytes).unwrap() else {
        panic!("encrypted")
    };
    header.counter
}

fn same_signals(a: &Actions, b: &Actions) {
    assert!(a.signals == b.signals);
}

#[test]
fn both_suites_authenticate_loss_reorder_replay_and_preserve_media_identity() {
    for suite in [Suite::Aes256Gcm, Suite::ChaChaPoly] {
        let now = Instant::now();
        let (mut a, mut b) = pair(suite);
        connect(&mut a, &mut b, (1 << 48) + 1, 1000, now);
        assert_eq!(a.suite(), suite);
        let frames = [
            voice(&mut a, 1),
            voice(&mut a, 2),
            voice(&mut a, 3),
            voice(&mut a, 4),
        ];
        let third = b.receive(&frames[2], now).unwrap().packet;
        assert_eq!(third.sequence, 3);
        assert_eq!(third.context.sender.player, 10);
        assert_eq!(third.context.receiver.player, 20);
        assert_eq!(third.context.sender.generation, 1 << 40);
        assert_eq!(third.context.key_epoch, 1);
        assert_eq!(b.receive(&frames[0], now).unwrap().packet.sequence, 1);
        assert!(matches!(b.receive(&frames[2], now), Err(Error::Replay)));
        assert_eq!(b.receive(&frames[3], now).unwrap().packet.sequence, 4);
        // frame 2 is deliberately lost. the next packet does not wait for it.
        assert_eq!(
            b.receive(&voice(&mut a, 5), now).unwrap().packet.sequence,
            5
        );
    }
}

#[test]
fn lost_noise1_noise2_metadata_confirmation_and_ack_recover_without_media() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::Aes256Gcm);
    let first = a.begin(1, ReceiverHandle(500), now).unwrap();
    let retry = a.retry().unwrap();
    same_signals(&first, &retry);
    let second = b
        .on_signal(10, &retry.signals[0], ReceiverHandle(600), now)
        .unwrap();
    let duplicate = b
        .on_signal(10, &a.retry().unwrap().signals[0], ReceiverHandle(601), now)
        .unwrap();
    same_signals(&second, &duplicate);
    same_signals(&second, &b.retry().unwrap());
    let lost_confirmation = a
        .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
        .unwrap();
    assert_eq!(a.active_epoch(), None);
    assert_eq!(b.active_epoch(), None);
    let retry = a.retry().unwrap();
    assert!(counter(&retry.datagrams[0]) > counter(&lost_confirmation.datagrams[0]));
    same_signals(&lost_confirmation, &retry);
    // metadata never arrives; both ends derive the token from the handshake.
    let lost_ack = b.receive(&retry.datagrams[0], now).unwrap();
    assert!(lost_ack.actions.activated);
    let ping = a.retry().unwrap();
    let ack = b.receive(&ping.datagrams[0], now).unwrap();
    assert!(!ack.actions.activated);
    assert!(counter(&ack.actions.datagrams[0]) > counter(&lost_ack.actions.datagrams[0]));
    assert!(
        a.receive(&ack.actions.datagrams[0], now)
            .unwrap()
            .actions
            .activated
    );
    assert!(a.receive(&lost_ack.actions.datagrams[0], now).is_ok());
    assert!(matches!(
        b.receive(&ping.datagrams[0], now),
        Err(Error::Replay)
    ));
    assert_eq!(a.active_epoch(), Some(1));
    assert_eq!(b.active_epoch(), Some(1));
}

#[test]
fn lost_confirmation_pong_then_any_valid_kind_activates_pending_key() {
    for suite in [Suite::Aes256Gcm, Suite::ChaChaPoly] {
        for kind in [
            Kind::Start,
            Kind::Voice,
            Kind::Stop,
            Kind::Ping,
            Kind::Pong,
            Kind::PolicyStart,
            Kind::PolicyVoice,
            Kind::PolicyStop,
        ] {
            let now = Instant::now();
            let (mut a, mut b) = pair(suite);
            let first = a.begin(1, ReceiverHandle(100), now).unwrap();
            let second = b
                .on_signal(10, &first.signals[0], ReceiverHandle(101), now)
                .unwrap();
            let confirmation = a
                .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
                .unwrap();
            let Signal::KeyConfirmation { token, .. } = confirmation.signals[0] else {
                panic!("confirmation metadata")
            };
            assert!(
                b.receive(&confirmation.datagrams[0], now)
                    .unwrap()
                    .actions
                    .activated
            );
            // every confirmation PONG is lost; this unrelated packet proves possession.
            let probe = (token ^ 1).to_be_bytes();
            let body = match kind {
                Kind::Voice => &[1, 2, 3][..],
                Kind::Ping | Kind::Pong => &probe,
                Kind::Start | Kind::Stop => &[],
                Kind::PolicyVoice => &[0; 9][..],
                Kind::PolicyStart | Kind::PolicyStop => &probe,
            };
            let packet = b
                .encrypt(Inner {
                    kind,
                    sequence: 37,
                    talkspurt: 23,
                    body,
                })
                .unwrap();
            let mut bad_tag = packet.clone();
            *bad_tag.last_mut().unwrap() ^= 1;
            assert!(matches!(a.receive(&bad_tag, now), Err(Error::Crypto)));
            let mut bad_counter = packet.clone();
            bad_counter[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(matches!(a.receive(&bad_counter, now), Err(Error::Crypto)));
            let mut bad_kind = packet.clone();
            bad_kind[0] = if kind == Kind::Start {
                Kind::Voice
            } else {
                Kind::Start
            } as u8;
            assert!(matches!(a.receive(&bad_kind, now), Err(Error::Codec(_))));
            assert_eq!(a.active_epoch(), None);

            let received = a.receive(&packet, now).unwrap();
            assert!(received.actions.activated, "{suite:?} {kind:?}");
            assert_eq!(received.packet.kind, kind);
            assert_eq!(received.packet.sequence, 37);
            assert_eq!(received.packet.talkspurt, 23);
            assert_eq!(received.packet.body, body);
            assert_eq!(received.packet.context.sender.player, 20);
            assert_eq!(received.packet.context.sender.generation, (1 << 40) + 1);
            assert_eq!(received.packet.context.receiver.player, 10);
            assert_eq!(received.packet.context.key_epoch, 1);
            assert_eq!(
                received.actions.datagrams.len(),
                usize::from(kind == Kind::Ping)
            );
            if kind == Kind::Ping {
                let reply = b.receive(&received.actions.datagrams[0], now).unwrap();
                assert_eq!(reply.packet.kind, Kind::Pong);
                assert_eq!(reply.packet.body, probe);
                assert!(reply.actions.datagrams.is_empty());
            }
            assert!(a.retry().unwrap().signals.is_empty());
            assert!(a.retry().unwrap().datagrams.is_empty());
            assert!(matches!(a.receive(&packet, now), Err(Error::Replay)));
        }
    }
}

#[test]
fn media_after_lost_rekey_pong_activates_and_retains_old_key_window() {
    for suite in [Suite::Aes256Gcm, Suite::ChaChaPoly] {
        let now = Instant::now();
        let (mut a, mut b) = pair(suite);
        connect(&mut a, &mut b, 1, 100, now);
        let seen = voice(&mut b, 1);
        let delayed = voice(&mut b, 2);
        let expired = voice(&mut b, 3);
        let identity = a.receive(&seen, now).unwrap().packet.context.sender;
        let first = a.begin(2, ReceiverHandle(200), now).unwrap();
        let second = b
            .on_signal(10, &first.signals[0], ReceiverHandle(201), now)
            .unwrap();
        let confirmation = a
            .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
            .unwrap();
        b.receive(&confirmation.datagrams[0], now).unwrap();
        assert!(matches!(a.receive(&seen, now), Err(Error::Replay)));
        assert_eq!(a.active_epoch(), Some(1));
        let media = voice(&mut b, 4);
        let received = a.receive(&media, now).unwrap();
        assert!(received.actions.activated);
        assert_eq!(received.packet.context.sender, identity);
        assert_eq!(received.packet.context.key_epoch, 2);
        assert_eq!(received.packet.sequence, 4);
        assert_eq!(a.active_epoch(), Some(2));
        assert!(a.retry().unwrap().signals.is_empty());
        assert!(a.retry().unwrap().datagrams.is_empty());
        assert!(matches!(a.receive(&media, now), Err(Error::Replay)));
        assert!(matches!(a.receive(&seen, now), Err(Error::Replay)));
        let old = a
            .receive(&delayed, now + Duration::from_millis(4999))
            .unwrap();
        assert_eq!(old.packet.context.key_epoch, 1);
        assert_eq!(old.packet.context.sender, identity);
        assert!(matches!(
            a.receive(&expired, now + Duration::from_secs(5)),
            Err(Error::WrongReceiver)
        ));
    }
}

#[test]
fn failed_auth_counter_type_receiver_and_phase_do_not_advance_window() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    connect(&mut a, &mut b, 1, 800, now);
    let packet = voice(&mut a, 1);
    let mut bad_tag = packet.clone();
    *bad_tag.last_mut().unwrap() ^= 1;
    assert!(matches!(b.receive(&bad_tag, now), Err(Error::Crypto)));
    let mut bad_counter = packet.clone();
    bad_counter[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(b.receive(&bad_counter, now), Err(Error::Crypto)));
    let mut bad_type = packet.clone();
    bad_type[0] = Kind::Start as u8;
    assert!(matches!(b.receive(&bad_type, now), Err(Error::Codec(_))));
    let mut bad_handle = packet.clone();
    bad_handle[2..6].copy_from_slice(&20u32.to_be_bytes());
    assert!(matches!(
        b.receive(&bad_handle, now),
        Err(Error::WrongReceiver)
    ));
    let mut bad_phase = packet.clone();
    bad_phase[1] ^= 1;
    assert!(matches!(
        b.receive(&bad_phase, now),
        Err(Error::WrongReceiver)
    ));
    let received = b.receive(&packet, now).unwrap().packet;
    assert_eq!(received.sequence, 1);
    assert!(matches!(b.receive(&packet, now), Err(Error::Replay)));
}

#[test]
fn rekey_without_media_keeps_old_window_for_exactly_five_seconds() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::Aes256Gcm);
    connect(&mut a, &mut b, 1, 100, now);
    let seen = voice(&mut a, 1);
    let delayed = voice(&mut a, 2);
    let expired = voice(&mut a, 3);
    let identity = b.receive(&seen, now).unwrap().packet.context.sender;
    connect(&mut a, &mut b, 2, 200, now);
    assert_eq!(a.active_epoch(), Some(2));
    assert!(matches!(b.receive(&seen, now), Err(Error::Replay)));
    assert_eq!(
        b.receive(&delayed, now + Duration::from_millis(4999))
            .unwrap()
            .packet
            .context
            .sender,
        identity
    );
    assert!(matches!(
        b.receive(&expired, now + Duration::from_secs(5)),
        Err(Error::WrongReceiver)
    ));
    let new = b
        .receive(&voice(&mut a, 4), now + Duration::from_secs(5))
        .unwrap()
        .packet;
    assert_eq!(new.context.sender, identity);
    assert_eq!(new.context.key_epoch, 2);
}

#[test]
fn overlapping_phase_cannot_be_reused_and_failed_attempts_skip_live_phase() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    connect(&mut a, &mut b, 1, 100, now);
    let failed = a.begin(2, ReceiverHandle(200), now).unwrap();
    let second = b
        .on_signal(10, &failed.signals[0], ReceiverHandle(201), now)
        .unwrap();
    let mut corrupt = second.signals[0].clone();
    if let Signal::Noise2(h) = &mut corrupt {
        *h.noise.last_mut().unwrap() ^= 1;
    }
    assert!(matches!(
        a.on_signal(20, &corrupt, ReceiverHandle(0), now),
        Err(Error::Crypto)
    ));
    connect(&mut a, &mut b, 3, 300, now);
    // attempt 3 has the active old key's phase; it was skipped, never reused.
    assert_eq!(a.active_epoch(), Some(4));
    assert!(matches!(
        a.begin(4, ReceiverHandle(400), now),
        Err(Error::PhaseInUse)
    ));
    connect(&mut a, &mut b, 4, 400, now + crypto::OLD_KEY_OVERLAP);
    assert_eq!(a.active_epoch(), Some(5));
}

#[test]
fn fresh_attempt_has_fresh_noise_and_stale_messages_cannot_activate() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    let old = a.begin(1, ReceiverHandle(100), now).unwrap();
    let old_second = b
        .on_signal(10, &old.signals[0], ReceiverHandle(101), now)
        .unwrap();
    let new = a.begin(2, ReceiverHandle(200), now).unwrap();
    let (Signal::Noise1(old), Signal::Noise1(new)) = (&old.signals[0], &new.signals[0]) else {
        panic!("noise1")
    };
    assert_ne!(old.noise, new.noise);
    assert!(matches!(
        a.on_signal(20, &old_second.signals[0], ReceiverHandle(0), now),
        Err(Error::StaleAttempt)
    ));
    let second = b
        .on_signal(10, &Signal::Noise1(new.clone()), ReceiverHandle(201), now)
        .unwrap();
    assert!(matches!(
        b.on_signal(10, &Signal::Noise1(old.clone()), ReceiverHandle(301), now),
        Err(Error::StaleAttempt)
    ));
    let ping = a
        .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
        .unwrap();
    let mut wrong_metadata = ping.signals[0].clone();
    if let Signal::KeyConfirmation { token, .. } = &mut wrong_metadata {
        *token ^= 1;
    }
    assert!(matches!(
        b.on_signal(10, &wrong_metadata, ReceiverHandle(0), now),
        Err(Error::InvalidConfirmation)
    ));
    b.on_signal(10, &ping.signals[0], ReceiverHandle(0), now)
        .unwrap();
    assert_eq!(b.active_epoch(), None);
    let ack = b.receive(&ping.datagrams[0], now).unwrap();
    a.receive(&ack.actions.datagrams[0], now).unwrap();
}

#[test]
fn snapshots_suite_player_indices_full_generation_and_handles_are_bound() {
    let now = Instant::now();
    let (aa, ak) = announce(vec![Suite::ChaChaPoly, Suite::Aes256Gcm], 1);
    let (bb, bk) = announce(vec![Suite::Aes256Gcm, Suite::ChaChaPoly], 2);
    let original_hash = crypto::announce_hash(&aa).unwrap();
    let mut changed_aa = aa.clone();
    changed_aa.ice.password.push('x');
    assert_ne!(original_hash, crypto::announce_hash(&changed_aa).unwrap());
    let mut a = Session::new(10, &aa, ak.clone(), 20, &bb).unwrap();
    assert_eq!(a.suite(), Suite::Aes256Gcm);
    let first = a.begin((1 << 40) + 1, ReceiverHandle(90_000), now).unwrap();
    let mut b_wrong_snapshot = Session::new(20, &bb, bk.clone(), 10, &changed_aa).unwrap();
    assert!(matches!(
        b_wrong_snapshot.on_signal(10, &first.signals[0], ReceiverHandle(400), now),
        Err(Error::Crypto)
    ));
    let mut b_wrong_index = Session::new(20, &bb, bk.clone(), 11, &aa).unwrap();
    assert!(matches!(
        b_wrong_index.on_signal(11, &first.signals[0], ReceiverHandle(400), now),
        Err(Error::Crypto)
    ));
    let mut b = Session::new(20, &bb, bk, 10, &aa).unwrap();
    for mutation in 0..3 {
        let mut changed = first.signals[0].clone();
        if let Signal::Noise1(h) = &mut changed {
            match mutation {
                0 => h.session_generation ^= 1 << 40,
                1 => h.attempt += 1,
                _ => h.receive_handle.0 += 1,
            }
        }
        assert!(b.on_signal(10, &changed, ReceiverHandle(400), now).is_err());
    }
    let second = b
        .on_signal(10, &first.signals[0], ReceiverHandle(400), now)
        .unwrap();
    let ping = a
        .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
        .unwrap();
    let ack = b.receive(&ping.datagrams[0], now).unwrap();
    a.receive(&ack.actions.datagrams[0], now).unwrap();
    assert_eq!(a.generation(), (1 << 40) + 1);
    let receive = b.receive(&voice(&mut a, 1), now).unwrap().packet;
    assert_eq!(receive.context.key_epoch, 1);
}

#[test]
fn higher_player_cannot_initiate_kk_and_lower_attempt_wins() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    assert!(matches!(
        b.begin(1, ReceiverHandle(50), now),
        Err(Error::NotInitiator)
    ));
    connect(&mut a, &mut b, 1, 100, now);
    assert!(matches!(
        b.begin(2, ReceiverHandle(150), now),
        Err(Error::NotInitiator)
    ));
    connect(&mut a, &mut b, 2, 200, now);
}

#[test]
fn game_sender_binding_rejects_unexpected_noise_and_rekey_without_mutation() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    let first = a.begin(1, ReceiverHandle(100), now).unwrap();
    assert!(matches!(
        b.on_signal(11, &first.signals[0], ReceiverHandle(101), now),
        Err(Error::WrongPeer)
    ));
    assert_eq!(b.generation(), 0);
    let second = b
        .on_signal(10, &first.signals[0], ReceiverHandle(101), now)
        .unwrap();
    assert!(matches!(
        a.on_signal(21, &second.signals[0], ReceiverHandle(0), now),
        Err(Error::WrongPeer)
    ));
    let ping = a
        .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
        .unwrap();
    let pong = b.receive(&ping.datagrams[0], now).unwrap();
    a.receive(&pong.actions.datagrams[0], now).unwrap();
    let request = b.request_rekey(ReceiverHandle(0), now).unwrap();
    assert!(matches!(
        a.on_signal(21, &request.signals[0], ReceiverHandle(200), now),
        Err(Error::WrongPeer)
    ));
    assert_eq!(a.generation(), 1);
    assert!(a.retry().unwrap().signals.is_empty());
    let first = a
        .on_signal(20, &request.signals[0], ReceiverHandle(200), now)
        .unwrap();
    assert!(matches!(first.signals[0], Signal::Noise1(_)));
}

#[test]
fn caller_deadline_abort_burns_attempt_ids_and_preserves_live_key_windows() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    connect(&mut a, &mut b, 1, 100, now);
    let seen_old = voice(&mut a, 1);
    let delayed_old = voice(&mut a, 2);
    b.receive(&seen_old, now).unwrap();
    connect(&mut a, &mut b, 2, 200, now);
    let seen_active = voice(&mut a, 3);
    b.receive(&seen_active, now).unwrap();
    // aborting a higher-side request must not discard either accepted key.
    b.request_rekey(ReceiverHandle(0), now).unwrap();
    b.abort_pending().unwrap();
    assert!(b.retry().unwrap().signals.is_empty());
    assert!(matches!(b.receive(&seen_old, now), Err(Error::Replay)));
    assert!(matches!(b.receive(&seen_active, now), Err(Error::Replay)));
    b.receive(&delayed_old, now + Duration::from_secs(4))
        .unwrap();
    let later = now + crypto::OLD_KEY_OVERLAP;
    let first = a.begin(3, ReceiverHandle(300), later).unwrap();
    let second = b
        .on_signal(10, &first.signals[0], ReceiverHandle(301), later)
        .unwrap();
    // the caller's attempt deadline expires before the initiator sends confirmation.
    a.abort_pending().unwrap();
    b.abort_pending().unwrap();
    assert!(a.retry().unwrap().signals.is_empty());
    assert!(b.retry().unwrap().signals.is_empty());
    assert!(matches!(
        a.on_signal(20, &second.signals[0], ReceiverHandle(0), later),
        Err(Error::StaleAttempt)
    ));
    assert!(matches!(
        b.on_signal(10, &first.signals[0], ReceiverHandle(301), later),
        Err(Error::StaleAttempt)
    ));
    assert!(matches!(
        a.begin(3, ReceiverHandle(300), later),
        Err(Error::StaleAttempt)
    ));
    // discarded preconfirmation slots may be reused; stale handshake IDs cannot revive.
    connect(&mut a, &mut b, 4, 300, later);
    assert_eq!(a.active_epoch(), Some(5));
}

#[test]
fn all_confirmation_acks_lost_then_abort_requires_fresh_snapshot_context() {
    let now = Instant::now();
    let (mut a, mut b) = pair(Suite::ChaChaPoly);
    connect(&mut a, &mut b, 1, 100, now);
    let received_before_rekey = voice(&mut b, 1);
    let identity = a
        .receive(&received_before_rekey, now)
        .unwrap()
        .packet
        .context
        .sender;
    let first = a.begin(2, ReceiverHandle(200), now).unwrap();
    let second = b
        .on_signal(10, &first.signals[0], ReceiverHandle(201), now)
        .unwrap();
    let ping = a
        .on_signal(20, &second.signals[0], ReceiverHandle(0), now)
        .unwrap();
    b.receive(&ping.datagrams[0], now).unwrap();
    for _ in 0..3 {
        b.receive(&a.retry().unwrap().datagrams[0], now).unwrap();
    }
    assert_eq!(a.active_epoch(), Some(1));
    assert_eq!(b.active_epoch(), Some(2));
    assert!(matches!(
        a.begin(3, ReceiverHandle(300), now),
        Err(Error::NotReady)
    ));
    assert!(matches!(a.abort_pending(), Err(Error::ResetRequired)));
    assert!(a.retry().unwrap().signals.is_empty());
    assert!(a.retry().unwrap().datagrams.is_empty());
    assert!(matches!(
        a.begin(3, ReceiverHandle(300), now + Duration::from_secs(10)),
        Err(Error::ResetRequired)
    ));
    assert!(matches!(
        a.on_signal(20, &second.signals[0], ReceiverHandle(0), now),
        Err(Error::ResetRequired)
    ));
    assert!(matches!(
        a.receive(&received_before_rekey, now),
        Err(Error::Replay)
    ));
    // both peers adopt replacement snapshots; stream identity does not change.
    let (mut aa, ak) = announce(vec![Suite::ChaChaPoly], 1 << 40);
    let (mut bb, bk) = announce(vec![Suite::ChaChaPoly], (1 << 40) + 1);
    for announce in [&mut aa, &mut bb] {
        announce.ice.restart_generation = 2;
        announce.ice.password = ["fresh-", "test-credential"].concat();
    }
    let mut fresh_a = Session::new(10, &aa, ak, 20, &bb).unwrap();
    let mut fresh_b = Session::new(20, &bb, bk, 10, &aa).unwrap();
    connect(&mut fresh_a, &mut fresh_b, 1, 300, now);
    assert_eq!(
        fresh_a
            .receive(&voice(&mut fresh_b, 2), now)
            .unwrap()
            .packet
            .context
            .sender,
        identity
    );
}
