//! relay core, BIND and hub policy (DESIGN.md, "Relay hub"): registration only after a
//! proof from the exact source, forwarding only between registered endpoints, caps,
//! expiry, close, rebinding, and one forward through the NAT lab router.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

#[path = "netlab/router.rs"]
mod router;

use ptt_radio_companion::transport::hub::Hub;
use ptt_radio_companion::transport::relay::{
    BindClient, CHALLENGE_MS, Dropped, IssueError, Limits, MAX_GRANTS, Relay, Reply, bind_proof,
};
use ptt_radio_companion::transport::signaling::{
    Announce, CAP_MEDIA_V2, CAP_UDP, IceCredentials, PROTOCOL_VERSION, RelayGrant, Signal, Suite,
    decode_game, encode_game,
};
use ptt_radio_companion::transport::wire::{
    Bind, BindChallenge, BindRequest, Datagram, Header, Kind, ReceiverHandle, decode_datagram,
    encode_datagram,
};

const GEN: u64 = 0x1234_5678_9abc_def0;

fn addr(host: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([203, 0, 113, host], port))
}

fn relay() -> Relay {
    Relay::new(0, GEN, Limits::default())
}

fn media(counter: u32) -> Vec<u8> {
    encode_datagram(Datagram::Encrypted {
        header: Header {
            kind: Kind::Voice,
            phase: false,
            receiver: ReceiverHandle(77),
            counter,
        },
        ciphertext: &[0xab; 60],
    })
    .unwrap()
}

fn wrap(session: u32, datagram: &[u8]) -> Vec<u8> {
    encode_datagram(Datagram::Relay { session, datagram }).unwrap()
}

fn sent(reply: Result<Reply, Dropped>) -> (SocketAddr, Vec<u8>) {
    match reply {
        Ok(Reply::Send(to, bytes)) => (to, bytes),
        other => panic!("expected a reply, got {other:?}"),
    }
}

fn challenge_of(bytes: &[u8]) -> BindChallenge {
    match decode_datagram(bytes) {
        Ok(Datagram::Bind(Bind::Challenge(c))) => c,
        _ => panic!("not a challenge"),
    }
}

/// the full request/challenge/commit/ack exchange from `source`; returns the commit bytes
fn bind(relay: &mut Relay, client: &mut BindClient, source: SocketAddr, now: u64) -> Vec<u8> {
    let (to, bytes) = sent(relay.handle(now, source, &client.request()));
    assert_eq!(to, source);
    let commit = client.on_challenge(challenge_of(&bytes)).unwrap();
    let (to, ack) = sent(relay.handle(now, source, &commit));
    assert_eq!(to, source);
    let Ok(Datagram::BindAck { challenge, proof }) = decode_datagram(&ack) else {
        panic!("not an ack")
    };
    // BIND reflection: the ack confirms the address the relay saw
    assert_eq!(client.on_ack(challenge, &proof), Some(source));
    commit
}

fn bound_pair(relay: &mut Relay, a: SocketAddr, b: SocketAddr) -> RelayGrant {
    let grant = relay.issue(1, 2, 0).unwrap();
    bind(
        relay,
        &mut BindClient::new(grant.clone(), 1, 1).unwrap(),
        a,
        1,
    );
    bind(
        relay,
        &mut BindClient::new(grant.clone(), 2, 1).unwrap(),
        b,
        1,
    );
    grant
}

#[test]
fn both_endpoints_bind_and_traffic_flows_both_ways_keeping_the_wrapper() {
    let mut relay = relay();
    let (a, b) = (addr(1, 40000), addr(2, 40000));
    let grant = bound_pair(&mut relay, a, b);
    assert_eq!((grant.relay, grant.hub_generation), (0, GEN));
    for (from, to) in [(a, b), (b, a)] {
        let inner = media(3);
        let wrapped = wrap(grant.session, &inner);
        // Forward means the received bytes go out unchanged: wrapper and ciphertext
        assert_eq!(relay.handle(2, from, &wrapped), Ok(Reply::Forward(to)));
        let Ok(Datagram::Relay { session, datagram }) = decode_datagram(&wrapped) else {
            panic!("wrapper lost")
        };
        assert_eq!((session, datagram), (grant.session, &inner[..]));
    }
}

