//! wire format shared with the mod (see ptt-radio/DESIGN.md, "Формат пакетов").
//!
//! companion -> game: R, 1, kind, seq u16 BE, body
//! game -> companion: R, 1, kind, sender u16 BE, rest
//!   forwarded START/VOICE/STOP: rest = seq u16 BE + body
//!   ROSTER: rest = utf-8 lines "index\tname\tr,g,b"
//!   MIC_STATE: rest = one byte, 0 open mic off, 1 on
//!   HALF_DUPLEX: rest = one byte, 0 off, 1 mute incoming voices while PTT is held
//!   SELF_TEST: exactly 7 bytes, header + test_id u16 BE (mod -> own companion)
//!   SELF_TEST_RESULT: exactly 16 bytes, companion -> game, see `SelfTestResult`

pub const MAGIC: u8 = 0x52;
pub const VERSION: u8 = 1;

pub const HELLO: u8 = 0x01;
pub const START: u8 = 0x02;
pub const VOICE: u8 = 0x03;
pub const STOP: u8 = 0x04;
pub const ECHO_STATE: u8 = 0x05;
pub const ROSTER: u8 = 0x10;
pub const MIC_STATE: u8 = 0x11;
pub const HALF_DUPLEX: u8 = 0x12;
pub const SELF_TEST: u8 = 0x16;
pub const SELF_TEST_RESULT: u8 = 0x18;

const SELF_TEST_LEN: usize = 7;
const SELF_TEST_RESULT_LEN: usize = 16;

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
    // sent only to the player's own companion, like MIC_STATE
    HalfDuplex {
        on: bool,
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
        (HALF_DUPLEX, [state @ (0 | 1)]) => Some(Incoming::HalfDuplex { on: *state == 1 }),
        _ => None,
    }
}

