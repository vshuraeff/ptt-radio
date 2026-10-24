//! the companion's half of the in-game echo test: on SELF_TEST from the mod it plays
//! a short tone locally, records 3 s through the normal capture gate, sends the
//! recording as one talkspurt, plays the copy the game returns (the mod grants a
//! temporary self-monitor for it) and answers SELF_TEST_RESULT.
//!
//! sans-I/O, behind one leaf lock in `Shared`: the receive thread files requests
//! and counts returned frames, the main thread starts runs and ends them on time,
//! the transmit thread records and sends frames, the playout thread renders the
//! tone. the test never presses PTT, so half-duplex never mutes the returned copy;
//! PTT or open mic during a run ends it as busy. the recording is bound to the capture
//! fence its first frame passed: a denied frame or a closed fence drops it unsent.

use std::time::{Duration, Instant};

use ptt_radio_companion::audio::OPUS_RATE;
use ptt_radio_companion::packet::{Outcome, SelfTestResult};

pub const TONE: Duration = Duration::from_millis(500);
const TONE_HZ: f32 = 440.0;
const TONE_LEVEL: f32 = 0.3;
/// fade in and out, so the tone does not click
const TONE_RAMP: usize = 240;
pub const CAPTURE: Duration = Duration::from_secs(3);
/// after the last frame sent, how long returned frames are still waited for
pub const RETURN_WAIT: Duration = Duration::from_secs(2);
/// how long the microphone may take to open, or frames may stop, before the run fails
pub const STALL: Duration = Duration::from_secs(2);

/// what the main thread sees at each poll
pub struct Status {
    /// PTT held or open mic on
    pub busy: bool,
    /// the capture gate: the current POLICY permits transmit
    pub permitted: bool,
    /// the runtime's capture reset counter
    pub reset: u64,
    /// the input device is open (always true for synthetic sources)
    pub mic_open: bool,
    /// an output exists to play the tone and the returned copy
    pub playback: bool,
}

/// `Cancelled`: the transmit thread saw the fence close; the next poll reports it
enum Phase {
    MicWait { until: Instant },
    Tone { until: Instant },
    Capture { until: Instant },
    Send { next: usize, until: Instant },
    Wait { until: Instant },
    Cancelled,
}

/// the capture fence a frame passed: any change discards what was captured before it
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fence {
    /// the runtime's capture reset counter
    pub reset: u64,
    /// the gated device's capture generation; None for synthetic sources
    pub generation: Option<u64>,
}

struct Run {
    id: u16,
    phase: Phase,
    /// the fence of the first recorded frame
    fence: Option<Fence>,
    tone_left: usize,
    tone_at: usize,
    recorded: usize,
    recording: Vec<Vec<f32>>,
    peak: Option<f32>,
    sent: u16,
    returned: u16,
    first_sent: Option<Instant>,
    first_return: Option<Duration>,
}

/// what the transmit thread does with the frame it just captured
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// the usual path
    Live,
    /// send this recorded frame instead; `last` ends the talkspurt
    Send { pcm: Vec<f32>, last: bool },
}

#[derive(Default)]
pub struct SelfTest {
    request: Option<u16>,
    run: Option<Run>,
}

impl SelfTest {
    /// a SELF_TEST from the mod; a second one while a test is pending or running is busy
    pub fn request(&mut self, id: u16) -> Option<SelfTestResult> {
        if self.request.is_some() || self.run.is_some() {
            return Some(blank(id, Outcome::Busy));
        }
        self.request = Some(id);
        None
    }

    pub fn idle(&self) -> bool {
        self.request.is_none() && self.run.is_none()
    }

