//! one stream per speaker: a reorder stage, one opus decoder and a jitter queue,
//! mixed every SLICE tick. a packet that continues its stream is decoded as it
//! arrives (the game transport is lossless and in order); one behind a gap waits
//! in the reorder stage until the gap fills or its deadline, when the gap is
//! concealed (PLC, or FEC from the next packet). decoded audio is queued as
//! SLICE-long pieces, so senders with 10 ms and 20 ms packets mix alike and
//! nobody's setting has to match. a backlog the jitter queue reports is shed one
//! slice per tick: a quiet slice (pause, pre-roll before the first word) at once,
//! any slice once the backlog has stood for SHED_SPEECH_AFTER, with a short
//! crossfade over the cut.
//!
//! a player is on legacy game media until trusted signaling selects a stream
//! generation for it (`select`); then only its v2 media of that generation plays,
//! under the talkspurt rules of `game`, and its legacy copies are ignored. its
//! order and decoder outlive an idle close of its pcm stream until a newer
//! talkspurt opens, so a late copy of what already played stays a duplicate.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use opusic_c::{Channels, Decoder, SampleRate};

use crate::audio::OPUS_RATE;
use crate::jitter::{Pull, SLICE, Stream};
use crate::reorder::{self, Reorder};
use crate::transport::wire::{GameMedia, Kind};
use crate::transport::{
    CompressedFrame, InsertResult, Ordered, PreDecodeOrder, SerialOrder, StreamId, selected_stream,
    serial_order,
};

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
/// the generation of legacy game media, which carries none; a companion's own is never 0
const LEGACY: u64 = 0;
/// a selected stream's talkspurt is forgotten after this long without its media, long
/// before 32768 presses could make a newer one compare as older
const TALKSPURT_FORGET: Duration = Duration::from_secs(60);

/// SLICE of decoded audio and where it came from: the packet's seq and the sample
/// offset of its first sample in that packet
pub struct Slice {
    pub seq: u16,
    pub offset: usize,
    pcm: [f32; SLICE_SAMPLES],
}

struct Speaker {
    /// None: the pcm stream closed, but a selected stream keeps its order and decoder
    stream: Option<Stream<Slice>>,
    /// this speaker's compressed packets in order, before its one decoder
    order: Reorder,
    decoder: Decoder,
    /// decoded samples not yet a whole slice, each with its packet seq and offset
    pending: VecDeque<(u16, usize, f32)>,
    /// recent rms level of this speaker's decoded packets
    level: f32,
    /// length of the last decoded packet: how long a missing one is concealed
    packet_samples: usize,
}

/// the stream trusted signaling selected for a player, and its newest talkspurt
struct Selection {
    generation: u64,
    /// newest talkspurt seen and whether it is still open (no STOP yet)
    talkspurt: Option<(u16, bool)>,
    last_seen: Instant,
}

#[derive(Default)]
pub struct MixStats {
    /// packets
    pub decoded: u64,
    pub decode_errors: u64,
    /// packets never decoded: duplicates, late after their gap was concealed, another
    /// stream generation, an older or closed talkspurt
    pub discarded: u64,
    /// gaps concealed before decoding
    pub missing: u64,
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
    fn new(stream: StreamId, now: Instant) -> Self {
        Speaker {
            stream: Some(Stream::new(now)),
            order: Reorder::new(stream, reorder::GAME_ONLY),
            decoder: Decoder::new(Channels::Mono, SampleRate::Hz48000).expect("opus decoder"),
            pending: VecDeque::new(),
            level: 0.0,
            // 20 ms, the default packet, until one is decoded
            packet_samples: OPUS_RATE as usize / 50,
        }
    }

    /// decodes what the reorder stage releases at `now`
    fn drain(&mut self, now: Instant, stats: &mut MixStats) {
        loop {
            let dropped = match self.order.poll(now) {
                Ordered::Frame(frame) => {
                    let (ok, dropped) = self.decode(frame.id.sequence as u16, &frame.opus, now);
                    if ok {
                        stats.decoded += 1;
                    } else {
                        stats.decode_errors += 1;
                    }
                    dropped
                }
                Ordered::Missing { id, next } => {
                    stats.missing += 1;
                    self.missing(id.sequence as u16, next.map(|f| f.opus), now)
                }
                Ordered::Wait => return,
            };
            stats.dropped_late += dropped as u64;
        }
    }

