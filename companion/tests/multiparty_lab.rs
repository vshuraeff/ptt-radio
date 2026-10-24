//! offline closed-loop echo lab for several players, block by block (10 ms, 48 khz),
//! faster than real time. each player's mic hears his voice plus, on speakers, his
//! own speaker through a coupling; the speaker plays the others' streams, each sent
//! through the sender's canceller and opus at the companion defaults, and delayed by
//! both players' pings and the game's transport. echoes recirculate; a run that
//! would howl is stopped and reported.
//! cargo test --release --test multiparty_lab -- --ignored --nocapture
//! voices (48 khz mono wav, 30-40 s of speech with pauses): PTT_LAB_A, PTT_LAB_B,
//! PTT_LAB_C. listening wavs go to PTT_LAB_OUT, by default the cargo target tmpdir.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cpu_time::ProcessTime;
use dtln_runtime_lab::{RateConverter, Stream48};
use opusic_c::{Channels, Decoder, SampleRate};
use ptt_radio_companion::echo::{EchoPath, GateState};
use ptt_radio_companion::packet::VOICE;
use ptt_radio_companion::tx::{Codec, Transmitter};
use ptt_radio_companion::virtual_audio::{Coupling, read_wav};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use sonora::config::{EchoCanceller, TransparentModeType};
use sonora::{AudioProcessing, Config, StreamConfig};

const RATE: usize = 48_000;
const BLOCK: usize = RATE / 100;

const fn ms(ms: usize) -> usize {
    ms * RATE / 1000
}

// game transport (relay model in ../../DESIGN.md): 72 ms, delivered on a 60 hz tick
const GAME: usize = ms(72);
const TICK: usize = RATE / 60;
// receive queue after shedding, as measured end to end; the lab holds it fixed, so
// the per-tick bursts it smooths are not modelled
const RECEIVE: usize = ms(24);
// the mix of block t sounds during block t + 1: the companion's one-slice output queue
const AHEAD: usize = BLOCK;
const COUPLE_DELAY: Duration = Duration::from_millis(2);
// (ms, weight) of the room after the direct path, the taps of tests/scenarios.rs
const ROOM: [(f64, f32); 8] = [
    (0.0, 1.0),
    (3.1, 0.5),
    (5.7, -0.35),
    (9.2, 0.25),
    (14.0, -0.18),
    (21.0, 0.12),
    (33.0, 0.08),
    (47.0, -0.05),
];
// every voice is scaled to -24 dBFS rms over its active speech
const VOICE_RMS: f64 = 0.063;
// a speaker mix above -10 dBFS rms over half a second, 14 dB over one voice, before
// the mixer's soft limiter: the loop has run away
const HOWL_RMS: f64 = 0.316;
const HOWL_WINDOW: usize = 50;

// ---- cancellers

/// one player's canceller, fed the way the companion's playout and transmit workers
/// feed src/echo.rs: every 10 ms block that goes to the speaker, then every captured
/// 20 ms frame before it is encoded
trait Canceller {
    fn far_end(&mut self, block: &[f32]);
    fn near_end(&mut self, frame: &mut [f32]);
    fn gates(&self) -> &[GateState] {
        &[]
    }
}

struct Bypass;

impl Canceller for Bypass {
    fn far_end(&mut self, _block: &[f32]) {}
    fn near_end(&mut self, _frame: &mut [f32]) {}
}

// render blocks waiting for a capture block; older ones are analysed alone
const MAX_PENDING_RENDER: usize = 2;

/// stock sonora 0.2.0 aec3 with the hmm transparent mode, render paced one block per
/// capture block: src/echo.rs as of commit 9ffc493 (unit u2), ported here unchanged
struct Sonora {
    processor: AudioProcessing,
    render: VecDeque<[f32; BLOCK]>,
}

impl Sonora {
    fn new() -> Self {
        let stream = StreamConfig::new(RATE as u32, 1);
        let processor = AudioProcessing::builder()
            .config(Config {
                echo_canceller: Some(EchoCanceller {
                    transparent_mode: TransparentModeType::Hmm,
                    ..EchoCanceller::default()
                }),
                ..Config::default()
            })
            .capture_config(stream)
            .render_config(stream)
            .build();
        Sonora {
            processor,
            render: VecDeque::new(),
        }
    }

    fn render(&mut self, block: &[f32; BLOCK]) {
        let mut output = [0.0; BLOCK];
        self.processor
            .process_render_f32(&[block], &mut [&mut output])
            .expect("48 khz mono render block");
    }
}

impl Canceller for Sonora {
    fn far_end(&mut self, block: &[f32]) {
        self.render.extend(block.as_chunks::<BLOCK>().0);
    }

    fn near_end(&mut self, frame: &mut [f32]) {
        let mut output = [0.0; BLOCK];
        for block in frame.as_chunks_mut::<BLOCK>().0 {
            while self.render.len() > MAX_PENDING_RENDER {
                let stale = self.render.pop_front().unwrap();
                self.render(&stale);
            }
            if let Some(far) = self.render.pop_front() {
                self.render(&far);
            }
            self.processor
                .process_capture_f32(&[block], &mut [&mut output])
                .expect("48 khz mono capture block");
            block.copy_from_slice(&output);
        }
    }
}

/// a fresh canceller for one player
type MakeCanceller = fn() -> Box<dyn Canceller>;

struct Gated {
    echo: EchoPath,
    states: Vec<GateState>,
}

impl Canceller for Gated {
    fn far_end(&mut self, block: &[f32]) {
        self.echo.far_end(block);
    }

    fn near_end(&mut self, frame: &mut [f32]) {
        for block in frame.as_chunks_mut::<BLOCK>().0 {
            self.echo.near_end(block);
            self.states.push(self.echo.gate_state());
        }
    }

    fn gates(&self) -> &[GateState] {
        &self.states
    }
}

struct Neural {
    stream: Stream48,
    render: VecDeque<[f32; BLOCK]>,
}

impl Neural {
    fn new(units: usize) -> Self {
        let variable = format!("PTT_LAB_DTLN_{units}");
        let default =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("experiments/dtln-runtime/models");
        let default = if units == 512 {
            default.join("512")
        } else {
            default
        };
        let directory = std::env::var_os(&variable).map_or(default, PathBuf::from);
        Self {
            stream: Stream48::load(&directory, units).unwrap_or_else(|e| {
                panic!(
                    "load DTLN {units} at {} ({variable}): {e:#}",
                    directory.display()
                )
            }),
            render: VecDeque::new(),
        }
    }
}

impl Canceller for Neural {
    fn far_end(&mut self, block: &[f32]) {
        self.render.extend(block.as_chunks::<BLOCK>().0);
    }

    fn near_end(&mut self, frame: &mut [f32]) {
        for block in frame.as_chunks_mut::<BLOCK>().0 {
            let far = self
                .render
                .pop_front()
                .expect("one render per capture block");
            *block = self.stream.process(block, &far).expect("DTLN inference");
        }
    }
}

#[derive(Default)]
struct Resampled(RateConverter);

impl Canceller for Resampled {
    fn far_end(&mut self, _block: &[f32]) {}

    fn near_end(&mut self, frame: &mut [f32]) {
        for block in frame.as_chunks_mut::<BLOCK>().0 {
            let down = self.0.down(block);
            *block = self.0.up(&down);
        }
    }
}