/// returns the test id of a SELF_TEST packet, None for any other shape or length
pub fn parse_self_test(data: &[u8]) -> Option<u16> {
    match data {
        [MAGIC, VERSION, SELF_TEST, 0, 0, hi, lo] if data.len() == SELF_TEST_LEN => {
            Some(u16::from_be_bytes([*hi, *lo]))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok = 0,
    TxNotPermitted = 1,
    MicOpenFailed = 2,
    NoFramesReturned = 3,
    Busy = 4,
}

impl Outcome {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Ok),
            1 => Some(Self::TxNotPermitted),
            2 => Some(Self::MicOpenFailed),
            3 => Some(Self::NoFramesReturned),
            4 => Some(Self::Busy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfTestResult {
    pub test_id: u16,
    pub outcome: Outcome,
    // 1 when the local test tone was played
    pub tone: u8,
    // -127..=0, -128 means no capture
    pub mic_peak_dbfs: i8,
    pub frames_sent: u16,
    pub frames_returned: u16,
    pub first_return_ms: u16,
}

pub fn encode_self_test_result(result: &SelfTestResult) -> Vec<u8> {
    let mut out = Vec::with_capacity(SELF_TEST_RESULT_LEN);
    out.extend_from_slice(&[MAGIC, VERSION, SELF_TEST_RESULT, 0, 0]);
    out.extend_from_slice(&result.test_id.to_be_bytes());
    out.push(result.outcome as u8);
    out.push(result.tone);
    out.extend_from_slice(&result.mic_peak_dbfs.to_be_bytes());
    out.extend_from_slice(&result.frames_sent.to_be_bytes());
    out.extend_from_slice(&result.frames_returned.to_be_bytes());
    out.extend_from_slice(&result.first_return_ms.to_be_bytes());
    out
}

/// returns None unless `data` is exactly one well-formed SELF_TEST_RESULT
pub fn decode_self_test_result(data: &[u8]) -> Option<SelfTestResult> {
    let [
        MAGIC,
        VERSION,
        SELF_TEST_RESULT,
        _,
        _,
        id_hi,
        id_lo,
        outcome,
        tone,
        peak,
        sent_hi,
        sent_lo,
        ret_hi,
        ret_lo,
        ms_hi,
        ms_lo,
    ] = data
    else {
        return None;
    };
    Some(SelfTestResult {
        test_id: u16::from_be_bytes([*id_hi, *id_lo]),
        outcome: Outcome::from_byte(*outcome)?,
        tone: *tone,
        mic_peak_dbfs: i8::from_be_bytes([*peak]),
        frames_sent: u16::from_be_bytes([*sent_hi, *sent_lo]),
        frames_returned: u16::from_be_bytes([*ret_hi, *ret_lo]),
        first_return_ms: u16::from_be_bytes([*ms_hi, *ms_lo]),
    })
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
    fn half_duplex() {
        assert_eq!(
            parse(&[0x52, 1, HALF_DUPLEX, 0, 1, 1]),
            Some(Incoming::HalfDuplex { on: true })
        );
        assert_eq!(
            parse(&[0x52, 1, HALF_DUPLEX, 0, 1, 0]),
            Some(Incoming::HalfDuplex { on: false })
        );
        assert_eq!(parse(&[0x52, 1, HALF_DUPLEX, 0, 1, 2]), None);
    }

    #[test]
    fn self_test_exact_length() {
        assert_eq!(
            parse_self_test(&[0x52, 1, SELF_TEST, 0, 0, 0x12, 0x34]),
            Some(0x1234)
        );
        assert_eq!(
            parse_self_test(&[0x52, 1, SELF_TEST, 0, 0, 0xff, 0xff]),
            Some(65535)
        );
        assert_eq!(parse_self_test(&[0x52, 1, SELF_TEST, 0, 0, 0x12]), None);
        assert_eq!(
            parse_self_test(&[0x52, 1, SELF_TEST, 0, 0, 0x12, 0x34, 0]),
            None
        );
        assert_eq!(parse_self_test(&[]), None);
    }

    #[test]
    fn self_test_rejects_wrong_header() {
        assert_eq!(parse_self_test(&[0x51, 1, SELF_TEST, 0, 0, 0, 1]), None);
        assert_eq!(parse_self_test(&[0x52, 2, SELF_TEST, 0, 0, 0, 1]), None);
        assert_eq!(
            parse_self_test(&[0x52, 1, SELF_TEST_RESULT, 0, 0, 0, 1]),
            None
        );
        // the reserved field must be zero
        assert_eq!(parse_self_test(&[0x52, 1, SELF_TEST, 0, 1, 0, 1]), None);
        // the generic parser does not know the kind
        assert_eq!(parse(&[0x52, 1, SELF_TEST, 0, 0, 0, 1]), None);
    }

    fn sample_result() -> SelfTestResult {
        SelfTestResult {
            test_id: 0xabcd,
            outcome: Outcome::Ok,
            tone: 1,
            mic_peak_dbfs: -20,
            frames_sent: 150,
            frames_returned: 148,
            first_return_ms: 90,
        }
    }

    #[test]
    fn self_test_result_layout() {
        let bytes = encode_self_test_result(&sample_result());
        assert_eq!(bytes.len(), 16);
        assert_eq!(
            bytes,
            vec![
                0x52, 1, 0x18, 0, 0, 0xab, 0xcd, 0, 1, 0xec, 0, 150, 0, 148, 0, 90
            ]
        );
    }

    #[test]
    fn self_test_result_round_trip() {
        for (outcome, peak) in [
            (Outcome::Ok, 0),
            (Outcome::TxNotPermitted, -1),
            (Outcome::MicOpenFailed, -127),
            (Outcome::NoFramesReturned, -128),
            (Outcome::Busy, -128),
        ] {
            let result = SelfTestResult {
                outcome,
                mic_peak_dbfs: peak,
                ..sample_result()
            };
            assert_eq!(
                decode_self_test_result(&encode_self_test_result(&result)),
                Some(result)
            );
        }
        let bytes = encode_self_test_result(&SelfTestResult {
            mic_peak_dbfs: -128,
            ..sample_result()
        });
        assert_eq!(bytes[9], 0x80);
    }

    #[test]
    fn self_test_result_rejects_malformed() {
        let good = encode_self_test_result(&sample_result());
        assert_eq!(decode_self_test_result(&good[..15]), None);
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode_self_test_result(&long), None);
        for (at, value) in [(0, 0x51), (1, 2), (2, SELF_TEST), (7, 5)] {
            let mut bad = good.clone();
            bad[at] = value;
            assert_eq!(decode_self_test_result(&bad), None, "byte {at}");
        }
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