    /// decodes one packet into slices; false when it was not opus
    fn decode(&mut self, seq: u16, packet: &[u8], now: Instant) -> (bool, usize) {
        let mut decoded = [0.0f32; MAX_DECODE];
        let (ok, n) = match self
            .decoder
            .decode_float_to_slice(packet, &mut decoded, false)
        {
            Ok(n) => {
                self.packet_samples = n;
                (true, n)
            }
            Err(_) => {
                decoded[..GARBAGE_SAMPLES].fill(0.0);
                (false, GARBAGE_SAMPLES)
            }
        };
        (ok, self.queue(seq, &decoded[..n], now))
    }

    /// a packet that never came: FEC from the one after it, or with none of that (or no
    /// FEC in it) the decoder's own concealment; returns slices dropped like `decode`
    fn missing(&mut self, seq: u16, next: Option<Vec<u8>>, now: Instant) -> usize {
        let mut pcm = [0.0f32; MAX_DECODE];
        let next = next.as_deref();
        let n = next
            .and_then(|p| self.decoder.get_nb_samples(p).ok())
            .unwrap_or(self.packet_samples)
            .min(MAX_DECODE);
        let n = self
            .decoder
            .decode_float_to_slice(next.unwrap_or(&[]), &mut pcm[..n], next.is_some())
            .unwrap_or(n);
        self.queue(seq, &pcm[..n], now)
    }

    /// queues decoded samples of packet `seq` as slices; returns how many were dropped
    fn queue(&mut self, seq: u16, pcm: &[f32], now: Instant) -> usize {
        let n = pcm.len();
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
            // audio of a selected stream after its pcm stream closed opens a new one
            dropped += self
                .stream
                .get_or_insert_with(|| Stream::new(now))
                .push(slice, now);
        }
        dropped
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
    /// players on v2 game media; the rest are on legacy media
    selected: BTreeMap<u16, Selection>,
    pub stats: MixStats,
    /// what the last `tick` played, per audible speaker
    pub pulled: Vec<Pulled>,
}

fn legacy(sender: u16) -> StreamId {
    StreamId {
        player: sender,
        generation: LEGACY,
    }
}

impl Mixer {
    /// legacy START. it carries no generation, so the sequence begins anew with the next
    /// VOICE: a restarted legacy companion counts from 1 again
    pub fn start(&mut self, sender: u16, now: Instant) {
        if self.selected.contains_key(&sender) {
            return;
        }
        self.start_stream(legacy(sender), now);
        let speaker = self.speakers.get_mut(&sender).expect("opened above");
        speaker.order = Reorder::new(legacy(sender), reorder::GAME_ONLY);
    }

    /// legacy VOICE; returns whether it was queued for decoding
    pub fn voice(&mut self, sender: u16, seq: u16, packet: &[u8], now: Instant) -> bool {
        if self.selected.contains_key(&sender) {
            // the v2 copy of this frame is the one that plays
            self.stats.discarded += 1;
            return false;
        }
        self.push(legacy(sender), 0, seq, packet, now)
    }

    /// legacy STOP
    pub fn stop(&mut self, sender: u16) {
        if !self.selected.contains_key(&sender) {
            self.stop_stream(sender);
        }
    }

    /// trusted signaling (ANNOUNCE) selected `stream` for its player: from now on only v2
    /// media of that generation plays, and the player's legacy copies are ignored. a new
    /// generation is a restarted companion: the former stream, its decoder and queues go
    pub fn select(&mut self, stream: StreamId, now: Instant) {
        let player = stream.player;
        if self
            .selected
            .get(&player)
            .is_some_and(|s| s.generation == stream.generation)
        {
            return;
        }
        self.selected.insert(
            player,
            Selection {
                generation: stream.generation,
                talkspurt: None,
                last_seen: now,
            },
        );
        self.speakers.remove(&player);
    }

