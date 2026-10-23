//! one opus decoder and jitter queue per speaker, mixed every SLICE tick.
//! packets are decoded as they arrive (the transport is lossless and in order,
//! so arrival order is decode order) and queued as SLICE-long pieces, so senders
//! with 10 ms and 20 ms packets mix alike and nobody's setting has to match.
//! a backlog the jitter queue reports is shed one slice per tick: a quiet slice
//! (pause, pre-roll before the first word) at once, any slice once the backlog
//! has stood for SHED_SPEECH_AFTER, with a short crossfade over the cut.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use opusic_c::{Channels, Decoder, SampleRate};

use crate::audio::OPUS_RATE;
use crate::jitter::{Pull, SLICE, Stream};

pub const SLICE_SAMPLES: usize = OPUS_RATE as usize * SLICE.as_millis() as usize / 1000;
// largest opus frame is 120 ms
const MAX_DECODE: usize = 5760;
/// an undecodable packet (e.g. radio.py's fake frames) plays as this much silence
const GARBAGE_SAMPLES: usize = 960;
/// a slice this far below the speaker's recent level, or below QUIET_FLOOR, is a pause
const QUIET_RATIO: f32 = 0.1;
/// -50 dBFS rms
const QUIET_FLOOR: f32 = 0.003;
/// the level follows peaks at once and falls 20 dB in about 2 s, per 20 ms of audio
const LEVEL_DECAY: f32 = 0.977;
/// continuous speech with no pause to cut: shed a voiced slice after this
const SHED_SPEECH_AFTER: Duration = Duration::from_millis(400);
/// 2.5 ms
const CROSSFADE: usize = 120;

/// SLICE of decoded audio and where it came from: the packet's seq and the sample
/// offset of its first sample in that packet
pub struct Slice {
    pub seq: u16,
    pub offset: usize,
    pcm: [f32; SLICE_SAMPLES],
}

struct Speaker {
    stream: Stream<Slice>,
    decoder: Decoder,
    /// decoded samples not yet a whole slice, each with its packet seq and offset
    pending: VecDeque<(u16, usize, f32)>,
    /// recent rms level of this speaker's decoded packets
    level: f32,
}

#[derive(Default)]
pub struct MixStats {
    /// packets
    pub decoded: u64,
    pub decode_errors: u64,
    /// slices
    pub dropped_late: u64,
    pub underruns: u64,
    /// slices skipped to cut a backlog
    pub shed: u64,
}

/// one speaker's contribution to the last tick, for the trace
pub struct Pulled {
    pub sender: u16,
    /// (seq, offset) of the slice played; None: underrun, the decoder concealed it
    pub slice: Option<(u16, usize)>,
    /// a slice skipped in this tick to cut the backlog
    pub shed: Option<(u16, usize)>,
    /// slices still queued after this pull
    pub queued: usize,
}

impl Speaker {
    /// decodes one packet into slices; false when it was not opus
    fn decode(&mut self, seq: u16, packet: &[u8], now: Instant) -> (bool, usize) {
        let mut decoded = [0.0f32; MAX_DECODE];
        let (ok, n) = match self
            .decoder
            .decode_float_to_slice(packet, &mut decoded, false)
        {
            Ok(n) => (true, n),
            Err(_) => {
                decoded[..GARBAGE_SAMPLES].fill(0.0);
                (false, GARBAGE_SAMPLES)
            }
        };
        let pcm = &decoded[..n];
        // per 20 ms of audio, whatever the packet duration
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
        let decay = LEVEL_DECAY.powf(n as f32 / 960.0);
        self.level = rms.max(self.level * decay);
        self.pending
            .extend(pcm.iter().enumerate().map(|(i, &s)| (seq, i, s)));
        let mut dropped = 0;
        while self.pending.len() >= SLICE_SAMPLES {
            let (seq, offset, _) = self.pending[0];
            let mut slice = Slice {
                seq,
                offset,
                pcm: [0.0; SLICE_SAMPLES],
            };
            for (out, (_, _, s)) in slice
                .pcm
                .iter_mut()
                .zip(self.pending.drain(..SLICE_SAMPLES))
            {
                *out = s;
            }
            dropped += self.stream.push(slice, now);
        }
        (ok, dropped)
    }