    /// starts a requested run and moves the timed phases; returns the RESULT to send
    pub fn poll(&mut self, now: Instant, status: Status) -> Option<SelfTestResult> {
        if let Some(id) = self.request.take() {
            if status.busy {
                return Some(blank(id, Outcome::Busy));
            }
            if !status.permitted {
                return Some(blank(id, Outcome::TxNotPermitted));
            }
            self.run = Some(Run {
                id,
                phase: Phase::MicWait { until: now + STALL },
                fence: None,
                tone_left: 0,
                tone_at: 0,
                recorded: 0,
                recording: Vec::new(),
                peak: None,
                sent: 0,
                returned: 0,
                first_sent: None,
                first_return: None,
            });
        }
        let run = self.run.as_mut()?;
        let fence_closed = run.fence.is_some_and(|fence| fence.reset != status.reset);
        let end = if matches!(run.phase, Phase::Cancelled) {
            Some(Outcome::TxNotPermitted)
        } else if status.busy {
            Some(Outcome::Busy)
        } else if !status.permitted || fence_closed {
            Some(Outcome::TxNotPermitted)
        } else {
            match run.phase {
                Phase::MicWait { .. } if status.mic_open => {
                    if status.playback {
                        run.tone_left = (TONE.as_secs_f64() * f64::from(OPUS_RATE)) as usize;
                        run.phase = Phase::Tone { until: now + TONE };
                    } else {
                        run.phase = capture(now);
                    }
                    None
                }
                Phase::Tone { until } if now >= until => {
                    run.phase = capture(now);
                    None
                }
                Phase::MicWait { until } | Phase::Capture { until } | Phase::Send { until, .. }
                    if now >= until =>
                {
                    Some(Outcome::MicOpenFailed)
                }
                Phase::Wait { until } if now >= until => Some(if run.returned == 0 {
                    Outcome::NoFramesReturned
                } else {
                    Outcome::Ok
                }),
                _ => None,
            }
        };
        end.map(|outcome| self.finish(outcome))
    }

    /// one captured frame, after the echo canceller, with the transmit permission and the
    /// capture fence it passed. a denial or a changed fence cancels the run and drops the
    /// recording before any stored frame can be sent
    pub fn frame(&mut self, pcm: &[f32], now: Instant, permitted: bool, fence: Fence) -> Frame {
        let Some(run) = &mut self.run else {
            return Frame::Live;
        };
        if !permitted || run.fence.is_some_and(|bound| bound != fence) {
            run.recording = Vec::new();
            run.phase = Phase::Cancelled;
        }
        match &mut run.phase {
            Phase::Capture { .. } => {
                run.fence.get_or_insert(fence);
                let peak = pcm.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()));
                run.peak = Some(run.peak.unwrap_or(0.0).max(peak));
                run.recording.push(pcm.to_vec());
                run.recorded += pcm.len();
                if run.recorded as f64 >= CAPTURE.as_secs_f64() * f64::from(OPUS_RATE) {
                    run.phase = Phase::Send {
                        next: 0,
                        until: now + CAPTURE + STALL,
                    };
                }
                Frame::Live
            }
            Phase::Send { next, .. } => {
                let pcm = std::mem::take(&mut run.recording[*next]);
                *next += 1;
                let last = *next == run.recording.len();
                if last {
                    run.phase = Phase::Wait {
                        until: now + RETURN_WAIT,
                    };
                }
                Frame::Send { pcm, last }
            }
            _ => Frame::Live,
        }
    }

    /// VOICE frames the transmitter made of a `Frame::Send`
    pub fn sent(&mut self, voices: u64, now: Instant) {
        if let Some(run) = &mut self.run {
            run.sent = run
                .sent
                .saturating_add(voices.try_into().unwrap_or(u16::MAX));
            if voices > 0 {
                run.first_sent.get_or_insert(now);
            }
        }
    }

    /// an own VOICE frame came back through the game
    pub fn returned(&mut self, now: Instant) {
        if let Some(run) = &mut self.run
            && let Some(first) = run.first_sent
        {
            run.returned = run.returned.saturating_add(1);
            run.first_return.get_or_insert(now - first);
        }
    }

    /// adds the local tone to one playout slice; it is never captured on purpose or sent
    pub fn tone(&mut self, pcm: &mut [f32]) {
        let Some(run) = &mut self.run else {
            return;
        };
        let total = run.tone_at + run.tone_left;
        for sample in pcm.iter_mut().take(run.tone_left) {
            let at = run.tone_at;
            let ramp = (at.min(total - at) as f32 / TONE_RAMP as f32).min(1.0);
            let phase = std::f32::consts::TAU * TONE_HZ * at as f32 / OPUS_RATE as f32;
            *sample += TONE_LEVEL * ramp * phase.sin();
            run.tone_at += 1;
            run.tone_left -= 1;
        }
    }

    fn finish(&mut self, outcome: Outcome) -> SelfTestResult {
        let run = self.run.take().expect("a running test");
        SelfTestResult {
            test_id: run.id,
            outcome,
            tone: u8::from(run.tone_at > 0),
            mic_peak_dbfs: run.peak.map_or(-128, dbfs),
            frames_sent: run.sent,
            frames_returned: run.returned,
            first_return_ms: run
                .first_return
                .map_or(0, |d| d.as_millis().min(u128::from(u16::MAX)) as u16),
        }
    }
}