    /// v2 game media (START2, VOICE2, STOP2) of a selected stream; returns whether it was
    /// taken. talkspurts: a repeated START of the open one changes nothing, STOP closes
    /// only the open one it names and any other STOP is discarded, anything of an older
    /// or already closed one is discarded, and START or VOICE of a newer one opens it
    /// even when its START was lost
    pub fn game(&mut self, media: GameMedia<'_>, now: Instant) -> bool {
        let player = media.stream.player;
        let Some(selection) = self.selected.get_mut(&player) else {
            self.stats.discarded += 1;
            return false;
        };
        let selected = StreamId {
            player,
            generation: selection.generation,
        };
        if selected_stream(selected, player, media.stream.generation).is_none() {
            self.stats.discarded += 1;
            return false;
        }
        if now - selection.last_seen > TALKSPURT_FORGET {
            selection.talkspurt = None;
        }
        selection.last_seen = now;
        let order = selection
            .talkspurt
            .map(|(talkspurt, open)| (serial_order(media.talkspurt, talkspurt), open));
        if media.kind == Kind::Stop {
            // a STOP of another talkspurt is not remembered: on several paths it can
            // overtake its own talkspurt's media, which then plays and closes when idle
            if order != Some((SerialOrder::Equal, true)) {
                self.stats.discarded += 1;
                return false;
            }
            selection.talkspurt = Some((media.talkspurt, false));
            self.stop_stream(player);
            return true;
        }
        let opens = match order {
            None | Some((SerialOrder::Newer, _)) => true,
            Some((SerialOrder::Equal, true)) => false,
            _ => {
                self.stats.discarded += 1;
                return false;
            }
        };
        selection.talkspurt = Some((media.talkspurt, true));
        if opens
            && self
                .speakers
                .get(&player)
                .is_some_and(|s| s.stream.is_none())
        {
            // what the closed pcm stream kept serves only its own talkspurt, whose media
            // is refused from now on: a new one starts afresh, like a first press
            self.speakers.remove(&player);
        }
        if opens || media.kind == Kind::Start {
            self.start_stream(selected, now);
        }
        if media.kind == Kind::Voice {
            return self.push(selected, media.talkspurt, media.sequence, media.opus, now);
        }
        true
    }

    /// START on a stream that still plays keeps its decoder and queues (Stream::restart)
    fn start_stream(&mut self, stream: StreamId, now: Instant) {
        match self.speakers.get_mut(&stream.player) {
            Some(speaker) => match &mut speaker.stream {
                Some(pcm) => pcm.restart(now),
                None => speaker.stream = Some(Stream::new(now)),
            },
            None => {
                self.speakers
                    .insert(stream.player, Speaker::new(stream, now));
            }
        }
    }

    /// into the reorder stage, and decoded at once when it continues the stream
    fn push(
        &mut self,
        stream: StreamId,
        talkspurt: u16,
        seq: u16,
        opus: &[u8],
        now: Instant,
    ) -> bool {
        // without a speaker the START was missed (companion started mid-transmission)
        let speaker = self
            .speakers
            .entry(stream.player)
            .or_insert_with(|| Speaker::new(stream, now));
        let queued = speaker.order.media_id(seq).is_some_and(|id| {
            let frame = CompressedFrame {
                id,
                talkspurt,
                opus: opus.to_vec(),
            };
            speaker.order.push(frame, now) == InsertResult::Queued
        });
        if !queued {
            self.stats.discarded += 1;
            return false;
        }
        speaker.drain(now, &mut self.stats);
        true
    }

    /// STOP: what waits behind a gap is released now, then the queue plays out and closes
    fn stop_stream(&mut self, player: u16) {
        if let Some(speaker) = self.speakers.get_mut(&player) {
            while let Some(at) = speaker.order.deadline() {
                speaker.drain(at, &mut self.stats);
            }
            if let Some(pcm) = &mut speaker.stream {
                pcm.stop();
            }
        }
    }

    /// no pcm stream left to play
    pub fn is_idle(&self) -> bool {
        self.speakers.values().all(|s| s.stream.is_none())
    }

