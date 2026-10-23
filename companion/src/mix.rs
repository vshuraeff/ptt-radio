//! one jitter queue and opus decoder per speaker, mixed every 20 ms tick.
//! a backlog the jitter queue reports is shed one frame per tick: a quiet frame
//! (pause, pre-roll before the first word) at once, any frame once the backlog
//! has stood for SHED_SPEECH_AFTER, with a short crossfade over the cut.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use opusic_c::{Channels, Decoder, SampleRate};

use crate::jitter::{Pull, Stream};
use crate::tx::FRAME_SAMPLES;

// largest opus frame is 120 ms
const MAX_DECODE: usize = 5760;
/// a frame this far below the speaker's recent level, or below QUIET_FLOOR, is a pause
const QUIET_RATIO: f32 = 0.1;
/// -50 dBFS rms
const QUIET_FLOOR: f32 = 0.003;
/// the level follows peaks at once and falls 20 dB in about 2 s
const LEVEL_DECAY: f32 = 0.977;
/// continuous speech with no pause to cut: shed a voiced frame after this
const SHED_SPEECH_AFTER: Duration = Duration::from_millis(400);
/// 2.5 ms
const CROSSFADE: usize = 120;

struct Speaker {
    // (seq, opus frame)
    stream: Stream<(u16, Vec<u8>)>,
    decoder: Decoder,
    /// recent rms level of this speaker's decoded frames
    level: f32,
}

#[derive(Default)]
pub struct MixStats {
    pub decoded: u64,
    pub decode_errors: u64,
    pub dropped_late: u64,
    pub underruns: u64,
    /// frames skipped to cut a backlog
    pub shed: u64,
}

/// one speaker's contribution to the last tick, for the trace
pub struct Pulled {
    pub sender: u16,
    /// None: underrun, the decoder concealed the missing frame
    pub seq: Option<u16>,
    /// a frame skipped in this tick to cut the backlog
    pub shed: Option<u16>,
    /// frames still queued after this pull
    pub queued: usize,
}

impl Speaker {
    /// decodes into `out`, zeroed on an undecodable frame; false when it was not opus
    fn decode(&mut self, frame: &[u8], out: &mut [f32]) -> bool {
        let mut decoded = [0.0f32; MAX_DECODE];
        out.fill(0.0);
        let Ok(n) = self
            .decoder
            .decode_float_to_slice(frame, &mut decoded, false)
        else {
            return false;
        };
        let n = n.min(out.len());
        out[..n].copy_from_slice(&decoded[..n]);
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        self.level = rms.max(self.level * LEVEL_DECAY);
        true
    }

    /// a frame that did not arrive in time, continued from the decoder's state (opus
    /// packet-loss concealment) instead of a gap
    fn conceal(&mut self, out: &mut [f32; FRAME_SAMPLES]) {
        if self.decoder.decode_float_to_slice(&[], out, false).is_err() {
            out.fill(0.0);
        }
    }

    fn is_quiet(&self, pcm: &[f32]) -> bool {
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt();
        rms < QUIET_FLOOR.max(self.level * QUIET_RATIO)
    }
}

#[derive(Default)]
pub struct Mixer {
    speakers: BTreeMap<u16, Speaker>,
    pub stats: MixStats,
    /// what the last `tick` played, per audible speaker
    pub pulled: Vec<Pulled>,
}

impl Mixer {
    pub fn start(&mut self, sender: u16, now: Instant) {
        match self.speakers.get_mut(&sender) {
            Some(speaker) => speaker.stream.restart(now),
            None => self.open(sender, now),
        }
    }

    pub fn voice(&mut self, sender: u16, seq: u16, frame: &[u8], now: Instant) {
        if !self.speakers.contains_key(&sender) {
            // START was missed (companion started mid-transmission)
            self.open(sender, now);
        }
        let speaker = self.speakers.get_mut(&sender).expect("opened above");
        self.stats.dropped_late += speaker.stream.push((seq, frame.to_vec()), now) as u64;
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
        let mut pcm = [0.0f32; FRAME_SAMPLES];
        let mut next = [0.0f32; FRAME_SAMPLES];
        let stats = &mut self.stats;
        let pulled = &mut self.pulled;
        pulled.clear();
        self.speakers
            .retain(|&sender, speaker| match speaker.stream.pull(now) {
                Pull::Wait => true,
                Pull::Closed => false,
                Pull::Silence => {
                    stats.underruns += 1;
                    audible = true;
                    speaker.conceal(&mut pcm);
                    for (out, sample) in mix.iter_mut().zip(&pcm) {
                        *out += sample;
                    }
                    pulled.push(Pulled {
                        sender,
                        seq: None,
                        shed: None,
                        queued: 0,
                    });
                    true
                }
                Pull::Frame((mut seq, frame)) => {
                    audible = true;
                    let mut count = |ok: bool| {
                        // not opus (e.g. radio.py's fake frames): this speaker is silent for the frame
                        if ok {
                            stats.decoded += 1
                        } else {
                            stats.decode_errors += 1
                        }
                    };
                    count(speaker.decode(&frame, &mut pcm));
                    let mut shed = None;
                    let backlog = speaker.stream.backlog() > 0;
                    let speech_due = speaker.stream.backlog_for(now) >= SHED_SPEECH_AFTER;
                    if backlog
                        && (speaker.is_quiet(&pcm) || speech_due)
                        && let Some((next_seq, next_frame)) = speaker.stream.shed()
                    {
                        count(speaker.decode(&next_frame, &mut next));
                        // the cut frame continues the previous one: fade from it into the next
                        for i in 0..CROSSFADE {
                            let w = (i + 1) as f32 / (CROSSFADE + 1) as f32;
                            next[i] = pcm[i] * (1.0 - w) + next[i] * w;
                        }
                        pcm = next;
                        stats.shed += 1;
                        shed = Some(seq);
                        seq = next_seq;
                    }
                    for (out, sample) in mix.iter_mut().zip(&pcm) {
                        *out += sample;
                    }
                    pulled.push(Pulled {
                        sender,
                        seq: Some(seq),
                        shed,
                        queued: speaker.stream.queued(),
                    });
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
                level: 0.0,
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
        mixer.voice(1, 1, opus_frame, t0);
        mixer.voice(1, 2, opus_frame, t0);
        // code-3 packet with a frame count of 0 is invalid opus
        mixer.start(0, t0);
        mixer.voice(0, 7, &[0x03, 0x00], t0);
        mixer.voice(0, 8, &[0x03, 0x00], t0);
        let frame = mixer.tick(t0).expect("audible");
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert!(frame.iter().all(|s| s.abs() < 1.0));
        assert_eq!((mixer.stats.decoded, mixer.stats.decode_errors), (1, 1));
        let pulled: Vec<_> = mixer
            .pulled
            .iter()
            .map(|p| (p.sender, p.seq, p.queued))
            .collect();
        assert_eq!(pulled, [(0, Some(7), 1), (1, Some(1), 1)]);

        mixer.stop(1);
        mixer.stop(0);
        mixer.tick(t0 + Duration::from_millis(20));
        assert!(mixer.tick(t0 + Duration::from_millis(40)).is_none());
        assert!(mixer.is_idle());
    }
}
