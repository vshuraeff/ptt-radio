//! aec trains continuously; only a detected render-to-capture path selects its output.
//! all processing stays on the playout/transmit workers, outside cpal callbacks.

mod detector;

use std::collections::VecDeque;
use std::sync::Mutex;

use sonora::config::{EchoCanceller, TransparentModeType};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::audio::OPUS_RATE;
use detector::Detector;

pub const CANCELLER: &str = "sonora-aec3-0.2.0-gated";
pub const ALGORITHMIC_DELAY_MS: f64 = 436.0 * 1000.0 / OPUS_RATE as f64;
const BLOCK: usize = OPUS_RATE as usize / 100;
const MAX_PENDING_RENDER: usize = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct GateState {
    pub engaged: bool,
    pub score: f64,
    pub lag_ms: f64,
    /// capture sample index of the last transition, before the 10 ms crossfade
    pub capture_sample: u64,
}

struct Canceller {
    processor: AudioProcessing,
    render: VecDeque<[f32; BLOCK]>,
    detector: Detector,
    wet_mix: f32,
    capture_sample: u64,
    transition_sample: u64,
}

fn processor() -> AudioProcessing {
    let stream = StreamConfig::new(OPUS_RATE, 1);
    AudioProcessing::builder()
        .config(Config {
            echo_canceller: Some(EchoCanceller {
                transparent_mode: TransparentModeType::Hmm,
                ..EchoCanceller::default()
            }),
            ..Config::default()
        })
        .capture_config(stream)
        .render_config(stream)
        .build()
}

impl Canceller {
    fn render(&mut self, block: &[f32; BLOCK]) {
        let mut output = [0.0; BLOCK];
        self.processor
            .process_render_f32(&[block], &mut [&mut output])
            .expect("48 khz mono render block");
    }
}

pub struct EchoPath {
    canceller: Option<Mutex<Canceller>>,
}

impl Default for EchoPath {
    fn default() -> Self {
        Self::new(true)
    }
}

impl EchoPath {
    pub fn new(enabled: bool) -> Self {
        Self {
            canceller: enabled.then(|| {
                Mutex::new(Canceller {
                    processor: processor(),
                    render: VecDeque::with_capacity(MAX_PENDING_RENDER),
                    detector: Detector::default(),
                    wet_mix: 0.0,
                    capture_sample: 0,
                    transition_sample: 0,
                })
            }),
        }
    }

    pub fn far_end(&self, frame: &[f32]) {
        let Some(canceller) = &self.canceller else {
            return;
        };
        assert!(frame.len().is_multiple_of(BLOCK));
        let mut canceller = canceller.lock().unwrap();
        for block in frame.as_chunks::<BLOCK>().0 {
            canceller.detector.render(block);
            if canceller.render.len() == MAX_PENDING_RENDER {
                let old = canceller.render.pop_front().unwrap();
                canceller.render(&old);
            }
            canceller.render.push_back(*block);
        }
    }

    pub fn near_end(&self, capture: &mut [f32]) {
        let Some(canceller) = &self.canceller else {
            return;
        };
        assert!(capture.len().is_multiple_of(BLOCK));
        let mut canceller = canceller.lock().unwrap();
        let mut output = [0.0; BLOCK];
        for block in capture.as_chunks_mut::<BLOCK>().0 {
            let was_engaged = canceller.detector.engaged;
            canceller.detector.capture(block);
            if was_engaged != canceller.detector.engaged {
                canceller.transition_sample = canceller.capture_sample;
            }
            if let Some(far) = canceller.render.pop_front() {
                canceller.render(&far);
            }
            canceller
                .processor
                .process_capture_f32(&[block], &mut [&mut output])
                .expect("48 khz mono capture block");
            let target = if canceller.detector.engaged { 1.0 } else { 0.0 };
            for (dry, &wet) in block.iter_mut().zip(&output) {
                canceller.wet_mix +=
                    (target - canceller.wet_mix).clamp(-1.0 / BLOCK as f32, 1.0 / BLOCK as f32);
                if canceller.wet_mix > 0.0 {
                    *dry = *dry * (1.0 - canceller.wet_mix) + wet * canceller.wet_mix;
                }
            }
            canceller.capture_sample += BLOCK as u64;
        }
    }

    pub fn gate_state(&self) -> GateState {
        let Some(canceller) = &self.canceller else {
            return GateState::default();
        };
        let canceller = canceller.lock().unwrap();
        GateState {
            engaged: canceller.detector.engaged,
            score: canceller.detector.evidence.score,
            lag_ms: canceller.detector.evidence.lag_ms,
            capture_sample: canceller.transition_sample,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wet_impulse_delay_is_pinned_at_three_phases() {
        for offset in [0, 123, 479] {
            let mut processor = processor();
            let impulse = OPUS_RATE as usize + offset;
            let mut samples = Vec::new();
            for n in 0..150 {
                let mut input = [0.0; BLOCK];
                let mut output = [0.0; BLOCK];
                if (n * BLOCK..(n + 1) * BLOCK).contains(&impulse) {
                    input[impulse % BLOCK] = 0.5;
                }
                processor
                    .process_render_f32(&[&[0.0; BLOCK]], &mut [&mut output])
                    .unwrap();
                processor
                    .process_capture_f32(&[&input], &mut [&mut output])
                    .unwrap();
                samples.extend(output);
            }
            let (peak, _) = samples
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
                .unwrap();
            assert_eq!(
                (peak - impulse) as f64 * 1000.0 / OPUS_RATE as f64,
                ALGORITHMIC_DELAY_MS
            );
        }
    }

    #[test]
    fn no_echo_preserves_every_sample_while_training() {
        let echo = EchoPath::new(true);
        for n in 0..150 {
            let mut near = std::array::from_fn::<_, BLOCK, _>(|i| {
                ((n * BLOCK + i) as f32 * 0.043).sin() * 0.3
            });
            let dry = near;
            echo.far_end(&[0.0; BLOCK]);
            echo.near_end(&mut near);
            assert_eq!(near, dry);
            assert!(!echo.gate_state().engaged);
        }
    }

    #[test]
    fn render_only_queue_is_bounded_and_capture_resumes() {
        let echo = EchoPath::new(true);
        for _ in 0..1000 {
            echo.far_end(&[0.0; BLOCK]);
        }
        assert_eq!(
            echo.canceller
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .render
                .len(),
            MAX_PENDING_RENDER
        );
        let mut near = [0.25; 2 * BLOCK];
        echo.near_end(&mut near);
        assert_eq!(near, [0.25; 2 * BLOCK]);
        assert!(
            echo.canceller
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .render
                .is_empty()
        );
    }
}
