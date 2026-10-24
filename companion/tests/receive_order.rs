//! receive rules of the mixer and its reorder stage, through the public api only

use std::time::{Duration, Instant};

use ptt_radio_companion::jitter::SLICE;
use ptt_radio_companion::mix::Mixer;
use ptt_radio_companion::reorder::{GAME_ONLY, MAX_CONCEAL, Reorder};
use ptt_radio_companion::transport::wire::{GameMedia, Kind};
use ptt_radio_companion::transport::{
    CompressedFrame, InsertResult, Ordered, PreDecodeOrder, StreamId,
};

const STREAM: StreamId = StreamId {
    player: 1,
    generation: 42,
};
/// a valid 20 ms opus packet of silence
const SILENCE: &[u8] = &[0xf8, 0xff, 0xfe];
/// mix::TALKSPURT_FORGET
const FORGET: Duration = Duration::from_secs(60);

fn media(kind: Kind, talkspurt: u16, sequence: u16) -> GameMedia<'static> {
    GameMedia {
        kind,
        stream: STREAM,
        talkspurt,
        sequence,
        opus: if kind == Kind::Voice { SILENCE } else { &[] },
    }
}

fn selected(now: Instant) -> Mixer {
    let mut mixer = Mixer::default();
    mixer.select(STREAM, now);
    mixer
}

#[test]
fn a_newer_stop_leaves_the_open_talkspurt_playing() {
    let t0 = Instant::now();
    let mut mixer = selected(t0);
    assert!(mixer.game(media(Kind::Start, 5, 0), t0));
    assert!(mixer.game(media(Kind::Voice, 5, 1), t0));
    assert!(!mixer.game(media(Kind::Stop, 6, 2), t0));
    assert!(mixer.game(media(Kind::Voice, 5, 2), t0));
    for n in 0..8 {
        mixer.tick(t0 + SLICE * n);
    }
    assert!(!mixer.is_idle(), "closed by another talkspurt's STOP");
    assert!(mixer.game(media(Kind::Stop, 5, 2), t0 + SLICE * 8));
    // the unmatched STOP was not remembered: talkspurt 6 still opens
    assert!(mixer.game(media(Kind::Voice, 6, 3), t0 + SLICE * 8));
    assert_eq!(mixer.stats.decoded, 3);
}

#[test]
fn a_copy_played_before_the_pcm_stream_closed_stays_a_duplicate() {
    let t0 = Instant::now();
    let mut mixer = selected(t0);
    assert!(mixer.game(media(Kind::Start, 5, 0), t0));
    assert!(mixer.game(media(Kind::Voice, 5, 1), t0));
    // STOP lost: the pcm stream closes when idle, the talkspurt stays open
    for n in 0..=40 {
        mixer.tick(t0 + SLICE * n);
    }
    assert!(mixer.is_idle());
    let later = t0 + SLICE * 41;
    assert!(!mixer.game(media(Kind::Voice, 5, 1), later));
    assert_eq!(mixer.stats.decoded, 1);
    // new media of the stream is still taken, and decoded at once
    assert!(mixer.game(media(Kind::Voice, 5, 2), later));
    assert_eq!((mixer.stats.decoded, mixer.stats.decode_errors), (2, 0));
    assert!(!mixer.is_idle());
    // after the next idle close a new talkspurt starts afresh: a sequence jump since
    // the last press waits for nothing and conceals nothing
    for n in 41..=82 {
        mixer.tick(t0 + SLICE * n);
    }
    assert!(mixer.is_idle());
    assert!(mixer.game(media(Kind::Voice, 6, 40), t0 + SLICE * 83));
    assert_eq!((mixer.stats.decoded, mixer.stats.missing), (3, 0));
}

#[test]
fn a_talkspurt_is_forgotten_after_a_minute_without_media() {
    let t0 = Instant::now();
    let mut mixer = selected(t0);
    assert!(mixer.game(media(Kind::Start, 5, 0), t0));
    assert!(mixer.game(media(Kind::Voice, 5, 1), t0));
    assert!(mixer.game(media(Kind::Stop, 5, 1), t0));
    for n in 0..4 {
        mixer.tick(t0 + SLICE * n);
    }
    assert!(mixer.is_idle());
    assert!(!mixer.game(media(Kind::Start, 4, 1), t0 + FORGET));
    // the talkspurt is forgotten, and the stream restarts as on a first press
    let later = t0 + FORGET * 2 + SLICE;
    assert!(mixer.game(media(Kind::Start, 4, 1), later));
    assert!(mixer.game(media(Kind::Voice, 4, 1), later));
    assert_eq!(mixer.stats.decoded, 2);
}

#[test]
fn a_sequence_jump_conceals_a_bounded_number_of_frames() {
    let t0 = Instant::now();
    let mut mixer = Mixer::default();
    mixer.start(1, t0);
    assert!(mixer.voice(1, 1, SILENCE, t0));
    assert!(mixer.voice(1, 32768, SILENCE, t0));
    mixer.tick(t0 + GAME_ONLY.wait);
    assert_eq!(mixer.stats.missing, u64::from(MAX_CONCEAL));
    assert_eq!((mixer.stats.decoded, mixer.stats.dropped_late), (2, 0));
    // skipped, concealed and played copies are all refused; the stream goes on
    for seq in [2, 32767, 32768] {
        assert!(!mixer.voice(1, seq, SILENCE, t0 + GAME_ONLY.wait));
    }
    assert!(mixer.voice(1, 32769, SILENCE, t0 + GAME_ONLY.wait));
}

fn push(order: &mut Reorder, seq: u16, at: Instant) -> InsertResult {
    let frame = CompressedFrame {
        id: order.media_id(seq).expect("in window"),
        talkspurt: 1,
        opus: SILENCE.to_vec(),
    };
    order.push(frame, at)
}

#[test]
fn a_long_gap_reports_only_its_last_frames_missing() {
    let t0 = Instant::now();
    let mut order = Reorder::new(STREAM, GAME_ONLY);
    push(&mut order, 1, t0);
    assert!(matches!(order.poll(t0), Ordered::Frame(_)));
    push(&mut order, 1001, t0);
    let at = t0 + GAME_ONLY.wait;
    let mut missing = Vec::new();
    let mut fec = None;
    let after = loop {
        match order.poll(at) {
            Ordered::Missing { id, next } => {
                missing.push(id.sequence as u16);
                fec = next.map(|f| f.id.sequence as u16);
            }
            other => break other,
        }
    };
    let last_skipped = 1000 - MAX_CONCEAL as u16;
    assert_eq!(missing, (last_skipped + 1..=1000).collect::<Vec<_>>());
    assert_eq!(fec, Some(1001), "the last missing frame still gets FEC");
    assert!(matches!(after, Ordered::Frame(f) if f.id.sequence as u16 == 1001));
    for seq in [2, last_skipped, 1000] {
        assert_eq!(push(&mut order, seq, at), InsertResult::Late);
    }
    assert_eq!(push(&mut order, 1001, at), InsertResult::Duplicate);
}
