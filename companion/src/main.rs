//! ptt-radio companion: carries push-to-talk voice through the Factorio mod
//! ptt-radio over the game's Lua UDP API. see ../DESIGN.md.
//!
//! threads: main (OS events for the PTT key, HELLO heartbeat, auto-ptt),
//! transmit (capture or tone -> opus -> game), receive (game -> jitter queues),
//! playout (20 ms ticks: decode, mix -> output device and/or wav).
//! everything OS-specific is in ptt_key.rs; audio goes through cpal only.

mod audio;
mod jitter;
mod mix;
mod packet;
mod ptt_key;
mod tx;

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;

use audio::{OPUS_RATE, Resampler, Samples};
use mix::Mixer;
use packet::{HELLO, Incoming, RosterEntry, VOICE};
use ptt_key::PttKey;
use tx::{FRAME_SAMPLES, Transmitter};

const FRAME: Duration = Duration::from_millis(20);
const HELLO_EVERY: Duration = Duration::from_secs(5);
const ROSTER_WARN_AFTER: Duration = Duration::from_secs(3);
const AUTO_PTT_GIVE_UP: Duration = Duration::from_secs(10);
const DRAIN_MIN: Duration = Duration::from_millis(1500);
const DRAIN_MAX: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    version,
    about = "Push-to-talk voice companion for the ptt-radio Factorio mod"
)]
struct Args {
    /// UDP port the game listens on (its --enable-lua-udp value)
    #[arg(long, default_value_t = 34556)]
    game_port: u16,
    /// UDP port this companion listens on (mod setting ptt-radio-companion-port)
    #[arg(long, default_value_t = 34555)]
    port: u16,
    /// Opus bitrate in bit/s
    #[arg(long, default_value_t = 16_000, value_parser = clap::value_parser!(u32).range(500..=512_000))]
    bitrate: u32,
    /// global push-to-talk key in global-hotkey syntax, e.g. F8, F13, shift+F8
    #[arg(long, default_value = "F8")]
    ptt_key: String,
    /// test: transmit a sine of this frequency instead of the microphone
    #[arg(long, value_name = "HZ")]
    tone: Option<f32>,
    /// test: once the roster arrives hold PTT this long, then drain and exit
    #[arg(long, value_name = "SECONDS")]
    auto_ptt: Option<f64>,
    /// do not open the output device
    #[arg(long)]
    no_playback: bool,
    /// write the decoded received mix (48 kHz mono, only while someone is audible) to this file
    #[arg(long, value_name = "FILE.wav")]
    record_received: Option<PathBuf>,
}

