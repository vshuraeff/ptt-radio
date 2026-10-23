//! end-to-end scenarios for the whole voice path without Factorio: the
//! unmodified companion binary runs once per simulated player, with virtual
//! audio (--mic-wav, --speaker-wav, coupling) and a per-frame trace, against
//! the relay emulator (src/relay.rs) in this process. everything runs in real
//! time; analysis happens afterwards on the recorded WAVs and traces.
//!
//!   cargo test --test scenarios -- --nocapture          # results tables
//!   PTT_E2E_VOICE=talk.wav cargo test --test scenarios -- --nocapture
//!
//! PTT_E2E_VOICE replaces player A's synthetic voice with any WAV. the files
//! of the last run stay in target/tmp/scenarios/<scenario>/ for listening.
//!
//! assertions pin today's baseline with a margin so a regression fails; they
//! are not targets. the numbers are printed either way.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::UdpSocket;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use ptt_radio_companion::packet::VOICE;
use ptt_radio_companion::relay::{self, Relay, Stall};
use ptt_radio_companion::{percentile, virtual_audio};

const FS: usize = 48_000;
// envelope hop: 1 ms
const HOP: usize = FS / 1000;
const SLICE_SAMPLES: usize = ptt_radio_companion::mix::SLICE_SAMPLES;
const SLICE_MS: f64 = SLICE_SAMPLES as f64 * 1000.0 / FS as f64;
// player A talks, B listens or talks back; the relay numbers players from 1, A is player 1
const A: u16 = 1;
// speaker-to-mic coupling of the "player with speakers": -6 dB, 2 ms of air, a small room
const COUPLE_GAIN_DB: f32 = -6.0;
const COUPLE_DELAY_MS: f64 = 2.0;
// (ms, weight): direct path and a few decaying early reflections
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

// the scenarios measure real-time latency: running two at once would skew both
static SERIAL: Mutex<()> = Mutex::new(());

/// A talks with `codec` (extra companion flags), B listens with the defaults
fn latency_session(name: &str, codec: &[String]) -> Outcome {
    Session {
        name: name.to_string(),
        players: vec![
            Talker::ptt(place(0.8, talk(3.0), 6.0), 3.8).with_args(codec),
            Talker::listener(),
        ],
        stall: None,
        seconds: 6.0,
    }
    .run()
}

/// what the latency scenario reads from one run
struct Latency {
    acoustic: Lag,
    stages: Vec<(String, String)>,
    /// mean of the per-frame trace totals
    total: f64,
    /// jitter stage, mean ms
    jitter: f64,
    first_live: f64,
    /// slices of A that B played, concealed, shed (live ones of them)
    played: usize,
    silence: usize,
    shed: usize,
    shed_live: usize,
    /// B's queue after a pull, median, ms
    depth_ms: f64,
}

fn latency(run: &Outcome) -> Latency {
    let (a, b) = (&run.players[0], &run.players[1]);
    let frames = frame_timings(a, A, &run.relay, b);
    let live: Vec<&FrameTiming> = frames.iter().filter(|f| !f.preroll).collect();
    let acoustic = acoustic_lag(a, b, None);
    let shed = b.trace.shed_from(A);
    Latency {
        stages: stage_table(&live, acoustic.lag_ms),
        acoustic,
        total: mean(live.iter().map(|f| f.total_ms())),
        jitter: mean(live.iter().map(|f| (f.pull_us - f.rx_us) / 1000.0)),
        first_live: live.first().map_or(f64::NAN, |f| f.total_ms()),
        played: b.trace.pulls_from(A).filter(|p| p.seq.is_some()).count(),
        silence: b.trace.pulls_from(A).filter(|p| p.seq.is_none()).count(),
        shed_live: shed
            .iter()
            .filter(|(seq, _)| !a.trace.tx[seq].preroll)
            .count(),
        shed: shed.len(),
        depth_ms: median(live.iter().map(|f| f.queued as f64 * SLICE_MS)),
    }
}

/// frame duration x bitrate x complexity (and FEC once): latency, traffic and how close A's
/// voice at B is to what A said. B keeps the default codec, so every row also checks a
/// receiver configured differently from its sender.
///
///   cargo test --release --test scenarios quality_sweep -- --ignored --nocapture
#[test]
#[ignore = "25 real-time runs, about 4 minutes"]
fn quality_sweep() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings: Vec<(usize, u32, u8, bool)> = Vec::new();
    for frame_ms in [10, 20] {
        for kbit in [12, 16, 24, 32] {
            for complexity in [5, 9, 10] {
                settings.push((frame_ms, kbit, complexity, false));
            }
        }
    }
    settings.push((20, 16, 9, true));
    let mut rows = Vec::new();
    for (frame_ms, kbit, complexity, fec) in settings {
        let name = format!(
            "sweep-{frame_ms}ms-{kbit}k-c{complexity}{}",
            if fec { "-fec" } else { "" }
        );
        let mut codec = vec![
            "--frame-ms".to_string(),
            frame_ms.to_string(),
            "--bitrate".into(),
            (kbit * 1000).to_string(),
            "--complexity".into(),
            complexity.to_string(),
        ];
        if fec {
            codec.push("--fec".into());
        }
        let run = latency_session(&name, &codec);
        let l = latency(&run);
        let (a, b) = (&run.players[0], &run.players[1]);
        let live: Vec<&TxFrame> = a.trace.tx.values().filter(|t| !t.preroll).collect();
        let packets_per_s = 1000.0 / frame_ms as f64;
        // the 5-byte ptt-radio header on top of opus
        let bytes_per_s = mean(live.iter().map(|t| t.bytes as f64 + 5.0)) * packets_per_s;
        let q = quality(a, b, A);
        rows.push(format!(
            "{frame_ms:>5} {kbit:>5} {complexity:>3} {:>4} {:>6.0} {:>6.1} {:>5.0} {:>6.0} {:>6.1} {:>5.2} {:>4}  {}",
            if fec { "on" } else { "off" },
            l.acoustic.lag_ms,
            l.jitter,
            packets_per_s,
            bytes_per_s,
            q.seg_snr_db,
            q.lsd_db,
            l.silence,
            Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join("scenarios")
                .join(&name)
                .join("b-speaker.wav")
                .display()
        ));
    }
    println!(
        "\nquality sweep: A talks with the row's codec, B listens with the defaults\n\
         frame  kbit  cx  fec  m2e,ms jit,ms pkt/s    B/s segSNR   LSD conc  B's speaker wav"
    );
    for row in &rows {
        println!("{row}");
    }
}

