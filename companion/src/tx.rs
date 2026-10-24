//! transmit state machine: decides per captured frame (10 or 20 ms) what goes to
//! the game. on air while PTT is held or open mic is on; going on air sends START,
//! then the pre-roll of already-captured frames, then live VOICE; going off air
//! sends the frame in progress, then STOP twice.

use std::collections::VecDeque;

use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate};

use crate::audio::OPUS_RATE;
use crate::packet::{self, START, STOP, VOICE};
use crate::transport::wire::MAX_POLICY_OPUS;

/// audio captured before the press, the frame in progress included
const PREROLL_MS: usize = 120;

/// encoder settings (--frame-ms, --bitrate, --complexity, --fec). a receiver needs none
/// of them: opus packets carry their own duration
#[derive(Clone, Copy, Debug)]
pub struct Codec {
    pub frame_ms: usize,
    pub bitrate: u32,
    /// 0..=10; libopus 1.6.1 defaults to 9 for VOIP
    pub complexity: u8,
    /// in-band FEC, told to expect 10 % loss: without an expected loss the encoder spends
    /// nothing on it, and only a lost packet is ever decoded from it
    pub fec: bool,
}

impl Default for Codec {
    fn default() -> Self {
        Codec {
            frame_ms: 20,
            bitrate: 16_000,
            complexity: 9,
            fec: false,
        }
    }
}

impl Codec {
    pub fn frame_samples(&self) -> usize {
        OPUS_RATE as usize / 1000 * self.frame_ms
    }
}

pub struct Transmitter {
    encoder: Encoder,
    frame_samples: usize,
    preroll_frames: usize,
    seq: u16,
    ptt: bool,
    open_mic: bool,
    on_air: bool,
    preroll: VecDeque<Vec<f32>>,
    permitted: bool,
    policy_reset: u64,
}

impl Transmitter {
    pub fn new(codec: Codec) -> Result<Self, opusic_c::ErrorCode> {
        let mut encoder = Encoder::new(Channels::Mono, SampleRate::Hz48000, Application::Voip)?;
        encoder.set_bitrate(Bitrate::Value(codec.bitrate))?;
        encoder.set_complexity(codec.complexity)?;
        if codec.fec {
            encoder.set_inband_fec(InbandFec::Mode1)?;
            encoder.set_packet_loss(10)?;
        } else {
            encoder.set_inband_fec(InbandFec::Off)?;
        }
        encoder.set_dtx(false)?;
        Ok(Transmitter {
            encoder,
            frame_samples: codec.frame_samples(),
            preroll_frames: PREROLL_MS / codec.frame_ms,
            seq: 0,
            ptt: false,
            open_mic: false,
            on_air: false,
            preroll: VecDeque::new(),
            permitted: true,
            policy_reset: 0,
        })
    }

    /// 48 kHz samples `frame` takes
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn set_ptt(&mut self, held: bool) {
        self.ptt = held;
    }

    /// MIC_STATE carries the wanted state, so repeating it changes nothing
    pub fn set_open_mic(&mut self, open: bool) {
        self.open_mic = open;
    }

    pub fn set_policy(&mut self, permitted: bool, _revision: Option<u64>, reset: u64) {
        if !permitted || reset != self.policy_reset {
            self.on_air = false;
            self.preroll.clear();
        }
        self.permitted = permitted;
        self.policy_reset = reset;
    }

    /// takes one 48 kHz mono frame of `frame_samples` and returns the packets to send, in order
    pub fn frame(&mut self, pcm: &[f32]) -> Vec<Vec<u8>> {
        if !self.permitted {
            return Vec::new();
        }
        let want = self.ptt || self.open_mic;
        let mut out = Vec::new();
        if self.on_air {
            out.push(self.voice(pcm));
            if !want {
                out.push(packet::encode(STOP, self.seq, &[]));
                out.push(packet::encode(STOP, self.seq, &[]));
                self.on_air = false;
            }
            return out;
        }
        self.preroll.push_back(pcm.to_vec());
        if self.preroll.len() > self.preroll_frames {
            self.preroll.pop_front();
        }
        if want {
            // a fresh transmission must not continue the previous one's prediction state
            self.encoder.reset().expect("opus encoder reset");
            out.push(packet::encode(START, self.seq, &[]));
            let frames: Vec<_> = self.preroll.drain(..).collect();
            out.extend(frames.iter().map(|f| self.voice(f)));
            self.on_air = true;
        }
        out
    }

