//! a virtual duplex audio device that stands in for the cpal input and output
//! (audio.rs) through the same sample queues, so the rest of the companion
//! runs unchanged. one thread imitates the device callback every 10 ms on its
//! own timeline, where sample k sounds at `start + k / 48 kHz`:
//! - it delivers the mic chunk spoken during the last period: a WAV played in
//!   real time (silence after its end) plus, when coupling is set, the
//!   speaker's own output arriving back through the room;
//! - it takes the next chunk from the output queue (silence when empty), which
//!   sounds during the coming period, and records it to a WAV.

use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{OPUS_RATE, Resampler, Samples, cap_capture};

pub const PERIOD: Duration = Duration::from_millis(10);
const PERIOD_SAMPLES: usize = OPUS_RATE as usize / 100;

type WavOut = hound::WavWriter<BufWriter<File>>;

/// speaker to mic path: `mic[k] += gain * sum_i ir[i] * speaker[k - delay - i]`
pub struct Coupling {
    pub gain: f32,
    pub delay_samples: usize,
    /// nonzero taps (offset, weight); a plain delay is [(0, 1.0)]
    pub taps: Vec<(usize, f32)>,
}

impl Coupling {
    pub fn new(gain_db: f32, delay: Duration, ir: &[f32]) -> Self {
        Coupling {
            gain: 10f32.powf(gain_db / 20.0),
            delay_samples: (delay.as_secs_f64() * OPUS_RATE as f64).round() as usize,
            taps: ir
                .iter()
                .enumerate()
                .filter(|(_, w)| w.abs() > 1e-6)
                .map(|(i, w)| (i, *w))
                .collect(),
        }
    }

    fn span(&self) -> usize {
        self.delay_samples + self.taps.last().map_or(0, |(i, _)| i + 1)
    }
}

pub struct VirtualDevice {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<(), hound::Error>>,
    /// wall clock of sample 0 of both the mic and the speaker timeline
    pub start_us: u64,
    /// samples (silence included) the speaker timeline has taken from the output queue; updated
    /// under the queue lock, so `popped + queue.len()` read under it is the timeline index at
    /// which the next sample pushed will sound, whenever the device thread wakes
    pub popped: Arc<AtomicU64>,
}