const WET_DELAY: usize = 436;
const ROWS: [(&str, MakeCanceller); 6] = [
    ("none", || Box::new(Bypass)),
    ("sonora-hmm", || Box::new(Sonora::new())),
    ("aec3-gate", || {
        Box::new(Gated {
            echo: EchoPath::default(),
            states: Vec::new(),
        })
    }),
    ("resample-only", || Box::new(Resampled::default())),
    ("dtln-128", || Box::new(Neural::new(128))),
    ("dtln-512", || Box::new(Neural::new(512))),
];

/// capture delay through a canceller: where an impulse comes out with no far end
fn canceller_delay(make: MakeCanceller) -> usize {
    let mut canceller = make();
    let impulse = RATE + 123;
    let mut out = Vec::new();
    for n in 0..150 {
        let mut frame = [0.0; 2 * BLOCK];
        if (n * frame.len()..(n + 1) * frame.len()).contains(&impulse) {
            frame[impulse % frame.len()] = 0.5;
        }
        canceller.far_end(&[0.0; BLOCK]);
        canceller.far_end(&[0.0; BLOCK]);
        canceller.near_end(&mut frame);
        out.extend(frame);
    }
    let peak = argmax(&out, |s| s.abs() as f64);
    assert!(out[peak].abs() > 0.01, "impulse was suppressed");
    peak - impulse
}

/// opus delay at the companion defaults: lag of the decoded voice behind its input
fn codec_delay(voice: &[f32]) -> usize {
    let mut link = Link::new();
    let input = &voice[..5 * RATE];
    let mut out = Vec::new();
    for frame in input.chunks_exact(link.frame) {
        out.extend(link.send(frame));
    }
    let dot = |d: usize| -> f64 {
        input[..4 * RATE]
            .iter()
            .zip(&out[d..])
            .map(|(a, b)| *a as f64 * *b as f64)
            .sum()
    };
    let lags: Vec<f64> = (0..ms(20)).map(dot).collect();
    argmax(&lags, |x| *x)
}

fn argmax<T>(v: &[T], key: impl Fn(&T) -> f64) -> usize {
    (0..v.len())
        .max_by(|a, b| key(&v[*a]).total_cmp(&key(&v[*b])))
        .unwrap()
}

/// a player's transmit chain as one listener's decoder hears it: the companion's
/// transmitter on open mic and one decoder (every listener decodes the same bytes)
struct Link {
    tx: Transmitter,
    decoder: Decoder,
    frame: usize,
}

impl Link {
    fn new() -> Self {
        let mut tx = Transmitter::new(Codec::default()).expect("opus encoder");
        tx.set_open_mic(true);
        Link {
            frame: tx.frame_samples(),
            tx,
            decoder: Decoder::new(Channels::Mono, SampleRate::Hz48000).expect("opus decoder"),
        }
    }

    fn send(&mut self, pcm: &[f32]) -> Vec<f32> {
        let packets = self.tx.frame(pcm);
        let voice: Vec<_> = packets.iter().filter(|p| p[2] == VOICE).collect();
        // open mic from the first frame: one VOICE per frame, the first after START
        assert_eq!(voice.len(), 1);
        let mut out = vec![0.0; self.frame];
        let n = self
            .decoder
            .decode_float_to_slice(&voice[0][5..], &mut out, false)
            .expect("opus decode");
        assert_eq!(n, self.frame);
        out
    }
}

// ---- scenarios

/// seat i speaks voice i (PTT_LAB_A, _B, _C)
struct Seat {
    name: &'static str,
    ping_ms: usize,
    /// total speaker-to-mic coupling, direct path and room together; None: headphones
    couple_db: Option<f64>,
}

/// (seat, from s, to s)
type Turn = (usize, f64, f64);

struct Scenario {
    name: &'static str,
    title: &'static str,
    seats: [Seat; 3],
    /// the measured schedule
    talk: &'static [Turn],
    /// far-end speech before it in a warm run, content from the end of each voice
    warm: &'static [Turn],
    seconds: f64,
}

const WARM_SECONDS: f64 = 10.0;

fn scenarios() -> [Scenario; 2] {
    let seat = |name, ping_ms, couple_db| Seat {
        name,
        ping_ms,
        couple_db,
    };
    [
        Scenario {
            name: "a",
            title: "three on speakers and open mic, coupling -6 dB: one, two, three talk at once",
            seats: [
                seat("A", 20, Some(-6.0)),
                seat("B", 60, Some(-6.0)),
                seat("C", 120, Some(-6.0)),
            ],
            talk: &[
                (0, 0.0, 6.0),
                (1, 7.0, 13.0),
                (2, 14.0, 20.0),
                // an argument: b cuts in on a, then c joins both
                (0, 21.0, 31.0),
                (1, 22.5, 31.0),
                (0, 32.0, 42.0),
                (1, 32.5, 42.0),
                (2, 33.0, 42.0),
            ],
            warm: &[(0, 0.0, 3.0), (1, 3.3, 6.3), (2, 6.6, 9.6)],
            seconds: 46.0,
        },
        Scenario {
            name: "b",
            title: "A and B on headphones talk, C silent on open mic, speakers at +6 dB coupling",
            seats: [
                seat("A", 20, None),
                seat("B", 60, None),
                seat("C", 120, Some(6.0)),
            ],
            talk: &[
                (0, 0.0, 6.0),
                (1, 7.0, 13.0),
                (0, 14.0, 28.0),
                (1, 15.0, 28.0),
            ],
            warm: &[(0, 0.0, 4.5), (1, 5.0, 9.5)],
            seconds: 32.0,
        },
    ]
}

/// one-way delay from one player's capture to another's receive queue output
fn path(from: &Seat, to: &Seat) -> usize {
    PATH_MODEL.with(|model| match model.get() {
        PathModel::Game => {
            (ms(from.ping_ms) + GAME + ms(to.ping_ms)).div_ceil(TICK) * TICK + RECEIVE
        }
        PathModel::Direct => {
            ms(match (from.name, to.name) {
                ("A", "B") | ("B", "A") => 15,
                ("A", "C") | ("C", "A") => 35,
                _ => 25,
            }) + RECEIVE
        }
        PathModel::Hub => ms(from.ping_ms + to.ping_ms) + RECEIVE,
    })
}

#[derive(Clone, Copy)]
enum PathModel {
    Game,
    Direct,
    Hub,
}

thread_local! { static PATH_MODEL: std::cell::Cell<PathModel> = const { std::cell::Cell::new(PathModel::Game) }; }

fn room_ir() -> Vec<f32> {
    let mut ir = vec![0.0f32; ms(50)];
    for (at, w) in ROOM {
        ir[(at * RATE as f64 / 1000.0).round() as usize] = w;
    }
    ir
}

/// gain of the direct path so that path and room together give `total_db`
fn coupling(total_db: f64) -> Coupling {
    let room: f64 = ROOM.iter().map(|(_, w)| (*w as f64).powi(2)).sum();
    Coupling::new(
        (total_db - 10.0 * room.log10()) as f32,
        COUPLE_DELAY,
        &room_ir(),
    )
}

/// sample `k` of what the mic picks up from `speaker` through `c`
fn heard(c: &Coupling, speaker: &[f32], k: usize) -> f32 {
    let sum: f32 = c
        .taps
        .iter()
        .filter_map(|(j, w)| k.checked_sub(c.delay_samples + j).map(|x| speaker[x] * w))
        .sum();
    sum * c.gain
}