#[test]
fn replayed_bind_does_not_redirect() {
    let mut relay = relay();
    let (a, b, attacker) = (addr(1, 40000), addr(2, 40000), addr(66, 6666));
    let grant = relay.issue(1, 2, 0).unwrap();
    let mut client = BindClient::new(grant.clone(), 1, 1).unwrap();
    let request = client.request();
    let commit = bind(&mut relay, &mut client, a, 1);
    bind(
        &mut relay,
        &mut BindClient::new(grant.clone(), 2, 1).unwrap(),
        b,
        1,
    );

    // the captured commit from another address, and again after a replayed request
    assert_eq!(
        relay.handle(2, attacker, &commit),
        Err(Dropped::NoChallenge)
    );
    assert_eq!(
        relay.handle(2, attacker, &request),
        Err(Dropped::StaleAttempt)
    );
    // a fresh request from the attacker gets a challenge for its own address, but no
    // proof for it without the secret
    let forged = BindClient::new(grant.clone(), 1, 2).unwrap();
    let (_, bytes) = sent(relay.handle(2, attacker, &forged.request()));
    let challenge = challenge_of(&bytes);
    assert_eq!(challenge.endpoint, attacker);
    let mut wrong = grant.clone();
    wrong.secret[0] ^= 1;
    let bad = encode_datagram(Datagram::Bind(Bind::Commit {
        challenge,
        proof: bind_proof(&wrong, challenge, false),
    }))
    .unwrap();
    assert_eq!(relay.handle(2, attacker, &bad), Err(Dropped::BadProof));
    // the old commit sent with the attacker's own challenge swapped in still fails
    let Ok(Datagram::Bind(Bind::Commit { proof, .. })) = decode_datagram(&commit) else {
        panic!()
    };
    let spliced = encode_datagram(Datagram::Bind(Bind::Commit { challenge, proof })).unwrap();
    assert_eq!(relay.handle(2, attacker, &spliced), Err(Dropped::BadProof));

    // a's slot still points at a, and the attacker cannot inject
    let wrapped = wrap(grant.session, &media(1));
    assert_eq!(relay.handle(3, b, &wrapped), Ok(Reply::Forward(a)));
    assert_eq!(
        relay.handle(3, attacker, &wrapped),
        Err(Dropped::UnregisteredSource)
    );
    // the same commit from a itself (its ack was lost) is acked again and changes nothing
    let (to, _) = sent(relay.handle(3, a, &commit));
    assert_eq!(to, a);
    assert_eq!(relay.handle(4, b, &wrapped), Ok(Reply::Forward(a)));
}

#[test]
fn bind_with_wrong_mac_player_generation_or_expiry_is_refused() {
    let mut relay = relay();
    let a = addr(1, 40000);
    let grant = relay.issue(1, 2, 0).unwrap();

    // wrong MAC: a client holding another secret
    let mut wrong = grant.clone();
    wrong.secret = [0; 16];
    let mut client = BindClient::new(wrong, 1, 1).unwrap();
    let (_, bytes) = sent(relay.handle(1, a, &client.request()));
    let commit = client.on_challenge(challenge_of(&bytes)).unwrap();
    assert_eq!(relay.handle(1, a, &commit), Err(Dropped::BadProof));

    // wrong player: not an endpoint of the grant
    assert!(BindClient::new(grant.clone(), 3, 1).is_none());
    let request = |player, hub_generation, session| {
        encode_datagram(Datagram::Bind(Bind::Request(BindRequest {
            session,
            hub_generation,
            player,
            attempt: 1,
            client_nonce: [9; 32],
        })))
        .unwrap()
    };
    assert_eq!(
        relay.handle(1, a, &request(3, GEN, grant.session)),
        Err(Dropped::WrongPlayer)
    );
    assert_eq!(
        relay.handle(1, a, &request(0, GEN, grant.session)),
        Err(Dropped::WrongPlayer)
    );
    // wrong generation in the request
    assert_eq!(
        relay.handle(1, a, &request(1, GEN + 1, grant.session)),
        Err(Dropped::WrongGeneration)
    );

    // a challenge answered too late
    let mut late = BindClient::new(grant.clone(), 1, 1).unwrap();
    let (_, bytes) = sent(relay.handle(10, a, &late.request()));
    let commit = late.on_challenge(challenge_of(&bytes)).unwrap();
    assert_eq!(
        relay.handle(11 + CHALLENGE_MS, a, &commit),
        Err(Dropped::NoChallenge)
    );

    // an expired grant
    let client = BindClient::new(grant.clone(), 1, 1).unwrap();
    assert_eq!(
        relay.handle(grant.expires_at_ms, a, &client.request()),
        Err(Dropped::Expired)
    );

    // a grant of the previous game session
    relay.set_generation(GEN + 1);
    assert_eq!(
        relay.handle(20, a, &request(1, GEN, grant.session)),
        Err(Dropped::UnknownSession)
    );
    let fresh = relay.issue(1, 2, 20).unwrap();
    assert_eq!(fresh.hub_generation, GEN + 1);
    assert_eq!(
        relay.handle(20, a, &request(1, GEN, fresh.session)),
        Err(Dropped::WrongGeneration)
    );
}