    /// STOP twice if a transmission is open, for shutdown mid-transmission
    pub fn close(&mut self) -> Vec<Vec<u8>> {
        if !self.on_air {
            return Vec::new();
        }
        self.on_air = false;
        vec![
            packet::encode(STOP, self.seq, &[]),
            packet::encode(STOP, self.seq, &[]),
        ]
    }

    fn voice(&mut self, pcm: &[f32]) -> Vec<u8> {
        // libopus fits each packet into the buffer it is given, so every frame fits every
        // route, the relay's included; opus alone would allow 1275 bytes
        let mut buf = [0u8; MAX_POLICY_OPUS];
        let len = self
            .encoder
            .encode_float_to_slice(pcm, &mut buf)
            .expect("opus encode of a 10 or 20 ms frame");
        self.seq = self.seq.wrapping_add(1);
        packet::encode(VOICE, self.seq, &buf[..len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_policy_revision_preserves_idle_pcm_preroll() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        tx.set_policy(true, Some(7), 1);
        let lead_in: Vec<_> = (0..tx.frame_samples())
            .map(|sample| (sample as f32 * std::f32::consts::TAU * 880.0 / 48_000.0).sin() * 0.3)
            .collect();
        for _ in 0..3 {
            assert!(tx.frame(&lead_in).is_empty());
        }
        tx.set_policy(true, Some(8), 1);
        tx.set_ptt(true);
        let packets = tx.frame(&silence());
        assert_eq!(kinds(&packets), [START, VOICE, VOICE, VOICE, VOICE]);
        let mut decoder = opusic_c::Decoder::new(Channels::Mono, SampleRate::Hz48000).unwrap();
        let mut pcm = [0.0; 960];
        decoder
            .decode_float_to_slice(&packets[1][5..], &mut pcm, false)
            .unwrap();
        assert!(pcm.iter().any(|sample| sample.abs() > 0.05));
    }

    #[test]
    fn own_permission_changes_discard_idle_preroll() {
        for mute in [false, true] {
            let mut tx = Transmitter::new(Codec::default()).unwrap();
            tx.set_policy(true, Some(7), 1);
            for _ in 0..3 {
                assert!(tx.frame(&silence()).is_empty());
            }
            tx.set_policy(!mute, Some(8), 2);
            if mute {
                assert!(tx.frame(&silence()).is_empty());
                tx.set_policy(true, Some(9), 3);
            }
            tx.set_ptt(true);
            assert_eq!(kinds(&tx.frame(&silence())), [START, VOICE]);
        }
    }

    #[test]
    fn policy_mute_discards_preroll_and_open_mic_reenables_as_a_fresh_transmission() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        tx.set_open_mic(true);
        tx.set_policy(true, Some(7), 1);
        assert_eq!(kinds(&tx.frame(&silence())), [START, VOICE]);
        tx.set_policy(true, Some(8), 1);
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE]);
        tx.set_policy(false, Some(9), 2);
        for _ in 0..10 {
            assert!(tx.frame(&silence()).is_empty());
        }
        tx.set_policy(true, Some(10), 3);
        assert_eq!(kinds(&tx.frame(&silence())), [START, VOICE]);
        tx.set_policy(true, Some(11), 4);
        assert_eq!(kinds(&tx.frame(&silence())), [START, VOICE]);
        tx.set_policy(true, Some(11), 4);
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE]);
    }

    fn kinds(packets: &[Vec<u8>]) -> Vec<u8> {
        packets.iter().map(|p| p[2]).collect()
    }

    fn silence() -> Vec<f32> {
        vec![0.0; Codec::default().frame_samples()]
    }

    #[test]
    fn ten_ms_frames_keep_120_ms_of_preroll() {
        let codec = Codec {
            frame_ms: 10,
            ..Codec::default()
        };
        let mut tx = Transmitter::new(codec).unwrap();
        assert_eq!(tx.frame_samples(), 480);
        for _ in 0..20 {
            tx.frame(&[0.0; 480]);
        }
        tx.set_ptt(true);
        let press = tx.frame(&[0.0; 480]);
        assert_eq!(kinds(&press).len(), 1 + 12);
        // a 10 ms opus packet says so itself: 480 samples at 48 kHz
        let decoder =
            opusic_c::Decoder::new(Channels::Mono, SampleRate::Hz48000).expect("opus decoder");
        assert_eq!(decoder.get_nb_samples(&press[1][5..]).unwrap(), 480);
    }

    #[test]
    fn idle_sends_nothing() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        for _ in 0..10 {
            assert!(tx.frame(&silence()).is_empty());
        }
    }

    #[test]
    fn ptt_press_sends_start_and_preroll_release_sends_frame_and_two_stops() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        for _ in 0..10 {
            tx.frame(&silence());
        }
        tx.set_ptt(true);
        let press = tx.frame(&silence());
        assert_eq!(
            kinds(&press),
            [START, VOICE, VOICE, VOICE, VOICE, VOICE, VOICE]
        );
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE]);
        tx.set_ptt(false);
        let release = tx.frame(&silence());
        assert_eq!(kinds(&release), [VOICE, STOP, STOP]);
        // VOICE seq runs 1..=8 and STOP carries the last one
        assert_eq!(&release[0][3..5], &8u16.to_be_bytes());
        assert_eq!(&release[2][3..5], &8u16.to_be_bytes());
        assert!(tx.frame(&silence()).is_empty());
    }

    #[test]
    fn preroll_is_shorter_right_after_startup_and_after_release() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        tx.set_ptt(true);
        assert_eq!(kinds(&tx.frame(&silence())), [START, VOICE]);
        tx.set_ptt(false);
        tx.frame(&silence());
        tx.frame(&silence());
        tx.set_ptt(true);
        assert_eq!(kinds(&tx.frame(&silence())), [START, VOICE, VOICE]);
    }

    #[test]
    fn mic_state_is_idempotent_and_overrides_ptt() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        tx.set_open_mic(true);
        assert_eq!(kinds(&tx.frame(&silence()))[0], START);
        tx.set_open_mic(true);
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE]);
        // PTT press and release while open mic is on change nothing
        tx.set_ptt(true);
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE]);
        tx.set_ptt(false);
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE]);
        tx.set_open_mic(false);
        assert_eq!(kinds(&tx.frame(&silence())), [VOICE, STOP, STOP]);
        tx.set_open_mic(false);
        assert!(tx.frame(&silence()).is_empty());
    }

    #[test]
    fn the_highest_bitrate_on_noise_still_fits_the_transport_cap() {
        use crate::transport::StreamId;
        use crate::transport::wire::{self, GameMedia, Kind};
        let codec = Codec {
            bitrate: 512_000,
            complexity: 10,
            ..Codec::default()
        };
        let mut tx = Transmitter::new(codec).unwrap();
        tx.set_ptt(true);
        let mut state = 1u32;
        let mut largest = 0;
        for _ in 0..50 {
            let pcm: Vec<f32> = (0..codec.frame_samples())
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 8) as f32 / (1u32 << 23) as f32 - 1.0
                })
                .collect();
            for p in tx.frame(&pcm).iter().filter(|p| p[2] == VOICE) {
                let opus = &p[5..];
                largest = largest.max(opus.len());
                let media = GameMedia {
                    stream: StreamId {
                        player: 1,
                        generation: 1,
                    },
                    kind: Kind::Voice,
                    sequence: 1,
                    talkspurt: 1,
                    opus,
                };
                assert!(
                    wire::encode_game(media, false).is_ok(),
                    "{} bytes",
                    opus.len()
                );
            }
        }
        // the encoder runs at the cap, so a larger buffer would have overrun it
        assert!(largest > MAX_POLICY_OPUS * 9 / 10, "{largest}");
    }

    #[test]
    fn close_stops_only_an_open_transmission() {
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        assert!(tx.close().is_empty());
        tx.set_ptt(true);
        tx.frame(&silence());
        assert_eq!(kinds(&tx.close()), [STOP, STOP]);
    }
}