/// scales a voice to VOICE_RMS over its active 10 ms blocks
fn normalise(mut voice: Vec<f32>) -> Vec<f32> {
    let blocks: Vec<f64> = voice.chunks(BLOCK).map(energy).collect();
    let loudest = blocks.iter().copied().fold(0.0, f64::max);
    let active: Vec<f64> = blocks.into_iter().filter(|e| *e > loudest * 1e-4).collect();
    let rms = (active.iter().sum::<f64>() / (active.len() * BLOCK) as f64).sqrt();
    let gain = (VOICE_RMS / rms) as f32;
    voice.iter_mut().for_each(|s| *s *= gain);
    voice
}

/// lays `turns` (shifted by `offset` s) out on a timeline of `len` samples, taking
/// consecutive speech from `cursor` on, with a 5 ms fade at each edge
fn place(voice: &[f32], turns: &[(f64, f64)], offset: f64, mut cursor: usize, out: &mut [f32]) {
    let fade = ms(5);
    for (from, to) in turns {
        let start = ((from + offset) * RATE as f64) as usize;
        let n = ((to - from) * RATE as f64) as usize;
        for i in 0..n {
            let edge = i.min(n - 1 - i);
            let w = if edge < fade {
                0.5 - 0.5 * (std::f32::consts::PI * edge as f32 / fade as f32).cos()
            } else {
                1.0
            };
            out[start + i] += voice[(cursor + i) % voice.len()] * w;
        }
        cursor += n;
    }
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|s| (*s as f64).powi(2)).sum()
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.log10()
}

// ---- the closed loop

struct Run {
    /// per seat, on one timeline of capture samples
    voice: Vec<Vec<f32>>,
    /// what the coupling put into each mic
    echo: Vec<Vec<f32>>,
    /// each seat's stream as every listener decodes it, before the path delay
    decoded: Vec<Vec<f32>>,
    /// what each seat hears
    speaker: Vec<Vec<f32>>,
    /// samples clipped at each mic
    clipped: Vec<usize>,
    howl: Option<f64>,
    /// schedule offset, s
    offset: f64,
    gates: Vec<Vec<GateState>>,
    cpu: Vec<Duration>,
    blocks: Vec<usize>,
}

fn simulate(scn: &Scenario, voices: &[Vec<f32>], make: MakeCanceller, warm: bool) -> Run {
    let offset = if warm { WARM_SECONDS } else { 0.0 };
    let len = ((scn.seconds + offset) * RATE as f64) as usize / (2 * BLOCK) * (2 * BLOCK);
    let seats = &scn.seats;
    let voice: Vec<Vec<f32>> = (0..seats.len())
        .map(|p| {
            let mut out = vec![0.0; len];
            let turns = |list: &[Turn]| -> Vec<(f64, f64)> {
                list.iter()
                    .filter(|t| t.0 == p)
                    .map(|t| (t.1, t.2))
                    .collect()
            };
            place(&voices[p], &turns(scn.talk), offset, 0, &mut out);
            if warm {
                let pre = turns(scn.warm);
                let total: f64 = pre.iter().map(|(a, z)| z - a).sum();
                let cursor = voices[p]
                    .len()
                    .saturating_sub((total * RATE as f64) as usize);
                place(&voices[p], &pre, 0.0, cursor, &mut out);
            }
            out
        })
        .collect();
    let couplings: Vec<Option<Coupling>> =
        seats.iter().map(|s| s.couple_db.map(coupling)).collect();
    let mut cancellers: Vec<Box<dyn Canceller>> = seats.iter().map(|_| make()).collect();
    let mut links: Vec<Link> = seats.iter().map(|_| Link::new()).collect();
    let n = seats.len();
    let mut run = Run {
        voice,
        echo: vec![vec![0.0; len]; n],
        decoded: vec![vec![0.0; len]; n],
        speaker: vec![vec![0.0; len + AHEAD]; n],
        clipped: vec![0; n],
        howl: None,
        offset,
        gates: vec![Vec::new(); n],
        cpu: vec![Duration::ZERO; n],
        blocks: vec![0; n],
    };
    let mut frames = vec![vec![0.0f32; 2 * BLOCK]; n];
    let mut loudness = vec![0.0; n];
    'run: for t in 0..len / BLOCK {
        let k0 = t * BLOCK;
        for l in 0..n {
            let mut mix = [0.0f32; BLOCK];
            for (from, decoded) in run.decoded.iter().enumerate().filter(|(p, _)| *p != l) {
                let d = path(&seats[from], &seats[l]);
                for (i, m) in mix.iter_mut().enumerate() {
                    if let Some(k) = (k0 + i).checked_sub(d) {
                        *m += decoded[k];
                    }
                }
            }
            loudness[l] += energy(&mix);
            if t % HOWL_WINDOW == HOWL_WINDOW - 1 {
                if loudness[l] > HOWL_RMS.powi(2) * (HOWL_WINDOW * BLOCK) as f64 {
                    run.howl = Some(k0 as f64 / RATE as f64 - offset);
                    break 'run;
                }
                loudness[l] = 0.0;
            }
            // the mixer's soft limiter
            mix.iter_mut().for_each(|s| *s = s.tanh());
            run.speaker[l][k0 + AHEAD..k0 + AHEAD + BLOCK].copy_from_slice(&mix);
            let cpu = ProcessTime::now();
            cancellers[l].far_end(&mix);
            run.cpu[l] += cpu.elapsed();
            run.blocks[l] += 1;
            let frame = &mut frames[l][(t % 2) * BLOCK..(t % 2 + 1) * BLOCK];
            for (i, s) in frame.iter_mut().enumerate() {
                let k = k0 + i;
                if let Some(c) = &couplings[l] {
                    run.echo[l][k] = heard(c, &run.speaker[l], k);
                }
                let mic = run.voice[l][k] + run.echo[l][k];
                run.clipped[l] += usize::from(mic.abs() > 1.0);
                *s = mic.clamp(-1.0, 1.0);
            }
            if t % 2 == 1 {
                let cpu = ProcessTime::now();
                cancellers[l].near_end(&mut frames[l]);
                run.cpu[l] += cpu.elapsed();
                let pcm = links[l].send(&frames[l]);
                run.decoded[l][k0 + BLOCK - 2 * BLOCK..k0 + BLOCK].copy_from_slice(&pcm);
            }
        }
    }
    for (states, canceller) in run.gates.iter_mut().zip(&cancellers) {
        states.extend_from_slice(canceller.gates());
    }
    run
}

/// a magnitude bound on the amplitude gain per hop of the loop without a canceller at its
/// worst frequency, and that frequency: a stream t sends reaches every other seat l, whose
/// mic sends it on at l's coupling. this is the spectral radius of |H_l(f)| over the seats,
/// phase discarded, so it is the gain if every path lined up in phase; below 1 (0 dB) the
/// loop cannot howl, above it it may, and the simulation's own howl is the evidence
fn loop_gain(scn: &Scenario) -> (f64, f64) {
    let worst = |f: f64| -> f64 {
        let h: Vec<f64> = scn
            .seats
            .iter()
            .map(|s| {
                s.couple_db.map_or(0.0, |d| {
                    let c = coupling(d);
                    let w = 2.0 * std::f64::consts::PI * f / RATE as f64;
                    let sum = c.taps.iter().fold(Complex::new(0.0, 0.0), |acc, (i, x)| {
                        acc + Complex::from_polar(*x as f64, -w * *i as f64)
                    });
                    c.gain as f64 * sum.norm()
                })
            })
            .collect();
        // power iteration on the non-negative matrix h_l for t != l
        let mut x = vec![1.0; h.len()];
        let mut rho = 0.0;
        for _ in 0..100 {
            let total: f64 = x.iter().sum();
            let next: Vec<f64> = (0..h.len()).map(|l| h[l] * (total - x[l])).collect();
            let norm: f64 = next.iter().sum();
            if norm == 0.0 {
                return 0.0;
            }
            rho = norm / x.iter().sum::<f64>();
            x = next.iter().map(|v| v / norm).collect();
        }
        rho
    };
    (50..800)
        .map(|f| (worst(f as f64 * 10.0), f as f64 * 10.0))
        .fold((0.0, 0.0), |a, b| if b.0 > a.0 { b } else { a })
}

