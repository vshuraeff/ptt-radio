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
/// a path whose peak stands out only from what the render's self-similarity explains
/// (a voiced far end, a render recirculating through another speaker player) needs this
/// score: independent headphone voices reached 0.47 and earlier trials showed 0.75 safe
const SELF_SIMILAR_ENGAGE: f64 = 0.75;
const ISOLATED: f64 = 1.4;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Evidence {
    pub score: f64,
    pub lag_ms: f64,
    pub peak_ratio: f64,
    /// peak against side peaks less what the render's own autocorrelation predicts
    explained_ratio: f64,
    /// best correlation with the capture leading the render: the near voice coming back
    reverse_score: f64,
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
    autos: Vec<Correlation>,
    reverse: Vec<Correlation>,
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
        self.capture.push(block, HISTORY);
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
        let render = self.render.samples.make_contiguous();
        let capture = self.capture.samples.make_contiguous();
        let recent = &capture[capture.len() - WINDOW..];
        self.evidence = correlate(
            render,
            recent,
            &mut self.correlations,
            &mut self.autos,
            self.offset - offset,
            selected_lag,
        );
        self.evidence.reverse_score =
            reverse_score(render, capture, &mut self.reverse, offset - self.offset);
        self.offset = offset;
        self.evidence.lag_ms +=
            (self.capture.total as f64 - self.render.total as f64) * 1000.0 / RATE as f64;
        let fast = self.evidence.score >= 0.8 && self.evidence.peak_ratio >= ISOLATED;
        // a self-similar near voice that comes back in the render correlates both ways, but
        // the direction it really travels is the direct one and scores higher
        let eligible = (self.evidence.score >= ENGAGE && self.evidence.peak_ratio >= ISOLATED)
            || (self.evidence.score >= SELF_SIMILAR_ENGAGE
                && self.evidence.explained_ratio >= ISOLATED
                && self.evidence.reverse_score < self.evidence.score);
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
        } else if eligible {
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
    autos: &mut Vec<Correlation>,
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
    let mut best_lag = None;
    let lags = render.len() - n + 1;
    let mut norms = vec![0.0; lags];
    let previous = std::mem::take(correlations);
    correlations.resize(lags, Correlation::default());
    for start in 0..=render.len() - n {
        let lag = render.len() - n - start;
        let norm = energy - sum * sum / n as f64;
        if norm > n as f64 * 1e-10 {
            norms[lag] = norm;
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
                best_lag = Some(lag);
                best = Evidence {
                    score: score.min(1.0),
                    lag_ms: lag as f64 * 1000.0 / RATE as f64,
                    peak_ratio: 0.0,
                    explained_ratio: 0.0,
                    reverse_score: 0.0,
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
    let Some(peak) = best_lag else {
        return best;
    };
    // a real path makes the capture a delayed copy of the render, so around the peak the
    // correlation repeats the render's own self-similarity: a voiced far end's pitch
    // period, a render that recirculates through another speaker player. side peaks
    // count only by what that self-similarity does not explain
    if autos.len() != 2 * lags - 1 {
        *autos = vec![Correlation::default(); 2 * lags - 1];
    }
    let at = |lag: usize| lag + lags - 1 - peak;
    let start = render.len() - n - peak;
    let window = &render[start..start + n];
    let mean = window.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
    let reference: Vec<f64> = window.iter().map(|&s| s as f64 - mean).collect();
    for (lag, &norm) in norms.iter().enumerate().filter(|(_, norm)| **norm > 0.0) {
        let start = render.len() - n - lag;
        let dot: f64 = reference
            .iter()
            .zip(&render[start..start + n])
            .map(|(a, &b)| a * b as f64)
            .sum();
        let old = autos[at(lag)];
        autos[at(lag)] = Correlation {
            cross: old.cross * 0.8 + dot,
            render: old.render * 0.8 + norm,
            capture: old.capture * 0.8 + norms[peak],
        };
    }
    let signed = |c: &Correlation| {
        if c.render > 0.0 && c.capture > 0.0 {
            c.cross / (c.capture * c.render).sqrt()
        } else {
            0.0
        }
    };
    let main = signed(&correlations[peak]);
    let (side, unexplained) = correlations
        .iter()
        .enumerate()
        .filter(|(lag, _)| (*lag as f64 - peak as f64).abs() * 1000.0 / RATE as f64 > 3.0)
        .filter(|(_, c)| c.render > 0.0 && c.capture > 0.0)
        .map(|(lag, c)| {
            let s = signed(c);
            (s.abs(), (s - main * signed(&autos[at(lag)])).abs())
        })
        .fold((0.0, 0.0), |(a, b), (s, u)| {
            (f64::max(a, s), f64::max(b, u))
        });
    best.peak_ratio = best.score / side.max(1e-10);
    best.explained_ratio = best.score / unexplained.max(1e-10);
    best
}

/// the newest render window against capture windows up to the history back, accumulated
/// like the forward correlations; `shift` is how far the render's stream lead grew
fn reverse_score(
    render: &[f32],
    capture: &[f32],
    reverse: &mut Vec<Correlation>,
    shift: isize,
) -> f64 {
    let n = WINDOW;
    if render.len() < n || capture.len() < n {
        reverse.clear();
        return 0.0;
    }
    let window = &render[render.len() - n..];
    let mean = window.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
    let centered: Vec<f64> = window.iter().map(|&s| s as f64 - mean).collect();
    let render_energy = centered.iter().map(|s| s * s).sum::<f64>();
    let lags = capture.len() - n + 1;
    let previous = std::mem::take(reverse);
    reverse.resize(lags, Correlation::default());
    if render_energy < n as f64 * 1e-10 {
        return 0.0;
    }
    let mut sum: f64 = capture[..n].iter().map(|&s| s as f64).sum();
    let mut energy: f64 = capture[..n].iter().map(|&s| (s as f64).powi(2)).sum();
    let mut best: f64 = 0.0;
    for start in 0..lags {
        let lag = lags - 1 - start;
        let norm = energy - sum * sum / n as f64;
        if norm > n as f64 * 1e-10 {
            let dot: f64 = centered
                .iter()
                .zip(&capture[start..start + n])
                .map(|(r, &c)| r * c as f64)
                .sum();
            let old = lag
                .checked_add_signed(shift)
                .and_then(|i| previous.get(i))
                .copied()
                .unwrap_or_default();
            let accumulated = Correlation {
                cross: old.cross * 0.8 + dot,
                render: old.render * 0.8 + render_energy,
                capture: old.capture * 0.8 + norm,
            };
            reverse[lag] = accumulated;
            best = best
                .max(accumulated.cross.abs() / (accumulated.capture * accumulated.render).sqrt());
        }
        if let Some(&next) = capture.get(start + n) {
            let (old, next) = (capture[start] as f64, next as f64);
            sum += next - old;
            energy += next * next - old * old;
        }
    }
    best.min(1.0)
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

    /// the render repeats itself `loop_ms` later at `again` (its echo coming back through
    /// another speaker player); after 2 s, once the repeats fill the detector's history,
    /// the mic starts hearing the render 20 ms later at 0.5
    fn recirculating(loop_ms: usize, again: f32) -> Detector {
        let mut detector = Detector::default();
        let mut seed = 71;
        let mut past = VecDeque::from(vec![0.0f32; loop_ms * 48]);
        let mut history = VecDeque::from(vec![0.0; 960]);
        for block in 0..400 {
            let far: Vec<f32> = past
                .drain(..480)
                .map(|old| noise(&mut seed) + again * old)
                .collect();
            past.extend(&far);
            history.extend(&far);
            let coupled = if block < 200 { 0.0 } else { 0.5 };
            let near: Vec<f32> = history.drain(..480).map(|s| s * coupled).collect();
            detector.render(&far);
            detector.capture(&near);
        }
        detector
    }

    #[test]
    fn recirculating_render_still_engages() {
        for loop_ms in [140, 450] {
            let detector = recirculating(loop_ms, 0.95);
            assert!(detector.engaged, "{loop_ms} ms: {:?}", detector.evidence);
            assert!((detector.evidence.lag_ms - 20.0).abs() <= 1.0);
        }
    }

    #[test]
    fn own_voice_coming_back_does_not_engage() {
        // a headphone player: the mic has only his voice, which returns in the render
        // 450 ms later through another player's mic, over that player's own voice
        let mut detector = Detector::default();
        let mut seed = 29;
        let mut back = VecDeque::from(vec![0.0f32; 450 * 48]);
        for _ in 0..300 {
            let near = std::array::from_fn::<_, 480, _>(|_| noise(&mut seed));
            back.extend(near);
            let far: Vec<f32> = back
                .drain(..480)
                .map(|own| 0.5 * own + noise(&mut seed))
                .collect();
            detector.render(&far);
            detector.capture(&near);
            assert!(!detector.engaged, "{:?}", detector.evidence);
        }
    }

    /// a steady voiced near talker: 200 Hz with two harmonics, a slow glide and vibrato
    fn voiced(i: usize, vibrato: f64) -> f32 {
        let t = i as f64 / 48_000.0;
        let phase = std::f64::consts::TAU * (200.0 * t + 0.1 * t * t)
            + vibrato * (std::f64::consts::TAU * 2.0 * t).sin();
        let envelope = 0.7 + 0.3 * (std::f64::consts::TAU * 0.2 * t).sin();
        let wave = phase.sin() + 0.3 * (2.0 * phase).sin() + 0.1 * (3.0 * phase).sin();
        (envelope * wave * 0.3) as f32
    }

    #[test]
    fn own_voiced_talk_coming_back_does_not_engage() {
        // the render is only the near voice 450 ms later; its periodicity correlates at
        // short forward lags too, which the side-peak explanation alone would accept
        for vibrato in [0.0, 0.5] {
            let mut detector = Detector::default();
            for block in 0..600 {
                let near = std::array::from_fn::<_, 480, _>(|j| voiced(block * 480 + j, vibrato));
                let far = std::array::from_fn::<_, 480, _>(|j| {
                    (block * 480 + j)
                        .checked_sub(450 * 48)
                        .map_or(0.0, |i| 0.5 * voiced(i, vibrato))
                });
                detector.render(&far);
                detector.capture(&near);
                assert!(
                    !detector.engaged,
                    "{vibrato} {block}: {:?}",
                    detector.evidence
                );
            }
        }
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