#[test]
fn latency_a_talks_b_listens() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let run = latency_session("latency", &[]);
    let (a, b) = (&run.players[0], &run.players[1]);
    let press = a.trace.first_ptt_us();
    assert!(
        press < a.trace.device_us + 800_000,
        "PTT pressed after the voice started"
    );
    let l = latency(&run);
    print_table(
        "latency: A talks (PTT), B listens",
        &[
            l.stages.as_slice(),
            &[
                row("mouth-to-ear (acoustic xcorr)", fmt_ms(l.acoustic.lag_ms)),
                row("  xcorr peak", format!("{:.2}", l.acoustic.peak)),
                row("first live frame, total", fmt_ms(l.first_live)),
                row("B jitter queue after pull (median)", fmt_ms(l.depth_ms)),
                row(
                    "slices A sent / B played / shed (live of them)",
                    format!(
                        "{} / {} / {} ({})",
                        a.trace.sent_slices(),
                        l.played,
                        l.shed,
                        l.shed_live
                    ),
                ),
                row(
                    "slices dropped / concealed at B",
                    format!("{} / {}", b.trace.stats.dropped, l.silence),
                ),
                row("relay delay p50 / p95", relay_delay(&run.relay)),
            ],
        ]
        .concat(),
    );

    // the acoustic measure and the frame trace must tell the same story
    let acoustic = &l.acoustic;
    assert!(
        acoustic.peak > 0.5,
        "voice not found at B: {}",
        acoustic.peak
    );
    // the delta is the codec (opus 6.5 ms lookahead), the 1 ms envelope hop and the talker's
    // echo canceller delay: measured 3.8-7.0 ms without a canceller, so 10 ms leaves 3 ms for load
    let expected = a.trace.canceller_delay_ms;
    let total = l.total;
    assert!(
        (acoustic.lag_ms - total - expected).abs() < 10.0,
        "acoustic {} ms vs frames {total:.1} ms + canceller {expected} ms",
        acoustic.lag_ms
    );
    // complete: every slice A sent was played or shed as backlog, nothing dropped. with no
    // margin kept until an underrun, a loaded machine can starve one pull (seen once in a debug
    // build beside parallel test runs, never in 5 release runs); the decoder conceals it and
    // the margin then holds, so one is allowed
    assert_eq!((l.played + l.shed) as u64, a.trace.sent_slices());
    assert_eq!(b.trace.stats.dropped, 0);
    assert!(l.silence <= 1, "{} underruns at B", l.silence);
    // the pre-roll burst (120 ms) is shed: what stays after a pull is the margin plus what the
    // tick batching needs between deliveries, 10-30 ms by median over 10 ms pulls (2026-10-24)
    assert!(
        l.depth_ms <= 30.0,
        "start backlog not drained: {} ms queued",
        l.depth_ms
    );
    // 2026-10-24: 240-262 ms before the backlog was shed, 152-153 ms after (3 runs) without a
    // canceller. margin: one relay tick and a few ms of load
    assert!(
        acoustic.lag_ms < 175.0 + expected,
        "mouth-to-ear regressed: {} ms (canceller {expected} ms)",
        acoustic.lag_ms
    );
}

#[test]
fn echo_return_b_has_speakers_and_open_mic() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let run = Session {
        name: "echo".into(),
        players: vec![
            Talker::ptt(place(0.8, talk(3.0), 6.0), 3.8),
            Talker::open_mic(Vec::new(), true),
        ],
        stall: None,
        seconds: 6.0,
    }
    .run();
    let (a, b) = (&run.players[0], &run.players[1]);
    // A's echo setting is off: anything A hears is its own voice coming back through B
    let back = acoustic_lag(a, a, None);
    let echo_return = energy_db(&a.speaker) - energy_db(&a.voice);
    let coupling = coupling_db();
    print_table(
        "echo: A talks, B has speakers (coupled) and open mic",
        &[
            row("canceller at B", b.trace.canceller.clone()),
            row("coupling at B (gain + room)", format!("{coupling:.1} dB")),
            row(
                "A's voice back at A (echo return)",
                format!("{echo_return:.1} dB"),
            ),
            row("echo path A -> B -> A", fmt_ms(back.lag_ms)),
            row("  xcorr peak", format!("{:.2}", back.peak)),
            row(
                "B sent while A talked",
                format!("{} frames", b.trace.stats.sent),
            ),
        ],
    );
    assert_eq!(b.trace.canceller, "none");
    // without a canceller the echo comes back at about the coupling level, codec and limiter
    // included; a canceller only has to push this down, so only the upper bound is pinned
    assert!(
        echo_return < coupling + 3.0,
        "echo return {echo_return:.1} dB above the coupling {coupling:.1} dB"
    );
    assert!(back.peak > 0.5 && back.lag_ms > 300.0 && back.lag_ms < 700.0);
}