fn capture(now: Instant) -> Phase {
    Phase::Capture {
        until: now + CAPTURE + STALL,
    }
}

fn blank(test_id: u16, outcome: Outcome) -> SelfTestResult {
    SelfTestResult {
        test_id,
        outcome,
        tone: 0,
        mic_peak_dbfs: -128,
        frames_sent: 0,
        frames_returned: 0,
        first_return_ms: 0,
    }
}

/// -127..=0; digital silence is -127, since -128 means no capture
fn dbfs(peak: f32) -> i8 {
    if peak <= 0.0 {
        return -127;
    }
    (20.0 * peak.log10()).round().clamp(-127.0, 0.0) as i8
}

#[cfg(test)]
mod tests {
    use super::*;
    use ptt_radio_companion::runtime::{Output, Runtime, RuntimeConfig};
    use ptt_radio_companion::transport::signaling::{self, Audience, Policy};
    use ptt_radio_companion::transport::wire;
    use ptt_radio_companion::tx::{Codec, Transmitter};

    const FRAME: usize = 960;
    const FENCE: Fence = Fence {
        reset: 1,
        generation: Some(1),
    };

    fn status(busy: bool, permitted: bool, mic_open: bool) -> Status {
        Status {
            busy,
            permitted,
            reset: FENCE.reset,
            mic_open,
            playback: true,
        }
    }

    /// a permitted frame through the unchanged fence
    fn fed(test: &mut SelfTest, pcm: &[f32], now: Instant) -> Frame {
        test.frame(pcm, now, true, FENCE)
    }

    fn ready() -> Status {
        status(false, true, true)
    }

    #[test]
    fn busy_or_not_permitted_requests_are_answered_at_once() {
        let now = Instant::now();
        let mut test = SelfTest::default();
        assert_eq!(test.request(1), None);
        let busy = test.poll(now, status(true, true, true)).unwrap();
        assert_eq!((busy.test_id, busy.outcome), (1, Outcome::Busy));
        assert_eq!(busy.mic_peak_dbfs, -128);
        assert!(test.idle());
        assert_eq!(test.request(2), None);
        let denied = test.poll(now, status(false, false, true)).unwrap();
        assert_eq!(
            (denied.test_id, denied.outcome),
            (2, Outcome::TxNotPermitted)
        );
        assert_eq!(test.request(3), None);
        assert!(test.poll(now, ready()).is_none());
        assert_eq!(test.request(4).unwrap().outcome, Outcome::Busy);
        assert!(!test.idle());
    }

    #[test]
    fn a_microphone_that_never_opens_fails_the_run() {
        let now = Instant::now();
        let mut test = SelfTest::default();
        test.request(5);
        assert!(test.poll(now, status(false, true, false)).is_none());
        assert!(
            test.poll(now + STALL / 2, status(false, true, false))
                .is_none()
        );
        let failed = test.poll(now + STALL, status(false, true, false)).unwrap();
        assert_eq!(failed.outcome, Outcome::MicOpenFailed);
        assert_eq!((failed.tone, failed.mic_peak_dbfs), (0, -128));
    }

    #[test]
    fn a_full_run_plays_the_tone_records_three_seconds_sends_them_once_and_counts_returns() {
        let start = Instant::now();
        let mut test = SelfTest::default();
        test.request(0x0102);
        test.poll(start, ready());
        let mut slice = [0.0; 480];
        test.tone(&mut slice);
        assert!(slice.iter().any(|s| s.abs() > 0.1));
        assert!(slice.iter().all(|s| s.abs() <= TONE_LEVEL));
        assert_eq!(
            fed(&mut test, &[0.5; FRAME], start),
            Frame::Live,
            "no capture during the tone"
        );
        let mut now = start + TONE;
        test.poll(now, ready());
        for index in 0..150 {
            let level = if index == 7 { -0.5 } else { 0.1 };
            assert_eq!(fed(&mut test, &[level; FRAME], now), Frame::Live);
            now += Duration::from_millis(20);
        }
        let mut sent = Vec::new();
        loop {
            let Frame::Send { pcm, last } = fed(&mut test, &[0.9; FRAME], now) else {
                panic!("sending the recording");
            };
            sent.push(pcm);
            test.sent(1, now);
            if sent.len() == 3 {
                test.returned(now + Duration::from_millis(85));
            }
            now += Duration::from_millis(20);
            if last {
                break;
            }
        }
        assert_eq!(sent.len(), 150);
        assert_eq!(
            sent[7],
            vec![-0.5; FRAME],
            "the recording, not the live input"
        );
        assert_eq!(fed(&mut test, &[0.0; FRAME], now), Frame::Live);
        for _ in 0..148 {
            test.returned(now);
        }
        assert!(test.poll(now + RETURN_WAIT / 2, ready()).is_none());
        let result = test.poll(now + RETURN_WAIT, ready()).unwrap();
        assert_eq!(
            result,
            SelfTestResult {
                test_id: 0x0102,
                outcome: Outcome::Ok,
                tone: 1,
                mic_peak_dbfs: -6,
                frames_sent: 150,
                frames_returned: 149,
                first_return_ms: 125,
            }
        );
        assert!(test.idle());
    }

