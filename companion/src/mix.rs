//! one jitter queue and opus decoder per speaker, mixed every 20 ms tick

use std::collections::BTreeMap;
use std::time::Instant;

use opusic_c::{Channels, Decoder, SampleRate};

use crate::jitter::{Pull, Stream};
use crate::tx::FRAME_SAMPLES;

// largest opus frame is 120 ms
const MAX_DECODE: usize = 5760;

struct Speaker {
    stream: Stream,
    decoder: Decoder,
}

#[derive(Default)]
pub struct MixStats {
    pub decoded: u64,
    pub decode_errors: u64,
    pub dropped_late: u64,
    pub underruns: u64,
}

#[derive(Default)]
pub struct Mixer {
    speakers: BTreeMap<u16, Speaker>,
    pub stats: MixStats,
}

impl Mixer {
    pub fn start(&mut self, sender: u16, now: Instant) {
        match self.speakers.get_mut(&sender) {
            Some(speaker) => speaker.stream.restart(now),
            None => self.open(sender, now),
        }
    }

    pub fn voice(&mut self, sender: u16, frame: &[u8], now: Instant) {
        if !self.speakers.contains_key(&sender) {
            // START was missed (companion started mid-transmission)
            self.open(sender, now);
        }
        let speaker = self.speakers.get_mut(&sender).expect("opened above");
        self.stats.dropped_late += speaker.stream.push(frame.to_vec(), now) as u64;
    }

    pub fn stop(&mut self, sender: u16) {
        if let Some(speaker) = self.speakers.get_mut(&sender) {
            speaker.stream.stop();
        }
    }

    pub fn is_idle(&self) -> bool {
        self.speakers.is_empty()
    }

    /// one 20 ms frame of the mix at 48 kHz, or None when nobody is audible
    pub fn tick(&mut self, now: Instant) -> Option<Vec<f32>> {
        let mut mix = vec![0.0f32; FRAME_SAMPLES];
        let mut audible = false;
        let mut decoded = [0.0f32; MAX_DECODE];
        let stats = &mut self.stats;
        self.speakers
            .retain(|_, speaker| match speaker.stream.pull(now) {
                Pull::Wait => true,
                Pull::Closed => false,
                Pull::Silence => {
                    stats.underruns += 1;
                    audible = true;
                    true
                }
                Pull::Frame(frame) => {
                    audible = true;
                    match speaker
                        .decoder
                        .decode_float_to_slice(&frame, &mut decoded, false)
                    {
                        Ok(n) => {
                            stats.decoded += 1;
                            for (out, sample) in
                                mix.iter_mut().zip(&decoded[..n.min(FRAME_SAMPLES)])
                            {
                                *out += sample;
                            }
                        }
                        // not opus (e.g. radio.py's fake frames): this speaker is silent for the frame
                        Err(_) => stats.decode_errors += 1,
                    }
                    true
                }
            });
        if !audible {
            return None;
        }
        for sample in &mut mix {
            *sample = sample.tanh();
        }
        Some(mix)
    }

    fn open(&mut self, sender: u16, now: Instant) {
        let decoder = Decoder::new(Channels::Mono, SampleRate::Hz48000).expect("opus decoder");
        self.speakers.insert(
            sender,
            Speaker {
                stream: Stream::new(now),
                decoder,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::Transmitter;
    use std::time::Duration;

    #[test]
    fn decodes_mixes_and_survives_garbage() {
        let t0 = Instant::now();
        let mut tx = Transmitter::new(16_000).unwrap();
        tx.set_ptt(true);
        let tone: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| 0.3 * (i as f32 * 0.05).sin())
            .collect();
        let packets = tx.frame(&tone);
        let opus_frame = &packets[1][5..];

        let mut mixer = Mixer::default();
        mixer.start(1, t0);
        mixer.voice(1, opus_frame, t0);
        mixer.voice(1, opus_frame, t0);
        // code-3 packet with a frame count of 0 is invalid opus
        mixer.start(0, t0);
        mixer.voice(0, &[0x03, 0x00], t0);
        mixer.voice(0, &[0x03, 0x00], t0);
        let frame = mixer.tick(t0).expect("audible");
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert!(frame.iter().all(|s| s.abs() < 1.0));
        assert_eq!((mixer.stats.decoded, mixer.stats.decode_errors), (1, 1));

        mixer.stop(1);
        mixer.stop(0);
        mixer.tick(t0 + Duration::from_millis(20));
        assert!(mixer.tick(t0 + Duration::from_millis(40)).is_none());
        assert!(mixer.is_idle());
    }
}
