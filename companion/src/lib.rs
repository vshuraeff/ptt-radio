//! building blocks of the ptt-radio companion (src/main.rs) and of its test
//! harness: the game relay emulator (src/bin/relay.rs) and the end-to-end
//! scenarios (tests/scenarios.rs). see ../DESIGN.md.

#[cfg(feature = "audio")]
pub mod audio;
#[cfg(feature = "audio")]
pub mod connectivity;
#[cfg(feature = "audio")]
pub mod crypto;
#[cfg(feature = "audio")]
pub mod echo;
#[cfg(feature = "audio")]
pub mod jitter;
#[cfg(feature = "audio")]
pub mod mix;
pub mod packet;
#[cfg(feature = "audio")]
pub mod policy;
#[cfg(feature = "audio")]
pub mod quality;
#[cfg(feature = "audio")]
pub mod relay;
#[cfg(feature = "audio")]
pub mod reorder;
#[cfg(feature = "audio")]
pub mod runtime;
#[cfg(feature = "audio")]
pub mod runtime_io;
pub mod transport;
#[cfg(feature = "audio")]
pub mod tx;
#[cfg(feature = "audio")]
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