#[test]
fn old_generation_grants_stop_forwarding() {
    let mut relay = relay();
    let (a, b) = (addr(1, 40000), addr(2, 40000));
    let grant = bound_pair(&mut relay, a, b);
    let wrapped = wrap(grant.session, &media(1));
    assert_eq!(relay.handle(2, a, &wrapped), Ok(Reply::Forward(b)));
    relay.set_generation(GEN + 1);
    assert_eq!(relay.handle(3, a, &wrapped), Err(Dropped::UnknownSession));
}

#[test]
fn guessed_session_number_forwards_nothing() {
    let mut relay = relay();
    let (a, b, stranger) = (addr(1, 40000), addr(2, 40000), addr(9, 9));
    let grant = relay.issue(1, 2, 0).unwrap();
    let wrapped = wrap(grant.session, &media(1));
    bind(
        &mut relay,
        &mut BindClient::new(grant.clone(), 1, 1).unwrap(),
        a,
        1,
    );
    // registered but the other end has not bound
    assert_eq!(relay.handle(2, a, &wrapped), Err(Dropped::PeerUnbound));
    bind(
        &mut relay,
        &mut BindClient::new(grant.clone(), 2, 1).unwrap(),
        b,
        1,
    );
    for guess in [grant.session.wrapping_add(1), 0, u32::MAX] {
        if guess != grant.session {
            let guessed = wrap(guess, &media(1));
            assert_eq!(relay.handle(3, a, &guessed), Err(Dropped::UnknownSession));
            assert_eq!(
                relay.handle(3, stranger, &guessed),
                Err(Dropped::UnknownSession)
            );
        }
    }
    // the right number from an unregistered address
    assert_eq!(
        relay.handle(3, stranger, &wrapped),
        Err(Dropped::UnregisteredSource)
    );
    // unwrapped media, challenges and acks are not relay input; garbage never panics
    assert_eq!(relay.handle(3, a, &media(1)), Err(Dropped::NotForRelay));
    for junk in [
        &[][..],
        &[0x10],
        &[0x10, 0, 0, 0, 1, 0x10],
        &[0x11, 9],
        &[0xff; 1300],
    ] {
        assert!(matches!(
            relay.handle(3, a, junk),
            Err(Dropped::Malformed(_))
        ));
    }
}

#[test]
fn rate_cap_drops_above_the_cap() {
    let len = wrap(1, &media(1)).len() as u64;
    let limits = Limits {
        // the challenge and ack sent to a player during its bind take 237 of the 300
        bytes_per_second: 300 + 10 * len,
        ..Limits::default()
    };
    let mut relay = Relay::new(0, GEN, limits);
    let (a, b) = (addr(1, 40000), addr(2, 40000));
    let grant = bound_pair(&mut relay, a, b);
    let wrapped = wrap(grant.session, &media(1));
    let burst: Vec<_> = (0..30).map(|_| relay.handle(1, a, &wrapped)).collect();
    let forwarded = burst
        .iter()
        .filter(|r| **r == Ok(Reply::Forward(b)))
        .count();
    let limited = burst
        .iter()
        .filter(|r| **r == Err(Dropped::RateLimited))
        .count();
    // the budget is one second of the rate, minus the challenge and ack sent to a
    assert_eq!(forwarded, 10);
    assert_eq!(forwarded + limited, 30);
    // the other player's budget is its own
    assert_eq!(relay.handle(1, b, &wrapped), Ok(Reply::Forward(a)));
    // refilled at the configured rate
    assert_eq!(relay.handle(1001, a, &wrapped), Ok(Reply::Forward(b)));
}

#[test]
fn grant_cap_per_player() {
    let limits = Limits {
        grants_per_player: 2,
        ..Limits::default()
    };
    let mut relay = Relay::new(0, GEN, limits);
    let first = relay.issue(1, 2, 0).unwrap();
    relay.issue(1, 3, 0).unwrap();
    assert_eq!(relay.issue(1, 4, 0).err(), Some(IssueError::Cap));
    assert_eq!(relay.issue(4, 1, 0).err(), Some(IssueError::Cap));
    relay.issue(2, 3, 0).unwrap();
    assert!(!relay.withdraw(first.session).is_empty());
    relay.issue(1, 4, 0).unwrap();
}