#[test]
fn double_talk_a_and_b_at_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let run = Session {
        name: "double-talk".into(),
        players: vec![
            Talker::ptt(place(0.8, talk(3.5), 6.0), 4.3),
            Talker::open_mic(place(0.9, speech(2, 210.0, 3.4), 6.0), true),
        ],
        stall: None,
        seconds: 6.0,
    }
    .run();
    let (a, b) = (&run.players[0], &run.players[1]);
    // A's output holds B's voice plus A's echo; B's output holds A's voice
    let b_at_a = acoustic_lag(b, a, None);
    let echo_at_a = acoustic_lag(a, a, None);
    let a_at_b = acoustic_lag(a, b, None);
    let (b_gain, echo_gain) = fit_two(a, (b, b_at_a.lag_ms), (a, echo_at_a.lag_ms));
    // B's echo setting is off, so B hears nothing but A
    let a_gain = energy_db(&b.speaker) - energy_db(&a.voice);
    print_table(
        "double talk: A and B talk at once, B coupled",
        &[
            row("canceller at B", b.trace.canceller.clone()),
            row(
                "B's voice at A",
                format!("{b_gain:.1} dB (lag {})", fmt_ms(b_at_a.lag_ms)),
            ),
            row(
                "A's echo at A",
                format!("{echo_gain:.1} dB (lag {})", fmt_ms(echo_at_a.lag_ms)),
            ),
            row(
                "A's voice at B",
                format!("{a_gain:.1} dB (lag {})", fmt_ms(a_at_b.lag_ms)),
            ),
            row(
                "coupling at B (gain + room)",
                format!("{:.1} dB", coupling_db()),
            ),
        ],
    );
    // both voices must come through at about their own level; a canceller that clips
    // B during double talk shows up as a drop of "B's voice at A"
    assert!(b_gain > -4.0, "B's voice lost at A: {b_gain:.1} dB");
    assert!(a_gain > -4.0, "A's voice lost at B: {a_gain:.1} dB");
    assert!(echo_gain < coupling_db() + 3.0);
}

#[test]
fn stall_burst_recovers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stall = Stall {
        at: Duration::from_millis(4000),
        len: Duration::from_millis(500),
        every: None,
    };
    let run = Session {
        name: "stall".into(),
        players: vec![
            Talker::ptt(place(0.8, talk(7.0), 10.0), 7.8),
            Talker::listener(),
        ],
        stall: Some(stall),
        seconds: 10.0,
    }
    .run();
    let (a, b) = (&run.players[0], &run.players[1]);
    let &(stall_start, stall_end) = run.relay.stalls.first().expect("the stall happened");
    let frames = frame_timings(a, A, &run.relay, b);
    let live: Vec<&FrameTiming> = frames.iter().filter(|f| !f.preroll).collect();
    let window = |from: f64, to: f64| {
        live.iter()
            .filter(move |f| f.mic_us >= from && f.mic_us < to)
            .map(|f| f.total_ms())
    };
    let before = mean(window(0.0, stall_start as f64 - 300_000.0));
    let after = mean(window(stall_end as f64 + 1_000_000.0, f64::MAX));
    let peak = live.iter().map(|f| f.total_ms()).fold(0.0, f64::max);
    let depth = |from: f64, to: f64| {
        median(
            live.iter()
                .filter(|f| f.mic_us >= from && f.mic_us < to)
                .map(|f| f.queued as f64 * SLICE_MS),
        )
    };
    let depth_before = depth(0.0, stall_start as f64 - 300_000.0);
    let depth_after = depth(stall_end as f64 + 1_000_000.0, f64::MAX);
    // settled: from this point after the stall every frame plays within 10 ms of `after`
    let settled = live
        .iter()
        .filter(|f| f.play_us > stall_end as f64 && (f.total_ms() - after).abs() > 10.0)
        .map(|f| (f.play_us - stall_end as f64 + f.frame_us) / 1000.0)
        .fold(0.0, f64::max);
    let acoustic_before = acoustic_lag(a, b, Some((0, stall_start)));
    let acoustic_after = acoustic_lag(a, b, Some((stall_end + 1_000_000, u64::MAX)));
    let played = b.trace.pulls_from(A).filter(|p| p.seq.is_some()).count() as u64;
    let silence = b.trace.pulls_from(A).filter(|p| p.seq.is_none()).count();
    print_table(
        "stall: relay stops 500 ms mid-utterance, then bursts",
        &[
            row(
                "stall",
                format!(
                    "{} ms, {:.1} s into the run",
                    (stall_end - stall_start) / 1000,
                    (stall_start - a.trace.device_us) as f64 / 1e6
                ),
            ),
            row(
                "latency before (frames / acoustic)",
                format!("{} / {}", fmt_ms(before), fmt_ms(acoustic_before.lag_ms)),
            ),
            row("latency peak of played frames", fmt_ms(peak)),
            row(
                "latency after (frames / acoustic)",
                format!("{} / {}", fmt_ms(after), fmt_ms(acoustic_after.lag_ms)),
            ),
            row(
                "steady (within 10 ms) after stall end",
                if settled == 0.0 {
                    "from the first frame".into()
                } else {
                    fmt_ms(settled)
                },
            ),
            row(
                "B jitter queue before / after",
                format!("{} / {}", fmt_ms(depth_before), fmt_ms(depth_after)),
            ),
            row(
                "slices sent / played / dropped / concealed",
                format!(
                    "{} / {played} / {} / {silence}",
                    a.trace.sent_slices(),
                    b.trace.stats.dropped
                ),
            ),
        ],
    );
    // the stall starves B (silence) and the burst is trimmed, not played back to back
    let shed = b.trace.shed_from(A).len() as u64;
    assert!(silence > 0, "the stall did not reach B");
    assert!(
        b.trace.stats.dropped + shed > 0,
        "the burst was played instead of trimmed"
    );
    // nothing lost besides what the jitter buffer dropped or shed on purpose
    assert_eq!(played + b.trace.stats.dropped + shed, a.trace.sent_slices());
    // the burst after the stall is shed back to the depth before it; the underrun keeps 20 ms
    // of margin for a while, so allow that. 2026-10-24 with 20 ms slices: 0 / 0 frames queued,
    // 145 ms before and 142-143 ms after (3 runs); without shedding the trim left 2..10 frames
    // and up to 100 ms extra for the rest of the utterance
    assert!(
        depth_after <= depth_before + 20.0,
        "stall backlog not drained: {depth_before:.0} -> {depth_after:.0} ms"
    );
    assert!(
        after <= before + 20.0,
        "latency after the stall {after:.0} ms vs before {before:.0} ms"
    );
    assert!(
        settled < 1000.0,
        "latency still moving {settled:.0} ms after the stall"
    );
}

