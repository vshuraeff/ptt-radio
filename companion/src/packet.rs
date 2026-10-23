//! wire format shared with the mod (see ptt-radio/DESIGN.md, "Формат пакетов").
//!
//! companion -> game: R, 1, kind, seq u16 BE, body
//! game -> companion: R, 1, kind, sender u16 BE, rest
//!   forwarded START/VOICE/STOP: rest = seq u16 BE + body
//!   ROSTER: rest = utf-8 lines "index\tname\tr,g,b"
//!   MIC_STATE: rest = one byte, 0 open mic off, 1 on

pub const MAGIC: u8 = 0x52;
pub const VERSION: u8 = 1;

pub const HELLO: u8 = 0x01;
pub const START: u8 = 0x02;
pub const VOICE: u8 = 0x03;
pub const STOP: u8 = 0x04;
pub const ROSTER: u8 = 0x10;
pub const MIC_STATE: u8 = 0x11;

pub fn encode(kind: u8, seq: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.extend_from_slice(&[MAGIC, VERSION, kind]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(body);
    out
}

#[derive(Debug, PartialEq)]
pub struct RosterEntry {
    pub index: u16,
    pub name: String,
    pub color: String,
}

#[derive(Debug, PartialEq)]
pub enum Incoming<'a> {
    Start {
        sender: u16,
        seq: u16,
    },
    Voice {
        sender: u16,
        seq: u16,
        frame: &'a [u8],
    },
    Stop {
        sender: u16,
        seq: u16,
    },
    Roster(Vec<RosterEntry>),
    // `player` is the receiving player's own index: the mod sends MIC_STATE only to that player's companion
    MicState {
        player: u16,
        open: bool,
    },
}

/// returns None for anything that is not a well-formed packet of a known kind
pub fn parse(data: &[u8]) -> Option<Incoming<'_>> {
    let [MAGIC, VERSION, kind, s_hi, s_lo, rest @ ..] = data else {
        return None;
    };
    let sender = u16::from_be_bytes([*s_hi, *s_lo]);
    match (*kind, rest) {
        (START, [q_hi, q_lo, ..]) => Some(Incoming::Start {
            sender,
            seq: u16::from_be_bytes([*q_hi, *q_lo]),
        }),
        (STOP, [q_hi, q_lo, ..]) => Some(Incoming::Stop {
            sender,
            seq: u16::from_be_bytes([*q_hi, *q_lo]),
        }),
        (VOICE, [q_hi, q_lo, frame @ ..]) if !frame.is_empty() => Some(Incoming::Voice {
            sender,
            seq: u16::from_be_bytes([*q_hi, *q_lo]),
            frame,
        }),
        (ROSTER, text) => Some(Incoming::Roster(parse_roster(&String::from_utf8_lossy(
            text,
        )))),
        (MIC_STATE, [state @ (0 | 1)]) => Some(Incoming::MicState {
            player: sender,
            open: *state == 1,
        }),
        _ => None,
    }
}

fn parse_roster(text: &str) -> Vec<RosterEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let index = fields.next()?.parse().ok()?;
            let name = fields.next()?.to_string();
            let color = fields.next().unwrap_or("").to_string();
            Some(RosterEntry { index, name, color })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // what the mod's fan_out produces: header(kind, sender) .. string.sub(payload, 4)
    fn forwarded(own: &[u8], sender: u16) -> Vec<u8> {
        let mut out = vec![MAGIC, VERSION, own[2]];
        out.extend_from_slice(&sender.to_be_bytes());
        out.extend_from_slice(&own[3..]);
        out
    }

    #[test]
    fn encode_layout() {
        assert_eq!(encode(VOICE, 0x0102, &[9, 8]), vec![0x52, 1, 3, 1, 2, 9, 8]);
        assert_eq!(encode(HELLO, 0, b"v"), vec![0x52, 1, 1, 0, 0, b'v']);
    }

    #[test]
    fn forwarded_voice_round_trip() {
        let own = encode(VOICE, 513, &[0, 7, 0]);
        let back = forwarded(&own, 3);
        assert_eq!(
            parse(&back),
            Some(Incoming::Voice {
                sender: 3,
                seq: 513,
                frame: &[0, 7, 0]
            })
        );
    }

    #[test]
    fn forwarded_start_and_stop() {
        assert_eq!(
            parse(&forwarded(&encode(START, 41, &[]), 1)),
            Some(Incoming::Start { sender: 1, seq: 41 })
        );
        assert_eq!(
            parse(&forwarded(&encode(STOP, 42, &[]), 1)),
            Some(Incoming::Stop { sender: 1, seq: 42 })
        );
        // the mod's synthetic STOP for a dead companion carries seq 0
        assert_eq!(
            parse(&[0x52, 1, STOP, 0, 7, 0, 0]),
            Some(Incoming::Stop { sender: 7, seq: 0 })
        );
    }

    #[test]
    fn roster() {
        let mut data = vec![0x52, 1, ROSTER, 0, 0];
        data.extend_from_slice(b"0\tserver\t204,204,204\n1\tvshuraeff\t255,0,10\nbroken line");
        let Some(Incoming::Roster(entries)) = parse(&data) else {
            panic!("not a roster")
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1],
            RosterEntry {
                index: 1,
                name: "vshuraeff".into(),
                color: "255,0,10".into()
            }
        );
    }

    #[test]
    fn mic_state() {
        assert_eq!(
            parse(&[0x52, 1, MIC_STATE, 0, 1, 1]),
            Some(Incoming::MicState {
                player: 1,
                open: true
            })
        );
        assert_eq!(
            parse(&[0x52, 1, MIC_STATE, 0, 1, 0]),
            Some(Incoming::MicState {
                player: 1,
                open: false
            })
        );
        // mod v0.1 sent a blind TOGGLE without a state byte; values other than 0/1 are not defined
        assert_eq!(parse(&[0x52, 1, MIC_STATE, 0, 1]), None);
        assert_eq!(parse(&[0x52, 1, MIC_STATE, 0, 1, 2]), None);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0x52, 1, VOICE, 0]), None);
        assert_eq!(parse(&[0x52, 2, VOICE, 0, 1, 0, 1, 9]), None);
        assert_eq!(parse(&[0x51, 1, VOICE, 0, 1, 0, 1, 9]), None);
        assert_eq!(parse(&[0x52, 1, VOICE, 0, 1, 0, 1]), None);
        assert_eq!(parse(&[0x52, 1, START, 0, 1, 0]), None);
        assert_eq!(parse(&[0x52, 1, 0x7f, 0, 1, 0, 1]), None);
    }
}
