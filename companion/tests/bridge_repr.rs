//! the game bridge sends each listener one representation of a sender
//! like control.lua, the relay emulator keeps v2 for
//! companions whose HELLO body ends with " v2", converts v2 to v1 for the
//! rest, never gives a v2 listener a v1 copy of a v2 sender, and sends the
//! synthetic timeout STOP to legacy listeners only.

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::thread;
use std::time::Duration;

use ptt_radio_companion::packet::{self, HELLO, START, VOICE};
use ptt_radio_companion::relay::{Config, Player, Relay};
use ptt_radio_companion::transport::StreamId;
use ptt_radio_companion::transport::wire::{self, GameMedia, Kind};

const GENERATION: u64 = 0x0102_0304_0506_0708;

struct Bridge {
    relay: Relay,
    companions: Vec<UdpSocket>,
}

/// players 1, 2, ... with the given echo settings, no delay or jitter
fn bridge(echo: &[bool]) -> Bridge {
    let companions: Vec<UdpSocket> = echo
        .iter()
        .map(|_| {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket
        })
        .collect();
    let players = companions
        .iter()
        .zip(echo)
        .map(|(socket, &echo)| {
            let player = Player {
                name: "test".into(),
                companion_port: socket.local_addr().unwrap().port(),
                echo,
                open_mic: false,
                half_duplex: false,
            };
            (0, player)
        })
        .collect();
    let config = Config {
        delay: Duration::ZERO,
        jitter: Duration::ZERO,
        stall: None,
    };
    Bridge {
        relay: Relay::start(config, players).unwrap(),
        companions,
    }
}

impl Bridge {
    fn send(&self, index: u16, data: &[u8]) {
        self.companions[index as usize - 1]
            .send_to(data, ("127.0.0.1", self.relay.game_port(index)))
            .unwrap();
    }

    fn recv(&self, index: u16) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let len = self.companions[index as usize - 1].recv(&mut buf).unwrap();
        buf[..len].to_vec()
    }

    /// HELLO, then the ROSTER, MIC_STATE, HALF_DUPLEX and AUDIENCE it is answered with
    fn hello(&self, index: u16, body: &[u8]) {
        self.send(index, &packet::encode(HELLO, 0, body));
        for _ in 0..4 {
            self.recv(index);
        }
    }

    fn quiet(&self, index: u16) {
        let socket = &self.companions[index as usize - 1];
        socket
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        let mut buf = [0; 2048];
        let err = socket.recv(&mut buf).unwrap_err();
        assert!(matches!(
            err.kind(),
            ErrorKind::TimedOut | ErrorKind::WouldBlock
        ));
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
    }
}

fn media(player: u16, kind: Kind, sequence: u16, talkspurt: u16, opus: &[u8]) -> GameMedia<'_> {
    GameMedia {
        kind,
        stream: StreamId {
            player,
            generation: GENERATION,
        },
        sequence,
        talkspurt,
        opus,
    }
}

/// what a companion sends: no sender index
fn own(kind: Kind, sequence: u16, talkspurt: u16, opus: &[u8]) -> Vec<u8> {
    wire::encode_game(media(0, kind, sequence, talkspurt, opus), false).unwrap()
}

/// what a v2 listener gets: the sender index after the kind
fn forwarded(sender: u16, kind: Kind, sequence: u16, talkspurt: u16, opus: &[u8]) -> Vec<u8> {
    wire::encode_game(media(sender, kind, sequence, talkspurt, opus), true).unwrap()
}

#[test]
fn capability_comes_from_the_hello_body() {
    let bridge = bridge(&[false, false]);
    let start = own(Kind::Start, 10, 1, &[]);
    let legacy_start = [0x52, 1, START, 0, 1, 0, 10];
    for body in [&b""[..], b"0.4.0", b"0.4.0v2", b"0.4.0 v2 ", b"0.4.0 V2"] {
        bridge.hello(2, body);
        bridge.send(1, &start);
        assert_eq!(bridge.recv(2), legacy_start, "{body:?}");
    }
    bridge.hello(2, b"ptt-radio-companion 0.4.0 v2");
    bridge.send(1, &start);
    assert_eq!(bridge.recv(2), forwarded(1, Kind::Start, 10, 1, &[]));
    // a later HELLO without the token makes the companion legacy again
    bridge.hello(2, b"ptt-radio-companion 0.4.0");
    bridge.send(1, &start);
    assert_eq!(bridge.recv(2), legacy_start);
    bridge.relay.stop();
}