// ---- the session: relay in this process, one companion process per player

struct Talker {
    voice: Vec<f32>,
    auto_ptt: Option<f64>,
    open_mic: bool,
    coupled: bool,
    /// extra companion flags, e.g. the codec settings
    args: Vec<String>,
}

impl Talker {
    fn ptt(voice: Vec<f32>, hold_seconds: f64) -> Self {
        Talker {
            voice,
            auto_ptt: Some(hold_seconds),
            open_mic: false,
            coupled: false,
            args: Vec::new(),
        }
    }

    fn listener() -> Self {
        Talker {
            voice: Vec::new(),
            auto_ptt: None,
            open_mic: false,
            coupled: false,
            args: Vec::new(),
        }
    }

    fn open_mic(voice: Vec<f32>, coupled: bool) -> Self {
        Talker {
            voice,
            auto_ptt: None,
            open_mic: true,
            coupled,
            args: Vec::new(),
        }
    }

    fn with_args(mut self, args: &[String]) -> Self {
        self.args = args.to_vec();
        self
    }
}

struct Session {
    name: String,
    players: Vec<Talker>,
    stall: Option<Stall>,
    seconds: f64,
}

struct PlayerRun {
    /// near-end voice on this player's device timeline
    voice: Vec<f32>,
    /// everything this player's speaker played
    speaker: Vec<f32>,
    trace: Trace,
}

struct Outcome {
    players: Vec<PlayerRun>,
    relay: relay::Report,
}

impl Session {
    fn run(self) -> Outcome {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("scenarios")
            .join(&self.name);
        // the previous run's files, if any; create_dir_all below reports real trouble
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let ir = dir.join("room-ir.wav");
        write_wav(&ir, &room_ir());

        let companion_ports: Vec<u16> = self.players.iter().map(|_| free_port()).collect();
        let relay = Relay::start(
            relay::Config {
                stall: self.stall,
                ..relay::Config::default()
            },
            self.players
                .iter()
                .zip(&companion_ports)
                .enumerate()
                .map(|(i, (t, &port))| {
                    let player = relay::Player {
                        name: format!("{}", (b'A' + i as u8) as char),
                        companion_port: port,
                        echo: false,
                        open_mic: t.open_mic,
                    };
                    (0, player)
                })
                .collect(),
        )
        .unwrap();

        let mut children = Vec::new();
        for (i, talker) in self.players.iter().enumerate() {
            let name = (b'a' + i as u8) as char;
            let mic = dir.join(format!("{name}-mic.wav"));
            write_wav(&mic, &talker.voice);
            let log = fs::File::create(dir.join(format!("{name}.log"))).unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_ptt-radio-companion"));
            cmd.arg("--game-port")
                .arg(relay.game_port(i as u16 + 1).to_string())
                .arg("--port")
                .arg(companion_ports[i].to_string())
                .args(["--ptt-key", "none", "--mic-wav"])
                .arg(&mic)
                .arg("--speaker-wav")
                .arg(dir.join(format!("{name}-speaker.wav")))
                .arg("--trace")
                .arg(dir.join(format!("{name}.trace")))
                .arg("--exit-after")
                .arg(self.seconds.to_string())
                .stdout(log.try_clone().unwrap())
                .stderr(log);
            if let Some(hold) = talker.auto_ptt {
                cmd.arg("--auto-ptt").arg(hold.to_string());
            }
            cmd.args(&talker.args);
            if talker.coupled {
                cmd.arg("--couple-gain-db")
                    .arg(COUPLE_GAIN_DB.to_string())
                    .arg("--couple-delay-ms")
                    .arg(COUPLE_DELAY_MS.to_string())
                    .arg("--couple-ir")
                    .arg(&ir);
            }
            children.push(cmd.spawn().expect("companion binary"));
        }
        let deadline = Instant::now() + Duration::from_secs_f64(self.seconds + 15.0);
        for (i, child) in children.iter_mut().enumerate() {
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() > deadline {
                    // best effort before failing the test: a child that already exited cannot be killed
                    let _ = child.kill();
                    panic!("companion {i} did not exit, see {}", dir.display());
                }
                thread::sleep(Duration::from_millis(50));
            };
            assert!(
                status.success(),
                "companion {i}: {status}, see {}",
                dir.display()
            );
        }
        let relay = relay.stop();
        let players = self
            .players
            .into_iter()
            .enumerate()
            .map(|(i, talker)| {
                let name = (b'a' + i as u8) as char;
                PlayerRun {
                    voice: talker.voice,
                    speaker: virtual_audio::read_wav(&dir.join(format!("{name}-speaker.wav")))
                        .unwrap(),
                    trace: Trace::read(&dir.join(format!("{name}.trace"))),
                }
            })
            .collect();
        println!("files: {}", dir.display());
        Outcome { players, relay }
    }
}

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .unwrap()
        .port()
}

