use std::{collections::VecDeque, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use tract_onnx::prelude::*;

pub const HOP: usize = 128;
pub const FRAME: usize = 512;
pub const BLOCK_48: usize = 480;

struct Stage {
    plan: Arc<TypedRunnableModel>,
    state: Tensor,
    width: usize,
}

impl Stage {
    fn load(path: &Path, units: usize, names: [&str; 3], width: usize) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)?
            .with_input_names(names)?
            .with_outputs_by_name(["Identity", "Identity_1"])?
            .into_optimized()?;
        let shapes = [vec![1, 1, width], vec![1, 2, units, 2], vec![1, 1, width]];
        for (&outlet, shape) in model.input_outlets()?.iter().zip(&shapes) {
            let fact = model.outlet_fact(outlet)?;
            ensure!(
                fact.datum_type == DatumType::F32
                    && fact.shape.as_concrete() == Some(shape.as_slice()),
                "unexpected input fact: {fact:?}"
            );
        }
        ensure!(
            model.input_outlets()?.len() == 3 && model.output_outlets()?.len() == 2,
            "unexpected model ABI"
        );
        Ok(Self {
            plan: model.into_runnable()?,
            state: Tensor::zero::<f32>(&shapes[1])?,
            width,
        })
    }

    fn run(&mut self, near: &[f32], far: &[f32]) -> Result<Vec<f32>> {
        let near = Tensor::from_shape(&[1, 1, self.width], near)?;
        let far = Tensor::from_shape(&[1, 1, self.width], far)?;
        let mut outputs =
            self.plan
                .run(tvec![near.into(), self.state.clone().into(), far.into()])?;
        self.state = outputs.remove(1).into_tensor();
        Ok(outputs
            .remove(0)
            .to_plain_array_view::<f32>()?
            .iter()
            .copied()
            .collect())
    }
}

/// independent recurrent state per stream; input/output is an untrimmed 8 ms hop.
pub struct Dtln {
    first: Stage,
    second: Stage,
    fft: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    near: [f32; FRAME],
    far: [f32; FRAME],
    overlap: [f32; FRAME],
}

impl Dtln {
    pub fn load(directory: &Path, units: usize) -> Result<Self> {
        ensure!([128, 512].contains(&units), "units must be 128 or 512");
        let first = Stage::load(
            &directory.join(format!("dtln_aec_{units}_1.onnx")),
            units,
            ["input_3", "input_5", "input_4"],
            FRAME / 2 + 1,
        )?;
        let second = Stage::load(
            &directory.join(format!("dtln_aec_{units}_2.onnx")),
            units,
            ["input_6", "input_8", "input_7"],
            FRAME,
        )?;
        let mut planner = FftPlanner::new();
        Ok(Self {
            first,
            second,
            fft: planner.plan_fft_forward(FRAME),
            inverse: planner.plan_fft_inverse(FRAME),
            near: [0.0; FRAME],
            far: [0.0; FRAME],
            overlap: [0.0; FRAME],
        })
    }

    pub fn process(&mut self, near: &[f32; HOP], far: &[f32; HOP]) -> Result<[f32; HOP]> {
        self.near.copy_within(HOP.., 0);
        self.far.copy_within(HOP.., 0);
        self.near[FRAME - HOP..].copy_from_slice(near);
        self.far[FRAME - HOP..].copy_from_slice(far);
        let mut spectrum: Vec<_> = self
            .near
            .iter()
            .map(|&re| Complex32::new(re, 0.0))
            .collect();
        let mut reference: Vec<_> = self.far.iter().map(|&re| Complex32::new(re, 0.0)).collect();
        self.fft.process(&mut spectrum);
        self.fft.process(&mut reference);
        let magnitude: Vec<_> = spectrum[..=FRAME / 2].iter().map(|c| c.norm()).collect();
        let reference: Vec<_> = reference[..=FRAME / 2].iter().map(|c| c.norm()).collect();
        let mask = self.first.run(&magnitude, &reference)?;
        ensure!(mask.len() == FRAME / 2 + 1, "invalid mask shape");
        for k in 0..=FRAME / 2 {
            spectrum[k] *= mask[k];
        }
        for k in FRAME / 2 + 1..FRAME {
            spectrum[k] = spectrum[FRAME - k].conj();
        }
        self.inverse.process(&mut spectrum);
        let estimated: Vec<_> = spectrum.iter().map(|c| c.re / FRAME as f32).collect();
        let frame = self.second.run(&estimated, &self.far)?;
        ensure!(
            frame.len() == FRAME && frame.iter().all(|s| s.is_finite()),
            "invalid model output"
        );
        self.overlap.copy_within(HOP.., 0);
        self.overlap[FRAME - HOP..].fill(0.0);
        for (sample, predicted) in self.overlap.iter_mut().zip(frame) {
            *sample += predicted;
        }
        Ok(self.overlap[..HOP].try_into()?)
    }
}

const TAPS: usize = 127;