#[test]
fn mixed_listeners_get_one_representation_each() {
    // 1 and 2 speak v2, 3 is legacy
    let bridge = bridge(&[false, false, false]);
    bridge.hello(1, b"ptt-radio-companion 0.4.0 v2");
    bridge.hello(2, b"ptt-radio-companion 0.4.0 v2");
    bridge.hello(3, b"ptt-radio-companion 0.4.0");

    bridge.send(1, &own(Kind::Start, 10, 3, &[]));
    assert_eq!(bridge.recv(2), forwarded(1, Kind::Start, 10, 3, &[]));
    assert_eq!(bridge.recv(3), [0x52, 1, 0x02, 0, 1, 0, 10]);

    // a v1 copy from a v2 sender reaches the legacy listener only: the v2
    // listener's next packet is the VOICE2 sent after it
    bridge.send(1, &packet::encode(VOICE, 11, &[9]));
    bridge.send(1, &own(Kind::Voice, 11, 3, &[4, 0, 2]));
    assert_eq!(bridge.recv(2), forwarded(1, Kind::Voice, 11, 3, &[4, 0, 2]));
    assert_eq!(bridge.recv(3), [0x52, 1, 0x03, 0, 1, 0, 11, 9]);
    assert_eq!(bridge.recv(3), [0x52, 1, 0x03, 0, 1, 0, 11, 4, 0, 2]);

    // a legacy sender reaches every listener unchanged
    bridge.send(3, &packet::encode(VOICE, 7, &[5]));
    assert_eq!(bridge.recv(1), [0x52, 1, 0x03, 0, 3, 0, 7, 5]);
    assert_eq!(bridge.recv(2), [0x52, 1, 0x03, 0, 3, 0, 7, 5]);

    bridge.send(1, &own(Kind::Stop, 12, 3, &[]));
    assert_eq!(bridge.recv(2), forwarded(1, Kind::Stop, 12, 3, &[]));
    assert_eq!(bridge.recv(3), [0x52, 1, 0x04, 0, 1, 0, 12]);
    bridge.quiet(1);
    bridge.relay.stop();
}

#[test]
fn self_echo_follows_the_senders_own_representation() {
    // both hear themselves; 1 speaks v2, 2 is legacy
    let bridge = bridge(&[true, true]);
    bridge.hello(1, b"ptt-radio-companion 0.4.0 v2");
    bridge.hello(2, b"ptt-radio-companion 0.4.0");

    bridge.send(1, &own(Kind::Start, 1, 1, &[]));
    assert_eq!(bridge.recv(1), forwarded(1, Kind::Start, 1, 1, &[]));
    assert_eq!(bridge.recv(2), [0x52, 1, 0x02, 0, 1, 0, 1]);
    // the v2 sender's own v1 copy does not come back to it
    bridge.send(1, &packet::encode(START, 1, &[]));
    bridge.send(1, &own(Kind::Voice, 2, 1, &[8]));
    assert_eq!(bridge.recv(1), forwarded(1, Kind::Voice, 2, 1, &[8]));

    bridge.send(2, &own(Kind::Start, 5, 1, &[]));
    assert_eq!(bridge.recv(2), [0x52, 1, 0x02, 0, 1, 0, 1]);
    assert_eq!(bridge.recv(2), [0x52, 1, 0x03, 0, 1, 0, 2, 8]);
    assert_eq!(bridge.recv(2), [0x52, 1, 0x02, 0, 2, 0, 5]);
    assert_eq!(bridge.recv(1), forwarded(2, Kind::Start, 5, 1, &[]));
    bridge.relay.stop();
}

#[test]
fn synthetic_stop_goes_to_legacy_listeners_only() {
    // 1 and 2 speak v2, 3 is legacy
    let bridge = bridge(&[false, false, false]);
    bridge.hello(1, b"ptt-radio-companion 0.4.0 v2");
    bridge.hello(2, b"ptt-radio-companion 0.4.0 v2");
    bridge.hello(3, b"ptt-radio-companion 0.4.0");
    bridge.send(1, &own(Kind::Start, 1, 1, &[]));
    bridge.recv(2);
    bridge.recv(3);
    bridge.send(3, &packet::encode(START, 1, &[]));
    bridge.recv(1);
    bridge.recv(2);
    // both badges time out after 30 game ticks without a frame
    thread::sleep(Duration::from_millis(750));
    assert_eq!(bridge.recv(3), [0x52, 1, 0x04, 0, 1, 0, 0]);
    // sender 3's badge may expire one 15-tick check later; anything sent meanwhile stays queued
    thread::sleep(Duration::from_millis(300));
    bridge.quiet(1);
    bridge.quiet(2);
    bridge.quiet(3);
    bridge.relay.stop();
}