    #[test]
    fn nothing_returned_and_no_playback_are_reported() {
        let mut now = Instant::now();
        let mut test = SelfTest::default();
        test.request(9);
        let quiet = Status {
            playback: false,
            ..ready()
        };
        test.poll(now, quiet);
        let mut slice = [0.0; 480];
        test.tone(&mut slice);
        assert!(slice.iter().all(|s| *s == 0.0), "no output, no tone");
        while let Frame::Live = fed(&mut test, &[0.0; FRAME], now) {
            now += Duration::from_millis(20);
        }
        while let Frame::Send { last: false, .. } = fed(&mut test, &[0.0; FRAME], now) {
            test.sent(1, now);
        }
        let result = test.poll(now + RETURN_WAIT, ready()).unwrap();
        assert_eq!(result.outcome, Outcome::NoFramesReturned);
        assert_eq!((result.tone, result.mic_peak_dbfs), (0, -127));
        assert_eq!(result.frames_returned, 0);
    }

    #[test]
    fn ptt_or_a_revoked_permission_ends_a_running_test() {
        for (busy, permitted, outcome) in [
            (true, true, Outcome::Busy),
            (false, false, Outcome::TxNotPermitted),
        ] {
            let now = Instant::now();
            let mut test = SelfTest::default();
            test.request(1);
            test.poll(now, ready());
            test.poll(now + TONE, ready());
            fed(&mut test, &[0.25; FRAME], now + TONE);
            let result = test
                .poll(now + TONE, status(busy, permitted, true))
                .unwrap();
            assert_eq!(result.outcome, outcome);
            assert_eq!(result.mic_peak_dbfs, -12);
            assert_eq!(fed(&mut test, &[0.0; FRAME], now), Frame::Live);
        }
    }

    /// a run past its tone, recording one 0.25 frame under `FENCE`
    fn recording(now: Instant) -> SelfTest {
        let mut test = SelfTest::default();
        test.request(1);
        test.poll(now, ready());
        test.poll(now + TONE, ready());
        assert_eq!(fed(&mut test, &[0.25; FRAME], now + TONE), Frame::Live);
        test
    }

    #[test]
    fn a_closed_fence_or_a_denied_frame_during_capture_drops_the_recording_unsent() {
        let reset = Fence { reset: 3, ..FENCE };
        let reopened = Fence {
            generation: Some(3),
            ..FENCE
        };
        for (permitted, closed, polled) in [
            (true, reset, reset.reset),
            (true, reopened, FENCE.reset),
            (false, FENCE, FENCE.reset),
        ] {
            let now = Instant::now() + TONE;
            let mut test = recording(now);
            assert_eq!(
                test.frame(&[0.0; FRAME], now, permitted, closed),
                Frame::Live
            );
            // the restored gate later lets frames through again: the run must not resume
            for index in 0..300 {
                let at = now + Duration::from_millis(20 * index);
                assert_eq!(test.frame(&[0.0; FRAME], at, true, closed), Frame::Live);
            }
            let result = test
                .poll(
                    now,
                    Status {
                        reset: polled,
                        ..ready()
                    },
                )
                .unwrap();
            assert_eq!(result.outcome, Outcome::TxNotPermitted);
            assert_eq!((result.frames_sent, result.mic_peak_dbfs), (0, -12));
            assert!(test.idle());
        }
    }

    #[test]
    fn a_reset_seen_by_the_poll_first_ends_the_run() {
        let now = Instant::now();
        let mut test = recording(now);
        let result = test
            .poll(
                now + TONE,
                Status {
                    reset: FENCE.reset + 2,
                    ..ready()
                },
            )
            .unwrap();
        assert_eq!(result.outcome, Outcome::TxNotPermitted);
        assert_eq!(fed(&mut test, &[0.0; FRAME], now), Frame::Live);
    }