/// speaker level decay over the silent tail after the last turn, dB/s, per seat;
/// None when the speaker falls below -90 dBFS too soon to fit a slope
fn tail_decay(run: &Run, scn: &Scenario) -> Vec<Option<f64>> {
    let last = scn.talk.iter().map(|t| t.2).fold(0.0, f64::max);
    let from = ((last + 0.5 + run.offset) * RATE as f64) as usize;
    let hop = ms(250);
    run.speaker
        .iter()
        .map(|s| {
            let pts: Vec<(f64, f64)> = s[from..s.len() - AHEAD]
                .chunks_exact(hop)
                .enumerate()
                .map(|(i, c)| (i as f64 * 0.25, db(energy(c) / hop as f64)))
                .take_while(|(_, level)| *level > -90.0)
                .collect();
            if pts.len() < 3 {
                return None;
            }
            let n = pts.len() as f64;
            let (mx, my) = (
                pts.iter().map(|p| p.0).sum::<f64>() / n,
                pts.iter().map(|p| p.1).sum::<f64>() / n,
            );
            let cov: f64 = pts.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
            let var: f64 = pts.iter().map(|(x, _)| (x - mx).powi(2)).sum();
            Some(cov / var)
        })
        .collect()
}

// ---- the per-band near-end fit (Y = A·N + B·E + R per bin and window)

const FFT: usize = 1024;
const HOP: usize = BLOCK;
/// stft frames per fit window: 90 ms
const M: usize = 9;
/// bins (46.9 Hz) of <1 kHz, 1-4 kHz, 4-8 kHz
const BANDS: [(usize, usize); 3] = [(1, 21), (22, 85), (86, 170)];
const TOP_BIN: usize = 170;
/// a window and bin whose normalised 2x2 gram has a larger condition number is skipped
const COND_MAX: f64 = 100.0;
/// the |ρ| at that condition number, (1 + |ρ|) / (1 - |ρ|) = COND_MAX
const RHO_MAX: f64 = (COND_MAX - 1.0) / (COND_MAX + 1.0);
/// a bin's near-end loss -20·log10|A| is capped here, so a fully cut bin counts as 60 dB
const LOSS_CAP_DB: f64 = 60.0;
/// a regressor's window counts towards its loss or echo reduction above this share of
/// its loudest window
const ACTIVE: f64 = 1e-3;
/// and takes part in the fit above this share: a quieter one left out would push its
/// residue onto the other
const PRESENT: f64 = 1e-6;
/// echo reduction counts a window's band only where the echo is at most 10 dB below
/// the near end
const ECHO_VS_NEAR: f64 = 0.1;
/// a window whose band loss exceeds this is a hole
const HOLE_DB: f64 = 6.0;

type Spectrum = Vec<Vec<Complex<f64>>>;

fn stft(x: &[f32], fft: &Arc<dyn Fft<f64>>) -> Spectrum {
    let window: Vec<f64> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / FFT as f64).cos())
        .collect();
    (0..(x.len() - FFT) / HOP + 1)
        .map(|f| {
            let mut buf: Vec<Complex<f64>> = x[f * HOP..f * HOP + FFT]
                .iter()
                .zip(&window)
                .map(|(s, w)| Complex::new(*s as f64 * w, 0.0))
                .collect();
            fft.process(&mut buf);
            buf.truncate(TOP_BIN + 1);
            buf
        })
        .collect()
}

#[derive(Default, Clone, Copy)]
struct Band {
    /// |N|², |A·N|², |E|², |B·E|², |R|² summed over fitted bins
    n: f64,
    an: f64,
    /// each fitted bin's loss -20·log10|A| (capped) weighted by its |N|²
    loss: f64,
    e: f64,
    be: f64,
    r: f64,
    bins: usize,
    skipped: usize,
}

struct WindowFit {
    /// sample at the window centre
    centre: usize,
    n_active: bool,
    e_active: bool,
    bands: [Band; 3],
}

/// whole fit windows of M frames
fn windows(s: &Spectrum) -> &[[Vec<Complex<f64>>; M]] {
    s.as_chunks::<M>().0
}

/// fits `y` on near end `n` and echo `e`, all on one timeline
fn fit(y: &[f32], n: &[f32], e: &[f32], fft: &Arc<dyn Fft<f64>>) -> Vec<WindowFit> {
    let (sy, sn, se) = (stft(y, fft), stft(n, fft), stft(e, fft));
    let window_energy = |s: &Spectrum| -> Vec<f64> {
        windows(s)
            .iter()
            .map(|w| {
                w.iter()
                    .map(|f| f[1..=TOP_BIN].iter().map(|c| c.norm_sqr()).sum::<f64>())
                    .sum()
            })
            .collect()
    };
    let above = |s: &Spectrum, share: f64| -> Vec<bool> {
        let en = window_energy(s);
        let loudest = en.iter().copied().fold(0.0, f64::max);
        en.iter()
            .map(|x| loudest > 0.0 && *x > loudest * share)
            .collect()
    };
    let (n_in, e_in) = (above(&sn, PRESENT), above(&se, PRESENT));
    let (n_on, e_on) = (above(&sn, ACTIVE), above(&se, ACTIVE));
    let mut out = Vec::new();
    for (w, ((fy, fn_), fe)) in windows(&sy)
        .iter()
        .zip(windows(&sn))
        .zip(windows(&se))
        .enumerate()
    {
        let mut bands = [Band::default(); 3];
        if n_on[w] || e_on[w] {
            for (band, (lo, hi)) in bands.iter_mut().zip(BANDS) {
                for k in lo..=hi {
                    fit_bin(band, k, fy, fn_, fe, n_in[w], e_in[w]);
                }
            }
        }
        out.push(WindowFit {
            centre: w * M * HOP + ((M - 1) * HOP + FFT) / 2,
            n_active: n_on[w],
            e_active: e_on[w],
            bands,
        });
    }
    out
}

