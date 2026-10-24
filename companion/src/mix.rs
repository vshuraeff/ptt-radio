//! one stream per speaker: a reorder stage, one opus decoder and a jitter queue,
//! mixed every SLICE tick. a packet that continues its stream is decoded as it
//! arrives; one behind a gap waits
//! in the reorder stage until the gap fills or its deadline, when the gap is
//! concealed (PLC, or FEC from the next packet). decoded audio is queued as
//! SLICE-long pieces, so senders with 10 ms and 20 ms packets mix alike and
//! nobody's setting has to match. a backlog the jitter queue reports is shed one
//! slice per tick: a quiet slice (pause, pre-roll before the first word) at once,
//! any slice once the backlog has stood for SHED_SPEECH_AFTER, with a short
//! crossfade over the cut.
//!
//! a legacy player needs no stream selection. v2 game media waits in a bounded
//! hold until trusted signaling selects its generation (`select`); then only
//! v2 media of that generation plays under the talkspurt rules of `game`, and
//! legacy copies are ignored. its
//! order and decoder outlive an idle close of its pcm stream until a newer
//! talkspurt opens, so a late copy of what already played stays a duplicate.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use opusic_c::{Channels, Decoder, SampleRate};

use crate::audio::OPUS_RATE;
use crate::jitter::{Pull, SLICE, Stream};
use crate::quality::{Continuity, Status};
use crate::reorder::{self, Reorder};
use crate::transport::wire::{GameMedia, Kind, MAX_OPUS};
use crate::transport::{
    CompressedFrame, InsertResult, Ordered, PreDecodeOrder, SerialOrder, StreamId, extend_sequence,
    selected_stream, serial_order,
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
const EARLY_LIMIT: usize = 20;
const EARLY_PLAYERS: usize = 256;
const EARLY_TTL: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaOutcome {
    Accepted,
    Held,
    Duplicate,
    Dropped(&'static str),
}

struct Held {
    kind: Kind,
    sequence: u16,
    talkspurt: u16,
    opus: Vec<u8>,
    at: Instant,
}

struct Early {
    generation: u64,
    voice: Vec<Held>,
    start: Option<Held>,
    stop: Option<Held>,
}

impl Early {
    fn len(&self) -> usize {
        self.voice.len() + usize::from(self.start.is_some()) + usize::from(self.stop.is_some())
    }

    fn expire(&mut self, now: Instant) {
        self.voice
            .retain(|m| now.saturating_duration_since(m.at) < EARLY_TTL);
        if self
            .start
            .as_ref()
            .is_some_and(|m| now.saturating_duration_since(m.at) >= EARLY_TTL)
        {
            self.start = None;
        }
        if self
            .stop
            .as_ref()
            .is_some_and(|m| now.saturating_duration_since(m.at) >= EARLY_TTL)
        {
            self.stop = None;
        }
    }

    fn push(&mut self, media: GameMedia<'_>, now: Instant) -> bool {
        if media.opus.len() > MAX_OPUS {
            return false;
        }
        let held = Held {
            kind: media.kind,
            sequence: media.sequence,
            talkspurt: media.talkspurt,
            opus: media.opus.to_vec(),
            at: now,
        };
        match media.kind {
            Kind::Voice => {
                if self.len() == EARLY_LIMIT && self.start.take().is_none() {
                    self.voice.remove(0);
                }
                self.voice.push(held);
            }
            Kind::Start => {
                if let Some(start) = &self.start {
                    if serial_order(media.talkspurt, start.talkspurt) != SerialOrder::Newer {
                        return false;
                    }
                } else if self.len() == EARLY_LIMIT {
                    return false;
                }
                self.start = Some(held);
            }
            Kind::Stop => {
                if let Some(stop) = &self.stop {
                    if serial_order(media.talkspurt, stop.talkspurt) != SerialOrder::Newer
                        && !(media.talkspurt == stop.talkspurt
                            && serial_order(media.sequence, stop.sequence) == SerialOrder::Newer)
                    {
                        return false;
                    }
                } else if self.len() == EARLY_LIMIT && self.start.take().is_none() {
                    self.voice.remove(0);
                }
                self.stop = Some(held);
            }
            _ => return false,
        }
        true
    }

    fn ordered(mut self) -> Vec<Held> {
        self.voice.extend(self.start);
        self.voice.extend(self.stop);
        let Some(first) = self.voice.iter().min_by_key(|m| m.at) else {
            return Vec::new();
        };
        let talk_reference = 0x10000 | u32::from(first.talkspurt);
        let sequence_reference = 0x10000 | u32::from(first.sequence);
        self.voice.retain(|m| {
            extend_sequence(m.talkspurt, talk_reference).is_some()
                && extend_sequence(m.sequence, sequence_reference).is_some()
        });
        self.voice.sort_by_key(|m| {
            (
                extend_sequence(m.talkspurt, talk_reference).unwrap(),
                extend_sequence(m.sequence, sequence_reference).unwrap(),
                match m.kind {
                    Kind::Start => 0,
                    Kind::Voice => 1,
                    _ => 2,
                },
            )
        });
        self.voice
    }
}

/// SLICE of decoded audio and where it came from: the packet's seq and the sample
/// offset of its first sample in that packet
pub struct Slice {
    pub seq: u16,
    pub offset: usize,
    pcm: [f32; SLICE_SAMPLES],
    impaired: usize,
    talkspurt: u16,
}

struct Speaker {
    /// None: the pcm stream closed, but a selected stream keeps its order and decoder
    stream: Option<Stream<Slice>>,
    /// this speaker's compressed packets in order, before its one decoder
    order: Reorder,
    decoder: Decoder,
    /// decoded samples not yet a whole slice, each with its packet seq and offset
    pending: VecDeque<(u16, usize, f32, bool, u16)>,
    /// recent rms level of this speaker's decoded packets
    level: f32,
    /// length of the last decoded packet: how long a missing one is concealed
    packet_samples: usize,
    continuity: Continuity,
    talkspurt: u16,
    active: bool,
    heard: bool,
    late_samples: usize,
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
    fn new(stream: StreamId, wait: Option<Duration>, now: Instant) -> Self {
        let mut order = Reorder::new(stream, reorder::GAME_ONLY);
        if let Some(wait) = wait {
            order.set_wait(wait);
        }
        Speaker {
            stream: Some(Stream::new(now)),
            order,
            decoder: Decoder::new(Channels::Mono, SampleRate::Hz48000).expect("opus decoder"),
            pending: VecDeque::new(),
            level: 0.0,
            // 20 ms, the default packet, until one is decoded
            packet_samples: OPUS_RATE as usize / 50,
            continuity: Continuity::default(),
            talkspurt: 0,
            active: true,
            heard: false,
            late_samples: 0,
        }
    }

    /// decodes what the reorder stage releases at `now`
    fn drain(&mut self, now: Instant, stats: &mut MixStats) {
        loop {
            let dropped = match self.order.poll(now) {
                Ordered::Frame(frame) => {
                    let (ok, dropped) =
                        self.decode(frame.id.sequence as u16, frame.talkspurt, &frame.opus, now);
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
            self.late_samples += dropped * SLICE_SAMPLES;
        }
    }

    /// decodes one packet into slices; false when it was not opus
    fn decode(&mut self, seq: u16, talkspurt: u16, packet: &[u8], now: Instant) -> (bool, usize) {
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
        (ok, self.queue(seq, talkspurt, &decoded[..n], !ok, now))
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
        self.queue(seq, self.talkspurt, &pcm[..n], true, now)
    }

    /// queues decoded samples of packet `seq` as slices; returns how many were dropped
    fn queue(
        &mut self,
        seq: u16,
        talkspurt: u16,
        pcm: &[f32],
        impaired: bool,
        now: Instant,
    ) -> usize {
        let n = pcm.len();
        // per 20 ms of audio, whatever the packet duration
        let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
        let decay = LEVEL_DECAY.powf(n as f32 / 960.0);
        self.level = rms.max(self.level * decay);
        self.pending.extend(
            pcm.iter()
                .enumerate()
                .map(|(i, &s)| (seq, i, s, impaired, talkspurt)),
        );
        let mut dropped = 0;
        while self.pending.len() >= SLICE_SAMPLES {
            let (seq, offset, _, _, talkspurt) = self.pending[0];
            let mut slice = Slice {
                seq,
                offset,
                pcm: [0.0; SLICE_SAMPLES],
                impaired: 0,
                talkspurt,
            };
            for (out, (_, _, s, impaired, _)) in slice
                .pcm
                .iter_mut()
                .zip(self.pending.drain(..SLICE_SAMPLES))
            {
                *out = s;
                slice.impaired += usize::from(impaired);
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
    early: BTreeMap<u16, Early>,
    waits: BTreeMap<u16, Duration>,
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
    /// changes the gap budget without replacing the decoder or queued identities.
    pub fn reorder_wait(&mut self, player: u16, wait: Duration) {
        self.waits.insert(player, wait);
        if let Some(speaker) = self.speakers.get_mut(&player) {
            speaker.order.set_wait(wait);
        }
    }

    /// legacy START. it carries no generation, so the sequence begins anew with the next
    /// VOICE: a restarted legacy companion counts from 1 again
    pub fn start(&mut self, sender: u16, now: Instant) {
        if self.selected.contains_key(&sender) {
            return;
        }
        self.start_stream(legacy(sender), now);
        let speaker = self.speakers.get_mut(&sender).expect("opened above");
        speaker.continuity = Continuity::default();
        speaker.heard = false;
        speaker.active = true;
        speaker.late_samples = 0;
        speaker.order = Reorder::new(legacy(sender), reorder::GAME_ONLY);
        if let Some(wait) = self.waits.get(&sender) {
            speaker.order.set_wait(*wait);
        }
    }

    /// legacy VOICE; returns whether it was queued for decoding
    pub fn voice(&mut self, sender: u16, seq: u16, packet: &[u8], now: Instant) -> bool {
        if self.selected.contains_key(&sender) {
            // the v2 copy of this frame is the one that plays
            self.stats.discarded += 1;
            return false;
        }
        self.push(legacy(sender), 0, seq, packet, now) == MediaOutcome::Accepted
    }

    /// legacy STOP
    pub fn stop(&mut self, sender: u16) {
        if !self.selected.contains_key(&sender) {
            self.stop_stream(sender);
        }
    }

    /// trusted signaling (ANNOUNCE) selected `stream` for its player: from now on only v2
    /// media of that generation plays, and the player's legacy copies are ignored. a new
    /// generation is a restarted companion: the former stream, its decoder and queues go.
    /// returns the voice identities released from the bounded game hold, for logical rx traces.
    pub fn select(&mut self, stream: StreamId, now: Instant) -> Vec<(u16, u16)> {
        self.expire_early(now);
        let player = stream.player;
        if self
            .selected
            .get(&player)
            .is_some_and(|s| s.generation == stream.generation)
        {
            return Vec::new();
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
        let mut received = Vec::new();
        if let Some(early) = self
            .early
            .remove(&player)
            .filter(|e| e.generation == stream.generation)
        {
            for held in early.ordered() {
                let media = GameMedia {
                    stream,
                    kind: held.kind,
                    sequence: held.sequence,
                    talkspurt: held.talkspurt,
                    opus: &held.opus,
                };
                if self.authenticated(media, now) && held.kind == Kind::Voice {
                    received.push((player, held.sequence));
                }
            }
        }
        received
    }

    fn expire_early(&mut self, now: Instant) {
        self.early.retain(|_, early| {
            early.expire(now);
            early.len() != 0
        });
    }

    /// trusted v2 game media: holds unselected generations without decoding and returns
    /// true only when admitted to the selected stream. talkspurts: a repeated START of
    /// the open one changes nothing, STOP closes
    /// only the open one it names and any other STOP is discarded, anything of an older
    /// or already closed one is discarded, and START or VOICE of a newer one opens it
    /// even when its START was lost
    pub fn game(&mut self, media: GameMedia<'_>, now: Instant) -> bool {
        self.receive_game(media, now) == MediaOutcome::Accepted
    }

    pub fn receive_game(&mut self, media: GameMedia<'_>, now: Instant) -> MediaOutcome {
        self.expire_early(now);
        if self
            .selected
            .get(&media.stream.player)
            .is_none_or(|s| s.generation != media.stream.generation)
        {
            if self.early.len() >= EARLY_PLAYERS && !self.early.contains_key(&media.stream.player) {
                self.stats.discarded += 1;
                return MediaOutcome::Dropped("hold-capacity");
            }
            let early = self
                .early
                .entry(media.stream.player)
                .or_insert_with(|| Early {
                    generation: media.stream.generation,
                    voice: Vec::new(),
                    start: None,
                    stop: None,
                });
            let outcome = if early.generation != media.stream.generation {
                MediaOutcome::Dropped("pending-generation")
            } else if media.kind == Kind::Voice
                && early
                    .voice
                    .iter()
                    .any(|m| m.sequence == media.sequence && m.talkspurt == media.talkspurt)
            {
                MediaOutcome::Duplicate
            } else if early.push(media, now) {
                MediaOutcome::Held
            } else {
                MediaOutcome::Dropped("hold-control")
            };
            if outcome != MediaOutcome::Held {
                self.stats.discarded += 1;
            }
            return outcome;
        }
        self.receive_authenticated(media, now)
    }

    /// authenticated UDP never establishes a stream or enters the game hold.
    pub fn authenticated(&mut self, media: GameMedia<'_>, now: Instant) -> bool {
        self.receive_authenticated(media, now) == MediaOutcome::Accepted
    }

    pub fn receive_authenticated(&mut self, media: GameMedia<'_>, now: Instant) -> MediaOutcome {
        let player = media.stream.player;
        let Some(selection) = self.selected.get_mut(&player) else {
            self.stats.discarded += 1;
            return MediaOutcome::Dropped("unselected");
        };
        let selected = StreamId {
            player,
            generation: selection.generation,
        };
        if selected_stream(selected, player, media.stream.generation).is_none() {
            self.stats.discarded += 1;
            return MediaOutcome::Dropped("generation");
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
                return MediaOutcome::Dropped("talkspurt-stop");
            }
            selection.talkspurt = Some((media.talkspurt, false));
            self.stop_stream(player);
            return MediaOutcome::Accepted;
        }
        let opens = match order {
            None | Some((SerialOrder::Newer, _)) => true,
            Some((SerialOrder::Equal, true)) => false,
            _ => {
                self.stats.discarded += 1;
                return MediaOutcome::Dropped("talkspurt");
            }
        };
        selection.talkspurt = Some((media.talkspurt, true));
        let history = if opens {
            self.speakers
                .get_mut(&player)
                .map(|speaker| std::mem::take(&mut speaker.continuity))
        } else {
            None
        };
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
        if opens {
            let speaker = self.speakers.get_mut(&player).expect("opened above");
            speaker.continuity = history.unwrap_or_default();
            speaker.talkspurt = media.talkspurt;
            speaker.active = true;
            speaker.heard = false;
            speaker.late_samples = 0;
        }
        if media.kind == Kind::Voice {
            return self.push(selected, media.talkspurt, media.sequence, media.opus, now);
        }
        MediaOutcome::Accepted
    }

    /// START on a stream that still plays keeps its decoder and queues (Stream::restart)
    fn start_stream(&mut self, stream: StreamId, now: Instant) {
        match self.speakers.get_mut(&stream.player) {
            Some(speaker) => match &mut speaker.stream {
                Some(pcm) => pcm.restart(now),
                None => speaker.stream = Some(Stream::new(now)),
            },
            None => {
                self.speakers.insert(
                    stream.player,
                    Speaker::new(stream, self.waits.get(&stream.player).copied(), now),
                );
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
    ) -> MediaOutcome {
        // without a speaker the START was missed (companion started mid-transmission)
        let speaker = self
            .speakers
            .entry(stream.player)
            .or_insert_with(|| Speaker::new(stream, self.waits.get(&stream.player).copied(), now));
        let outcome = speaker
            .order
            .media_id(seq)
            .map_or(MediaOutcome::Dropped("sequence"), |id| {
                let frame = CompressedFrame {
                    id,
                    talkspurt,
                    opus: opus.to_vec(),
                };
                match speaker.order.push(frame, now) {
                    InsertResult::Queued => MediaOutcome::Accepted,
                    InsertResult::Duplicate => MediaOutcome::Duplicate,
                    InsertResult::Late => MediaOutcome::Dropped("late"),
                    InsertResult::WrongStream => MediaOutcome::Dropped("stream"),
                    InsertResult::Ambiguous => MediaOutcome::Dropped("sequence"),
                }
            });
        if outcome != MediaOutcome::Accepted {
            self.stats.discarded += 1;
            return outcome;
        }
        speaker.drain(now, &mut self.stats);
        MediaOutcome::Accepted
    }

    /// STOP: what waits behind a gap is released now, then the queue plays out and closes
    fn stop_stream(&mut self, player: u16) {
        if let Some(speaker) = self.speakers.get_mut(&player) {
            speaker.active = false;
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

    /// only v2 talkers whose current talkspurt actually reached this listener's playout.
    pub fn hud_status(&self, now: Instant) -> Vec<Status> {
        self.speakers
            .iter()
            .filter_map(|(&sender, speaker)| {
                let selection = self.selected.get(&sender)?;
                (speaker.active && speaker.heard && speaker.continuity.has_recent_playout(now))
                    .then(|| Status {
                        sender,
                        generation: selection.generation,
                        talkspurt: speaker.talkspurt,
                        level: speaker.continuity.level(now),
                        route: 0,
                        delay_class: 0,
                    })
            })
            .collect()
    }

    /// one SLICE of the mix at 48 kHz, or None when nobody is audible
    pub fn tick(&mut self, now: Instant) -> Option<Vec<f32>> {
        self.expire_early(now);
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
                    if speaker.heard {
                        let late = std::mem::take(&mut speaker.late_samples);
                        speaker
                            .continuity
                            .record(now, SLICE_SAMPLES + late, SLICE_SAMPLES + late);
                    }
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
                        if slice.talkspurt == speaker.talkspurt {
                            speaker.late_samples += SLICE_SAMPLES;
                        }
                        shed = Some((slice.seq, slice.offset));
                        slice = next;
                    }
                    if slice.talkspurt == speaker.talkspurt {
                        speaker.heard = true;
                        let late = std::mem::take(&mut speaker.late_samples);
                        speaker
                            .continuity
                            .record(now, SLICE_SAMPLES + late, slice.impaired + late);
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

    #[test]
    fn hud_one_second_phrase_shows_measured_level_before_it_ends() {
        let start = Instant::now();
        let opus = opus_frames(10, 1);
        let mut mixer = Mixer::default();
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            start,
        );
        for n in 0..100u16 {
            let now = start + SLICE * u32::from(n);
            assert!(mixer.game(v2(Kind::Voice, GEN_A, 7, n, &opus[0]), now));
            mixer.tick(now);
            if n == 42 {
                assert_eq!(mixer.hud_status(now)[0].level, 0);
            }
            if n == 60 {
                assert_eq!(mixer.hud_status(now)[0].level, 3);
            }
        }
    }

    #[test]
    fn hud_history_survives_five_second_gap_but_resets_at_thirty_or_new_generation() {
        let start = Instant::now();
        let opus = opus_frames(10, 1);
        for (idle_secs, generation, expected) in [(5, GEN_A, 3), (30, GEN_A, 0), (5, GEN_B, 0)] {
            let mut mixer = Mixer::default();
            mixer.select(
                StreamId {
                    player: 1,
                    generation: GEN_A,
                },
                start,
            );
            for n in 0..55u16 {
                let now = start + SLICE * u32::from(n);
                assert!(mixer.game(v2(Kind::Voice, GEN_A, 7, n, &opus[0]), now));
                mixer.tick(now);
            }
            assert_eq!(mixer.hud_status(start + SLICE * 54)[0].level, 3);
            assert!(mixer.game(v2(Kind::Stop, GEN_A, 7, 54, &[]), start + SLICE * 55));
            for n in 55..=65 {
                mixer.tick(start + SLICE * n);
            }
            assert!(mixer.hud_status(start + SLICE * 65).is_empty());
            let resumed = start + SLICE * 65 + Duration::from_secs(idle_secs);
            if generation != GEN_A {
                mixer.select(
                    StreamId {
                        player: 1,
                        generation,
                    },
                    resumed,
                );
            }
            assert!(mixer.game(v2(Kind::Voice, generation, 8, 55, &opus[0]), resumed));
            assert!(mixer.hud_status(resumed).is_empty());
            let played = resumed + crate::jitter::START_WAIT;
            mixer.tick(played);
            let records = mixer.hud_status(played);
            assert_eq!(
                (
                    records[0].generation,
                    records[0].talkspurt,
                    records[0].level
                ),
                (generation, 8, expected)
            );
            let packet = crate::quality::Reporter::default()
                .batch(&records, played)
                .unwrap();
            assert_eq!(packet[19], expected, "first report after {idle_secs}s");
        }
    }

    #[test]
    fn hud_continuity_measures_playout_loss_and_deduplicates_late_copies() {
        let start = Instant::now();
        for frame_ms in [10, 20] {
            let opus = opus_frames(frame_ms, 1);
            let frame_slices = frame_ms as u16 / 10;
            let loss_start = 250 / frame_slices - 100;
            for (losses, expected) in [(0, 3), (4, 2), (20, 1)] {
                let mut mixer = Mixer::default();
                mixer.select(
                    StreamId {
                        player: 1,
                        generation: GEN_A,
                    },
                    start,
                );
                assert!(mixer.hud_status(start).is_empty());
                for n in 0..=250u16 {
                    let now = start + SLICE * u32::from(n);
                    // losses occur in the final two seconds, separated to avoid a long gap.
                    let seq = n / frame_slices;
                    let lost = seq >= loss_start
                        && (seq - loss_start) % 5 == 0
                        && (seq - loss_start) / 5 < losses;
                    if n % frame_slices == 0 && !lost {
                        let voice = v2(Kind::Voice, GEN_A, 7, seq, &opus[0]);
                        assert!(mixer.game(voice, now));
                        assert_eq!(mixer.receive_game(voice, now), MediaOutcome::Duplicate);
                    }
                    mixer.tick(now);
                    if n == 20 {
                        assert_eq!(mixer.hud_status(now)[0].level, 0);
                    }
                    let prior = seq.saturating_sub(5);
                    if n % frame_slices == 0
                        && seq >= loss_start + 5
                        && (prior - loss_start) % 5 == 0
                        && (prior - loss_start) / 5 < losses
                    {
                        // inject each missing identity only after its gap deadline and playout.
                        // it must not create another impaired sample or change continuity.
                        let before = mixer.speakers[&1].continuity.level(now);
                        let decoded = mixer.stats.decoded;
                        let late = v2(Kind::Voice, GEN_A, 7, prior, &opus[0]);
                        assert_eq!(mixer.receive_game(late, now), MediaOutcome::Dropped("late"));
                        assert_eq!(mixer.receive_game(late, now), MediaOutcome::Dropped("late"));
                        assert_eq!(mixer.stats.decoded, decoded);
                        assert_eq!(mixer.speakers[&1].continuity.level(now), before);
                    }
                }
                let now = start + SLICE * 250;
                let status = mixer.hud_status(now);
                assert_eq!(status.len(), 1);
                assert_eq!((status[0].generation, status[0].talkspurt), (GEN_A, 7));
                assert_eq!(status[0].level, expected, "{losses} induced losses");
            }
        }
    }

    #[test]
    fn hud_ignores_old_queued_spurt_and_reordered_frames_keep_good_continuity() {
        let start = Instant::now();
        let opus = opus_frames(10, 1);
        let mut mixer = Mixer::default();
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            start,
        );
        for n in 0..=252u16 {
            let now = start + SLICE * u32::from(n);
            if n % 2 == 0 {
                assert!(mixer.game(v2(Kind::Voice, GEN_A, 7, n + 1, &opus[0]), now));
                // the first packet establishes the reorder reference, so start it in order.
                if n == 0 {
                    continue;
                }
                assert!(mixer.game(v2(Kind::Voice, GEN_A, 7, n, &opus[0]), now));
            }
            mixer.tick(now);
        }
        let now = start + SLICE * 252;
        assert_eq!(mixer.hud_status(now)[0].level, 3);
        assert!(mixer.game(v2(Kind::Voice, GEN_A, 8, 254, &opus[0]), now));
        assert!(mixer.hud_status(now).is_empty());
        let mut reported = false;
        for n in 1..=10 {
            mixer.tick(now + SLICE * n);
            if let Some(status) = mixer.hud_status(now + SLICE * n).first() {
                assert_eq!(status.talkspurt, 8);
                if !reported {
                    assert_eq!(status.level, 3);
                }
                reported = true;
            }
        }
        assert_eq!(mixer.hud_status(now + SLICE * 10)[0].talkspurt, 8);
    }

    #[test]
    fn hud_stale_stop_spurt_and_held_media() {
        let start = Instant::now();
        let opus = opus_frames(10, 1);
        let mut mixer = Mixer::default();
        for n in 0..=250u16 {
            let now = start + SLICE * u32::from(n);
            let voice = v2(Kind::Voice, GEN_A, 7, n, &opus[0]);
            if n == 0 {
                assert_eq!(mixer.receive_game(voice, now), MediaOutcome::Held);
                assert!(mixer.hud_status(now).is_empty());
                mixer.select(voice.stream, now);
            } else {
                assert!(mixer.game(voice, now));
            }
            mixer.tick(now);
        }
        let now = start + SLICE * 250;
        assert_eq!(mixer.hud_status(now)[0].level, 3);
        assert!(mixer.game(v2(Kind::Start, GEN_A, 7, 250, &[]), now));
        assert_eq!(mixer.hud_status(now)[0].level, 3);
        for n in 1..=70 {
            mixer.tick(now + SLICE * n);
        }
        assert!(mixer.is_idle());
        assert_eq!(mixer.hud_status(now + SLICE * 70)[0].level, 0);
        let later = now + Duration::from_secs(1);
        assert!(mixer.game(v2(Kind::Voice, GEN_A, 8, 251, &opus[0]), later));
        assert!(mixer.hud_status(later).is_empty());
        mixer.tick(later + crate::jitter::START_WAIT);
        let status = mixer.hud_status(later + crate::jitter::START_WAIT);
        assert_eq!((status[0].talkspurt, status[0].level), (8, 1));
        assert!(!mixer.game(v2(Kind::Stop, GEN_A, 7, 251, &[]), later));
        assert_eq!(mixer.hud_status(later).len(), 1);
        assert!(mixer.game(v2(Kind::Stop, GEN_A, 8, 251, &[]), later));
        assert!(mixer.hud_status(later).is_empty());
        assert_eq!(
            mixer.receive_game(v2(Kind::Voice, GEN_A, 8, 252, &opus[0]), later),
            MediaOutcome::Dropped("talkspurt")
        );
        let held = v2(Kind::Voice, GEN_B, 1, 1, &opus[0]);
        assert_eq!(mixer.receive_game(held, later), MediaOutcome::Held);
        assert_eq!(
            mixer.receive_game(v2(Kind::Stop, GEN_B, 1, 1, &[]), later),
            MediaOutcome::Held
        );
        mixer.select(held.stream, later);
        mixer.tick(later);
        assert!(mixer.hud_status(later).is_empty());
    }

    #[test]
    fn hud_idle_report_expires_and_same_or_new_spurt_resumes_retained_history() {
        use crate::quality::Reporter;

        let start = Instant::now();
        let opus = opus_frames(10, 1);
        for next_spurt in [7, 8] {
            let mut mixer = Mixer::default();
            let mut reporter = Reporter::default();
            mixer.select(
                StreamId {
                    player: 1,
                    generation: GEN_A,
                },
                start,
            );
            for n in 0..=250u16 {
                let now = start + SLICE * u32::from(n);
                assert!(mixer.game(v2(Kind::Voice, GEN_A, 7, n, &opus[0]), now));
                mixer.tick(now);
            }
            let now = start + SLICE * 250;
            assert!(reporter.batch(&mixer.hud_status(now), now).is_some());
            for n in 1..=70 {
                mixer.tick(now + SLICE * n);
            }
            let stale = now + SLICE * 70;
            let records = mixer.hud_status(stale);
            assert_eq!(records[0].level, 0);
            let unknown = reporter
                .batch(&records, stale)
                .expect("bounded stale clear");
            assert_eq!(unknown[19], 0);
            // the last observation is at 300 ms: expiry is exactly two seconds later.
            let expires = now + Duration::from_millis(2300);
            assert_eq!(
                mixer.hud_status(expires - Duration::from_millis(1)).len(),
                1
            );
            for n in 0..=4 {
                let at = expires + Duration::from_secs(n);
                assert!(mixer.hud_status(at).is_empty());
                assert!(reporter.batch(&mixer.hud_status(at), at).is_none());
            }
            let resumes = expires + Duration::from_secs(5);
            assert!(mixer.game(v2(Kind::Voice, GEN_A, next_spurt, 251, &opus[0]), resumes));
            mixer.tick(resumes + crate::jitter::START_WAIT);
            let active_at = resumes + crate::jitter::START_WAIT;
            let records = mixer.hud_status(active_at);
            assert_eq!((records[0].talkspurt, records[0].level), (next_spurt, 1));
            assert!(reporter.batch(&records, active_at).is_some());
        }
    }

    #[test]
    fn wait_before_first_frame_survives_idle_talkspurt_and_generation_reset() {
        let now = Instant::now();
        let wait = Duration::from_millis(80);
        let mut mixer = Mixer::default();
        let mut stream = StreamId {
            player: 1,
            generation: 1,
        };
        mixer.reorder_wait(1, wait);
        mixer.select(stream, now);
        let voice = |stream, sequence, talkspurt| GameMedia {
            stream,
            sequence,
            talkspurt,
            kind: Kind::Voice,
            opus: &[0xf8, 0xff, 0xfe],
        };
        assert!(mixer.game(voice(stream, 1, 1), now));
        assert!(mixer.game(voice(stream, 3, 1), now));
        assert_eq!(mixer.speakers[&1].order.deadline(), Some(now + wait));
        assert!(mixer.game(
            GameMedia {
                kind: Kind::Stop,
                opus: &[],
                ..voice(stream, 3, 1)
            },
            now
        ));
        for n in 0..100 {
            mixer.tick(now + SLICE * n);
        }
        assert!(mixer.is_idle());
        let later = now + Duration::from_secs(1);
        assert!(mixer.game(voice(stream, 4, 2), later));
        assert!(mixer.game(voice(stream, 6, 2), later));
        assert_eq!(mixer.speakers[&1].order.deadline(), Some(later + wait));
        stream.generation = 2;
        mixer.select(stream, later);
        assert!(mixer.game(voice(stream, 7, 1), later));
        assert!(mixer.game(voice(stream, 9, 1), later));
        assert_eq!(mixer.speakers[&1].order.deadline(), Some(later + wait));
        mixer.reorder_wait(2, wait);
        mixer.start(2, now);
        mixer.voice(2, 1, &[0xf8, 0xff, 0xfe], now);
        mixer.voice(2, 3, &[0xf8, 0xff, 0xfe], now);
        assert_eq!(mixer.speakers[&2].order.deadline(), Some(now + wait));
    }

    #[test]
    fn receive_outcomes_distinguish_storage_duplicate_and_admission_drop() {
        let now = Instant::now();
        let mut mixer = Mixer::default();
        let media = GameMedia {
            stream: StreamId {
                player: 1,
                generation: 1,
            },
            kind: Kind::Voice,
            sequence: 1,
            talkspurt: 1,
            opus: &[0xf8, 0xff, 0xfe],
        };
        assert_eq!(
            mixer.receive_authenticated(media, now),
            MediaOutcome::Dropped("unselected")
        );
        assert_eq!(mixer.receive_game(media, now), MediaOutcome::Held);
        assert_eq!(mixer.receive_game(media, now), MediaOutcome::Duplicate);
        let other = GameMedia {
            stream: StreamId {
                generation: 2,
                ..media.stream
            },
            ..media
        };
        assert_eq!(
            mixer.receive_game(other, now),
            MediaOutcome::Dropped("pending-generation")
        );
        assert_eq!(mixer.select(media.stream, now), vec![(1, 1)]);
        assert_eq!(mixer.receive_game(media, now), MediaOutcome::Duplicate);
        assert_eq!(
            mixer.receive_authenticated(other, now),
            MediaOutcome::Dropped("generation")
        );
    }

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
    fn early_game_media_releases_in_sequence_once_even_when_stop_arrives_first() {
        let now = Instant::now();
        let mut mixer = Mixer::default();
        let opus = opus_frames(20, 3);
        assert!(!mixer.game(v2(Kind::Stop, GEN_A, 1, 3, &[]), now));
        for sequence in [3, 1, 2, 2] {
            assert!(!mixer.game(
                v2(
                    Kind::Voice,
                    GEN_A,
                    1,
                    sequence,
                    &opus[sequence as usize - 1]
                ),
                now
            ));
        }
        mixer.game(v2(Kind::Start, GEN_A, 1, 0, &[]), now);
        assert_eq!(mixer.stats.decoded, 0);
        assert_eq!(
            mixer.select(
                StreamId {
                    player: 1,
                    generation: GEN_A
                },
                now + Duration::from_millis(100)
            ),
            vec![(1, 1), (1, 2), (1, 3)]
        );
        assert_eq!(mixer.stats.decoded, 3);
        assert!(
            mixer
                .select(
                    StreamId {
                        player: 1,
                        generation: GEN_A
                    },
                    now + Duration::from_millis(101)
                )
                .is_empty()
        );
        assert!(!mixer.game(
            v2(Kind::Voice, GEN_A, 1, 2, &opus[1]),
            now + Duration::from_millis(101)
        ));
        let mut played = Vec::new();
        for n in 0..10 {
            mixer.tick(now + Duration::from_millis(100) + SLICE * n);
            played.extend(mixer.pulled.iter().filter_map(|p| p.slice));
        }
        assert_eq!(played.len(), 6);
        assert!(mixer.is_idle());
    }

    #[test]
    fn early_sort_crosses_sequence_and_talkspurt_wrap() {
        let now = Instant::now();
        let mut mixer = Mixer::default();
        let opus = opus_frames(20, 4);
        for (kind, talk, sequence, body) in [
            (Kind::Stop, 0, 1, &[][..]),
            (Kind::Voice, 0, 1, &opus[3][..]),
            (Kind::Voice, 65535, 65535, &opus[1][..]),
            (Kind::Voice, 0, 0, &opus[2][..]),
            (Kind::Start, 65535, 65533, &[][..]),
            (Kind::Voice, 65535, 65534, &opus[0][..]),
        ] {
            mixer.game(v2(kind, GEN_A, talk, sequence, body), now);
        }
        assert_eq!(
            mixer.select(
                StreamId {
                    player: 1,
                    generation: GEN_A
                },
                now
            ),
            vec![(1, 65534), (1, 65535), (1, 0), (1, 1)]
        );
        assert_eq!(mixer.stats.decoded, 4);
        assert!(!mixer.game(v2(Kind::Voice, GEN_A, 65535, 65535, &opus[1]), now));
    }

    #[test]
    fn early_hold_expires_and_another_generation_can_then_claim_it() {
        let now = Instant::now();
        let mut mixer = Mixer::default();
        let opus = opus_frames(20, 1);
        mixer.game(v2(Kind::Voice, GEN_A, 1, 1, &opus[0]), now);
        mixer.game(
            v2(Kind::Voice, GEN_B, 1, 2, &opus[0]),
            now + Duration::from_millis(100),
        );
        assert_eq!(mixer.early[&1].generation, GEN_A);
        assert!(
            mixer
                .select(
                    StreamId {
                        player: 1,
                        generation: GEN_A
                    },
                    now + EARLY_TTL
                )
                .is_empty()
        );
        mixer.game(v2(Kind::Voice, GEN_B, 1, 3, &opus[0]), now + EARLY_TTL);
        assert_eq!(mixer.early[&1].generation, GEN_B);
        mixer.game(v2(Kind::Voice, GEN_A, 1, 4, &opus[0]), now + EARLY_TTL);
        assert_eq!(mixer.early[&1].generation, GEN_B);
        assert_eq!(
            mixer.select(
                StreamId {
                    player: 1,
                    generation: GEN_B
                },
                now + EARLY_TTL
            ),
            vec![(1, 3)]
        );
        assert_eq!(mixer.stats.decoded, 2);
        assert_eq!(queued(&mixer), 2);
    }

    #[test]
    fn early_hold_caps_records_players_and_start_refreshes_do_not_displace_voice() {
        let now = Instant::now();
        let mut mixer = Mixer::default();
        let opus = opus_frames(20, 1);
        for sequence in 1..=25 {
            mixer.game(v2(Kind::Voice, GEN_A, 1, sequence, &opus[0]), now);
        }
        for _ in 0..100 {
            mixer.game(v2(Kind::Start, GEN_A, 1, 0, &[]), now);
        }
        assert_eq!(mixer.early[&1].len(), EARLY_LIMIT);
        assert_eq!(mixer.early[&1].voice.first().unwrap().sequence, 6);
        for player in 2..=257 {
            let mut media = v2(Kind::Voice, GEN_A, 1, 1, &opus[0]);
            media.stream.player = player;
            mixer.game(media, now);
        }
        assert_eq!(mixer.early.len(), EARLY_PLAYERS);
        assert_eq!(
            mixer
                .select(
                    StreamId {
                        player: 1,
                        generation: GEN_A
                    },
                    now
                )
                .len(),
            EARLY_LIMIT
        );
        mixer.tick(now + EARLY_TTL);
        assert!(mixer.early.is_empty());
    }

    #[test]
    fn selecting_another_generation_discards_hold_and_udp_never_enters_it() {
        let now = Instant::now();
        let mut mixer = Mixer::default();
        let opus = opus_frames(20, 1);
        assert!(!mixer.authenticated(v2(Kind::Voice, GEN_A, 1, 1, &opus[0]), now));
        assert!(mixer.early.is_empty());
        mixer.game(v2(Kind::Voice, GEN_A, 1, 1, &opus[0]), now);
        assert!(
            mixer
                .select(
                    StreamId {
                        player: 1,
                        generation: GEN_B
                    },
                    now
                )
                .is_empty()
        );
        assert!(mixer.early.is_empty());
        assert_eq!(mixer.stats.decoded, 0);
        assert!(mixer.game(v2(Kind::Voice, GEN_B, 1, 2, &opus[0]), now));
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            now,
        );
        assert!(!mixer.speakers.contains_key(&1));
    }

    #[test]
    fn legacy_and_announced_senders_keep_separate_streams() {
        let t0 = Instant::now();
        let opus = opus_frames(20, 3);
        let mut mixer = Mixer::default();
        assert!(!mixer.game(v2(Kind::Voice, GEN_A, 1, 1, &opus[0]), t0));
        assert!(mixer.voice(2, 1, &opus[0], t0));
        mixer.select(
            StreamId {
                player: 1,
                generation: GEN_A,
            },
            t0,
        );
        // a selected sender cannot inject legacy media into its announced stream
        assert!(!mixer.voice(1, 2, &opus[1], t0));
        assert!(mixer.game(v2(Kind::Voice, GEN_A, 1, 2, &opus[1]), t0));
        assert_eq!((mixer.stats.decoded, mixer.stats.discarded), (3, 1));
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