    #[test]
    fn a_closed_fence_during_send_stops_sending_and_ends_the_run() {
        let mut now = Instant::now() + TONE;
        let mut test = recording(now);
        for _ in 1..150 {
            assert_eq!(fed(&mut test, &[0.25; FRAME], now), Frame::Live);
            now += Duration::from_millis(20);
        }
        for _ in 0..3 {
            assert!(matches!(
                fed(&mut test, &[0.0; FRAME], now),
                Frame::Send { last: false, .. }
            ));
            test.sent(1, now);
        }
        let closed = Fence {
            reset: FENCE.reset + 2,
            generation: Some(3),
        };
        for _ in 0..200 {
            assert_eq!(test.frame(&[0.0; FRAME], now, true, closed), Frame::Live);
        }
        let result = test
            .poll(
                now,
                Status {
                    reset: closed.reset,
                    ..ready()
                },
            )
            .unwrap();
        assert_eq!(result.outcome, Outcome::TxNotPermitted);
        assert_eq!(result.frames_sent, 3);
    }

    #[test]
    fn a_fence_change_before_the_first_recorded_frame_binds_the_new_fence() {
        let now = Instant::now();
        let mut test = SelfTest::default();
        test.request(1);
        test.poll(now, ready());
        test.poll(now + TONE, ready());
        let later = Fence {
            reset: 5,
            generation: Some(4),
        };
        assert!(
            test.poll(
                now + TONE,
                Status {
                    reset: later.reset,
                    ..ready()
                }
            )
            .is_none()
        );
        let mut sent = 0;
        loop {
            match test.frame(&[0.25; FRAME], now + TONE, true, later) {
                Frame::Live => {}
                Frame::Send { last, .. } => {
                    sent += 1;
                    if last {
                        break;
                    }
                }
            }
        }
        assert_eq!(sent, 150);
    }

    fn policy(revision: u64, permitted: bool, send_to: Vec<u16>) -> Vec<u8> {
        signaling::encode_policy(&Policy {
            audience: Audience {
                revision,
                send_to,
                receive_from: vec![],
            },
            tx_permitted: permitted,
            self_monitor: permitted,
        })
        .unwrap()
    }

    /// the main thread's poll of a real microphone
    fn polled(runtime: &Runtime, now: Instant) -> Status {
        Status {
            busy: false,
            permitted: runtime.microphone_permitted(now),
            reset: runtime.capture_policy().2,
            mic_open: true,
            playback: false,
        }
    }

    /// one step of the transmit worker for a gated microphone frame: the frame, the
    /// self-test's choice, the transmitter and the runtime. returns the PCM of each
    /// recorded frame sent and the revision of each game VOICE the runtime emitted
    fn transmit(
        runtime: &mut Runtime,
        tx: &mut Transmitter,
        test: &mut SelfTest,
        pcm: &[f32],
        generation: u64,
        now: Instant,
    ) -> (Option<Vec<f32>>, Vec<u64>) {
        let (permitted, revision, reset) = runtime.capture_policy();
        tx.set_policy(permitted, revision, reset);
        let fence = Fence {
            reset,
            generation: Some(generation),
        };
        let (packets, sent) = match test.frame(pcm, now, permitted, fence) {
            Frame::Send { pcm, last } => {
                // as the worker does: the talkspurt carries no live pre-roll
                tx.clear_preroll();
                tx.set_ptt(!last);
                (tx.frame(&pcm), Some(pcm))
            }
            Frame::Live => {
                tx.set_ptt(false);
                let mut packets = tx.close();
                packets.extend(tx.frame(pcm));
                (packets, None)
            }
        };
        for packet in packets {
            runtime.transmit(&packet, now);
        }
        let mut voices = Vec::new();
        while let Some(output) = runtime.poll() {
            if let Output::Game(bytes) = output
                && bytes[2] == wire::GAME_POLICY_VOICE
            {
                let mut forwarded = bytes[..3].to_vec();
                forwarded.extend_from_slice(&1u16.to_be_bytes());
                forwarded.extend_from_slice(&bytes[3..]);
                voices.push(wire::decode_policy_game(&forwarded).unwrap().revision);
            }
        }
        (sent, voices)
    }