impl VirtualDevice {
    /// `mic` is 48 kHz mono; `input` and `output` are the queues the companion
    /// would otherwise share with cpal, both at 48 kHz
    pub fn start(
        mic: Vec<f32>,
        coupling: Option<Coupling>,
        speaker_wav: Option<&Path>,
        input: Samples,
        output: Samples,
    ) -> Result<Self, Box<dyn Error>> {
        let mut speaker = match speaker_wav {
            Some(path) => Some(hound::WavWriter::create(
                path,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: OPUS_RATE,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )?),
            None => None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let popped = Arc::new(AtomicU64::new(0));
        let counter = popped.clone();
        // the first callback comes one period after the start, as with a real device
        let start = Instant::now();
        let start_us = crate::unix_us();
        let thread = thread::spawn(move || {
            let result = run(
                start,
                &mic,
                coupling,
                speaker.as_mut(),
                (&input, &output),
                &counter,
                &flag,
            );
            match speaker {
                Some(writer) => result.and(writer.finalize()),
                None => result,
            }
        });
        Ok(VirtualDevice {
            stop,
            thread,
            start_us,
            popped,
        })
    }

    pub fn stop(self) -> Result<(), hound::Error> {
        self.stop.store(true, Relaxed);
        self.thread.join().expect("virtual audio thread")
    }
}

fn run(
    start: Instant,
    mic: &[f32],
    coupling: Option<Coupling>,
    mut speaker_wav: Option<&mut WavOut>,
    (input, output): (&Samples, &Samples),
    popped: &AtomicU64,
    stop: &AtomicBool,
) -> Result<(), hound::Error> {
    // speaker samples still needed by the coupling, history[0] is sample `history_start`
    let mut history: VecDeque<f32> = VecDeque::new();
    let mut history_start = 0usize;
    let mut chunk = vec![0.0f32; PERIOD_SAMPLES];
    for n in 0usize.. {
        thread::sleep((start + PERIOD * n as u32).saturating_duration_since(Instant::now()));
        if stop.load(Relaxed) {
            break;
        }
        if n > 0 {
            // mic samples [(n-1)P, nP): spoken during the period that just ended
            let first = (n - 1) * PERIOD_SAMPLES;
            for (i, out) in chunk.iter_mut().enumerate() {
                let k = first + i;
                let mut s = mic.get(k).copied().unwrap_or(0.0);
                if let Some(c) = &coupling {
                    let echo: f32 = c
                        .taps
                        .iter()
                        .filter_map(|(offset, w)| {
                            let at = k.checked_sub(c.delay_samples + offset)?;
                            let h = at.checked_sub(history_start)?;
                            history.get(h).map(|s| w * s)
                        })
                        .sum();
                    s += c.gain * echo;
                }
                *out = s;
            }
            let mut queue = input.lock().unwrap();
            queue.extend(&chunk);
            cap_capture(&mut queue, OPUS_RATE);
            drop(queue);
            if let Some(c) = &coupling {
                let keep_from = (first + PERIOD_SAMPLES).saturating_sub(c.span());
                let drop = keep_from.saturating_sub(history_start).min(history.len());
                history.drain(..drop);
                history_start += drop;
            }
        }
        // speaker samples [nP, (n+1)P): sound during the coming period
        {
            let mut queue = output.lock().unwrap();
            for out in chunk.iter_mut() {
                *out = queue.pop_front().unwrap_or(0.0);
            }
            // under the same lock as the pop, so a reader of the queue length sees both
            popped.fetch_add(PERIOD_SAMPLES as u64, Relaxed);
        }
        if coupling.is_some() {
            history.extend(&chunk);
        }
        if let Some(writer) = speaker_wav.as_mut() {
            for s in &chunk {
                writer.write_sample(*s)?;
            }
        }
    }
    Ok(())
}

/// reads a WAV as 48 kHz mono f32, averaging channels and resampling linearly
pub fn read_wav(path: &Path) -> Result<Vec<f32>, Box<dyn Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    let channels = spec.channels as usize;
    let mono: Vec<f32> = interleaved
        .chunks(channels)
        .map(|c| c.iter().sum::<f32>() / channels as f32)
        .collect();
    if spec.sample_rate == OPUS_RATE {
        return Ok(mono);
    }
    let mut out = Vec::new();
    Resampler::new(spec.sample_rate, OPUS_RATE).process(&mono, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // a click on the speaker comes back on the mic after delay + ir offset, scaled by gain
    #[test]
    fn coupling_returns_the_speaker_output_delayed_and_scaled() {
        let input = Samples::default();
        let output = Samples::default();
        let mut click = vec![0.0f32; 3 * PERIOD_SAMPLES];
        click[100] = 1.0;
        output.lock().unwrap().extend(&click);
        let coupling = Coupling::new(-6.0, Duration::from_millis(2), &[0.0, 1.0, 0.0, 0.5]);
        let device =
            VirtualDevice::start(vec![0.25; 10], Some(coupling), None, input.clone(), output)
                .unwrap();
        thread::sleep(PERIOD * 5);
        device.stop().unwrap();
        let mic: Vec<f32> = input.lock().unwrap().iter().copied().collect();
        assert!(mic.len() >= 3 * PERIOD_SAMPLES);
        assert_eq!(mic[..10], [0.25; 10]);
        let gain = 10f32.powf(-6.0 / 20.0);
        // 2 ms = 96 samples, then taps at +1 and +3
        let echo: Vec<(usize, f32)> = mic
            .iter()
            .enumerate()
            .skip(10)
            .filter(|(_, s)| s.abs() > 1e-6)
            .map(|(i, s)| (i, *s))
            .collect();
        assert_eq!(echo, [(100 + 96 + 1, gain), (100 + 96 + 3, 0.5 * gain)]);
    }
}