fn write_wav(path: &Path, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: FS as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for &s in samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
}

fn room_ir() -> Vec<f32> {
    let mut ir = vec![0.0f32; FS / 20];
    for (ms, w) in ROOM {
        ir[(ms * FS as f64 / 1000.0).round() as usize] = w;
    }
    ir
}

/// coupling level from the speaker to the mic, gain and room taps together
fn coupling_db() -> f64 {
    let room: f64 = ROOM.iter().map(|(_, w)| (*w as f64).powi(2)).sum();
    COUPLE_GAIN_DB as f64 + 10.0 * room.log10()
}

// ---- the companion's trace (see Shared::trace in src/main.rs)

#[derive(Default)]
struct Stats {
    sent: u64,
    dropped: u64,
}

struct TxFrame {
    frame: u64,
    preroll: bool,
    ready_us: u64,
    sent_us: u64,
    /// opus bytes
    bytes: usize,
}

struct PullEvent {
    at_us: u64,
    sender: u16,
    seq: Option<u16>,
    /// sample offset of the pulled slice in its packet
    offset: usize,
    /// slices still queued
    queued: usize,
    /// index on the device timeline at which the pulled slice's first sample sounds
    slot: u64,
}

#[derive(Default)]
struct Trace {
    /// wall clock of sample 0 of the virtual device timeline
    device_us: u64,
    canceller: String,
    canceller_delay_ms: f64,
    /// this companion's own encoder: samples per frame, bitrate, complexity, fec
    frame_samples: usize,
    ptt: Vec<(u64, bool)>,
    tx: HashMap<u16, TxFrame>,
    rx: HashMap<(u16, u16), u64>,
    pulls: Vec<PullEvent>,
    /// (sender, seq, offset) slices skipped by the receiver to cut a backlog
    shed: HashSet<(u16, u16, usize)>,
    stats: Stats,
}

impl Trace {
    fn read(path: &Path) -> Trace {
        let text = fs::read_to_string(path).unwrap();
        let mut trace = Trace::default();
        for line in text.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            let num = |i: usize| -> u64 { f[i].parse().unwrap() };
            match f[1] {
                "device" => trace.device_us = num(2),
                "canceller" => {
                    trace.canceller = f[2].to_string();
                    // optional: the canceller's algorithmic delay, invisible to the frame trace
                    trace.canceller_delay_ms = f.get(3).map_or(0.0, |d| d.parse().unwrap());
                }
                "codec" => trace.frame_samples = num(2) as usize,
                "ptt" => trace.ptt.push((num(0), f[2] == "1")),
                "tx" => {
                    trace.tx.insert(
                        num(2) as u16,
                        TxFrame {
                            frame: num(3),
                            preroll: f[4] == "1",
                            ready_us: num(5),
                            sent_us: num(0),
                            bytes: num(6) as usize,
                        },
                    );
                }
                "rx" => {
                    trace.rx.insert((num(2) as u16, num(3) as u16), num(0));
                }
                "shed" => {
                    trace
                        .shed
                        .insert((num(2) as u16, num(3) as u16, num(4) as usize));
                }
                "pull" => trace.pulls.push(PullEvent {
                    at_us: num(0),
                    sender: num(2) as u16,
                    seq: f[3].parse().ok(),
                    offset: num(4) as usize,
                    queued: num(5) as usize,
                    slot: num(6),
                }),
                "stats" => {
                    trace.stats = Stats {
                        sent: num(2),
                        dropped: num(5),
                    }
                }
                other => panic!("unknown trace event {other} in {}", path.display()),
            }
        }
        trace
    }

    fn first_ptt_us(&self) -> u64 {
        self.ptt
            .iter()
            .find(|(_, held)| *held)
            .expect("PTT pressed")
            .0
    }

    /// (seq, offset) of the slices of `sender` this listener shed
    fn shed_from(&self, sender: u16) -> Vec<(u16, usize)> {
        let mut slices: Vec<(u16, usize)> = self
            .shed
            .iter()
            .filter(|(s, _, _)| *s == sender)
            .map(|&(_, seq, offset)| (seq, offset))
            .collect();
        slices.sort_unstable();
        slices
    }

    fn pulls_from(&self, sender: u16) -> impl Iterator<Item = &PullEvent> {
        self.pulls.iter().filter(move |p| p.sender == sender)
    }

    /// when the first sample of a pulled slice sounds: sample k of the device timeline sounds at
    /// `device_us + k / FS`, and the companion traced the slice's k under the queue lock
    fn play_us(&self, pull: &PullEvent) -> f64 {
        self.device_us as f64 + pull.slot as f64 * 1e6 / FS as f64
    }

    fn frame_us(&self) -> f64 {
        self.frame_samples as f64 * 1e6 / FS as f64
    }

    /// slices in everything this companion sent
    fn sent_slices(&self) -> u64 {
        self.stats.sent * (self.frame_samples / SLICE_SAMPLES) as u64
    }
}

