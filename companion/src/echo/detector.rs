use std::collections::VecDeque;

use crate::audio::OPUS_RATE;

const RATE: usize = 2000;
const DECIMATE: usize = OPUS_RATE as usize / RATE;
const WINDOW: usize = RATE * 160 / 1000;
const HISTORY: usize = RATE + WINDOW;
const UPDATE_BLOCKS: usize = 10;
const ENGAGE: f64 = 0.20;
const RELEASE: f64 = 0.15;
const ENGAGE_HITS: usize = 3;
const RELEASE_HITS: usize = 10;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Evidence {
    pub score: f64,
    pub lag_ms: f64,
    pub peak_ratio: f64,
    instant_score: f64,
    render_energy: f64,
    capture_energy: f64,
    gain: f64,
}

#[derive(Default)]
struct Reduced {
    previous: f32,
    samples: VecDeque<f32>,
    total: usize,
}

impl Reduced {
    fn push(&mut self, block: &[f32], limit: usize) {
        // box-average before decimation; the difference reduces shared low-frequency voicing
        for chunk in block.as_chunks::<DECIMATE>().0 {
            let mean = chunk.iter().sum::<f32>() / DECIMATE as f32;
            self.samples.push_back(mean - self.previous);
            self.previous = mean;
            self.total += 1;
        }
        let excess = self.samples.len().saturating_sub(limit);
        self.samples.drain(..excess);
    }
}

#[derive(Default)]
pub(super) struct Detector {
    render: Reduced,
    capture: Reduced,
    blocks: usize,
    correlations: Vec<Correlation>,
    offset: isize,
    path_lag: Option<f64>,
    path_gain: f64,
    matched_lag: Option<f64>,
    hits: usize,
    misses: usize,
    pub engaged: bool,
    pub evidence: Evidence,
}

impl Detector {
    pub fn render(&mut self, block: &[f32]) {
        self.render.push(block, HISTORY);
    }

