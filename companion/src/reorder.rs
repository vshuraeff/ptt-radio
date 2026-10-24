//! ordering of one stream's compressed frames before its decoder (the
//! transport::PreDecodeOrder contract). a frame that continues the stream comes
//! out of the next poll at once, so in-order delivery waits for nothing; only a
//! frame behind a gap waits, at most `wait` after it arrived. then the gap is
//! reported Missing once (the caller conceals it or decodes FEC from the next
//! frame) and a frame that fills it afterwards is late and dropped. of a long gap
//! only the last MAX_CONCEAL frames are reported; the rest are skipped and late.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::transport::{
    CompressedFrame, InsertResult, MediaId, Ordered, PreDecodeOrder, ReorderConfig, StreamId,
    extend_sequence,
};

/// u13's initial policy is the secondary-minus-primary path delay plus 10 ms. the game
/// path is the only route today, so that delay is 0; the bound of 20 frames is 200 ms of
/// 10 ms frames, the most the pcm queue behind keeps anyway (jitter::MAX_SLICES)
pub const GAME_ONLY: ReorderConfig = ReorderConfig {
    wait: Duration::from_millis(10),
    max_frames: 20,
};

/// extended sequences this far from the newest one cannot be told apart from a wrap
const WINDOW: u32 = 32768;

/// a gap is concealed for at most its last this many frames, 100 ms of 20 ms frames: more
/// would cost the decoder one concealment each for audio the pcm queue (200 ms) sheds
/// anyway, and if that queue runs dry it conceals per slice itself. the frames before
/// are skipped, and a copy of one arriving later is late like any concealed frame
pub const MAX_CONCEAL: u32 = 5;

pub struct Reorder {
    stream: StreamId,
    config: ReorderConfig,
    /// newest extended sequence accepted: the reference for extending the next one
    newest: Option<u32>,
    /// next sequence to emit; set by the first frame
    next: Option<u32>,
    /// bit i: sequence next - 1 - i was emitted as a frame, not as Missing
    emitted: u64,
    queue: BTreeMap<u32, (CompressedFrame, Instant)>,
}

impl Reorder {
    pub fn set_wait(&mut self, wait: Duration) {
        self.config.wait = wait.min(Duration::from_millis(200));
    }

    pub fn new(stream: StreamId, config: ReorderConfig) -> Self {
        Reorder {
            stream,
            config,
            newest: None,
            next: None,
            emitted: 0,
            queue: BTreeMap::new(),
        }
    }

    /// the identity of a wire sequence of this stream; None when it is half the u16 range
    /// away from the newest frame or outside u32
    pub fn media_id(&self, sequence: u16) -> Option<MediaId> {
        // the first frame keeps one wrap below it for reordered predecessors
        let reference = self.newest.unwrap_or(0x10000 | u32::from(sequence));
        Some(MediaId {
            stream: self.stream,
            sequence: extend_sequence(sequence, reference)?,
        })
    }

    fn advance(&mut self, next: u32, emitted: bool) {
        self.emitted = (self.emitted << 1) | u64::from(emitted);
        self.next = Some(next + 1);
    }
}

impl PreDecodeOrder for Reorder {
    fn stream(&self) -> StreamId {
        self.stream
    }

    fn push(&mut self, frame: CompressedFrame, received_at: Instant) -> InsertResult {
        if frame.id.stream != self.stream {
            return InsertResult::WrongStream;
        }
        let seq = frame.id.sequence;
        if self
            .newest
            .is_some_and(|newest| seq.abs_diff(newest) >= WINDOW)
        {
            return InsertResult::Ambiguous;
        }
        if let Some(next) = self.next
            && seq < next
        {
            let behind = next - 1 - seq;
            return if behind < u64::BITS && (self.emitted >> behind) & 1 == 1 {
                InsertResult::Duplicate
            } else {
                InsertResult::Late
            };
        }
        if self.queue.contains_key(&seq) {
            return InsertResult::Duplicate;
        }
        self.newest = Some(self.newest.map_or(seq, |newest| newest.max(seq)));
        self.next.get_or_insert(seq);
        self.queue.insert(seq, (frame, received_at));
        InsertResult::Queued
    }

