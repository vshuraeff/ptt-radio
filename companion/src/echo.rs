//! the seam for acoustic echo cancellation. a player with speakers and an open
//! mic picks up everyone's voice from the speakers and sends it back; a
//! canceller removes it from the capture using what was played as reference.
//!
//! far end: every 20 ms frame playout hands to the speaker (48 kHz mono, the
//! mix before resampling to the device rate, silence included), about one
//! output-queue depth before the device plays it. near end: every captured
//! 20 ms frame (48 kHz mono) before it is encoded, which reaches this point one
//! capture buffering after it was spoken. the scenario "echo" in
//! tests/scenarios.rs measures both offsets and the echo that comes back.
//!
//! no canceller is plugged in yet: the capture passes through unchanged and
//! the far end is only kept bounded.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::audio::OPUS_RATE;

/// reported by the companion's trace so the scenarios can name what they measured
pub const CANCELLER: &str = "none";

#[derive(Default)]
pub struct EchoPath {
    far_end: Mutex<VecDeque<f32>>,
}

impl EchoPath {
    pub fn far_end(&self, frame: &[f32]) {
        let mut queue = self.far_end.lock().unwrap();
        queue.extend(frame);
        // a stalled capture side must not grow the reference without bound
        let excess = queue.len().saturating_sub(OPUS_RATE as usize);
        queue.drain(..excess);
    }

    pub fn near_end(&self, _capture: &mut [f32]) {
        self.far_end.lock().unwrap().clear();
    }
}