struct Shared {
    socket: UdpSocket,
    game: SocketAddr,
    done: AtomicBool,
    ptt: AtomicBool,
    open_mic: AtomicBool,
    roster_seen: AtomicBool,
    mixer: Mutex<Mixer>,
    // own VOICE frames by seq, to time their echo
    sent: Mutex<HashMap<u16, (Instant, Vec<u8>)>>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<ExitCode, Box<dyn Error>> {
    let socket = UdpSocket::bind(("127.0.0.1", args.port)).map_err(|err| {
        format!(
            "cannot bind 127.0.0.1:{}: {err} (another companion or radio.py running?)",
            args.port
        )
    })?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let shared = Arc::new(Shared {
        socket,
        game: SocketAddr::from(([127, 0, 0, 1], args.game_port)),
        done: AtomicBool::new(false),
        ptt: AtomicBool::new(false),
        open_mic: AtomicBool::new(false),
        roster_seen: AtomicBool::new(false),
        mixer: Mutex::new(Mixer::default()),
        sent: Mutex::new(HashMap::new()),
    });
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    ctrlc::set_handler(move || flag.store(true, Relaxed))?;
    println!(
        "ptt-radio-companion {} ({}, static), listening on 127.0.0.1:{}, game at 127.0.0.1:{}",
        env!("CARGO_PKG_VERSION"),
        opusic_c::version(),
        args.port,
        args.game_port
    );

    let ptt_key = PttKey::register(&args.ptt_key)?;

    // cpal streams are not Send: they live here on the main thread until exit
    let (source, _input) = match args.tone {
        Some(hz) => (
            Source::Tone {
                hz,
                phase: 0.0,
                next: Instant::now(),
            },
            None,
        ),
        None => {
            let samples = Samples::default();
            let (stream, rate) = audio::open_input(samples.clone())?;
            let source = Source::Mic {
                samples,
                resampler: Resampler::new(rate, OPUS_RATE),
                pending: Vec::new(),
            };
            (source, Some(stream))
        }
    };
    let (output, _output_stream) = if args.no_playback {
        (None, None)
    } else {
        let samples = Samples::default();
        let (stream, rate) = audio::open_output(samples.clone())?;
        (Some((samples, rate)), Some(stream))
    };
    let wav = match &args.record_received {
        Some(path) => Some(hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: OPUS_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?),
        None => None,
    };

    let tx_thread = {
        let (shared, bitrate) = (shared.clone(), args.bitrate);
        thread::spawn(move || transmit(&shared, source, bitrate))
    };
    let rx_thread = {
        let shared = shared.clone();
        thread::spawn(move || receive(&shared))
    };
    let playout_thread = {
        let shared = shared.clone();
        thread::spawn(move || playout(&shared, output, wav))
    };

    let started = Instant::now();
    let mut next_hello = started;
    let mut roster_warned = false;
    let mut auto = args.auto_ptt.map(|_| Auto::WaitRoster);
    let mut code = ExitCode::SUCCESS;
    let hello = packet::encode(
        HELLO,
        0,
        format!("ptt-radio-companion {}", env!("CARGO_PKG_VERSION")).as_bytes(),
    );
    while !interrupted.load(Relaxed) {
        if let Some(held) = ptt_key.poll(Duration::from_millis(10))
            && shared.ptt.swap(held, Relaxed) != held
        {
            println!("ptt {}", if held { "pressed" } else { "released" });
        }
        let now = Instant::now();
        // heartbeat for the whole lifetime: the mod marks the companion offline after 15 s without HELLO
        if now >= next_hello {
            send(&shared, &hello);
            next_hello += HELLO_EVERY;
        }
        let roster_seen = shared.roster_seen.load(Relaxed);
        if !roster_seen && !roster_warned && now - started > ROSTER_WARN_AFTER {
            eprintln!(
                "warning: no ROSTER from the game at 127.0.0.1:{}: Factorio not started with --enable-lua-udp {}, \
                 a different port, not in a multiplayer game, or the ptt-radio mod is missing; still trying",
                args.game_port, args.game_port
            );
            roster_warned = true;
        }
        match auto {
            Some(Auto::WaitRoster) if roster_seen => {
                println!(
                    "auto-ptt: holding for {} s",
                    args.auto_ptt.unwrap_or_default()
                );
                shared.ptt.store(true, Relaxed);
                auto = Some(Auto::Hold(
                    now + Duration::from_secs_f64(args.auto_ptt.unwrap_or_default()),
                ));
            }
            Some(Auto::WaitRoster) if now - started > AUTO_PTT_GIVE_UP => {
                eprintln!(
                    "auto-ptt: no roster after {} s, giving up",
                    AUTO_PTT_GIVE_UP.as_secs()
                );
                code = ExitCode::from(2);
                break;
            }
            Some(Auto::Hold(until)) if now >= until => {
                shared.ptt.store(false, Relaxed);
                auto = Some(Auto::Drain(now));
            }
            Some(Auto::Drain(since))
                if (now - since >= DRAIN_MIN && shared.mixer.lock().unwrap().is_idle())
                    || now - since >= DRAIN_MAX =>
            {
                break;
            }
            _ => {}
        }
    }

    shared.done.store(true, Relaxed);
    let frames_sent = tx_thread.join().expect("transmit thread");
    let rx = rx_thread.join().expect("receive thread");
    let recorded = playout_thread.join().expect("playout thread")?;
    let mix = &shared.mixer.lock().unwrap().stats;
    println!("{}", summary(frames_sent, &rx, mix, recorded));
    Ok(code)
}

enum Auto {
    WaitRoster,
    Hold(Instant),
    Drain(Instant),
}

fn send(shared: &Shared, packet: &[u8]) {
    if let Err(err) = shared.socket.send_to(packet, shared.game) {
        eprintln!("send to game failed: {err}");
    }
}

enum Source {
    Tone {
        hz: f32,
        phase: f32,
        next: Instant,
    },
    Mic {
        samples: Samples,
        resampler: Resampler,
        pending: Vec<f32>,
    },
}

impl Source {
    /// blocks until the next 20 ms frame at 48 kHz is ready; None once shutdown starts
    fn next_frame(&mut self, done: &AtomicBool) -> Option<Vec<f32>> {
        match self {
            Source::Tone { hz, phase, next } => {
                *next += FRAME;
                thread::sleep(next.saturating_duration_since(Instant::now()));
                let step = std::f32::consts::TAU * *hz / OPUS_RATE as f32;
                let frame = (0..FRAME_SAMPLES)
                    .map(|_| {
                        let s = 0.3 * phase.sin();
                        *phase = (*phase + step) % std::f32::consts::TAU;
                        s
                    })
                    .collect();
                (!done.load(Relaxed)).then_some(frame)
            }
            Source::Mic {
                samples,
                resampler,
                pending,
            } => loop {
                if pending.len() >= FRAME_SAMPLES {
                    return Some(pending.drain(..FRAME_SAMPLES).collect());
                }
                if done.load(Relaxed) {
                    return None;
                }
                let captured: Vec<f32> = samples.lock().unwrap().drain(..).collect();
                resampler.process(&captured, pending);
                if pending.len() < FRAME_SAMPLES {
                    thread::sleep(Duration::from_millis(5));
                }
            },
        }
    }
}

/// returns the number of VOICE frames sent
fn transmit(shared: &Shared, mut source: Source, bitrate: u32) -> u64 {
    let mut tx = Transmitter::new(bitrate).expect("opus encoder");
    let mut frames = 0;
    let mut send_all = |packets: Vec<Vec<u8>>| {
        for p in packets {
            match p[2] {
                VOICE => {
                    let seq = u16::from_be_bytes([p[3], p[4]]);
                    shared
                        .sent
                        .lock()
                        .unwrap()
                        .insert(seq, (Instant::now(), p[5..].to_vec()));
                    frames += 1;
                }
                kind => println!(
                    "sent {}",
                    if kind == packet::START {
                        "START"
                    } else {
                        "STOP"
                    }
                ),
            }
            send(shared, &p);
        }
    };
    while let Some(pcm) = source.next_frame(&shared.done) {
        tx.set_ptt(shared.ptt.load(Relaxed));
        tx.set_open_mic(shared.open_mic.load(Relaxed));
        send_all(tx.frame(&pcm));
    }
    send_all(tx.close());
    frames
}

#[derive(Default)]
struct SenderStats {
    starts: u64,
    voice: u64,
    stops: u64,
    out_of_order: u64,
    last_seq: Option<u16>,
}

#[derive(Default)]
struct RxStats {
    senders: BTreeMap<u16, SenderStats>,
    roster: Vec<RosterEntry>,
    rtt_ms: Vec<f64>,
    ignored: u64,
}

fn receive(shared: &Shared) -> RxStats {
    let mut stats = RxStats::default();
    let mut open_mic: Option<bool> = None;
    let mut buf = [0u8; 65536];
    while !shared.done.load(Relaxed) {
        let len = match shared.socket.recv(&mut buf) {
            Ok(len) => len,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(err) => {
                eprintln!("receive failed: {err}");
                continue;
            }
        };
        let now = Instant::now();
        let Some(incoming) = packet::parse(&buf[..len]) else {
            if stats.ignored == 0 {
                eprintln!(
                    "warning: ignoring a packet that is not ptt-radio protocol v1 (mod version mismatch?)"
                );
            }
            stats.ignored += 1;
            continue;
        };
        match incoming {
            Incoming::Roster(roster) => {
                shared.roster_seen.store(true, Relaxed);
                if roster != stats.roster {
                    println!("roster:");
                    for p in &roster {
                        println!("  player {:>3}  {:<24} rgb({})", p.index, p.name, p.color);
                    }
                    stats.roster = roster;
                }
            }
            Incoming::MicState { player, open } => {
                shared.open_mic.store(open, Relaxed);
                if open_mic != Some(open) {
                    println!(
                        "open mic {} (you are player {player})",
                        if open { "on" } else { "off" }
                    );
                    open_mic = Some(open);
                }
            }
            Incoming::Start { sender, seq } => {
                let s = stats.senders.entry(sender).or_default();
                s.starts += 1;
                s.last_seq = Some(seq);
                shared.mixer.lock().unwrap().start(sender, now);
            }
            Incoming::Voice { sender, seq, frame } => {
                let s = stats.senders.entry(sender).or_default();
                s.voice += 1;
                if s.last_seq.is_some_and(|last| seq != last.wrapping_add(1)) {
                    s.out_of_order += 1;
                }
                s.last_seq = Some(seq);
                // own echo: same seq and same opus bytes as a frame this companion sent
                let mut sent = shared.sent.lock().unwrap();
                if sent.get(&seq).is_some_and(|(_, body)| body == frame) {
                    let (at, _) = sent.remove(&seq).expect("checked above");
                    stats.rtt_ms.push((now - at).as_secs_f64() * 1000.0);
                }
                drop(sent);
                shared.mixer.lock().unwrap().voice(sender, frame, now);
            }
            Incoming::Stop { sender, .. } => {
                stats.senders.entry(sender).or_default().stops += 1;
                shared.mixer.lock().unwrap().stop(sender);
            }
        }
    }
    stats
}

/// returns the number of 20 ms frames written to the wav
fn playout(
    shared: &Shared,
    output: Option<(Samples, u32)>,
    mut wav: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
) -> Result<u64, hound::Error> {
    let mut resampler = output
        .as_ref()
        .map(|(_, rate)| Resampler::new(OPUS_RATE, *rate));
    let mut next = Instant::now();
    let mut recorded = 0;
    let mut out = Vec::new();
    while !shared.done.load(Relaxed) {
        match &output {
            // the output device's clock paces playout: keep about 40 ms queued ahead of it
            Some((samples, rate)) => {
                let ahead = 2 * *rate as usize * FRAME.as_millis() as usize / 1000;
                if samples.lock().unwrap().len() >= ahead {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
            }
            // no device: the system clock paces it
            None => {
                next += FRAME;
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        }
        let mix = shared.mixer.lock().unwrap().tick(Instant::now());
        if let (Some(frame), Some(writer)) = (&mix, wav.as_mut()) {
            let written = frame.iter().try_for_each(|s| {
                writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            });
            match written {
                Ok(()) => recorded += 1,
                // a failing recording must not silence playback: stop recording, keep playing
                Err(err) => {
                    eprintln!("--record-received: write failed, recording stopped: {err}");
                    wav = None;
                }
            }
        }
        if let (Some((samples, _)), Some(resampler)) = (&output, resampler.as_mut()) {
            out.clear();
            resampler.process(mix.as_deref().unwrap_or(&[0.0; FRAME_SAMPLES]), &mut out);
            samples.lock().unwrap().extend(&out);
        }
    }
    if let Some(writer) = wav {
        writer.finalize()?;
    }
    Ok(recorded)
}

fn summary(frames_sent: u64, rx: &RxStats, mix: &mix::MixStats, recorded: u64) -> String {
    let senders: Vec<String> = rx
        .senders
        .iter()
        .map(|(index, s)| {
            let name = rx
                .roster
                .iter()
                .find(|p| p.index == *index)
                .map_or("?", |p| p.name.as_str());
            format!(
                "{index}:{name} voice {} start {} stop {} out-of-order {}",
                s.voice, s.starts, s.stops, s.out_of_order
            )
        })
        .collect();
    let mut rtt = rx.rtt_ms.clone();
    rtt.sort_by(f64::total_cmp);
    let pick = |p: usize| {
        rtt.get((rtt.len() - 1) * p / 100)
            .copied()
            .unwrap_or(f64::NAN)
    };
    let rtt = if rtt.is_empty() {
        "rtt n/a (no own frames echoed)".to_string()
    } else {
        format!(
            "rtt p50 {:.0} ms p95 {:.0} ms max {:.0} ms (n={})",
            pick(50),
            pick(95),
            pick(100),
            rtt.len()
        )
    };
    format!(
        "stats: sent {frames_sent} frames | received [{}] | decoded {} decode-errors {} dropped-late {} underruns {} \
         | recorded {:.2} s | {rtt} | ignored packets {}",
        if senders.is_empty() {
            "nothing".to_string()
        } else {
            senders.join("; ")
        },
        mix.decoded,
        mix.decode_errors,
        mix.dropped_late,
        mix.underruns,
        recorded as f64 * FRAME.as_secs_f64(),
        rx.ignored
    )
}