#[test]
fn expiry_and_explicit_close_stop_forwarding() {
    let mut relay = relay();
    let (a, b) = (addr(1, 40000), addr(2, 40000));
    let grant = bound_pair(&mut relay, a, b);
    let wrapped = wrap(grant.session, &media(1));
    let end = grant.expires_at_ms;
    assert_eq!(relay.handle(end - 1, a, &wrapped), Ok(Reply::Forward(b)));
    assert_eq!(relay.handle(end, a, &wrapped), Err(Dropped::Expired));
    relay.expire(end);
    assert_eq!(relay.handle(end, a, &wrapped), Err(Dropped::UnknownSession));

    let mut relay = self::relay();
    let grant = bound_pair(&mut relay, a, b);
    let wrapped = wrap(grant.session, &media(1));
    let close = |hub_generation| Signal::RelayClose {
        relay: 0,
        session: grant.session,
        hub_generation,
    };
    // not an endpoint, or another generation: ignored
    assert!(relay.on_signal(3, &close(GEN)).is_empty());
    assert!(relay.on_signal(1, &close(GEN + 1)).is_empty());
    assert_eq!(relay.handle(2, a, &wrapped), Ok(Reply::Forward(b)));
    // an endpoint closes; the other one hears it
    let frames = relay.on_signal(1, &close(GEN));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].to, 2);
    assert!(matches!(
        decode_game(&frames[0].frame, false),
        Ok((2, Signal::RelayClose { session, .. })) if session == grant.session
    ));
    assert_eq!(relay.handle(3, a, &wrapped), Err(Dropped::Expired));
    assert_eq!(relay.handle(3, b, &wrapped), Err(Dropped::Expired));
}

#[test]
fn rebind_moves_the_endpoint_only_after_a_successful_challenge() {
    let mut relay = relay();
    let (a, b, moved) = (addr(1, 40000), addr(2, 40000), addr(1, 40001));
    let grant = bound_pair(&mut relay, a, b);
    let wrapped = wrap(grant.session, &media(1));

    let mut rebind = BindClient::new(grant.clone(), 1, 2).unwrap();
    let (_, bytes) = sent(relay.handle(5, moved, &rebind.request()));
    // challenged, not committed: the old address still holds the slot
    assert_eq!(relay.handle(5, b, &wrapped), Ok(Reply::Forward(a)));
    assert_eq!(
        relay.handle(5, moved, &wrapped),
        Err(Dropped::UnregisteredSource)
    );
    // a commit for that challenge sent from the old address does not count
    let commit = rebind.on_challenge(challenge_of(&bytes)).unwrap();
    assert_eq!(relay.handle(6, a, &commit), Err(Dropped::NoChallenge));
    sent(relay.handle(6, moved, &commit));
    assert_eq!(relay.handle(7, b, &wrapped), Ok(Reply::Forward(moved)));
    assert_eq!(relay.handle(7, moved, &wrapped), Ok(Reply::Forward(b)));
    assert_eq!(
        relay.handle(7, a, &wrapped),
        Err(Dropped::UnregisteredSource)
    );
    // the first bind cannot be replayed back onto the old address
    let old = BindClient::new(grant, 1, 1).unwrap();
    assert_eq!(
        relay.handle(8, a, &old.request()),
        Err(Dropped::StaleAttempt)
    );
}

#[test]
fn retransmitted_request_gets_the_same_challenge() {
    let mut relay = relay();
    let a = addr(1, 40000);
    let grant = relay.issue(1, 2, 0).unwrap();
    let mut client = BindClient::new(grant, 1, 1).unwrap();
    let first = challenge_of(&sent(relay.handle(1, a, &client.request())).1);
    let second = challenge_of(&sent(relay.handle(2, a, &client.request())).1);
    assert_eq!(first, second);
    let commit = client.on_challenge(first).unwrap();
    sent(relay.handle(3, a, &commit));
}