    /// one SLICE of the mix at 48 kHz, or None when nobody is audible
    pub fn tick(&mut self, now: Instant) -> Option<Vec<f32>> {
        let mut mix = vec![0.0f32; SLICE_SAMPLES];
        let mut audible = false;
        let mut pcm = [0.0f32; SLICE_SAMPLES];
        let stats = &mut self.stats;
        let pulled = &mut self.pulled;
        let selected = &self.selected;
        pulled.clear();
        self.speakers.retain(|&sender, speaker| {
            // a gap whose deadline passed since the last packet
            speaker.drain(now, stats);
            let Some(pull) = speaker.stream.as_mut().map(|s| s.pull(now)) else {
                return true;
            };
            match pull {
                Pull::Wait => true,
                // a selected stream keeps what it played, so a late copy stays a duplicate
                Pull::Closed => {
                    speaker.stream = None;
                    selected.contains_key(&sender)
                }
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
                    let quiet = speaker.is_quiet(&slice.pcm);
                    let stream = speaker.stream.as_mut().expect("pulled above");
                    let backlog = stream.backlog() > 0;
                    let speech_due = stream.backlog_for(now) >= SHED_SPEECH_AFTER;
                    if backlog
                        && (quiet || speech_due)
                        && let Some(mut next) = stream.shed()
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
                        queued: stream.queued(),
                    });
                    true
                }
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
}

// half-duplex mute ramp: 5 ms, short enough to be a cut, long enough not to click
const MUTE_RAMP: usize = OPUS_RATE as usize / 200;

/// output gain for half-duplex: ramps to 0 while muted and back to 1 after
pub struct Mute {
    gain: f32,
}

impl Default for Mute {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Mute {
    pub fn apply(&mut self, muted: bool, slice: &mut [f32]) {
        let target = if muted { 0.0 } else { 1.0 };
        if self.gain == target && target == 1.0 {
            return;
        }
        let step = 1.0 / MUTE_RAMP as f32;
        for s in slice {
            self.gain = if muted {
                (self.gain - step).max(0.0)
            } else {
                (self.gain + step).min(1.0)
            };
            *s *= self.gain;
        }
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

    const GEN_A: u64 = 0xa;
    const GEN_B: u64 = 0xb;

    fn v2<'a>(
        kind: Kind,
        generation: u64,
        talkspurt: u16,
        seq: u16,
        opus: &'a [u8],
    ) -> GameMedia<'a> {
        GameMedia {
            kind,
            stream: StreamId {
                player: 1,
                generation,
            },
            sequence: seq,
            talkspurt,
            opus: if kind == Kind::Voice { opus } else { &[] },
        }
    }

    fn queued(mixer: &Mixer) -> usize {
        mixer.speakers[&1].stream.as_ref().map_or(0, Stream::queued)
    }

