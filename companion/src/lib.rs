//! building blocks of the ptt-radio companion (src/main.rs) and of its test
//! harness: the game relay emulator (src/bin/relay.rs) and the end-to-end
//! scenarios (tests/scenarios.rs). see ../DESIGN.md.

pub mod audio;
pub mod connectivity;
pub mod crypto;
pub mod echo;
pub mod jitter;
pub mod mix;
pub mod packet;
pub mod policy;
pub mod quality;
pub mod relay;
pub mod reorder;
pub mod runtime;
pub mod runtime_io;
pub mod transport;
pub mod tx;
pub mod virtual_audio;

use std::time::{SystemTime, UNIX_EPOCH};

/// wall clock in microseconds: the one time base shared by the companion's
/// trace, the relay and the scenarios, which run in separate processes
pub fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_micros() as u64
}

/// nearest-rank percentile `p` (0..=100) of an ascending slice; NaN when empty
pub fn percentile(sorted: &[f64], p: usize) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(sorted.len() - 1) * p / 100]
}