#[test]
fn player_relay_runs_the_same_core_and_claims_only_relay_traffic() {
    // a companion of player 5 relaying for the pair 1-2
    let mut relay = Relay::new(5, 42, Limits::default());
    let (a, b, hub) = (addr(1, 40000), addr(2, 40000), addr(100, 34560));
    let frames = relay.offer(1, 2, 0).unwrap();
    assert_eq!(frames.iter().map(|f| f.to).collect::<Vec<_>>(), vec![1, 2]);
    let Ok((1, Signal::RelayGrant(grant))) = decode_game(&frames[0].frame, false) else {
        panic!("not a grant")
    };
    assert_eq!((grant.relay, grant.hub_generation), (5, 42));
    let mut client = BindClient::new(grant.clone(), 1, 1).unwrap();
    assert!(relay.claims(a, &client.request()));
    bind(&mut relay, &mut client, a, 1);
    bind(
        &mut relay,
        &mut BindClient::new(grant.clone(), 2, 1).unwrap(),
        b,
        1,
    );
    let wrapped = wrap(grant.session, &media(1));
    assert!(relay.claims(a, &wrapped));
    assert_eq!(relay.handle(2, a, &wrapped), Ok(Reply::Forward(b)));
    // the companion's own endpoint traffic: wrapped media from the hub, plain media
    assert!(!relay.claims(hub, &wrapped));
    assert!(!relay.claims(a, &media(1)));
    assert_eq!(relay.issue(5, 1, 0).err(), Some(IssueError::Invalid));
    // the relay ends the pair: both endpoints hear it
    let closed = relay.withdraw(grant.session);
    assert_eq!(closed.iter().map(|f| f.to).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(relay.handle(3, a, &wrapped), Err(Dropped::Expired));
}

fn announce(player: u16, stream_generation: u64, capabilities: u32) -> Vec<u8> {
    let signal = Signal::Announce(Announce {
        version: PROTOCOL_VERSION,
        capabilities,
        suites: vec![Suite::ChaChaPoly],
        stream_generation,
        static_key: [player as u8; 32],
        ice: IceCredentials {
            restart_generation: 1,
            ufrag: "ufrag".into(),
            password: "password-password".into(),
        },
        candidates: Vec::new(),
        candidates_complete: true,
    });
    encode_game(&signal, player, true).unwrap()
}

fn grants(frames: &[ptt_radio_companion::transport::relay::Outgoing]) -> Vec<(u16, RelayGrant)> {
    frames
        .iter()
        .filter_map(|f| match decode_game(&f.frame, false) {
            Ok((to, Signal::RelayGrant(g))) => {
                assert_eq!(to, f.to);
                Some((to, g))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn hub_grants_every_announced_pair_and_restarts_them_with_the_companion() {
    let mut hub = Hub::new(GEN, Limits::default());
    let udp = CAP_UDP | CAP_MEDIA_V2;
    assert!(hub.on_game(0, &announce(1, 11, udp)).is_empty());
    // no UDP capability: no grants
    assert!(hub.on_game(0, &announce(3, 33, CAP_MEDIA_V2)).is_empty());
    // not a SIGNAL at all
    assert!(hub.on_game(0, &[0x52, 1, 0x10, 0, 0, b'x']).is_empty());
    let first = grants(&hub.on_game(1, &announce(2, 22, udp)));
    assert_eq!(first.len(), 2);
    let grant = first[0].1.clone();
    assert_eq!(
        (
            first[0].0,
            first[1].0,
            grant.sender,
            grant.recipient,
            grant.relay
        ),
        (1, 2, 1, 2, 0)
    );
    assert!(first[1].1 == grant);
    // the same companion again gets the same grant back, nobody else hears anything
    let again = grants(&hub.on_game(2, &announce(1, 11, udp)));
    assert_eq!(again.len(), 1);
    assert!(again[0].0 == 1 && again[0].1 == grant);
    // a restarted companion (new stream generation): the old grant closes, a new one starts
    let frames = hub.on_game(3, &announce(1, 12, udp));
    let closes: Vec<u16> = frames
        .iter()
        .filter(|f| matches!(decode_game(&f.frame, false), Ok((_, Signal::RelayClose { session, .. })) if session == grant.session))
        .map(|f| f.to)
        .collect();
    assert_eq!(closes, vec![1, 2]);
    let renewed = grants(&frames);
    assert_eq!(renewed.len(), 2);
    assert_ne!(renewed[0].1.session, grant.session);
    assert!(hub.relay.live(3).all(|g| g.session != grant.session));
}

#[test]
fn hub_renews_a_grant_both_endpoints_still_use() {
    let limits = Limits {
        grant_ms: 4000,
        ..Limits::default()
    };
    let mut hub = Hub::new(GEN, limits);
    let udp = CAP_UDP;
    hub.on_game(0, &announce(1, 11, udp));
    let grant = grants(&hub.on_game(0, &announce(2, 22, udp)))[0].1.clone();
    let (a, b) = (addr(1, 40000), addr(2, 40000));
    bind(
        &mut hub.relay,
        &mut BindClient::new(grant.clone(), 1, 1).unwrap(),
        a,
        100,
    );
    bind(
        &mut hub.relay,
        &mut BindClient::new(grant.clone(), 2, 1).unwrap(),
        b,
        100,
    );
    // early in its life nothing happens
    assert!(hub.tick(2000).is_empty());
    let wrapped = wrap(grant.session, &media(1));
    hub.relay.handle(3500, a, &wrapped).unwrap();
    // used by a only: not renewed
    assert!(hub.tick(3500).is_empty());
    hub.relay.handle(3600, b, &wrapped).unwrap();
    let renewed = grants(&hub.tick(3600));
    assert_eq!(renewed.len(), 2);
    assert_ne!(renewed[0].1.session, grant.session);
    // only once: the new grant is now the newest of the pair
    assert!(hub.tick(3700).is_empty());
    // the old one keeps working until it expires
    assert_eq!(hub.relay.handle(3800, a, &wrapped), Ok(Reply::Forward(b)));
    assert_eq!(hub.relay.handle(4000, a, &wrapped), Err(Dropped::Expired));
}

/// one grant per player, pair (1, 2) granted, both endpoints bound at 3500 of a 4000 ms grant
fn hub_at_cap_with_bound_pair() -> (Hub, RelayGrant) {
    let limits = Limits {
        grant_ms: 4000,
        grants_per_player: 1,
        ..Limits::default()
    };
    let mut hub = Hub::new(GEN, limits);
    hub.on_game(0, &announce(1, 11, CAP_UDP));
    let grant = grants(&hub.on_game(0, &announce(2, 22, CAP_UDP)))[0]
        .1
        .clone();
    for player in [1, 2] {
        bind(
            &mut hub.relay,
            &mut BindClient::new(grant.clone(), player, 1).unwrap(),
            addr(player as u8, 40000),
            3500,
        );
    }
    (hub, grant)
}

#[test]
fn hub_renews_an_active_pair_at_the_grant_cap() {
    let (mut hub, old) = hub_at_cap_with_bound_pair();
    let (a, b) = (addr(1, 40000), addr(2, 40000));
    let renewed = grants(&hub.tick(3600));
    assert_eq!(
        renewed.iter().map(|(to, _)| *to).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let new = renewed[0].1.clone();
    assert!(new.session != old.session && (new.sender, new.recipient) == (1, 2));
    // the old grant keeps forwarding while the replacement is pending
    let wrapped = wrap(old.session, &media(1));
    assert_eq!(hub.relay.handle(3700, a, &wrapped), Ok(Reply::Forward(b)));
    assert!(hub.tick(3700).is_empty());
    // after the old grant's expiry the pair still holds a live grant that forwards once
    // rebound
    assert!(hub.tick(4000).is_empty());
    assert_eq!(
        hub.relay.handle(4000, a, &wrapped),
        Err(Dropped::UnknownSession)
    );
    let live: Vec<u32> = hub.relay.live(4000).map(|g| g.session).collect();
    assert_eq!(live, vec![new.session]);
    for (player, source) in [(1, a), (2, b)] {
        bind(
            &mut hub.relay,
            &mut BindClient::new(new.clone(), player, 1).unwrap(),
            source,
            4000,
        );
    }
    let wrapped = wrap(new.session, &media(2));
    assert_eq!(hub.relay.handle(4001, b, &wrapped), Ok(Reply::Forward(a)));
}

#[test]
fn a_renewal_frees_no_grant_for_another_pair() {
    let (mut hub, old) = hub_at_cap_with_bound_pair();
    assert_eq!(grants(&hub.tick(3600)).len(), 2);
    // during the overlap and after it, player 3 pairs with neither capped player
    for now in [3700, 4100] {
        hub.tick(now);
        assert!(grants(&hub.on_game(now, &announce(3, 33 + now, CAP_UDP))).is_empty());
        assert_eq!(hub.relay.issue(1, 3, now).err(), Some(IssueError::Cap));
        assert_eq!(hub.relay.issue(3, 2, now).err(), Some(IssueError::Cap));
        assert!(
            hub.relay
                .live(now)
                .all(|g| (g.sender, g.recipient) == (1, 2))
        );
    }
    assert!(hub.relay.live(4100).all(|g| g.session != old.session));
}

#[test]
fn a_refused_renewal_is_retried_on_a_later_tick() {
    let (mut hub, old) = hub_at_cap_with_bound_pair();
    // fill the grant table with other pairs, so the renewal is refused
    let fillers: Vec<RelayGrant> = (0..MAX_GRANTS as u16 - 1)
        .map(|i| hub.relay.issue(10 + 2 * i, 11 + 2 * i, 0).unwrap())
        .collect();
    assert!(hub.tick(3600).is_empty());
    assert!(!hub.relay.withdraw(fillers[0].session).is_empty());
    let renewed = grants(&hub.tick(3700));
    assert_eq!(renewed.len(), 2);
    assert_ne!(renewed[0].1.session, old.session);
    assert!(hub.tick(3800).is_empty());
}

fn hub_with_refused_renewal() -> (Hub, RelayGrant) {
    let (mut hub, old) = hub_at_cap_with_bound_pair();
    for i in 0..MAX_GRANTS as u16 - 1 {
        hub.relay.issue(10 + 2 * i, 11 + 2 * i, 0).unwrap();
    }
    assert!(hub.tick(3600).is_empty());
    assert_eq!(hub.failed_offers, 1);
    (hub, old)
}

#[test]
fn refused_renewal_recovers_after_full_table_expiry_without_an_announce() {
    let (mut hub, old) = hub_with_refused_renewal();
    // public expiry may run before the hub's next tick; retry authorization survives it
    hub.relay.expire(4000);
    let recovered = grants(&hub.tick(4100));
    assert_eq!(recovered.len(), 2);
    let new = &recovered[0].1;
    assert_ne!(new.session, old.session);
    assert_eq!((new.sender, new.recipient), (1, 2));
    assert!(hub.tick(4200).is_empty());
    assert_eq!(hub.relay.live(4200).count(), 1);
    assert_eq!(hub.relay.issue(1, 3, 4200).err(), Some(IssueError::Cap));
    assert_eq!(hub.relay.issue(3, 2, 4200).err(), Some(IssueError::Cap));
}

#[test]
fn post_expiry_close_requires_an_endpoint_and_cancels_recovery() {
    for sender in [1, 3] {
        let (mut hub, old) = hub_with_refused_renewal();
        hub.relay.expire(4000);
        let close = encode_game(
            &Signal::RelayClose {
                relay: 0,
                session: old.session,
                hub_generation: GEN,
            },
            sender,
            true,
        )
        .unwrap();
        let closed = hub.on_game(4001, &close);
        let recovered = grants(&hub.tick(4100));
        if sender == 1 {
            assert_eq!(closed.len(), 1);
            assert_eq!(closed[0].to, 2);
            assert!(recovered.is_empty());
        } else {
            assert!(closed.is_empty());
            assert_eq!(recovered.len(), 2);
        }
    }
}

#[test]
fn closing_an_announce_grant_after_a_refused_renewal_revives_nothing() {
    let (mut hub, old) = hub_with_refused_renewal();
    hub.relay.expire(4000);
    // the same companion again, same generation: the pair has no live grant, so a fresh one
    let fresh = grants(&hub.on_game(4001, &announce(1, 11, CAP_UDP)));
    assert_eq!(fresh.len(), 2);
    let session = fresh[0].1.session;
    assert_ne!(session, old.session);
    let close = encode_game(
        &Signal::RelayClose {
            relay: 0,
            session,
            hub_generation: GEN,
        },
        1,
        true,
    )
    .unwrap();
    let closed = hub.on_game(4002, &close);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].to, 2);
    // the endpoint ended the pair's grant: the old retry must not mint another
    assert!(grants(&hub.tick(4100)).is_empty());
    assert_eq!(hub.relay.live(4100).count(), 0);
}

#[test]
fn direct_withdraw_after_expiry_cancels_recovery() {
    let (mut hub, old) = hub_with_refused_renewal();
    hub.relay.expire(4000);
    assert_eq!(hub.relay.withdraw(old.session).len(), 2);
    assert!(hub.tick(4100).is_empty());
    assert_eq!(hub.relay.live(4100).count(), 0);
}

#[test]
fn relay_generation_reset_even_to_the_same_value_cancels_recovery() {
    let (mut hub, _) = hub_with_refused_renewal();
    hub.relay.set_generation(GEN);
    assert!(hub.tick(4100).is_empty());
    assert_eq!(hub.relay.live(4100).count(), 0);
}

#[test]
fn restarted_endpoint_does_not_recover_a_stale_pair() {
    for capabilities in [CAP_UDP, CAP_MEDIA_V2] {
        let (mut hub, old) = hub_with_refused_renewal();
        hub.relay.expire(4000);
        let fresh = grants(&hub.on_game(4001, &announce(1, 12, capabilities)));
        if capabilities == CAP_UDP {
            assert_eq!(fresh.len(), 2);
            assert_ne!(fresh[0].1.session, old.session);
        } else {
            assert!(fresh.is_empty());
        }
        assert!(hub.tick(4100).is_empty());
        assert_eq!(
            hub.relay.live(4100).count(),
            usize::from(capabilities == CAP_UDP)
        );
    }
}

#[test]
fn dropping_udp_capability_withdraws_live_grants() {
    let (mut hub, old) = hub_at_cap_with_bound_pair();
    let closed = hub.on_game(3550, &announce(1, 11, CAP_MEDIA_V2));
    assert_eq!(closed.len(), 2);
    assert!(closed.iter().all(|f| matches!(
        decode_game(&f.frame, false),
        Ok((_, Signal::RelayClose { session, .. })) if session == old.session
    )));
    assert!(hub.relay.live(3550).next().is_none());
    assert_eq!(
        hub.relay
            .handle(3550, addr(1, 40000), &wrap(old.session, &media(1))),
        Err(Dropped::Expired)
    );
    assert!(hub.tick(4100).is_empty());
}

#[test]
fn refused_renewal_deadline_is_fixed_and_finite() {
    let (mut hub, _) = hub_with_refused_renewal();
    assert!(hub.tick(3700).is_empty());
    hub.relay.expire(4000);
    // retries never extend the original expiry + 30 s deadline
    assert!(hub.tick(34_001).is_empty());
    assert!(hub.tick(34_002).is_empty());
    assert_eq!(hub.relay.live(34_002).count(), 0);
}

/// both companions behind port-restricted NATs: the hub reflects each public mapping in
/// its challenge and forwards between the mapped addresses
#[test]
fn hub_forwards_between_natted_players_through_the_lab_router() {
    use router::{Core, Edge, Filtering, Mapping, NatConfig};
    let ms = Duration::from_millis;
    let mut core = Core::new(3);
    let private = |n: u16| SocketAddr::from(([127, 0, 0, 1], 9100 + n));
    let hub_public = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 1), 34560);
    let edge = Edge {
        delay: ms(5),
        ..Edge::default()
    };
    core.add_server(private(0), hub_public, edge);
    for (n, host) in [(1, 1), (2, 2)] {
        let nat = NatConfig {
            id: n as u32,
            mapping: Mapping::EndpointDependent,
            filtering: Filtering::AddressAndPortDependent,
            ..NatConfig::default()
        };
        core.add_player(private(n), Ipv4Addr::new(203, 0, 113, host), nat, edge);
    }
    let mut relay = relay();
    let grant = relay.issue(1, 2, 0).unwrap();
    let mut clients = [
        BindClient::new(grant.clone(), 1, 1).unwrap(),
        BindClient::new(grant.clone(), 2, 1).unwrap(),
    ];
    let mut now = Duration::ZERO;
    // one router step: deliver what is due; the hub answers, players hand back to `on`
    let step = |core: &mut Core, relay: &mut Relay, now: Duration| {
        let mut to_players = Vec::new();
        for d in core.due(now) {
            if d.to == private(0) {
                let from = SocketAddr::V4(d.from);
                match relay.handle(now.as_millis() as u64, from, &d.payload) {
                    Ok(Reply::Forward(SocketAddr::V4(to))) => {
                        core.send(now, private(0), to, &d.payload)
                    }
                    Ok(Reply::Send(SocketAddr::V4(to), bytes)) => {
                        core.send(now, private(0), to, &bytes)
                    }
                    other => panic!("hub dropped {other:?}"),
                }
            } else {
                to_players.push(d);
            }
        }
        to_players
    };
    for (i, client) in clients.iter().enumerate() {
        core.send(now, private(i as u16 + 1), hub_public, &client.request());
    }
    let mut mapped = [None, None];
    let mut received = Vec::new();
    while now < ms(200) {
        now += ms(1);
        for d in step(&mut core, &mut relay, now) {
            let i = if d.to == private(1) { 0 } else { 1 };
            assert_eq!(d.from, hub_public);
            match decode_datagram(&d.payload).unwrap() {
                Datagram::Bind(Bind::Challenge(c)) => {
                    let commit = clients[i].on_challenge(c).unwrap();
                    core.send(now, d.to, hub_public, &commit);
                }
                Datagram::BindAck { challenge, proof } => {
                    mapped[i] = clients[i].on_ack(challenge, &proof);
                }
                Datagram::Relay { session, datagram } => {
                    received.push((i, session, datagram.to_vec()))
                }
                _ => panic!("unexpected datagram"),
            }
        }
        if mapped.iter().all(Option::is_some) && received.is_empty() && now == ms(100) {
            core.send(now, private(1), hub_public, &wrap(grant.session, &media(1)));
            core.send(now, private(2), hub_public, &wrap(grant.session, &media(2)));
        }
    }
    // each sees its own NAT mapping, different from its private address
    let [Some(SocketAddr::V4(a)), Some(SocketAddr::V4(b))] = mapped else {
        panic!("not both bound: {mapped:?}")
    };
    assert_eq!(
        (*a.ip(), *b.ip()),
        (Ipv4Addr::new(203, 0, 113, 1), Ipv4Addr::new(203, 0, 113, 2))
    );
    assert_eq!(
        received,
        vec![(1, grant.session, media(1)), (0, grant.session, media(2))]
    );
}