// ---- per-frame timing of one sender's frames at one listener

struct FrameTiming {
    preroll: bool,
    /// the talker's frame duration
    frame_us: f64,
    /// first sample of the frame spoken (talker's device timeline)
    mic_us: f64,
    ready_us: f64,
    sent_us: f64,
    relay_in_us: f64,
    relay_out_us: f64,
    rx_us: f64,
    pull_us: f64,
    play_us: f64,
    queued: usize,
}

impl FrameTiming {
    fn total_ms(&self) -> f64 {
        (self.play_us - self.mic_us) / 1000.0
    }
}

/// frames of `talker` (player `sender`) whose first slice `listener` played, in order
fn frame_timings(
    talker: &PlayerRun,
    sender: u16,
    relay: &relay::Report,
    listener: &PlayerRun,
) -> Vec<FrameTiming> {
    let relayed: HashMap<u16, &relay::Delivery> = relay
        .deliveries
        .iter()
        .filter(|d| d.sender == sender && d.kind == VOICE)
        .map(|d| (d.seq, d))
        .collect();
    listener
        .trace
        .pulls_from(sender)
        .filter(|pull| pull.offset == 0)
        .filter_map(|pull| {
            let seq = pull.seq?;
            let tx = &talker.trace.tx[&seq];
            let d = relayed[&seq];
            let frame_us = talker.trace.frame_us();
            Some(FrameTiming {
                preroll: tx.preroll,
                frame_us,
                mic_us: talker.trace.device_us as f64 + tx.frame as f64 * frame_us,
                ready_us: tx.ready_us as f64,
                sent_us: tx.sent_us as f64,
                relay_in_us: d.received_us as f64,
                relay_out_us: d.delivered_us as f64,
                rx_us: listener.trace.rx[&(sender, seq)] as f64,
                pull_us: pull.at_us as f64,
                play_us: listener.trace.play_us(pull),
                queued: pull.queued,
            })
        })
        .collect()
}

/// mean and p95 per stage over the live frames; the means add up to the frame total,
/// and the acoustic lag minus that total is what the codec adds
fn stage_table(live: &[&FrameTiming], acoustic_ms: f64) -> Vec<(String, String)> {
    let stage = |name: &str, f: &dyn Fn(&FrameTiming) -> f64| {
        let mut v: Vec<f64> = live.iter().map(|t| f(t) / 1000.0).collect();
        v.sort_by(f64::total_cmp);
        let m = v.iter().sum::<f64>() / v.len() as f64;
        row(name, format!("{m:6.1}  p95 {:6.1}", percentile(&v, 95)))
    };
    let total = mean(live.iter().map(|f| f.total_ms()));
    vec![
        row("stage (live frames)", "mean ms      ms".into()),
        stage("capture buffering", &|t| t.ready_us - t.mic_us - t.frame_us),
        stage("framing (one frame)", &|t| t.frame_us),
        stage("encode + send", &|t| t.sent_us - t.ready_us),
        stage("udp loopback (both legs)", &|t| {
            (t.relay_in_us - t.sent_us) + (t.rx_us - t.relay_out_us)
        }),
        stage("relay (game + server model)", &|t| {
            t.relay_out_us - t.relay_in_us
        }),
        stage("jitter buffer", &|t| t.pull_us - t.rx_us),
        stage("output queue + device", &|t| t.play_us - t.pull_us),
        stage("total, frame first sample", &|t| t.play_us - t.mic_us),
        row(
            "codec delay (acoustic - total)",
            format!("{:6.1}", acoustic_ms - total),
        ),
    ]
}

fn relay_delay(report: &relay::Report) -> String {
    let mut lag: Vec<f64> = report
        .deliveries
        .iter()
        .map(|d| (d.delivered_us - d.received_us) as f64 / 1000.0)
        .collect();
    lag.sort_by(f64::total_cmp);
    format!(
        "{:.1} / {:.1} ms",
        percentile(&lag, 50),
        percentile(&lag, 95)
    )
}

// ---- acoustic measures on 1 ms rms envelopes of a common wall-clock timeline

/// rms per 1 ms hop of `samples` (sample 0 at `start_us`), hop 0 at `origin_us`
fn envelope(samples: &[f32], start_us: u64, origin_us: u64, hops: usize) -> Vec<f32> {
    (0..hops)
        .map(|h| {
            let at = origin_us as i64 + h as i64 * 1000 - start_us as i64;
            let first = at * FS as i64 / 1_000_000;
            if first < 0 || first as usize + HOP > samples.len() {
                return 0.0;
            }
            let w = &samples[first as usize..first as usize + HOP];
            (w.iter().map(|s| s * s).sum::<f32>() / HOP as f32).sqrt()
        })
        .collect()
}

struct Lag {
    lag_ms: f64,
    peak: f64,
}