struct LowPass {
    taps: [f32; TAPS],
    history: [f32; TAPS],
    cursor: usize,
}

impl LowPass {
    fn new() -> Self {
        // unlike the device's linear interpolator, this decimator rejects frequencies above 8 khz
        let cutoff = 7200.0 / 48000.0;
        let mut taps = std::array::from_fn(|i| {
            let x = i as f64 - (TAPS - 1) as f64 / 2.0;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
            };
            let window = 0.42 - 0.5 * (std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos()
                + 0.08 * (2.0 * std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos();
            (sinc * window) as f32
        });
        let gain: f32 = taps.iter().sum();
        for tap in &mut taps {
            *tap /= gain;
        }
        Self {
            taps,
            history: [0.0; TAPS],
            cursor: 0,
        }
    }

    fn push(&mut self, sample: f32) -> f32 {
        self.history[self.cursor] = sample;
        let result = self
            .taps
            .iter()
            .enumerate()
            .map(|(i, tap)| tap * self.history[(self.cursor + TAPS - i) % TAPS])
            .sum();
        self.cursor = (self.cursor + 1) % TAPS;
        result
    }
}

/// fixed ratio FIR resampling, with a measured 126-sample round-trip group delay.
pub struct RateConverter {
    down: LowPass,
    up: LowPass,
}

impl Default for RateConverter {
    fn default() -> Self {
        Self {
            down: LowPass::new(),
            up: LowPass::new(),
        }
    }
}

impl RateConverter {
    pub fn down(&mut self, samples: &[f32; BLOCK_48]) -> [f32; 160] {
        let mut result = [0.0; 160];
        for (i, &sample) in samples.iter().enumerate() {
            let filtered = self.down.push(sample);
            if i % 3 == 0 {
                result[i / 3] = filtered;
            }
        }
        result
    }

    pub fn up(&mut self, samples: &[f32; 160]) -> [f32; BLOCK_48] {
        std::array::from_fn(|i| {
            self.up.push(if i % 3 == 0 {
                samples[i / 3] * 3.0
            } else {
                0.0
            })
        })
    }
}

/// 10 ms companion cadence around the model's 8 ms hop, with one hop of output reserve.
pub struct Stream48 {
    model: Dtln,
    near_rate: RateConverter,
    far_rate: RateConverter,
    near: VecDeque<f32>,
    far: VecDeque<f32>,
    output: VecDeque<f32>,
}

impl Stream48 {
    pub fn load(directory: &Path, units: usize) -> Result<Self> {
        Ok(Self {
            model: Dtln::load(directory, units)?,
            near_rate: RateConverter::default(),
            far_rate: RateConverter::default(),
            near: VecDeque::new(),
            far: VecDeque::new(),
            output: VecDeque::from(vec![0.0; HOP]),
        })
    }

    pub fn process(
        &mut self,
        near: &[f32; BLOCK_48],
        far: &[f32; BLOCK_48],
    ) -> Result<[f32; BLOCK_48]> {
        self.near.extend(self.near_rate.down(near));
        self.far.extend(self.far_rate.down(far));
        while self.near.len() >= HOP {
            let near: Vec<_> = self.near.drain(..HOP).collect();
            let far: Vec<_> = self.far.drain(..HOP).collect();
            self.output.extend(
                self.model
                    .process(near.as_slice().try_into()?, far.as_slice().try_into()?)?,
            );
        }
        ensure!(self.output.len() >= 160, "output reserve exhausted");
        let result: Vec<_> = self.output.drain(..160).collect();
        Ok(self.near_rate.up(result.as_slice().try_into()?))
    }
}

pub fn read_f32(path: &Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(bytes.len().is_multiple_of(4), "invalid raw float length");
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_delay_gain_and_alias_rejection() {
        let mut rate = RateConverter::default();
        let mut output = Vec::new();
        for block in 0..20 {
            let input = std::array::from_fn(|i| {
                if block * BLOCK_48 + i == 2400 {
                    1.0
                } else {
                    0.0
                }
            });
            let down = rate.down(&input);
            output.extend(rate.up(&down));
        }
        let peak = output
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .unwrap()
            .0;
        assert_eq!(peak - 2400, 126);
        assert!((output.iter().sum::<f32>() - 1.0).abs() < 1e-3);
        for (hz, minimum, maximum) in [(1000.0, 0.99, 1.01), (12000.0, 0.0, 0.001)] {
            let mut rate = RateConverter::default();
            let mut samples = Vec::new();
            for block in 0..100 {
                let input = std::array::from_fn(|i| {
                    (std::f32::consts::TAU * hz * (block * BLOCK_48 + i) as f32 / 48000.0).sin()
                });
                samples.extend(rate.down(&input));
            }
            let rms = (samples[160..].iter().map(|s| s * s).sum::<f32>()
                / (samples.len() - 160) as f32)
                .sqrt()
                * 2.0f32.sqrt();
            assert!((minimum..=maximum).contains(&rms), "{hz} Hz gain {rms}");
        }
    }
}