fn fit_bin(
    band: &mut Band,
    k: usize,
    fy: &[Vec<Complex<f64>>],
    fn_: &[Vec<Complex<f64>>],
    fe: &[Vec<Complex<f64>>],
    n_on: bool,
    e_on: bool,
) {
    let zero = Complex::new(0.0, 0.0);
    let (mut nn, mut ee, mut ne, mut ny, mut ey) = (0.0, 0.0, zero, zero, zero);
    for ((y, n), e) in fy.iter().zip(fn_).zip(fe) {
        let (y, n, e) = (
            y[k],
            if n_on { n[k] } else { zero },
            if e_on { e[k] } else { zero },
        );
        nn += n.norm_sqr();
        ee += e.norm_sqr();
        ne += n.conj() * e;
        ny += n.conj() * y;
        ey += e.conj() * y;
    }
    band.bins += 1;
    let (a, b) = match (nn > 0.0, ee > 0.0) {
        (true, true) => {
            // normalised gram [[1, ρ], [ρ*, 1]]: eigenvalues 1 ± |ρ|. rounding can put a
            // collinear pair's |ρ| at or above 1
            let rho = (ne.norm_sqr() / (nn * ee)).sqrt();
            if rho.is_nan() || rho > RHO_MAX {
                band.skipped += 1;
                return;
            }
            let det = nn * ee - ne.norm_sqr();
            ((ny * ee - ne * ey) / det, (ey * nn - ne.conj() * ny) / det)
        }
        (true, false) => (ny / nn, zero),
        (false, true) => (zero, ey / ee),
        (false, false) => {
            band.skipped += 1;
            return;
        }
    };
    let r: f64 = fy
        .iter()
        .zip(fn_)
        .zip(fe)
        .map(|((y, n), e)| {
            let model = if n_on { a * n[k] } else { zero } + if e_on { b * e[k] } else { zero };
            (y[k] - model).norm_sqr()
        })
        .sum();
    band.n += nn;
    band.an += a.norm_sqr() * nn;
    band.loss += nn * (-db(a.norm_sqr())).min(LOSS_CAP_DB);
    band.e += ee;
    band.be += b.norm_sqr() * ee;
    band.r += r;
}

/// one table row: per band near-end loss and hole share, echo reduction, and the
/// distortion ratio and skipped share over all bands
struct Summary {
    loss: [Option<(f64, f64)>; 3],
    erle: [Option<f64>; 3],
    dist: Option<f64>,
    skipped: f64,
}

fn summarise<'a>(windows: impl Iterator<Item = &'a WindowFit> + Clone) -> Summary {
    let mut loss = [None; 3];
    let mut erle = [None; 3];
    let (mut r, mut an, mut bins, mut skipped) = (0.0, 0.0, 0, 0);
    for b in 0..3 {
        let near = windows.clone().filter(|w| w.n_active);
        let loudest = near.clone().map(|w| w.bands[b].n).fold(0.0, f64::max);
        // a fully cut window still counts: its fitted near energy, not the output, decides
        let counted: Vec<&Band> = near
            .map(|w| &w.bands[b])
            .filter(|x| x.n > loudest * ACTIVE)
            .collect();
        if !counted.is_empty() {
            // energy-weighted mean of the per-bin dB loss, not a ratio of summed powers
            let (n, l): (f64, f64) = counted
                .iter()
                .fold((0.0, 0.0), |s, x| (s.0 + x.n, s.1 + x.loss));
            let holes = counted.iter().filter(|x| x.loss / x.n > HOLE_DB).count();
            loss[b] = Some((l / n, holes as f64 / counted.len() as f64));
        }
        // an echo far below the near end is lost in its codec noise: no reduction to read
        let echo: Vec<&Band> = windows
            .clone()
            .filter(|w| w.e_active)
            .map(|w| &w.bands[b])
            .filter(|x| x.e > ECHO_VS_NEAR * x.n)
            .collect();
        let (e, be) = echo.iter().fold((0.0, 0.0), |s, x| (s.0 + x.e, s.1 + x.be));
        if e > 0.0 {
            erle[b] = Some(db(e / be.max(e * 1e-12)));
        }
        for w in windows.clone() {
            let x = &w.bands[b];
            bins += x.bins;
            skipped += x.skipped;
            if w.n_active {
                r += x.r;
                an += x.an;
            }
        }
    }
    Summary {
        loss,
        erle,
        dist: (an > 0.0).then(|| db(r / an)),
        skipped: skipped as f64 / bins.max(1) as f64,
    }
}

fn delayed(x: &[f32], d: usize) -> Vec<f32> {
    let mut out = vec![0.0; x.len()];
    out[d..].copy_from_slice(&x[..x.len() - d]);
    out
}

/// decoded sample -> original capture sample; gate targets select the stable dry/wet delay
fn capture_at(sample: usize, codec: usize, delay: usize, gates: &[GateState]) -> Option<usize> {
    let output = sample.checked_sub(codec)?;
    let delay = gates
        .get(output / BLOCK)
        .map_or(delay, |s| if s.engaged { WET_DELAY } else { 0 });
    output.checked_sub(delay)
}

fn aligned(x: &[f32], codec: usize, delay: usize, gates: &[GateState]) -> Vec<f32> {
    (0..x.len())
        .map(|k| capture_at(k, codec, delay, gates).map_or(0.0, |i| x[i]))
        .collect()
}

fn transitions(gates: &[GateState]) -> impl Iterator<Item = (usize, &GateState)> {
    gates
        .iter()
        .enumerate()
        .filter(|(i, s)| s.engaged != i.checked_sub(1).is_some_and(|j| gates[j].engaged))
}

/// reject the whole fit support if it touches a crossfade or its conservative codec guard
fn stable_gate_window(w: &WindowFit, codec: usize, gates: &[GateState]) -> bool {
    let half = ((M - 1) * HOP + FFT) / 2;
    let from = w.centre.saturating_sub(half);
    let to = w.centre + half;
    transitions(gates).all(|(block, _)| {
        let at = block * BLOCK + codec;
        to <= at.saturating_sub(2 * BLOCK) || from >= at + BLOCK + 2 * BLOCK
    })
}

#[test]
fn gate_alignment_uses_wet_delay_and_excludes_transition_support() {
    let codec = 312;
    let at = 20 * BLOCK;
    let gates: Vec<_> = (0..60)
        .map(|i| GateState {
            engaged: (20..45).contains(&i),
            ..GateState::default()
        })
        .collect();
    assert_eq!(capture_at(at - 1 + codec, codec, 0, &gates), Some(at - 1));
    assert_eq!(
        capture_at(at + codec, codec, 0, &gates),
        Some(at - WET_DELAY)
    );
    assert_eq!(
        capture_at(45 * BLOCK + codec, codec, 0, &gates),
        Some(45 * BLOCK)
    );
    let window = |centre| WindowFit {
        centre,
        n_active: true,
        e_active: false,
        bands: [Band::default(); 3],
    };
    for centre in [at + codec - BLOCK, at + codec, at + codec + 3 * BLOCK] {
        assert!(!stable_gate_window(&window(centre), codec, &gates));
    }
    assert!(stable_gate_window(
        &window(at + codec + 10 * BLOCK),
        codec,
        &gates
    ));
    assert!(!stable_gate_window(
        &window(45 * BLOCK + codec),
        codec,
        &gates
    ));
    assert!(stable_gate_window(
        &window(at + codec - 10 * BLOCK),
        codec,
        &gates
    ));
    assert_eq!(transitions(&gates).count(), 2);
}

#[test]
fn new_adapters_preserve_cadence_and_resampler_delay() {
    assert_eq!(canceller_delay(|| Box::new(Resampled::default())), 126);
    assert_eq!(canceller_delay(ROWS[2].1), 0);
    let x: Vec<f32> = (0..RATE).map(|i| (i as f32 * 0.1).sin()).collect();
    assert_eq!(aligned(&x, 312, 126, &[]), delayed(&x, 438));
}