/// how much later `from`'s near-end voice shows up in `to`'s speaker output, by normalized
/// cross-correlation of envelopes; `window` limits the voice to a wall-clock span
fn acoustic_lag(from: &PlayerRun, to: &PlayerRun, window: Option<(u64, u64)>) -> Lag {
    let origin = from.trace.device_us;
    let hops = to.speaker.len() / HOP;
    let mut x = envelope(&from.voice, origin, origin, hops);
    if let Some((start, end)) = window {
        for (h, v) in x.iter_mut().enumerate() {
            let at = origin + h as u64 * 1000;
            if at < start || at >= end {
                *v = 0.0;
            }
        }
    }
    let y = envelope(&to.speaker, to.trace.device_us, origin, hops);
    let center = |v: &[f32]| {
        let m = v.iter().sum::<f32>() / v.len() as f32;
        v.iter().map(|s| (s - m) as f64).collect::<Vec<f64>>()
    };
    let (x, y) = (center(&x), center(&y));
    let norm = |v: &[f64]| v.iter().map(|s| s * s).sum::<f64>().sqrt();
    let (nx, ny) = (norm(&x), norm(&y));
    let mut best = Lag {
        lag_ms: f64::NAN,
        peak: 0.0,
    };
    for lag in 0..1500.min(hops) {
        let c: f64 = x[..hops - lag]
            .iter()
            .zip(&y[lag..])
            .map(|(a, b)| a * b)
            .sum();
        let c = c / (nx * ny);
        if c > best.peak {
            best = Lag {
                lag_ms: lag as f64,
                peak: c,
            };
        }
    }
    best
}

struct Quality {
    /// mean per-slice SNR of the received voice against the source, clamped to -10..35 dB
    seg_snr_db: f64,
    /// mean log-spectral distance up to 8 kHz (opus wideband), dB; 0 is identical
    lsd_db: f64,
}

/// how close `talker`'s voice at `listener` is to what the talker said, slice by slice: the
/// traces say which capture samples each played slice holds and where it sounds, so shed
/// slices and concealment do not misalign the rest. the codec delay (constant) is found by
/// cross-correlation, the received side is scaled by the least-squares gain, and only slices
/// whose source is within 40 dB of the loudest sample count
fn quality(talker: &PlayerRun, listener: &PlayerRun, sender: u16) -> Quality {
    const N: usize = SLICE_SAMPLES;
    let (source, received) = (&talker.voice, &listener.speaker);
    let size = talker.trace.frame_samples;
    let peak = source.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    // (first source sample, first received sample) of each active played slice
    let active: Vec<(usize, usize)> = listener
        .trace
        .pulls_from(sender)
        .filter_map(|p| {
            let tx = talker.trace.tx.get(&p.seq?)?;
            let s = tx.frame as usize * size + p.offset;
            let w = source.get(s..s + N)?;
            let loud = (w.iter().map(|x| x * x).sum::<f32>() / N as f32).sqrt() > peak * 0.01;
            loud.then_some((s, p.slot as usize))
        })
        .collect();
    // received sample r + i carries source sample s + i - delay
    let src = |s: usize, i: usize, delay: usize| -> f64 {
        (s + i).checked_sub(delay).map_or(0.0, |k| source[k] as f64)
    };
    let rcv = |r: usize, i: usize| -> f64 { received.get(r + i).map_or(0.0, |x| *x as f64) };
    let dot = |delay: usize| -> f64 {
        active
            .iter()
            .flat_map(|&(s, r)| (0..N).map(move |i| src(s, i, delay) * rcv(r, i)))
            .sum()
    };
    // opus lookahead is 312 samples at 48 kHz; search around it
    let delay = (200..=500)
        .max_by(|a, b| dot(*a).total_cmp(&dot(*b)))
        .unwrap();
    let rr: f64 = active
        .iter()
        .flat_map(|&(_, r)| (0..N).map(move |i| rcv(r, i).powi(2)))
        .sum();
    let gain = dot(delay) / rr.max(1e-12);
    // 100 Hz bins up to 8 kHz (opus wideband), hann window
    let bins = 80;
    let hann: Vec<f64> = (0..N)
        .map(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / N as f64).cos())
        .collect();
    let power = |x: &dyn Fn(usize) -> f64| -> Vec<f64> {
        (1..=bins)
            .map(|b| {
                let w = std::f64::consts::TAU * b as f64 / N as f64;
                let (mut re, mut im) = (0.0, 0.0);
                for (n, h) in hann.iter().enumerate() {
                    let v = x(n) * h;
                    re += v * (w * n as f64).cos();
                    im -= v * (w * n as f64).sin();
                }
                re * re + im * im
            })
            .collect()
    };
    let (mut snr, mut lsd) = (0.0, 0.0);
    for &(s, r) in &active {
        let src = |n: usize| src(s, n, delay);
        let rcv = |n: usize| rcv(r, n) * gain;
        let signal: f64 = (0..N).map(|n| src(n).powi(2)).sum();
        let noise: f64 = (0..N).map(|n| (src(n) - rcv(n)).powi(2)).sum();
        snr += (10.0 * (signal / noise.max(1e-12)).log10()).clamp(-10.0, 35.0);
        let (ps, pr) = (power(&src), power(&rcv));
        // bins more than 60 dB below the frame's strongest are floored there;
        // the delayed source slice can be silent, so both spectra set the peak
        let floor = ps.iter().chain(&pr).fold(1e-12f64, |m, p| m.max(*p)) * 1e-6;
        let d: f64 = ps
            .iter()
            .zip(&pr)
            .map(|(s, r)| (10.0 * (s.max(floor) / r.max(floor)).log10()).powi(2))
            .sum();
        lsd += (d / bins as f64).sqrt();
    }
    let frames = active.len().max(1) as f64;
    Quality {
        seg_snr_db: snr / frames,
        lsd_db: lsd / frames,
    }
}

