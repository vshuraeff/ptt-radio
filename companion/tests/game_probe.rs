use ptt_radio_companion::transport::signaling::{self as signal, Signal};
use ptt_radio_companion::transport::wire::{CodecError, MAX_DATAGRAM};

#[test]
fn game_probe_has_fixed_bytes_and_preserves_every_token_bit() {
    for (probe, token) in [
        (0, [0; 8]),
        (
            0x8182838485868788,
            [0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88],
        ),
        (u64::MAX, [255; 8]),
    ] {
        for reply in [false, true] {
            let value = Signal::GameProbe { probe, reply };
            let mut expected = vec![2, 11];
            expected.extend_from_slice(&token);
            expected.push(u8::from(reply));
            assert_eq!(signal::encode(&value).unwrap(), expected);
            assert!(signal::decode(&expected).unwrap() == value);

            for peer in [0u16, 0x1234, u16::MAX] {
                for inbound in [false, true] {
                    let mut envelope = vec![0x52, 1, if inbound { 0x13 } else { 0x06 }];
                    envelope.extend_from_slice(&peer.to_be_bytes());
                    envelope.extend_from_slice(&expected);
                    assert_eq!(
                        signal::encode_game(&value, peer, inbound).unwrap(),
                        envelope
                    );
                    let (decoded_peer, decoded) = signal::decode_game(&envelope, inbound).unwrap();
                    assert_eq!(decoded_peer, peer);
                    assert!(decoded == value);
                    assert!(matches!(
                        signal::decode_game(&envelope, !inbound),
                        Err(CodecError::UnknownType)
                    ));
                }
            }
        }
    }
}

#[test]
fn game_probe_rejects_truncation_trailing_bytes_and_non_boolean_replies() {
    for reply in [false, true] {
        let value = Signal::GameProbe {
            probe: 0x0102030405060708,
            reply,
        };
        let bytes = signal::encode(&value).unwrap();
        for n in 0..bytes.len() {
            assert!(matches!(
                signal::decode(&bytes[..n]),
                Err(CodecError::Truncated)
            ));
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(
            signal::decode(&trailing),
            Err(CodecError::Trailing)
        ));

        for inbound in [false, true] {
            let envelope = signal::encode_game(&value, 513, inbound).unwrap();
            for n in 0..envelope.len() {
                assert!(matches!(
                    signal::decode_game(&envelope[..n], inbound),
                    Err(CodecError::Truncated)
                ));
            }
            let mut trailing = envelope.clone();
            trailing.push(0);
            assert!(matches!(
                signal::decode_game(&trailing, inbound),
                Err(CodecError::Trailing)
            ));
            for invalid in 2..=255 {
                let mut malformed = envelope.clone();
                *malformed.last_mut().unwrap() = invalid;
                assert!(matches!(
                    signal::decode_game(&malformed, inbound),
                    Err(CodecError::Invalid)
                ));
            }
            let mut oversized = envelope;
            oversized.resize(MAX_DATAGRAM + 1, 0);
            assert!(matches!(
                signal::decode_game(&oversized, inbound),
                Err(CodecError::Oversize)
            ));
        }
        for invalid in 2..=255 {
            let mut malformed = bytes.clone();
            *malformed.last_mut().unwrap() = invalid;
            assert!(matches!(
                signal::decode(&malformed),
                Err(CodecError::Invalid)
            ));
        }
        let mut wrong_schema = bytes;
        wrong_schema[0] = 1;
        assert!(matches!(
            signal::decode(&wrong_schema),
            Err(CodecError::UnsupportedVersion)
        ));
    }
}