    pub fn capture(&mut self, block: &[f32]) {
        self.capture.push(block, WINDOW);
        self.blocks += 1;
        if !self.blocks.is_multiple_of(UPDATE_BLOCKS) || self.capture.samples.len() < WINDOW {
            return;
        }
        let offset = self.render.total as isize - self.capture.total as isize;
        // compare delays on stream sample counts, removing the workers' changing lead
        let selected_lag = self.path_lag.map(|lag| {
            usize::try_from((lag * RATE as f64 / 1000.0).round() as isize + offset)
                .unwrap_or(usize::MAX)
        });
        self.evidence = correlate(
            self.render.samples.make_contiguous(),
            self.capture.samples.make_contiguous(),
            &mut self.correlations,
            self.offset - offset,
            selected_lag,
        );
        self.offset = offset;
        self.evidence.lag_ms +=
            (self.capture.total as f64 - self.render.total as f64) * 1000.0 / RATE as f64;
        let fast = self.evidence.score >= 0.8 && self.evidence.peak_ratio >= 1.4;
        if self.blocks < 50 && !fast {
            return;
        }
        if self.engaged {
            if self.evidence.instant_score >= ENGAGE {
                if self.evidence.instant_score >= 0.75 {
                    self.path_gain = self.path_gain * 0.9 + self.evidence.gain * 0.1;
                }
                self.misses = 0;
            // a louder independent near voice makes missing correlation inconclusive
            } else if self.evidence.instant_score < RELEASE
                && self.evidence.render_energy > WINDOW as f64 * 1e-10
                && self.evidence.capture_energy
                    <= 4.0 * self.path_gain.powi(2) * self.evidence.render_energy
            {
                self.misses += 1;
                if self.misses >= RELEASE_HITS {
                    self.engaged = false;
                    self.path_lag = None;
                    self.hits = 0;
                }
            }
        } else if self.evidence.score >= ENGAGE && self.evidence.peak_ratio >= 1.4 {
            let stable = self
                .matched_lag
                .is_none_or(|lag| (lag - self.evidence.lag_ms).abs() <= 2.0);
            self.hits = if stable { self.hits + 1 } else { 1 };
            self.matched_lag = Some(self.evidence.lag_ms);
            self.misses = 0;
            if self.hits >= if fast { 2 } else { ENGAGE_HITS } {
                self.engaged = true;
                self.path_lag = Some(self.evidence.lag_ms);
                self.path_gain = self.evidence.gain;
            }
        } else {
            self.hits = 0;
            self.matched_lag = None;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Correlation {
    cross: f64,
    render: f64,
    capture: f64,
}

fn correlate(
    render: &[f32],
    capture: &[f32],
    correlations: &mut Vec<Correlation>,
    shift: isize,
    selected_lag: Option<usize>,
) -> Evidence {
    if render.len() < capture.len() {
        return Evidence::default();
    }
    let n = capture.len();
    let mean = capture.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
    let centered: Vec<f64> = capture.iter().map(|&s| s as f64 - mean).collect();
    let capture_energy = centered.iter().map(|s| s * s).sum::<f64>();
    let mut sum: f64 = render[..n].iter().map(|&s| s as f64).sum();
    let mut energy: f64 = render[..n].iter().map(|&s| (s as f64).powi(2)).sum();
    let mut best = Evidence::default();
    let previous = std::mem::take(correlations);
    correlations.resize(render.len() - n + 1, Correlation::default());
    for start in 0..=render.len() - n {
        let lag = render.len() - n - start;
        let norm = energy - sum * sum / n as f64;
        if norm > n as f64 * 1e-10 {
            let dot: f64 = centered
                .iter()
                .zip(&render[start..start + n])
                .map(|(c, &r)| c * r as f64)
                .sum();
            let old = lag
                .checked_add_signed(shift)
                .and_then(|i| previous.get(i))
                .copied()
                .unwrap_or_default();
            let accumulated = Correlation {
                // signed covariance accumulates a coherent path, rather than unrelated peaks
                cross: old.cross * 0.8 + dot,
                render: old.render * 0.8 + norm,
                capture: old.capture * 0.8 + capture_energy,
            };
            correlations[lag] = accumulated;
            let score = if capture_energy < n as f64 * 1e-10 {
                0.0
            } else {
                accumulated.cross.abs() / (accumulated.capture * accumulated.render).sqrt()
            };
            if selected_lag == Some(lag) || (selected_lag.is_none() && score > best.score) {
                best = Evidence {
                    score: score.min(1.0),
                    lag_ms: lag as f64 * 1000.0 / RATE as f64,
                    peak_ratio: 0.0,
                    instant_score: if capture_energy < n as f64 * 1e-10 {
                        0.0
                    } else {
                        dot.abs() / (capture_energy * norm).sqrt()
                    },
                    render_energy: norm,
                    capture_energy,
                    gain: dot.abs() / norm.max(1e-20),
                };
            }
        }
        if let Some(&next) = render.get(start + n) {
            let (old, next) = (render[start] as f64, next as f64);
            sum += next - old;
            energy += next * next - old * old;
        }
    }
    let side = correlations
        .iter()
        .enumerate()
        .filter(|(lag, _)| (*lag as f64 * 1000.0 / RATE as f64 - best.lag_ms).abs() > 3.0)
        .filter(|(_, c)| c.render > 0.0 && c.capture > 0.0)
        .map(|(_, c)| c.cross.abs() / (c.capture * c.render).sqrt())
        .fold(0.0, f64::max);
    best.peak_ratio = best.score / side.max(1e-10);
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f64 / u32::MAX as f64 - 0.5) as f32
    }

    #[test]
    fn delayed_echo_engages_and_uncoupling_releases() {
        let mut detector = Detector::default();
        let mut seed = 71;
        let mut history = VecDeque::from(vec![0.0; 960]);
        for _ in 0..100 {
            let far = std::array::from_fn::<_, 480, _>(|_| noise(&mut seed));
            history.extend(far);
            let near: Vec<f32> = history.drain(..480).map(|s| s * 0.03).collect();
            detector.render(&far);
            detector.capture(&near);
        }
        assert!(detector.engaged);
        assert!((detector.evidence.lag_ms - 20.0).abs() <= 1.0);
        for _ in 0..250 {
            detector.render(&[0.0; 480]);
            detector.capture(&[0.0; 480]);
            assert!(
                detector.engaged,
                "silence is not evidence that the path disappeared"
            );
        }
        for _ in 0..250 {
            let far = std::array::from_fn::<_, 480, _>(|_| noise(&mut seed));
            let near = [0.0; 480];
            detector.render(&far);
            detector.capture(&near);
        }
        assert!(!detector.engaged, "{:?}", detector.evidence);
        assert!(detector.render.samples.len() <= HISTORY);
    }

    #[test]
    fn independent_microphone_never_engages() {
        let mut detector = Detector::default();
        let mut seed = 13;
        for _ in 0..200 {
            let far = std::array::from_fn::<_, 480, _>(|_| noise(&mut seed));
            let near = std::array::from_fn::<_, 480, _>(|_| noise(&mut seed));
            detector.render(&far);
            detector.capture(&near);
            assert!(!detector.engaged);
        }
    }
}