/// the four controls of the metric, on the lab's own voices without codec or canceller
fn controls(n: &[f32], far: &[f32], fft: &Arc<dyn Fft<f64>>) {
    let c = coupling(-6.0);
    let e: Vec<f32> = (0..far.len()).map(|k| heard(&c, far, k)).collect();
    let mix =
        |a: &[f32], g: f32| -> Vec<f32> { a.iter().zip(&e).map(|(x, y)| x + g * y).collect() };
    let show = |name: &str, s: &Summary| {
        println!("  control {name:<22}{}", cells(s));
    };
    let check = |name: &str, y: &[f32], test: &dyn Fn(&Summary) -> bool| {
        let s = summarise(fit(y, n, &e, fft).iter());
        show(name, &s);
        assert!(test(&s), "control {name} failed");
    };
    let loss = |s: &Summary, b: usize| s.loss[b].unwrap().0;
    let erle = |s: &Summary, b: usize| s.erle[b].unwrap();
    check("identity y = n + e", &mix(n, 1.0), &|s| {
        (0..3).all(|b| loss(s, b).abs() < 0.1 && erle(s, b).abs() < 0.1) && s.dist.unwrap() < -40.0
    });
    check("removal y = n", n, &|s| {
        (0..3).all(|b| loss(s, b).abs() < 0.1 && erle(s, b) > 40.0)
    });
    check("echo -20 dB y = n + 0.1e", &mix(n, 0.1), &|s| {
        (0..3).all(|b| loss(s, b).abs() < 0.1 && (erle(s, b) - 20.0).abs() < 0.2)
    });
    // brick-wall band stop above 1 khz on the near end only
    let size = n.len();
    let mut spec: Vec<Complex<f64>> = n.iter().map(|s| Complex::new(*s as f64, 0.0)).collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(size).process(&mut spec);
    let cut = 1000 * size / RATE;
    for (i, x) in spec.iter_mut().enumerate() {
        if i.min(size - i) > cut {
            *x = Complex::new(0.0, 0.0);
        }
    }
    planner.plan_fft_inverse(size).process(&mut spec);
    let stopped: Vec<f32> = spec.iter().map(|x| (x.re / size as f64) as f32).collect();
    check("band stop > 1 kHz on n", &mix(&stopped, 1.0), &|s| {
        loss(s, 0) < 1.0 && loss(s, 1) > 15.0 && loss(s, 2) > 20.0
    });
}

fn cells(s: &Summary) -> String {
    let loss: Vec<String> = s
        .loss
        .iter()
        .map(|x| match x {
            Some((l, h)) => format!("{l:>6.1} {:>3.0}%", 100.0 * h),
            None => format!("{:>11}", "-"),
        })
        .collect();
    let erle: Vec<String> = s
        .erle
        .iter()
        .map(|x| x.map_or(format!("{:>6}", "-"), |v| format!("{v:>6.1}")))
        .collect();
    let dist = s
        .dist
        .map_or(format!("{:>6}", "-"), |v| format!("{v:>6.1}"));
    format!(
        "{} |{} |{} {:>5.1}%",
        loss.join(""),
        erle.join(""),
        dist,
        100.0 * s.skipped
    )
}

// ---- the fit and summary on synthetic spectra, without voices or codec

/// deterministic complex values, M frames of bins 0..=TOP_BIN
fn noise(seed: u64) -> [Vec<Complex<f64>>; M] {
    let mut s = seed;
    let mut next = move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    };
    std::array::from_fn(|_| {
        (0..=TOP_BIN)
            .map(|_| Complex::new(next(), next()))
            .collect()
    })
}

/// one fitted window of y = gain·n + echo·e
fn synth(
    gain: f64,
    echo: f64,
    n: &[Vec<Complex<f64>>; M],
    e: &[Vec<Complex<f64>>; M],
) -> WindowFit {
    let y: Vec<Vec<Complex<f64>>> = n
        .iter()
        .zip(e)
        .map(|(n, e)| n.iter().zip(e).map(|(n, e)| gain * n + echo * e).collect())
        .collect();
    let mut bands = [Band::default(); 3];
    for (band, (lo, hi)) in bands.iter_mut().zip(BANDS) {
        for k in lo..=hi {
            fit_bin(band, k, &y, n, e, true, true);
        }
    }
    WindowFit {
        centre: 0,
        n_active: true,
        e_active: true,
        bands,
    }
}

fn near_loss(windows: &[WindowFit]) -> [(f64, f64); 3] {
    summarise(windows.iter())
        .loss
        .map(|x| x.expect("near end counted"))
}

#[test]
fn metric_counts_a_fully_cut_window() {
    let (n, e) = (noise(1), noise(2));
    for (l, h) in near_loss(&[synth(1.0, 1.0, &n, &e), synth(0.0, 0.0, &n, &e)]) {
        assert!(
            (l - LOSS_CAP_DB / 2.0).abs() < 1e-6,
            "mixed identity/mute loss {l}"
        );
        assert_eq!(h, 0.5);
    }
    for (l, h) in near_loss(&[synth(0.0, 0.0, &n, &e), synth(0.0, 0.0, &n, &e)]) {
        assert!((l - LOSS_CAP_DB).abs() < 1e-6, "fully muted loss {l}");
        assert_eq!(h, 1.0);
    }
}

#[test]
fn metric_loss_is_a_mean_in_db() {
    let (n, e) = (noise(3), noise(4));
    for (l, h) in near_loss(&[synth(1.0, 1.0, &n, &e), synth(0.1, 1.0, &n, &e)]) {
        assert!((l - 10.0).abs() < 1e-6, "gains 1 and 0.1 loss {l}");
        assert_eq!(h, 0.5);
    }
}

#[test]
fn metric_skips_collinear_regressors() {
    let n = noise(5);
    let scaled = |m: &[Vec<Complex<f64>>; M], k: f64| -> [Vec<Complex<f64>>; M] {
        std::array::from_fn(|f| {
            n[f].iter()
                .zip(&m[f])
                .map(|(n, m)| 3.3 * n + k * m)
                .collect()
        })
    };
    let m = noise(6);
    for (name, e) in [("scaled", scaled(&m, 0.0)), ("near", scaled(&m, 0.01))] {
        let s = summarise([synth(1.0, 1.0, &n, &e)].iter());
        assert!(
            s.loss.iter().all(Option::is_none),
            "{name}: fitted {:?}",
            s.loss
        );
        assert_eq!(s.skipped, 1.0, "{name} collinearity");
    }
    // a correlated but identifiable echo is still fitted
    let s = summarise([synth(1.0, 1.0, &n, &scaled(&m, 3.0))].iter());
    assert!(s.skipped < 0.05, "skipped {}", s.skipped);
    assert!(s.loss.iter().flatten().all(|(l, _)| l.abs() < 1e-6));
}

// ---- own echo: a player's voice coming back to him through another player's mic

/// an echo return below this has no lag worth printing
const ECHO_FLOOR_DB: f64 = -50.0;

/// 1 ms rms envelope
fn envelope(x: &[f32]) -> Vec<f64> {
    x.chunks(ms(1))
        .map(|c| (energy(c) / c.len() as f64).sqrt())
        .collect()
}