    /// a slice that did not arrive in time, continued from the decoder's state (opus
    /// packet-loss concealment) instead of a gap
    fn conceal(&mut self, out: &mut [f32; SLICE_SAMPLES]) {
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

    pub fn voice(&mut self, sender: u16, seq: u16, packet: &[u8], now: Instant) {
        if !self.speakers.contains_key(&sender) {
            // START was missed (companion started mid-transmission)
            self.open(sender, now);
        }
        let speaker = self.speakers.get_mut(&sender).expect("opened above");
        let (ok, dropped) = speaker.decode(seq, packet, now);
        if ok {
            self.stats.decoded += 1;
        } else {
            self.stats.decode_errors += 1;
        }
        self.stats.dropped_late += dropped as u64;
    }

    pub fn stop(&mut self, sender: u16) {
        if let Some(speaker) = self.speakers.get_mut(&sender) {
            speaker.stream.stop();
        }
    }

    pub fn is_idle(&self) -> bool {
        self.speakers.is_empty()
    }

    /// one SLICE of the mix at 48 kHz, or None when nobody is audible
    pub fn tick(&mut self, now: Instant) -> Option<Vec<f32>> {
        let mut mix = vec![0.0f32; SLICE_SAMPLES];
        let mut audible = false;
        let mut pcm = [0.0f32; SLICE_SAMPLES];
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
                        slice: None,
                        shed: None,
                        queued: 0,
                    });
                    true
                }
                Pull::Frame(mut slice) => {
                    audible = true;
                    let mut shed = None;
                    let backlog = speaker.stream.backlog() > 0;
                    let speech_due = speaker.stream.backlog_for(now) >= SHED_SPEECH_AFTER;
                    if backlog
                        && (speaker.is_quiet(&slice.pcm) || speech_due)
                        && let Some(mut next) = speaker.stream.shed()
                    {
                        // the cut slice continues the previous one: fade from it into the next
                        for i in 0..CROSSFADE {
                            let w = (i + 1) as f32 / (CROSSFADE + 1) as f32;
                            next.pcm[i] = slice.pcm[i] * (1.0 - w) + next.pcm[i] * w;
                        }
                        stats.shed += 1;
                        shed = Some((slice.seq, slice.offset));
                        slice = next;
                    }
                    for (out, sample) in mix.iter_mut().zip(&slice.pcm) {
                        *out += sample;
                    }
                    pulled.push(Pulled {
                        sender,
                        slice: Some((slice.seq, slice.offset)),
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
                pending: VecDeque::new(),
                level: 0.0,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::{Codec, Transmitter};

    fn tone(samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| 0.3 * (i as f32 * 0.05).sin())
            .collect()
    }

    /// the VOICE packets' opus bodies of `frames` frames of a tone at `frame_ms`
    fn opus_frames(frame_ms: usize, frames: usize) -> Vec<Vec<u8>> {
        let mut tx = Transmitter::new(Codec {
            frame_ms,
            ..Codec::default()
        })
        .unwrap();
        tx.set_ptt(true);
        let pcm = tone(tx.frame_samples());
        (0..frames)
            .flat_map(|_| tx.frame(&pcm))
            .filter(|p| p[2] == crate::packet::VOICE)
            .map(|p| p[5..].to_vec())
            .collect()
    }

    #[test]
    fn decodes_mixes_and_survives_garbage() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 1);

        let mut mixer = Mixer::default();
        mixer.start(1, t0);
        mixer.voice(1, 1, &opus[0], t0);
        mixer.voice(1, 2, &opus[0], t0);
        // code-3 packet with a frame count of 0 is invalid opus
        mixer.start(0, t0);
        mixer.voice(0, 7, &[0x03, 0x00], t0);
        mixer.voice(0, 8, &[0x03, 0x00], t0);
        let frame = mixer.tick(t0).expect("audible");
        assert_eq!(frame.len(), SLICE_SAMPLES);
        assert!(frame.iter().all(|s| s.abs() < 1.0));
        assert_eq!((mixer.stats.decoded, mixer.stats.decode_errors), (2, 2));
        let pulled: Vec<_> = mixer
            .pulled
            .iter()
            .map(|p| (p.sender, p.slice, p.queued))
            .collect();
        // each 20 ms packet is two slices
        assert_eq!(pulled, [(0, Some((7, 0)), 3), (1, Some((1, 0)), 3)]);

        mixer.stop(1);
        mixer.stop(0);
        for n in 1..=4 {
            mixer.tick(t0 + SLICE * n);
        }
        assert!(mixer.tick(t0 + SLICE * 5).is_none());
        assert!(mixer.is_idle());
    }

    #[test]
    fn mixes_10_and_20_ms_senders_slice_by_slice() {
        let t0 = Instant::now();
        let mut mixer = Mixer::default();
        mixer.start(1, t0);
        mixer.start(2, t0);
        for (seq, opus) in opus_frames(10, 4).iter().enumerate() {
            mixer.voice(1, seq as u16 + 1, opus, t0);
        }
        for (seq, opus) in opus_frames(20, 2).iter().enumerate() {
            mixer.voice(2, seq as u16 + 1, opus, t0);
        }
        let mut played = Vec::new();
        for n in 0..4 {
            mixer.tick(t0 + SLICE * n).expect("audible");
            played.push(
                mixer
                    .pulled
                    .iter()
                    .map(|p| (p.sender, p.slice))
                    .collect::<Vec<_>>(),
            );
        }
        // 10 ms packets play one per tick, a 20 ms packet over two ticks
        assert_eq!(
            played,
            [
                [(1, Some((1, 0))), (2, Some((1, 0)))],
                [(1, Some((2, 0))), (2, Some((1, 480)))],
                [(1, Some((3, 0))), (2, Some((2, 0)))],
                [(1, Some((4, 0))), (2, Some((2, 480)))],
            ]
        );
        assert_eq!(mixer.stats.decoded, 6);
    }
}
