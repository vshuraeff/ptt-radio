//! transmit state machine: decides per captured 20 ms frame what goes to the game.
//! on air while PTT is held or open mic is on; going on air sends START, then
//! the pre-roll of already-captured frames, then live VOICE; going off air
//! sends the frame in progress, then STOP twice.

use std::collections::VecDeque;

use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate};

use crate::packet::{self, START, STOP, VOICE};

pub const FRAME_SAMPLES: usize = 960;
/// 120 ms of audio captured before the press, the frame in progress included
pub const PREROLL_FRAMES: usize = 6;
// opus never produces more than 1275 bytes per frame
const MAX_PACKET: usize = 1275;

pub struct Transmitter {
    encoder: Encoder,
    seq: u16,
    ptt: bool,
    open_mic: bool,
    on_air: bool,
    preroll: VecDeque<Vec<f32>>,
}

impl Transmitter {
    pub fn new(bitrate: u32) -> Result<Self, opusic_c::ErrorCode> {
        let mut encoder = Encoder::new(Channels::Mono, SampleRate::Hz48000, Application::Voip)?;
        encoder.set_bitrate(Bitrate::Value(bitrate))?;
        encoder.set_inband_fec(InbandFec::Off)?;
        encoder.set_dtx(false)?;
        Ok(Transmitter {
            encoder,
            seq: 0,
            ptt: false,
            open_mic: false,
            on_air: false,
            preroll: VecDeque::new(),
        })
    }

    pub fn set_ptt(&mut self, held: bool) {
        self.ptt = held;
    }

    /// MIC_STATE carries the wanted state, so repeating it changes nothing
    pub fn set_open_mic(&mut self, open: bool) {
        self.open_mic = open;
    }

    /// takes one 48 kHz mono frame of FRAME_SAMPLES and returns the packets to send, in order
    pub fn frame(&mut self, pcm: &[f32]) -> Vec<Vec<u8>> {
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
        if self.preroll.len() > PREROLL_FRAMES {
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
        let mut buf = [0u8; MAX_PACKET];
        let len = self
            .encoder
            .encode_float_to_slice(pcm, &mut buf)
            .expect("opus encode of a 20 ms frame");
        self.seq = self.seq.wrapping_add(1);
        packet::encode(VOICE, self.seq, &buf[..len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(packets: &[Vec<u8>]) -> Vec<u8> {
        packets.iter().map(|p| p[2]).collect()
    }

    fn silence() -> Vec<f32> {
        vec![0.0; FRAME_SAMPLES]
    }

    #[test]
    fn idle_sends_nothing() {
        let mut tx = Transmitter::new(16_000).unwrap();
        for _ in 0..10 {
            assert!(tx.frame(&silence()).is_empty());
        }
    }

    #[test]
    fn ptt_press_sends_start_and_preroll_release_sends_frame_and_two_stops() {
        let mut tx = Transmitter::new(16_000).unwrap();
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
        let mut tx = Transmitter::new(16_000).unwrap();
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
        let mut tx = Transmitter::new(16_000).unwrap();
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
    fn close_stops_only_an_open_transmission() {
        let mut tx = Transmitter::new(16_000).unwrap();
        assert!(tx.close().is_empty());
        tx.set_ptt(true);
        tx.frame(&silence());
        assert_eq!(kinds(&tx.close()), [STOP, STOP]);
    }
}
