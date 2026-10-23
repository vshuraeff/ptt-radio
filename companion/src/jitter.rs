//! per-speaker receive queue. delivery through the game is lossless and in
//! order but bursty (per tick, and a ~0.5 s burst after an autosave), so the
//! queue only smooths bursts and drops frames that arrive too late to matter.
//! it also tracks how many queued frames the arrival jitter never needed (the
//! pre-roll burst, the rest of a stall burst), so the mixer can shed them.
//! time is passed in so the rules are testable without a clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 40 ms of audio: playout starts at this depth, and an overflow is cut back to it.
/// the steady depth is not this but what shedding leaves: the lowest depth of the
/// window, which the relay model's tick batching puts at one frame after a pull
/// (jitter stage 24 ms, p95 33 ms). measured on that model, a start depth of 1 gives
/// the same latency (the pre-roll burst fills either at once) and drops two more
/// frames on an overflow cut, so 2 stays
pub const TARGET_FRAMES: usize = 2;
/// start anyway this long after START even if fewer frames arrived
pub const START_WAIT: Duration = Duration::from_millis(60);
/// more than 200 ms queued means a post-stall burst
pub const MAX_FRAMES: usize = 10;
/// close a stream that got no frame for this long
pub const IDLE_CLOSE: Duration = Duration::from_millis(300);
/// pulls over which the lowest queue depth is watched: 200 ms, two periods of the
/// 60 Hz tick against 20 ms frames (their pattern repeats every 100 ms)
pub const LOW_WATER_WINDOW: usize = 10;
/// frames kept above that lowest depth as margin against an underrun, for UNDERRUN_MEMORY
/// after the last underrun; without a recent one the queue is cut to the bare low-water mark.
/// measured on the relay model: no margin gives 148-152 ms mouth-to-ear but lets a stall
/// burst wobble by a frame for seconds, a fixed frame of margin gives 172 ms
pub const MARGIN_AFTER_UNDERRUN: usize = 1;
pub const UNDERRUN_MEMORY: Duration = Duration::from_secs(5);

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
    /// queue depth after each of the last LOW_WATER_WINDOW pulls
    depths: VecDeque<usize>,
    backlog_since: Option<Instant>,
    last_underrun: Option<Instant>,
    margin: usize,
}

impl<F> Stream<F> {
    pub fn new(now: Instant) -> Self {
        Stream {
            queue: VecDeque::new(),
            playing: false,
            stopping: false,
            opened_at: now,
            last_frame_at: now,
            depths: VecDeque::new(),
            backlog_since: None,
            last_underrun: None,
            margin: 0,
        }
    }

    /// frames that stayed queued through a whole window of pulls beyond the margin: the
    /// arrival jitter never needed them, they only add delay
    pub fn backlog(&self) -> usize {
        if self.depths.len() < LOW_WATER_WINDOW {
            return 0;
        }
        let low = self.depths.iter().min().copied().unwrap_or(0);
        low.saturating_sub(self.margin)
    }

    /// how long a backlog has been standing
    pub fn backlog_for(&self, now: Instant) -> Duration {
        self.backlog_since
            .map_or(Duration::ZERO, |since| now - since)
    }

    /// takes the next frame out of turn to cut one frame of backlog
    pub fn shed(&mut self) -> Option<F> {
        let frame = self.queue.pop_front()?;
        // the queue would have been one frame shallower all along
        for depth in &mut self.depths {
            *depth = depth.saturating_sub(1);
        }
        if self.backlog() == 0 {
            self.backlog_since = None;
        }
        Some(frame)
    }

    fn record_depth(&mut self, now: Instant) {
        if self.depths.len() == LOW_WATER_WINDOW {
            self.depths.pop_front();
        }
        self.depths.push_back(self.queue.len());
        let recent = self
            .last_underrun
            .is_some_and(|at| now - at < UNDERRUN_MEMORY);
        self.margin = if recent { MARGIN_AFTER_UNDERRUN } else { 0 };
        if self.backlog() == 0 {
            self.backlog_since = None;
        } else if self.backlog_since.is_none() {
            self.backlog_since = Some(now);
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
        // depths seen before the cut say nothing about the queue now
        self.depths.clear();
        self.backlog_since = None;
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
        let pull = match self.queue.pop_front() {
            Some(frame) => Pull::Frame(frame),
            None if self.stopping || now - self.last_frame_at > IDLE_CLOSE => Pull::Closed,
            None => {
                self.last_underrun = Some(now);
                Pull::Silence
            }
        };
        self.record_depth(now);
        pull
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
    fn backlog_is_what_a_window_of_pulls_never_needed() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        // a pre-roll burst of 6, then one frame per pull
        for n in 0..6 {
            s.push(frame(n), t0);
        }
        for i in 0..LOW_WATER_WINDOW {
            let now = t0 + 20 * MS * i as u32;
            assert_eq!(s.backlog(), 0, "no verdict before a full window");
            s.push(frame(6 + i as u8), now);
            assert!(matches!(s.pull(now), Pull::Frame(_)));
        }
        // depth after every pull was 6 and no underrun happened: all six are backlog
        assert_eq!(s.backlog(), 6);
        let now = t0 + 20 * MS * LOW_WATER_WINDOW as u32;
        assert_eq!(s.backlog_for(now), 20 * MS);
        for left in (0..6).rev() {
            assert!(s.shed().is_some());
            assert_eq!(s.backlog(), left);
        }
        assert_eq!(s.backlog_for(now), Duration::ZERO);
        assert_eq!(s.queued(), 0);
    }

    #[test]
    fn an_underrun_keeps_a_margin_for_a_while() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        s.push(frame(0), t0);
        s.push(frame(1), t0);
        let mut now = t0;
        let pull_with_depth = |s: &mut Stream<Vec<u8>>, depth: usize, now: Instant| {
            while s.queued() < depth + 1 {
                s.push(frame(9), now);
            }
            s.pull(now)
        };
        pull_with_depth(&mut s, 0, now);
        pull_with_depth(&mut s, 0, now);
        // underrun
        assert_eq!(s.pull(now), Pull::Silence);
        for _ in 0..LOW_WATER_WINDOW {
            now += 20 * MS;
            pull_with_depth(&mut s, 3, now);
        }
        assert_eq!(s.backlog(), 3 - MARGIN_AFTER_UNDERRUN);
        // the margin lapses UNDERRUN_MEMORY after the underrun
        now = t0 + UNDERRUN_MEMORY;
        pull_with_depth(&mut s, 3, now);
        assert_eq!(s.backlog(), 3);
    }

    #[test]
    fn an_overflow_cut_restarts_the_backlog_watch() {
        let t0 = Instant::now();
        let mut s = Stream::new(t0);
        for n in 0..5 {
            s.push(frame(n), t0);
        }
        for n in 0..LOW_WATER_WINDOW as u8 {
            s.push(frame(n), t0);
            s.pull(t0);
        }
        assert_eq!(s.backlog(), 5);
        while s.push(frame(0), t0) == 0 {}
        assert_eq!(s.queued(), TARGET_FRAMES);
        assert_eq!(s.backlog(), 0);
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