    /// when the oldest frame waiting behind a gap has waited `wait`
    fn deadline(&self) -> Option<Instant> {
        let (&first, _) = self.queue.first_key_value()?;
        if Some(first) == self.next {
            return None;
        }
        self.queue
            .values()
            .map(|(_, at)| *at + self.config.wait)
            .min()
    }

    fn poll(&mut self, now: Instant) -> Ordered {
        let (Some(mut next), Some((&first, _))) = (self.next, self.queue.first_key_value()) else {
            return Ordered::Wait;
        };
        if first == next {
            let (frame, _) = self.queue.remove(&first).expect("first key");
            self.advance(next, true);
            return Ordered::Frame(frame);
        }
        let full = self.queue.len() >= self.config.max_frames;
        if !full && self.deadline().is_some_and(|at| now < at) {
            return Ordered::Wait;
        }
        let gap = first - next;
        if gap > MAX_CONCEAL {
            // skipped, never emitted: late from now on
            let skipped = gap - MAX_CONCEAL;
            self.emitted = self.emitted.checked_shl(skipped).unwrap_or(0);
            next += skipped;
        }
        // in-band FEC of frame n travels in frame n + 1 only
        let following = (first == next + 1).then(|| self.queue[&first].0.clone());
        self.advance(next, false);
        Ordered::Missing {
            id: MediaId {
                stream: self.stream,
                sequence: next,
            },
            next: following,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const STREAM: StreamId = StreamId {
        player: 2,
        generation: 7,
    };

    fn frame(order: &Reorder, seq: u16) -> CompressedFrame {
        CompressedFrame {
            id: order.media_id(seq).expect("in window"),
            talkspurt: 1,
            opus: vec![seq as u8],
        }
    }

    fn push(order: &mut Reorder, seq: u16, at: Instant) -> InsertResult {
        let frame = frame(order, seq);
        order.push(frame, at)
    }

    /// what poll emits at `now` until it waits: wire seq of a frame, or !seq of a gap
    fn drain(order: &mut Reorder, now: Instant) -> Vec<i32> {
        let mut out = Vec::new();
        loop {
            match order.poll(now) {
                Ordered::Frame(f) => out.push(i32::from(f.id.sequence as u16)),
                Ordered::Missing { id, .. } => out.push(!i32::from(id.sequence as u16)),
                Ordered::Wait => return out,
            }
        }
    }

    #[test]
    fn path_delay_update_keeps_queued_and_emitted_frame_identities() {
        let now = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        assert_eq!(push(&mut order, 1, now), InsertResult::Queued);
        assert_eq!(drain(&mut order, now), [1]);
        assert_eq!(push(&mut order, 3, now), InsertResult::Queued);
        order.set_wait(Duration::from_secs(1));
        assert_eq!(order.deadline(), Some(now + Duration::from_millis(200)));
        assert!(drain(&mut order, now + Duration::from_millis(199)).is_empty());
        assert_eq!(push(&mut order, 1, now), InsertResult::Duplicate);
        assert_eq!(push(&mut order, 2, now), InsertResult::Queued);
        assert_eq!(drain(&mut order, now), [2, 3]);
    }

    #[test]
    fn in_order_frames_come_out_at_once() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        for seq in 100..105 {
            assert_eq!(push(&mut order, seq, t0), InsertResult::Queued);
            assert_eq!(order.deadline(), None);
            assert_eq!(drain(&mut order, t0), [i32::from(seq)]);
        }
    }

    #[test]
    fn n_plus_1_before_n_waits_for_n() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 1, t0);
        assert_eq!(drain(&mut order, t0), [1]);
        push(&mut order, 3, t0);
        assert_eq!(order.deadline(), Some(t0 + GAME_ONLY.wait));
        assert_eq!(drain(&mut order, t0 + 5 * MS), Vec::<i32>::new());
        assert_eq!(push(&mut order, 2, t0 + 5 * MS), InsertResult::Queued);
        assert_eq!(drain(&mut order, t0 + 5 * MS), [2, 3]);
        assert_eq!(order.deadline(), None);
    }

    #[test]
    fn duplicate_across_two_paths_is_dropped_queued_or_emitted() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        // the same identity from two routes: once waiting behind a gap, once already played
        push(&mut order, 10, t0);
        push(&mut order, 12, t0);
        assert_eq!(push(&mut order, 12, t0 + MS), InsertResult::Duplicate);
        assert_eq!(drain(&mut order, t0), [10]);
        assert_eq!(push(&mut order, 10, t0 + MS), InsertResult::Duplicate);
        push(&mut order, 11, t0 + MS);
        assert_eq!(drain(&mut order, t0 + MS), [11, 12]);
        assert_eq!(push(&mut order, 11, t0 + 2 * MS), InsertResult::Duplicate);
    }

    #[test]
    fn missing_n_is_reported_once_at_the_deadline_then_late() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 1, t0);
        drain(&mut order, t0);
        push(&mut order, 3, t0 + MS);
        let at = t0 + MS + GAME_ONLY.wait;
        assert_eq!(drain(&mut order, at - MS), Vec::<i32>::new());
        // the frame after the gap is offered for FEC and still decoded normally
        let Ordered::Missing { id, next } = order.poll(at) else {
            panic!("no gap reported")
        };
        assert_eq!(id.sequence as u16, 2);
        assert_eq!(next.map(|f| f.id.sequence as u16), Some(3));
        assert_eq!(drain(&mut order, at), [3]);
        assert_eq!(push(&mut order, 2, at + MS), InsertResult::Late);
        assert_eq!(drain(&mut order, at + MS), Vec::<i32>::new());
    }

    #[test]
    fn two_missing_frames_cost_one_wait_and_no_fec_across_two() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 1, t0);
        drain(&mut order, t0);
        push(&mut order, 4, t0);
        let Ordered::Missing { next, .. } = order.poll(t0 + GAME_ONLY.wait) else {
            panic!("no gap reported")
        };
        assert!(next.is_none(), "frame 4 carries no FEC for frame 2");
        assert_eq!(drain(&mut order, t0 + GAME_ONLY.wait), [!3, 4]);
    }

    #[test]
    fn a_full_queue_does_not_wait_for_the_deadline() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 0, t0);
        drain(&mut order, t0);
        let ahead = GAME_ONLY.max_frames as u16;
        for seq in 2..2 + ahead {
            push(&mut order, seq, t0);
        }
        let played = drain(&mut order, t0);
        assert_eq!(played.len(), 1 + GAME_ONLY.max_frames);
        assert_eq!(played[0], !1);
    }

    #[test]
    fn sequence_wrap_keeps_order_and_rejects_half_range() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 65534, t0);
        push(&mut order, 0, t0);
        push(&mut order, 65535, t0);
        push(&mut order, 1, t0);
        assert_eq!(drain(&mut order, t0), [65534, 65535, 0, 1]);
        assert_eq!(order.media_id(1).unwrap().sequence, 0x20001);
        // half the u16 range away from the newest frame is not a frame of this stream
        assert_eq!(order.media_id(1u16.wrapping_add(32768)), None);
        // a predecessor of the very first frame still has an identity, and it is late
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 0, t0);
        drain(&mut order, t0);
        assert_eq!(push(&mut order, 65535, t0), InsertResult::Late);
    }

    #[test]
    fn another_stream_or_a_far_identity_is_refused() {
        let t0 = Instant::now();
        let mut order = Reorder::new(STREAM, GAME_ONLY);
        push(&mut order, 5, t0);
        let mut other = frame(&order, 6);
        other.id.stream.generation = 8;
        assert_eq!(order.push(other, t0), InsertResult::WrongStream);
        let mut far = frame(&order, 6);
        far.id.sequence += WINDOW;
        assert_eq!(order.push(far, t0), InsertResult::Ambiguous);
    }
}
