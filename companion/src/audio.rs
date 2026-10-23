//! default input and output devices through cpal, exchanging mono f32 samples
//! at the device rate through shared queues, plus a linear resampler to and
//! from the 48 kHz opus rate.

use std::collections::VecDeque;
use std::error::Error;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub type Samples = Arc<Mutex<VecDeque<f32>>>;

pub const OPUS_RATE: u32 = 48_000;
// one second of capture is plenty: the transmit loop drains it every few ms
const MAX_CAPTURE_SECONDS: usize = 1;
// device callback period where the device lets us choose it, instead of the host
// default (512 frames, ~11 ms on coreaudio)
const CALLBACK_MS: u32 = 5;

fn low_latency(supported: cpal::SupportedStreamConfig) -> cpal::StreamConfig {
    let mut config = supported.config();
    if let cpal::SupportedBufferSize::Range { min, max } = *supported.buffer_size() {
        let frames = (config.sample_rate * CALLBACK_MS / 1000).clamp(min, max);
        config.buffer_size = cpal::BufferSize::Fixed(frames);
    }
    config
}

/// drops the oldest captured samples beyond `MAX_CAPTURE_SECONDS` at `rate`
pub fn cap_capture(queue: &mut VecDeque<f32>, rate: u32) {
    let excess = queue
        .len()
        .saturating_sub(rate as usize * MAX_CAPTURE_SECONDS);
    queue.drain(..excess);
}

/// starts the default input device; captured audio is averaged to mono and appended to `samples`
pub fn open_input(samples: Samples) -> Result<(cpal::Stream, u32), Box<dyn Error>> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no default input device")?;
    let config = low_latency(device.default_input_config()?);
    let (channels, rate) = (config.channels as usize, config.sample_rate);
    println!(
        "input: {} ({} Hz, {} ch)",
        device.description()?,
        rate,
        channels
    );
    let stream = device.build_input_stream::<f32, _, _>(
        config,
        move |data, _| {
            let mut queue = samples.lock().unwrap();
            queue.extend(
                data.chunks(channels)
                    .map(|c| c.iter().sum::<f32>() / channels as f32),
            );
            cap_capture(&mut queue, rate);
        },
        |err| eprintln!("input stream error: {err}"),
        None,
    )?;
    stream.play()?;
    Ok((stream, rate))
}

/// starts the default output device; it plays mono `samples` on every channel, silence when empty
pub fn open_output(samples: Samples) -> Result<(cpal::Stream, u32), Box<dyn Error>> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no default output device")?;
    let config = low_latency(device.default_output_config()?);
    let (channels, rate) = (config.channels as usize, config.sample_rate);
    println!(
        "output: {} ({} Hz, {} ch)",
        device.description()?,
        rate,
        channels
    );
    let stream = device.build_output_stream::<f32, _, _>(
        config,
        move |data, _| {
            let mut queue = samples.lock().unwrap();
            for frame in data.chunks_mut(channels) {
                frame.fill(queue.pop_front().unwrap_or(0.0));
            }
        },
        |err| eprintln!("output stream error: {err}"),
        None,
    )?;
    stream.play()?;
    Ok((stream, rate))
}

/// streaming linear interpolation. voice at 16 kbit/s is band-limited to 8 kHz
/// by opus, far below either nyquist, so linear is enough here.
pub struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler {
            step: from as f64 / to as f64,
            pos: 0.0,
            last: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        // position 0 is the last sample of the previous call, k is input[k - 1]
        let last = self.last;
        let at = |k: usize| if k == 0 { last } else { input[k - 1] };
        let n = input.len() as f64;
        while self.pos < n {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            out.push(at(i) + (at(i + 1) - at(i)) * frac);
            self.pos += self.step;
        }
        self.pos -= n;
        self.last = input[input.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_queue_keeps_the_newest_second() {
        let mut queue: VecDeque<f32> = (0..3 * 100).map(|i| i as f32).collect();
        cap_capture(&mut queue, 100);
        assert_eq!(queue.len(), 100);
        assert_eq!(queue[0], 200.0);
    }

    #[test]
    fn resampler_rate_and_continuity_across_chunks() {
        let mut r = Resampler::new(44_100, OPUS_RATE);
        let input: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 1, "{}", out.len());
        // a smooth input stays smooth: no jumps at chunk borders
        assert!(out.windows(2).skip(1).all(|w| (w[1] - w[0]).abs() < 0.011));
    }

    #[test]
    fn resampler_downsamples() {
        let mut r = Resampler::new(OPUS_RATE, 44_100);
        let mut out = Vec::new();
        for _ in 0..50 {
            r.process(&[0.5; 960], &mut out);
        }
        assert!((out.len() as i64 - 44_100).abs() <= 1, "{}", out.len());
        assert!(out[1..].iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }
}