    fn runtime(now: Instant) -> Runtime {
        let mut runtime = Runtime::new(
            vec![],
            RuntimeConfig {
                game_only: true,
                ..RuntimeConfig::default()
            },
        )
        .unwrap();
        runtime.identify(1, now).unwrap();
        runtime.game(&policy(7, true, vec![]), now).unwrap();
        while runtime.poll().is_some() {}
        runtime
    }

    #[test]
    fn deny_and_allow_before_the_next_poll_never_transmit_the_old_recording() {
        let now = Instant::now();
        let mut runtime = runtime(now);
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        let mut test = SelfTest::default();
        test.request(3);
        assert!(test.poll(now, polled(&runtime, now)).is_none());
        let (sent, voices) = transmit(&mut runtime, &mut tx, &mut test, &[0.25; FRAME], 1, now);
        assert_eq!((sent, voices), (None, vec![]));

        // POLICY 8 denies and 9 allows before the main thread polls again; the real
        // microphone closes and reopens, so later frames carry a new generation
        let reset = runtime.capture_policy().2;
        let mut mic_close = false;
        for (revision, permitted) in [(8, false), (9, true)] {
            runtime
                .game_with_capture_close(
                    &policy(revision, permitted, vec![]),
                    now,
                    |runtime, permitted| {
                        mic_close |= !permitted;
                        runtime.close_transmit();
                        while runtime.poll().is_some() {}
                    },
                )
                .unwrap();
        }
        assert!(mic_close);
        assert_eq!(runtime.capture_policy().2, reset + 2);
        for index in 0..300 {
            let at = now + Duration::from_millis(20 * index);
            let (sent, voices) = transmit(&mut runtime, &mut tx, &mut test, &[0.0; FRAME], 3, at);
            assert_eq!((sent, voices), (None, vec![]), "frame {index}");
        }
        let result = test.poll(now, polled(&runtime, now)).unwrap();
        assert_eq!(result.outcome, Outcome::TxNotPermitted);
        assert_eq!(result.frames_sent, 0);

        // a fresh run under the restored permission sends only its own recording
        test.request(4);
        assert!(test.poll(now, polled(&runtime, now)).is_none());
        let (mut sent, mut voices) = (Vec::new(), Vec::new());
        for index in 0..300 {
            let at = now + Duration::from_millis(20 * index);
            let (pcm, revisions) = transmit(&mut runtime, &mut tx, &mut test, &[0.5; FRAME], 3, at);
            sent.extend(pcm);
            voices.extend(revisions);
        }
        assert_eq!(sent.len(), 150);
        assert!(sent.iter().all(|pcm| *pcm == vec![0.5; FRAME]));
        assert_eq!(voices, vec![9; 150]);
    }

    #[test]
    fn a_policy_revision_that_keeps_the_capture_reset_keeps_the_run() {
        let now = Instant::now();
        let mut runtime = runtime(now);
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        let mut test = SelfTest::default();
        test.request(5);
        assert!(test.poll(now, polled(&runtime, now)).is_none());
        let reset = runtime.capture_policy().2;
        let mut sent = 0;
        for index in 0..300 {
            if index == 75 {
                // an audience change only: same transmit and monitor permission
                runtime.game(&policy(8, true, vec![2]), now).unwrap();
                assert_eq!(runtime.capture_policy(), (true, Some(8), reset));
                assert!(test.poll(now, polled(&runtime, now)).is_none());
            }
            let at = now + Duration::from_millis(20 * index);
            let (pcm, _) = transmit(&mut runtime, &mut tx, &mut test, &[0.25; FRAME], 1, at);
            sent += usize::from(pcm.is_some());
        }
        assert_eq!(sent, 150);
        assert!(!test.idle(), "waiting for the returned copy");
    }

    #[test]
    fn a_stalled_capture_fails_the_run() {
        let now = Instant::now();
        let mut test = SelfTest::default();
        test.request(1);
        test.poll(now, ready());
        test.poll(now + TONE, ready());
        assert!(test.poll(now + TONE + CAPTURE, ready()).is_none());
        let result = test.poll(now + TONE + CAPTURE + STALL, ready()).unwrap();
        assert_eq!(result.outcome, Outcome::MicOpenFailed);
    }

    #[test]
    fn peak_level_in_dbfs() {
        assert_eq!(dbfs(1.0), 0);
        assert_eq!(dbfs(2.0), 0);
        assert_eq!(dbfs(0.5), -6);
        assert_eq!(dbfs(0.0), -127);
        assert_eq!(dbfs(1e-9), -127);
    }
}