/// lag (samples, searched within 100 ms of `nominal`) and level (dB re the own voice)
/// of `back` against `voice` over [from, to)
fn echo_return(
    voice: &[f32],
    back: &[f32],
    from: usize,
    to: usize,
    nominal: usize,
) -> (usize, f64) {
    let ev = envelope(&voice[from..to]);
    let eb = envelope(&back[from..]);
    let span = ev.len();
    let centre = nominal / ms(1);
    let lag = (centre.saturating_sub(100)..centre + 100)
        .max_by(|a, b| {
            let dot =
                |d: usize| -> f64 { ev.iter().zip(&eb[d..d + span]).map(|(x, y)| x * y).sum() };
            dot(*a).total_cmp(&dot(*b))
        })
        .unwrap()
        * ms(1);
    let level = db(energy(&back[from + lag..to + lag]) / energy(&voice[from..to]));
    (lag, level)
}

fn write(path: &Path, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut wav = hound::WavWriter::create(path, spec).unwrap();
    for s in samples {
        wav.write_sample(*s).unwrap();
    }
    wav.finalize().unwrap();
}

#[test]
#[ignore = "long offline multi-party echo lab; run explicitly in release"]
fn multiparty_lab() {
    let model = std::env::var("PTT_LAB_PATH").unwrap_or_else(|_| "game".into());
    PATH_MODEL.set(match model.as_str() {
        "game" => PathModel::Game,
        "direct" => PathModel::Direct,
        "hub" => PathModel::Hub,
        _ => panic!("PTT_LAB_PATH must be game, direct or hub"),
    });
    println!(
        "path model: {model}; direct A-B/A-C/B-C = 15/35/25 ms one way; hub uses each seat's two legs; receive queue unchanged"
    );
    let voice = |var: &str| {
        let path = std::env::var(var).unwrap_or_else(|_| panic!("set {var}"));
        normalise(read_wav(Path::new(&path)).unwrap())
    };
    let voices: Vec<Vec<f32>> = ["PTT_LAB_A", "PTT_LAB_B", "PTT_LAB_C"]
        .iter()
        .map(|v| voice(v))
        .collect();
    let dir = std::env::var("PTT_LAB_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("multiparty_lab"));
    std::fs::create_dir_all(&dir).unwrap();
    let fft = FftPlanner::new().plan_fft_forward(FFT);

    println!(
        "metric controls (loss dB / holes per band | echo reduction per band | dist dB, skipped):"
    );
    controls(&voices[0][..10 * RATE], &voices[1][..10 * RATE], &fft);
    let codec = codec_delay(&voices[0]);
    let delays: Vec<usize> = ROWS
        .iter()
        .map(|(_, make)| canceller_delay(*make))
        .collect();
    assert_eq!(delays, [0, WET_DELAY, 0, 126, 1662, 1662]);
    println!(
        "opus delay {codec} samples; canceller delay: {}",
        ROWS.iter()
            .zip(&delays)
            .map(|((name, _), d)| format!("{name} {d} samples"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    // isolated controls remain measurable even when the uncancelled closed loop howls
    for (name, v) in ["A", "B", "C"].iter().zip(&voices) {
        for (label, make, delay) in [
            ("codec only", ROWS[0].1, 0),
            ("FIR + codec", ROWS[3].1, 126),
        ] {
            let mut link = Link::new();
            let mut canceller = make();
            let v = &v[..v.len() / link.frame * link.frame];
            let decoded: Vec<f32> = v
                .chunks_exact(link.frame)
                .flat_map(|f| {
                    let mut frame = f.to_vec();
                    canceller.near_end(&mut frame);
                    link.send(&frame)
                })
                .collect();
            let reference = delayed(v, codec + delay);
            let fits = fit(&decoded, &reference, &vec![0.0; v.len()], &fft);
            println!(
                "  {:<18}{}",
                format!("{name} {label}"),
                cells(&summarise(fits.iter()))
            );
            let tag = if delay == 0 {
                "codec-only"
            } else {
                "resample-codec"
            };
            write(
                &dir.join(format!("control-{tag}-{name}-decoded.wav")),
                &decoded,
            );
            write(
                &dir.join(format!("control-{tag}-{name}-aligned-clean.wav")),
                &reference,
            );
        }
    }

    for scn in scenarios() {
        let seats = &scn.seats;
        println!("\nscenario {}: {}", scn.name, scn.title);
        for s in seats {
            let couple = s.couple_db.map_or("headphones".into(), |d| {
                format!("speakers, coupling {d:+.0} dB")
            });
            println!("  {}: ping {} ms one way, {couple}", s.name, s.ping_ms);
        }
        let (rho, at) = loop_gain(&scn);
        if rho > 0.0 {
            println!(
                "  loop without a canceller: at most {:+.1} dB per hop at {at:.0} Hz, the worst \
                 frequency (magnitude bound, phase ignored; above 0 dB it may howl)",
                2.0 * db(rho)
            );
        } else {
            println!("  loop without a canceller: open (one coupled seat)");
        }
        for ((row, make), cd) in ROWS.iter().zip(&delays) {
            for warm in [false, true] {
                let start = if warm { "warm" } else { "cold" };
                let run = simulate(&scn, &voices, *make, warm);
                let tag = format!("{}-{row}-{start}", scn.name);
                let available = if run.howl.is_some() {
                    run.blocks.iter().min().unwrap() / 2 * (2 * BLOCK)
                } else {
                    run.voice[0].len()
                };
                let write = |path: &Path, samples: &[f32]| {
                    write(path, &samples[..available.min(samples.len())]);
                };
                for (l, s) in seats.iter().enumerate() {
                    let reference = aligned(&run.voice[l], codec, *cd, &run.gates[l]);
                    write(
                        &dir.join(format!("{tag}-{}-clean.wav", s.name)),
                        &run.voice[l],
                    );
                    write(
                        &dir.join(format!("{tag}-{}-decoded.wav", s.name)),
                        &run.decoded[l],
                    );
                    write(
                        &dir.join(format!("{tag}-{}-aligned-clean.wav", s.name)),
                        &reference,
                    );
                    let intervals: &[(&str, f64, f64)] = if scn.name == "a" {
                        &[("double", 23.0, 30.0), ("triple", 34.0, 41.0)]
                    } else {
                        &[("double", 16.0, 27.0)]
                    };
                    for &(phase, from, to) in intervals {
                        let from = ((from + run.offset) * RATE as f64) as usize;
                        let to = ((to + run.offset) * RATE as f64) as usize;
                        let invalid = dir.join(format!("{tag}-{}-{phase}-invalid.txt", s.name));
                        if to > available {
                            for kind in ["decoded", "aligned-clean"] {
                                let old = dir.join(format!("{tag}-{}-{phase}-{kind}.wav", s.name));
                                if old.exists() {
                                    std::fs::remove_file(old).unwrap();
                                }
                            }
                            std::fs::write(
                                invalid,
                                format!("invalid crop: simulation stopped for howl; only {available} fully processed capture samples ({:.3}s); requested [{from},{to})\n", available as f64 / RATE as f64),
                            ).unwrap();
                            continue;
                        }
                        if invalid.exists() {
                            std::fs::remove_file(invalid).unwrap();
                        }
                        for (kind, samples) in
                            [("decoded", &run.decoded[l]), ("aligned-clean", &reference)]
                        {
                            write(
                                &dir.join(format!("{tag}-{}-{phase}-{kind}.wav", s.name)),
                                &samples[from..to],
                            );
                        }
                    }
                    if !run.gates[l].is_empty() {
                        let mut trace = String::from(
                            "capture_sample,engaged,score,lag_ms,last_transition_sample\n",
                        );
                        use std::fmt::Write;
                        for (i, state) in run.gates[l].iter().enumerate() {
                            writeln!(
                                trace,
                                "{},{},{:.6},{:.3},{}",
                                i * BLOCK,
                                state.engaged,
                                state.score,
                                state.lag_ms,
                                state.capture_sample
                            )
                            .unwrap();
                        }
                        std::fs::write(dir.join(format!("{tag}-{}-gate.csv", s.name)), trace)
                            .unwrap();
                    }
                    write(
                        &dir.join(format!("{tag}-{}-hears.wav", s.name)),
                        &run.speaker[l],
                    );
                    if s.couple_db.is_some() {
                        let mic: Vec<f32> = run.voice[l]
                            .iter()
                            .zip(&run.echo[l])
                            .map(|(v, e)| v + e)
                            .collect();
                        write(&dir.join(format!("{tag}-{}-mic.wav", s.name)), &mic);
                    }
                    if *row == ROWS[0].0 {
                        // what this seat would hear over a perfect path: the others' clean voices
                        let mut ideal = vec![0.0; run.speaker[l].len()];
                        for (p, v) in run.voice.iter().enumerate().filter(|(p, _)| *p != l) {
                            let d = path(&seats[p], s) + AHEAD + codec;
                            for (o, x) in ideal[d..].iter_mut().zip(v) {
                                *o += x;
                            }
                        }
                        write(
                            &dir.join(format!("{}-{start}-{}-ideal.wav", scn.name, s.name)),
                            &ideal,
                        );
                    }
                }
                let decay = tail_decay(&run, &scn);
                let clipped: Vec<String> = seats
                    .iter()
                    .zip(&run.clipped)
                    .map(|(s, c)| format!("{} {c}", s.name))
                    .collect();
                println!(
                    "\n  {row}, {start}: speaker tail decay {} dB/s; clipped mic samples {}",
                    decay
                        .iter()
                        .map(|d| d.map_or("silent".into(), |d| format!("{d:.0}")))
                        .collect::<Vec<_>>()
                        .join("/"),
                    clipped.join(", ")
                );
                println!(
                    "    process CPU ms/player/10ms (far + near, no load/codec/analysis/IO): {}",
                    run.cpu
                        .iter()
                        .zip(&run.blocks)
                        .map(|(cpu, blocks)| format!(
                            "{:.4}",
                            cpu.as_secs_f64() * 1000.0 / (*blocks).max(1) as f64
                        ))
                        .collect::<Vec<_>>()
                        .join("/")
                );
                for (p, states) in run.gates.iter().enumerate().filter(|(_, s)| !s.is_empty()) {
                    let changes = transitions(states)
                        .map(|(i, s)| {
                            format!(
                                "{:.3}s:{} score={:.3} lag={:.1}ms",
                                i as f64 / 100.0 - run.offset,
                                if s.engaged { "wet" } else { "dry" },
                                s.score,
                                s.lag_ms
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "    {} gate: {} wet blocks / {}; transitions [{}]",
                        seats[p].name,
                        states.iter().filter(|s| s.engaged).count(),
                        states.len(),
                        changes
                    );
                }
                if let Some(at) = run.howl {
                    let when = if at < 0.0 {
                        format!("{:.1} s into the warm-up", at + run.offset)
                    } else {
                        format!("{at:.1} s")
                    };
                    println!("    HOWL: the loop ran away at {when}; metrics skipped");
                    continue;
                }
                if decay.iter().flatten().any(|d| *d > -1.0) {
                    println!(
                        "    HOWL: the speakers do not decay after the last turn; metrics skipped"
                    );
                    continue;
                }
                // own echo, in each seat's first turn, which nobody else overlaps
                for (l, s) in seats.iter().enumerate() {
                    let Some(&(_, from, to)) = scn.talk.iter().find(|t| t.0 == l) else {
                        continue;
                    };
                    let (from, to) = (
                        ((from + run.offset) * RATE as f64) as usize,
                        ((to + run.offset) * RATE as f64) as usize,
                    );
                    for (p, via) in seats
                        .iter()
                        .enumerate()
                        .filter(|(p, v)| *p != l && v.couple_db.is_some())
                    {
                        let back = delayed(&run.decoded[p], path(via, s) + AHEAD);
                        let return_delay = if *row == "aec3-gate" { WET_DELAY } else { *cd };
                        let nominal = 2 * (return_delay + codec + AHEAD)
                            + path(s, via)
                            + path(via, s)
                            + ms(2);
                        let (lag, level) = echo_return(&run.voice[l], &back, from, to, nominal);
                        let found = if level < ECHO_FLOOR_DB {
                            format!("below {ECHO_FLOOR_DB} dB")
                        } else {
                            format!("{level:>6.1} dB at {:>4} ms", lag * 1000 / RATE)
                        };
                        println!(
                            "    {} hears own voice via {}: {found} (path {} ms)",
                            s.name,
                            via.name,
                            nominal * 1000 / RATE
                        );
                    }
                }
                println!(
                    "    {:<18}{:>11}{:>11}{:>11} |{:>6}{:>6}{:>6} |{:>6} {:>6}",
                    "talker, phase", "<1k", "1-4k", "4-8k", "<1k", "1-4k", "4-8k", "dist", "skip"
                );
                for (p, s) in seats.iter().enumerate() {
                    let gates = &run.gates[p];
                    let fits = fit(
                        &run.decoded[p],
                        &aligned(&run.voice[p], codec, *cd, gates),
                        &aligned(&run.echo[p], codec, *cd, gates),
                        &fft,
                    );
                    if !gates.is_empty() {
                        println!(
                            "    {} gate alignment excluded {} / {} fit windows ({} near-active); remaining use dry 0/wet {WET_DELAY} samples + codec {codec}",
                            s.name,
                            fits.iter()
                                .filter(|w| !stable_gate_window(w, codec, gates))
                                .count(),
                            fits.len(),
                            fits.iter()
                                .filter(|w| w.n_active && !stable_gate_window(w, codec, gates))
                                .count()
                        );
                    }
                    // phase at the talker's own time: how many talk with him, or far end only;
                    // a window across a turn edge has none
                    let phase_at = |sample: usize| -> Option<usize> {
                        let t = capture_at(sample, codec, *cd, gates)? as f64 / RATE as f64
                            - run.offset;
                        let on: Vec<usize> = scn
                            .talk
                            .iter()
                            .filter(|x| (x.1..x.2).contains(&t))
                            .map(|x| x.0)
                            .collect();
                        match (on.contains(&p), on.len()) {
                            (_, 0) => None,
                            (true, k) => Some(k - 1),
                            (false, _) => Some(3),
                        }
                    };
                    let half = (M * HOP + FFT) / 2;
                    let phase = |w: &WindowFit| {
                        if !stable_gate_window(w, codec, gates) {
                            return None;
                        }
                        let at = phase_at(w.centre.saturating_sub(half));
                        (at == phase_at(w.centre + half)).then_some(at).flatten()
                    };
                    let names = ["alone", "with 1 more", "with 2 more", "far end only"];
                    for (k, name) in names.iter().enumerate() {
                        let summary = summarise(fits.iter().filter(|w| phase(w) == Some(k)));
                        // e.g. a headphone player while the others talk: nothing to measure
                        if summary.loss.iter().all(Option::is_none)
                            && summary.erle.iter().all(Option::is_none)
                        {
                            continue;
                        }
                        let label = format!("{} {name}", s.name);
                        println!("    {label:<18}{}", cells(&summary));
                    }
                }
            }
        }
    }
    println!("\nwavs: {}", dir.display());
}
