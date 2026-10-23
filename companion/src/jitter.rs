//! per-speaker receive queue. delivery through the game is lossless and in
//! order but bursty (per tick, and a ~0.5 s burst after an autosave), so the
//! queue only smooths bursts and drops frames that arrive too late to matter.
//! time is passed in so the rules are testable without a clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 40 ms of audio: playout starts at this depth, and an overflow is cut back to it
pub const TARGET_FRAMES: usize = 2;
/// start anyway this long after START even if fewer frames arrived
pub const START_WAIT: Duration = Duration::from_millis(60);
/// more than 200 ms queued means a post-stall burst
pub const MAX_FRAMES: usize = 10;
/// close a stream that got no frame for this long
pub const IDLE_CLOSE: Duration = Duration::from_millis(300);

#[derive(Debug, PartialEq)]
pub enum Pull<F> {
    /// still buffering: contributes nothing to the mix
    Wait,
    Frame(F),
    /// underrun mid-transmission: play one frame of silence
    Silence,
    Closed,
}

/// `F` is one queued frame, as the caller wants it back
pub struct Stream<F> {
    queue: VecDeque<F>,
    playing: bool,
    stopping: bool,
    opened_at: Instant,
    last_frame_at: Instant,
}

impl<F> Stream<F> {
    pub fn new(now: Instant) -> Self {
        Stream {
            queue: VecDeque::new(),
            playing: false,
            stopping: false,
            opened_at: now,
            last_frame_at: now,
        }
    }

    /// START on a stream that still exists (pressed again while the tail drains, or a repeated START)
    pub fn restart(&mut self, now: Instant) {
        self.stopping = false;
        self.last_frame_at = now;
    }

    /// queues one encoded frame; returns how many old frames were dropped
    pub fn push(&mut self, frame: F, now: Instant) -> usize {
        self.queue.push_back(frame);
        self.last_frame_at = now;
        if self.queue.len() <= MAX_FRAMES {
            return 0;
        }
        let dropped = self.queue.len() - TARGET_FRAMES;
        self.queue.drain(..dropped);
        dropped
    }

    /// STOP: play what is queued, then close
    pub fn stop(&mut self) {
        self.stopping = true;
    }

    /// frames waiting to be played
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// called once per 20 ms playout tick
    pub fn pull(&mut self, now: Instant) -> Pull<F> {
        if !self.playing {
            let ready = self.queue.len() >= TARGET_FRAMES
                || now - self.opened_at >= START_WAIT
                || self.stopping;
            if !ready {
                return Pull::Wait;
            }
            self.playing = true;
        }
        match self.queue.pop_front() {
            Some(frame) => Pull::Frame(frame),
            None if self.stopping || now - self.last_frame_at > IDLE_CLOSE => Pull::Closed,
            None => Pull::Silence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn frame(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn starts_at_40_ms_depth() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        s.push(frame(1), t0 + 10 * MS);
        assert_eq!(s.pull(t0 + 20 * MS), Pull::Wait);
        s.push(frame(2), t0 + 30 * MS);
        assert_eq!(s.pull(t0 + 40 * MS), Pull::Frame(frame(1)));
        assert_eq!(s.pull(t0 + 60 * MS), Pull::Frame(frame(2)));
    }

    #[test]
    fn starts_60_ms_after_start_with_one_frame() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        s.push(frame(1), t0 + 5 * MS);
        assert_eq!(s.pull(t0 + 40 * MS), Pull::Wait);
        assert_eq!(s.pull(t0 + 60 * MS), Pull::Frame(frame(1)));
        // playing now: an empty queue is an underrun, filled with silence
        assert_eq!(s.pull(t0 + 80 * MS), Pull::Silence);
    }

    #[test]
    fn overflow_drops_oldest_down_to_target() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        let mut dropped = 0;
        for n in 0..=(MAX_FRAMES as u8) {
            dropped += s.push(frame(n), t0);
        }
        assert_eq!(dropped, MAX_FRAMES + 1 - TARGET_FRAMES);
        assert_eq!(s.pull(t0), Pull::Frame(frame(MAX_FRAMES as u8 - 1)));
        assert_eq!(s.pull(t0), Pull::Frame(frame(MAX_FRAMES as u8)));
        // exactly MAX_FRAMES queued is still fine
        let mut s = Stream::new(t0);
        assert_eq!(
            (0..MAX_FRAMES as u8)
                .map(|n| s.push(frame(n), t0))
                .sum::<usize>(),
            0
        );
    }

    #[test]
    fn stop_drains_then_closes() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        s.push(frame(1), t0);
        s.stop();
        // a short burst ended by STOP plays at once, without waiting for depth or 60 ms
        assert_eq!(s.pull(t0), Pull::Frame(frame(1)));
        assert_eq!(s.pull(t0 + 20 * MS), Pull::Closed);
    }

    #[test]
    fn start_while_draining_keeps_stream_open() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        s.push(frame(1), t0);
        s.push(frame(2), t0);
        s.stop();
        s.restart(t0 + 10 * MS);
        assert_eq!(s.pull(t0 + 20 * MS), Pull::Frame(frame(1)));
        assert_eq!(s.pull(t0 + 40 * MS), Pull::Frame(frame(2)));
        assert_eq!(s.pull(t0 + 60 * MS), Pull::Silence);
    }

    #[test]
    fn closes_after_300_ms_without_frames() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        s.push(frame(1), t0);
        s.push(frame(2), t0);
        assert_eq!(s.pull(t0), Pull::Frame(frame(1)));
        assert_eq!(s.pull(t0 + 20 * MS), Pull::Frame(frame(2)));
        assert_eq!(s.pull(t0 + 300 * MS), Pull::Silence);
        assert_eq!(s.pull(t0 + 301 * MS), Pull::Closed);
        // a START that never gets a frame also times out
        let mut s = Stream::<Vec<u8>>::new(t0);
        assert_eq!(s.pull(t0 + 200 * MS), Pull::Silence);
        assert_eq!(s.pull(t0 + 301 * MS), Pull::Closed);
    }
}