    #[test]
    fn legacy_media_until_a_stream_is_selected_then_only_v2() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 3);
        let mut mixer = Mixer::default();
        assert!(!mixer.game(v2(Kind::Voice, GEN_A, 1, 1, &opus[0]), t0));
        assert!(mixer.voice(1, 1, &opus[0], t0));
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            t0,
        );
        // the legacy copy of a selected player is ignored, its v2 copy plays
        assert!(!mixer.voice(1, 2, &opus[1], t0));
        assert!(mixer.game(v2(Kind::Voice, GEN_A, 1, 2, &opus[1]), t0));
        assert_eq!((mixer.stats.decoded, mixer.stats.discarded), (2, 2));
    }

    #[test]
    fn repeated_start_changes_nothing_and_stop_closes_only_its_talkspurt() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 2);
        let mut mixer = Mixer::default();
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            t0,
        );
        assert!(mixer.game(v2(Kind::Start, GEN_A, 5, 0, &[]), t0));
        for (seq, opus) in opus.iter().enumerate() {
            mixer.game(v2(Kind::Voice, GEN_A, 5, seq as u16 + 1, opus), t0);
        }
        assert_eq!(queued(&mixer), 4);
        // the badge refresh: same talkspurt, no rebuffering, no new decoder
        assert!(mixer.game(v2(Kind::Start, GEN_A, 5, 2, &[]), t0 + SLICE));
        assert_eq!(queued(&mixer), 4);
        assert_eq!(mixer.stats.decoded, 2);
        // an older talkspurt's STOP leaves this one playing
        assert!(!mixer.game(v2(Kind::Stop, GEN_A, 4, 0, &[]), t0 + SLICE));
        for n in 0..8 {
            mixer.tick(t0 + SLICE * n);
        }
        assert!(!mixer.is_idle(), "closed by another talkspurt's STOP");
        assert!(mixer.game(v2(Kind::Stop, GEN_A, 5, 2, &[]), t0 + SLICE * 8));
        mixer.tick(t0 + SLICE * 9);
        assert!(mixer.is_idle());
    }

    #[test]
    fn late_old_stop_and_start_reopen_nothing() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 2);
        let mut mixer = Mixer::default();
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            t0,
        );
        mixer.game(v2(Kind::Start, GEN_A, 5, 0, &[]), t0);
        mixer.game(v2(Kind::Voice, GEN_A, 5, 1, &opus[0]), t0);
        assert!(mixer.game(v2(Kind::Stop, GEN_A, 5, 1, &[]), t0));
        // a late repeat of the closed talkspurt, and anything of an older one
        for media in [
            v2(Kind::Start, GEN_A, 5, 1, &[]),
            v2(Kind::Voice, GEN_A, 5, 2, &opus[1]),
            v2(Kind::Stop, GEN_A, 5, 1, &[]),
            v2(Kind::Start, GEN_A, 4, 0, &[]),
            v2(Kind::Stop, GEN_A, 4, 0, &[]),
        ] {
            assert!(!mixer.game(media, t0 + SLICE));
        }
        assert_eq!(mixer.stats.discarded, 5);
        assert_eq!(queued(&mixer), 2);
        // the next press, its START lost: its VOICE opens it, and talkspurts wrap
        let mut mixer = Mixer::default();
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            t0,
        );
        mixer.game(v2(Kind::Stop, GEN_A, 65535, 0, &[]), t0);
        assert!(mixer.game(v2(Kind::Voice, GEN_A, 0, 1, &opus[0]), t0));
        assert!(!mixer.game(v2(Kind::Start, GEN_A, 65535, 1, &[]), t0));
    }

    #[test]
    fn restart_with_a_new_generation_replaces_the_stream() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 2);
        let mut mixer = Mixer::default();
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            t0,
        );
        mixer.game(v2(Kind::Start, GEN_A, 9, 0, &[]), t0);
        mixer.game(v2(Kind::Voice, GEN_A, 9, 500, &opus[0]), t0);
        // the restarted companion counts talkspurts and seq from the start again
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_B,
            },
            t0,
        );
        assert!(mixer.is_idle(), "the former stream is dropped at once");
        assert!(mixer.game(v2(Kind::Start, GEN_B, 1, 0, &[]), t0));
        assert!(mixer.game(v2(Kind::Voice, GEN_B, 1, 1, &opus[1]), t0));
        assert_eq!(mixer.speakers[&1].order.stream().generation, GEN_B);
        assert_eq!(queued(&mixer), 2);
        // a delayed packet of the old generation selects nothing and plays nothing
        assert!(!mixer.game(v2(Kind::Voice, GEN_A, 9, 501, &opus[0]), t0));
        assert!(!mixer.game(v2(Kind::Stop, GEN_A, 9, 501, &[]), t0));
        assert_eq!(mixer.speakers[&1].order.stream().generation, GEN_B);
        // selecting the current generation again resets nothing
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_B,
            },
            t0,
        );
        assert_eq!(queued(&mixer), 2);
    }

    #[test]
    fn a_missing_packet_is_concealed_at_its_deadline_and_its_late_copy_discarded() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 3);
        let mut mixer = Mixer::default();
        mixer.start(1, t0);
        assert!(mixer.voice(1, 1, &opus[0], t0));
        assert!(mixer.voice(1, 3, &opus[2], t0));
        assert_eq!(queued(&mixer), 2, "3 waits behind the gap");
        // the playout tick past the deadline conceals 2 (FEC from 3) and decodes 3
        mixer.tick(t0 + reorder::GAME_ONLY.wait);
        assert_eq!((mixer.stats.missing, mixer.stats.decoded), (1, 2));
        assert_eq!(queued(&mixer), 6 - 1);
        assert!(!mixer.voice(1, 2, &opus[1], t0 + SLICE * 2));
        assert_eq!(mixer.stats.discarded, 1);
        // STOP does not leave a frame waiting behind a gap
        mixer.voice(1, 5, &opus[0], t0 + SLICE * 2);
        mixer.stop(1);
        assert_eq!((mixer.stats.missing, mixer.stats.decoded), (2, 3));
    }

    #[test]
    fn mute_ramps_down_and_back_without_steps() {
        let mut mute = Mute::default();
        let mut slice = [1.0; SLICE_SAMPLES];
        mute.apply(false, &mut slice);
        assert_eq!(slice, [1.0; SLICE_SAMPLES]);
        mute.apply(true, &mut slice);
        // 5 ms down, then silence for the rest of the 10 ms slice
        assert!(slice.windows(2).all(|w| w[0] >= w[1] && w[0] - w[1] < 0.01));
        assert!(slice[MUTE_RAMP..].iter().all(|s| *s == 0.0));
        let mut slice = [1.0; SLICE_SAMPLES];
        mute.apply(true, &mut slice);
        assert_eq!(slice, [0.0; SLICE_SAMPLES]);
        let mut slice = [1.0; SLICE_SAMPLES];
        mute.apply(false, &mut slice);
        assert!(slice.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] < 0.01));
        assert!(slice[MUTE_RAMP..].iter().all(|s| *s == 1.0));
    }
}
