//! shared p2p media, transport and signaling contracts consumed by runtime.rs.

pub mod hub;
#[cfg(feature = "audio")]
pub mod lab;
pub mod relay;
pub mod signaling;
pub mod wire;

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamId {
    pub player: u16,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MediaId {
    pub stream: StreamId,
    pub sequence: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialOrder {
    Older,
    Equal,
    Newer,
    Ambiguous,
}

/// compares talkspurts (or wire sequences) in a window smaller than 32768.
pub fn serial_order(value: u16, reference: u16) -> SerialOrder {
    match value.wrapping_sub(reference) {
        0 => SerialOrder::Equal,
        1..=32767 => SerialOrder::Newer,
        32768 => SerialOrder::Ambiguous,
        _ => SerialOrder::Older,
    }
}

/// expands near the accepted high water mark; the caller advances it only for newer frames.
/// half-range ambiguity and u32 exhaustion require dropping/resetting, not guessing.
/// initialise the first reference to 0x10000 | u32::from(first_sequence), leaving
/// one wrap of headroom for reordered predecessors at the beginning of a stream.
pub fn extend_sequence(sequence: u16, reference: u32) -> Option<u32> {
    let delta = sequence.wrapping_sub(reference as u16);
    match delta {
        0..=32767 => reference.checked_add(u32::from(delta)),
        32768 => None,
        _ => reference.checked_sub(65536 - u32::from(delta)),
    }
}

/// a delayed packet cannot select its own generation; only trusted ANNOUNCE does.
pub fn selected_stream(selected: StreamId, player: u16, generation: u64) -> Option<StreamId> {
    (selected.player == player && selected.generation == generation).then_some(selected)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressedFrame {
    pub id: MediaId,
    pub talkspurt: u16,
    pub opus: Vec<u8>,
    pub revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertResult {
    Queued,
    Duplicate,
    Late,
    WrongStream,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ordered {
    Frame(CompressedFrame),
    /// receiver chooses PLC, or FEC using next without consuming its normal decode.
    Missing {
        id: MediaId,
        next: Option<CompressedFrame>,
    },
    Wait,
}

/// owned by the receiver before Mixer::voice(sender, seq, opus, now).
/// one instance per selected stream, shared by every route and key epoch.
/// push drops repeats and frames older than the emitted cursor. poll waits for a
/// gap until deadline, emits Missing once, then permanently drops its late frame.
/// the receiver alone feeds ordered frames or PLC/FEC into its one opus decoder;
/// the pcm jitter stage and repeated START restart behavior remain downstream.
pub trait PreDecodeOrder {
    fn stream(&self) -> StreamId;
    fn push(&mut self, frame: CompressedFrame, received_at: Instant) -> InsertResult;
    fn deadline(&self) -> Option<Instant>;
    fn poll(&mut self, now: Instant) -> Ordered;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReorderConfig {
    /// initial policy: measured secondary-minus-primary delay plus 10 ms.
    pub wait: Duration,
    pub max_frames: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Path {
    Game,
    Direct,
    Hub { session: u32 },
    PlayerRelay { player: u16, session: u32 },
}

/// tx supplies separately framed plaintext game bytes and one encrypted UDP
/// datagram copied unchanged across UDP routes. send must not encrypt per path.
/// receive returns raw, untrusted bytes; path/header/relay metadata is not proof
/// of sender identity. authentication, audience and media dedup precede decoding.
/// the socket/connectivity owner below this seam retains source endpoints for
/// ICE consent, rebinding and STUN/media demux before returning accepted traffic.
pub trait Transport {
    type Error;
    fn send(&mut self, peer: u16, path: Path, packet: &[u8]) -> Result<(), Self::Error>;
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<Received>, Self::Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    pub path: Path,
    pub length: usize,
    pub received_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modular_talkspurt_order() {
        assert_eq!(serial_order(0, 65535), SerialOrder::Newer);
        assert_eq!(serial_order(65535, 0), SerialOrder::Older);
        assert_eq!(serial_order(1, 1), SerialOrder::Equal);
        assert_eq!(serial_order(32768, 0), SerialOrder::Ambiguous);
        assert_eq!(serial_order(0, 32768), SerialOrder::Ambiguous);
    }

    #[test]
    fn sequence_wrap_reordering_and_limits() {
        let initial = 0x10000 | u32::from(0u16);
        assert_eq!(extend_sequence(65535, initial), Some(65535));
        assert_eq!(extend_sequence(0, 65535), Some(65536));
        assert_eq!(extend_sequence(65535, 65536), Some(65535));
        assert_eq!(extend_sequence(1, 131070), Some(131073));
        assert_eq!(extend_sequence(7, 7), Some(7));
        assert_eq!(extend_sequence(32768, 0), None);
        assert_eq!(extend_sequence(65535, 0), None);
        assert_eq!(extend_sequence(0, u32::MAX), None);
    }

    #[test]
    fn announce_selects_generation() {
        let selected = StreamId {
            player: 7,
            generation: 42,
        };
        assert_eq!(selected_stream(selected, 7, 42), Some(selected));
        assert_eq!(selected_stream(selected, 7, 41), None);
        assert_eq!(selected_stream(selected, 8, 42), None);
    }
}