fn energy_db(samples: &[f32]) -> f64 {
    10.0 * samples
        .iter()
        .map(|s| (*s as f64).powi(2))
        .sum::<f64>()
        .log10()
}

/// least squares of `out`'s speaker power on two lagged near-end voices, in 10 ms windows;
/// returns each voice's power gain in dB (0 dB: arrives at its own level)
fn fit_two(out: &PlayerRun, one: (&PlayerRun, f64), two: (&PlayerRun, f64)) -> (f64, f64) {
    let origin = out.trace.device_us;
    let hops = out.speaker.len() / HOP;
    let windows = |env: Vec<f32>| -> Vec<f64> {
        env.chunks(10)
            .map(|w| w.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / w.len() as f64)
            .collect()
    };
    let y = windows(envelope(&out.speaker, origin, origin, hops));
    let lagged = |(p, lag_ms): (&PlayerRun, f64)| {
        // the voice as heard at the output: shifted later by the measured lag
        windows(envelope(
            &p.voice,
            p.trace.device_us,
            origin - (lag_ms * 1000.0) as u64,
            hops,
        ))
    };
    let (r1, r2) = (lagged(one), lagged(two));
    let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let (a11, a12, a22) = (dot(&r1, &r1), dot(&r1, &r2), dot(&r2, &r2));
    let (b1, b2) = (dot(&r1, &y), dot(&r2, &y));
    let det = a11 * a22 - a12 * a12;
    let (g1, g2) = ((b1 * a22 - b2 * a12) / det, (a11 * b2 - a12 * b1) / det);
    (10.0 * g1.log10(), 10.0 * g2.log10())
}

// ---- test signal

/// speech-like test signal: voiced syllables (harmonics of a gliding f0 under two
/// formant peaks) and some unvoiced (noise) ones, with syllable gaps and phrase
/// pauses; deterministic per seed, peak 0.35
fn speech(seed: u64, f0: f64, seconds: f64) -> Vec<f32> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let n = (seconds * FS as f64) as usize;
    let mut out = vec![0.0f32; n];
    let mut at = 0;
    let mut phrase_left = 0;
    while at < n {
        let len = (rng.range(0.09, 0.26) * FS as f64) as usize;
        let voiced = rng.unit() < 0.8;
        let (f_from, f_to) = (f0 * rng.range(0.9, 1.15), f0 * rng.range(0.85, 1.1));
        let (f1, f2) = (rng.range(300.0, 800.0), rng.range(900.0, 2300.0));
        let formant = |f: f64, center: f64| 1.0 / (1.0 + ((f - center) / 120.0).powi(2));
        let mut phase = [0.0f64; 64];
        for i in 0..len.min(n - at) {
            let attack = (i as f64 / (0.015 * FS as f64)).min(1.0);
            let release = ((len - i) as f64 / (0.03 * FS as f64)).min(1.0);
            let env = (attack * std::f64::consts::FRAC_PI_2).sin().powi(2)
                * (release * std::f64::consts::FRAC_PI_2).sin().powi(2);
            let s = if voiced {
                let f = f_from + (f_to - f_from) * i as f64 / len as f64;
                let mut s = 0.0;
                for (k, p) in phase.iter_mut().enumerate().skip(1) {
                    let fk = k as f64 * f;
                    if fk > 4000.0 {
                        break;
                    }
                    *p += std::f64::consts::TAU * fk / FS as f64;
                    s += (formant(fk, f1) + 0.7 * formant(fk, f2) + 0.03) * p.sin();
                }
                s
            } else {
                (rng.unit() * 2.0 - 1.0) * 0.4
            };
            out[at + i] = (env * s) as f32;
        }
        at += len;
        let gap = if phrase_left == 0 {
            phrase_left = rng.range(3.0, 6.0) as u32;
            rng.range(0.25, 0.6)
        } else {
            phrase_left -= 1;
            rng.range(0.03, 0.12)
        };
        at += (gap * FS as f64) as usize;
    }
    let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    out.iter_mut().for_each(|s| *s *= 0.35 / peak);
    out
}

/// player A's voice: the synthetic signal, or the WAV in PTT_E2E_VOICE cut to length
fn talk(seconds: f64) -> Vec<f32> {
    let n = (seconds * FS as f64) as usize;
    match std::env::var_os("PTT_E2E_VOICE") {
        Some(path) => {
            let mut voice = virtual_audio::read_wav(Path::new(&path)).expect("PTT_E2E_VOICE");
            voice.resize(n, 0.0);
            voice
        }
        None => speech(1, 120.0, seconds),
    }
}

/// `signal` starting `at` seconds into silence of `total` seconds
fn place(at: f64, signal: Vec<f32>, total: f64) -> Vec<f32> {
    let mut out = vec![0.0; (total * FS as f64) as usize];
    let first = (at * FS as f64) as usize;
    let n = signal.len().min(out.len() - first);
    out[first..first + n].copy_from_slice(&signal[..n]);
    out
}

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

// ---- small helpers

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = v.fold((0.0, 0), |(s, n), x| (s + x, n + 1));
    sum / n as f64
}

fn median(v: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = v.collect();
    v.sort_by(f64::total_cmp);
    percentile(&v, 50)
}

fn row(name: &str, value: String) -> (String, String) {
    (name.to_string(), value)
}

fn fmt_ms(ms: f64) -> String {
    format!("{ms:.0} ms")
}

fn print_table(title: &str, rows: &[(String, String)]) {
    println!("\n{title}");
    for (name, value) in rows {
        println!("  {name:<40} {value}");
    }
}
