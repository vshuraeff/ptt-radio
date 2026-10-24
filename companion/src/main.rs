//! ptt-radio companion: carries push-to-talk voice through the Factorio mod
//! ptt-radio over the game's Lua UDP API. see ../DESIGN.md.
//!
//! threads: main (OS events for the PTT key, HELLO heartbeat, auto-ptt),
//! transmit (capture or tone -> opus -> game), receive (game -> jitter queues),
//! playout (10 ms ticks: mix -> output device and/or wav).
//! everything OS-specific is in ptt_key.rs; audio goes through cpal, or
//! through the virtual device (src/virtual_audio.rs) for tests.

mod ptt_key;

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use clap::builder::{PossibleValuesParser, TypedValueParser};

use ptt_key::PttKey;
use ptt_radio_companion::audio::{self, OPUS_RATE, Resampler, Samples};
use ptt_radio_companion::echo::{self, EchoPath};
use ptt_radio_companion::jitter::SLICE;
use ptt_radio_companion::mix::{self, Mixer, Mute, SLICE_SAMPLES};
use ptt_radio_companion::packet::{self, HELLO, Incoming, RosterEntry, START, VOICE};
use ptt_radio_companion::tx::{Codec, Transmitter};
use ptt_radio_companion::virtual_audio::{self, Coupling, VirtualDevice};
use ptt_radio_companion::{percentile, unix_us};

/// audio kept queued ahead of the output device
const OUTPUT_AHEAD: Duration = Duration::from_millis(20);
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
    /// Opus frame (packet) duration in ms; receivers play any mix of durations
    #[arg(long, default_value_t = 20, value_parser = PossibleValuesParser::new(["10", "20"]).map(|s| s.parse::<usize>().unwrap()))]
    frame_ms: usize,
    /// Opus encoder complexity, 0 (cheapest) ..= 10
    #[arg(long, default_value_t = 9, value_parser = clap::value_parser!(u8).range(0..=10))]
    complexity: u8,
    /// Opus in-band FEC (costs bits; only a lost packet uses it, and the game loses none)
    #[arg(long)]
    fec: bool,
    /// global push-to-talk key in global-hotkey syntax, e.g. F8, F13, shift+F8; "none" registers no key
    #[arg(long, default_value = "F8")]
    ptt_key: String,
    /// test: transmit a sine of this frequency instead of the microphone
    #[arg(long, value_name = "HZ", conflicts_with = "mic_wav")]
    tone: Option<f32>,
    /// test: once the roster arrives hold PTT this long, then drain and exit
    #[arg(long, value_name = "SECONDS")]
    auto_ptt: Option<f64>,
    /// test: exit this long after the start
    #[arg(long, value_name = "SECONDS")]
    exit_after: Option<f64>,
    /// do not open the output device
    #[arg(long, conflicts_with = "speaker_wav")]
    no_playback: bool,
    /// write the decoded received mix (48 kHz mono, only while someone is audible) to this file
    #[arg(long, value_name = "FILE.wav")]
    record_received: Option<PathBuf>,
    /// test: virtual audio instead of the devices; the microphone plays this file in real time, then silence
    #[arg(long, value_name = "FILE.wav")]
    mic_wav: Option<PathBuf>,
    /// test: virtual audio instead of the devices; everything played, silence included, goes to this file
    #[arg(long, value_name = "FILE.wav")]
    speaker_wav: Option<PathBuf>,
    /// test: virtual speaker output leaks into the virtual microphone at this gain (a player with speakers)
    #[arg(long, value_name = "DB", allow_negative_numbers = true)]
    couple_gain_db: Option<f32>,
    /// test: delay of the speaker-to-microphone path
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 0.0,
        requires = "couple_gain_db"
    )]
    couple_delay_ms: f64,
    /// test: impulse response of the speaker-to-microphone path (default: a plain delay)
    #[arg(long, value_name = "FILE.wav", requires = "couple_gain_db")]
    couple_ir: Option<PathBuf>,
    /// test: append per-frame timestamps to this file for the end-to-end scenarios
    #[arg(long, value_name = "FILE")]
    trace: Option<PathBuf>,
}

struct Shared {
    socket: UdpSocket,
    game: SocketAddr,
    done: AtomicBool,
    ptt: AtomicBool,
    open_mic: AtomicBool,
    // mute incoming voices while PTT is held (mod setting ptt-radio-half-duplex)
    half_duplex: AtomicBool,
    roster_seen: AtomicBool,
    mixer: Mutex<Mixer>,
    echo: EchoPath,
    // own VOICE frames by seq, to time their echo
    sent: Mutex<HashMap<u16, (Instant, Vec<u8>)>>,
    trace: Option<Mutex<BufWriter<File>>>,
}

impl Shared {
    /// one trace line: "<unix us> <event> <fields>", read by tests/scenarios.rs
    fn trace(&self, event: fmt::Arguments) {
        if let Some(file) = &self.trace
            && let Err(err) = writeln!(file.lock().unwrap(), "{} {event}", unix_us())
        {
            eprintln!("--trace: write failed: {err}");
        }
    }
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
    let trace = match &args.trace {
        Some(path) => Some(Mutex::new(BufWriter::new(File::create(path)?))),
        None => None,
    };
    let shared = Arc::new(Shared {
        socket,
        game: SocketAddr::from(([127, 0, 0, 1], args.game_port)),
        done: AtomicBool::new(false),
        ptt: AtomicBool::new(false),
        open_mic: AtomicBool::new(false),
        half_duplex: AtomicBool::new(true),
        roster_seen: AtomicBool::new(false),
        mixer: Mutex::new(Mixer::default()),
        echo: EchoPath::default(),
        sent: Mutex::new(HashMap::new()),
        trace,
    });
    shared.trace(format_args!("canceller {}", echo::CANCELLER));
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

    let ptt_key = match args.ptt_key.as_str() {
        "none" => None,
        spec => Some(PttKey::register(spec)?),
    };

    // the virtual device replaces both cpal streams when either of its files is given
    let virtual_device = if args.mic_wav.is_some() || args.speaker_wav.is_some() {
        let mic = match &args.mic_wav {
            Some(path) => virtual_audio::read_wav(path)
                .map_err(|err| format!("--mic-wav {}: {err}", path.display()))?,
            None => Vec::new(),
        };
        let ir = match &args.couple_ir {
            Some(path) => virtual_audio::read_wav(path)
                .map_err(|err| format!("--couple-ir {}: {err}", path.display()))?,
            None => vec![1.0],
        };
        let coupling = args.couple_gain_db.map(|gain| {
            Coupling::new(
                gain,
                Duration::from_secs_f64(args.couple_delay_ms / 1000.0),
                &ir,
            )
        });
        let (input, output) = (Samples::default(), Samples::default());
        let device = VirtualDevice::start(
            mic,
            coupling,
            args.speaker_wav.as_deref(),
            input.clone(),
            output.clone(),
        )?;
        println!("audio: virtual device, 48000 Hz, 1 ch");
        shared.trace(format_args!(
            "device {} {}",
            device.start_us,
            virtual_audio::PERIOD.as_micros()
        ));
        Some((device, input, output))
    } else if args.couple_gain_db.is_some() {
        return Err(
            "--couple-gain-db needs the virtual device (--mic-wav or --speaker-wav)".into(),
        );
    } else {
        None
    };

    // cpal streams are not Send: they live here on the main thread until exit
    let (source, _input) = match (args.tone, &virtual_device) {
        (Some(hz), _) => (
            Source::Tone {
                hz,
                phase: 0.0,
                next: Instant::now(),
            },
            None,
        ),
        (None, Some((_, input, _))) => (Source::mic(input.clone(), OPUS_RATE), None),
        (None, None) => {
            let samples = Samples::default();
            let (stream, rate) = audio::open_input(samples.clone())?;
            (Source::mic(samples, rate), Some(stream))
        }
    };
    let (output, _output_stream) = match &virtual_device {
        _ if args.no_playback => (None, None),
        Some((device, _, output)) => (
            Some((output.clone(), OPUS_RATE, Some(device.popped.clone()))),
            None,
        ),
        None => {
            let samples = Samples::default();
            let (stream, rate) = audio::open_output(samples.clone())?;
            (Some((samples, rate, None)), Some(stream))
        }
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

    let codec = Codec {
        frame_ms: args.frame_ms,
        bitrate: args.bitrate,
        complexity: args.complexity,
        fec: args.fec,
    };
    shared.trace(format_args!(
        "codec {} {} {} {}",
        codec.frame_samples(),
        codec.bitrate,
        codec.complexity,
        codec.fec as u8
    ));
    let tx_thread = {
        let shared = shared.clone();
        thread::spawn(move || transmit(&shared, source, codec))
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
    let exit_at = args
        .exit_after
        .map(|s| started + Duration::from_secs_f64(s));
    while !interrupted.load(Relaxed) {
        match &ptt_key {
            Some(key) => {
                if let Some(held) = key.poll(Duration::from_millis(10)) {
                    set_ptt(&shared, held);
                }
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
        let now = Instant::now();
        if exit_at.is_some_and(|at| now >= at) {
            break;
        }
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
                set_ptt(&shared, true);
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
                set_ptt(&shared, false);
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
    if let Some((device, _, _)) = virtual_device {
        device.stop()?;
    }
    let mix = &shared.mixer.lock().unwrap().stats;
    shared.trace(format_args!(
        "stats {frames_sent} {} {} {} {} {}",
        mix.decoded, mix.decode_errors, mix.dropped_late, mix.underruns, mix.shed
    ));
    if let Some(file) = &shared.trace {
        file.lock().unwrap().flush()?;
    }
    println!("{}", summary(frames_sent, &rx, mix, recorded));
    Ok(code)
}

fn set_ptt(shared: &Shared, held: bool) {
    if shared.ptt.swap(held, Relaxed) != held {
        println!("ptt {}", if held { "pressed" } else { "released" });
        shared.trace(format_args!("ptt {}", held as u8));
    }
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
    fn mic(samples: Samples, rate: u32) -> Self {
        Source::Mic {
            samples,
            resampler: Resampler::new(rate, OPUS_RATE),
            pending: Vec::new(),
        }
    }

    /// blocks until the next frame of `n` samples at 48 kHz is ready; None once shutdown starts
    fn next_frame(&mut self, n: usize, done: &AtomicBool) -> Option<Vec<f32>> {
        match self {
            Source::Tone { hz, phase, next } => {
                *next += Duration::from_secs_f64(n as f64 / OPUS_RATE as f64);
                thread::sleep(next.saturating_duration_since(Instant::now()));
                let step = std::f32::consts::TAU * *hz / OPUS_RATE as f32;
                let frame = (0..n)
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
                if pending.len() >= n {
                    return Some(pending.drain(..n).collect());
                }
                if done.load(Relaxed) {
                    return None;
                }
                let captured: Vec<f32> = samples.lock().unwrap().drain(..).collect();
                resampler.process(&captured, pending);
                if pending.len() < n {
                    // the poll period adds up to its own length to every frame
                    thread::sleep(Duration::from_millis(1));
                }
            },
        }
    }
}

/// returns the number of VOICE frames sent
fn transmit(shared: &Shared, mut source: Source, codec: Codec) -> u64 {
    let mut tx = Transmitter::new(codec).expect("opus encoder");
    let mut sent = 0;
    // captured frames so far: frame n holds samples [n * size, (n + 1) * size) of the capture
    let mut frame = 0;
    while let Some(mut pcm) = source.next_frame(tx.frame_samples(), &shared.done) {
        let ready_us = unix_us();
        shared.echo.near_end(&mut pcm);
        tx.set_ptt(shared.ptt.load(Relaxed));
        tx.set_open_mic(shared.open_mic.load(Relaxed));
        sent += send_packets(shared, tx.frame(&pcm), frame, ready_us);
        frame += 1;
    }
    sent + send_packets(shared, tx.close(), frame, unix_us())
}

/// sends what the transmitter made of captured frame `frame`; returns the VOICE count
fn send_packets(shared: &Shared, packets: Vec<Vec<u8>>, frame: u64, ready_us: u64) -> u64 {
    let voices = packets.iter().filter(|p| p[2] == VOICE).count() as u64;
    // a START burst carries the pre-roll: its VOICE frames end with the current one
    let mut voice_frame = frame + 1 - voices;
    for p in packets {
        match p[2] {
            VOICE => {
                let seq = u16::from_be_bytes([p[3], p[4]]);
                shared
                    .sent
                    .lock()
                    .unwrap()
                    .insert(seq, (Instant::now(), p[5..].to_vec()));
                send(shared, &p);
                shared.trace(format_args!(
                    "tx {seq} {voice_frame} {} {ready_us} {}",
                    (voice_frame < frame) as u8,
                    p.len() - 5
                ));
                voice_frame += 1;
            }
            kind => {
                println!("sent {}", if kind == START { "START" } else { "STOP" });
                send(shared, &p);
            }
        }
    }
    voices
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
            Incoming::HalfDuplex { on } => {
                if shared.half_duplex.swap(on, Relaxed) != on {
                    println!("half-duplex {}", if on { "on" } else { "off" });
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
                shared.mixer.lock().unwrap().voice(sender, seq, frame, now);
                shared.trace(format_args!("rx {sender} {seq}"));
            }
            Incoming::Stop { sender, .. } => {
                stats.senders.entry(sender).or_default().stops += 1;
                shared.mixer.lock().unwrap().stop(sender);
            }
        }
    }
    stats
}

/// returns the number of slices written to the wav
fn playout(
    shared: &Shared,
    output: Option<(Samples, u32, Option<Arc<AtomicU64>>)>,
    mut wav: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
) -> Result<u64, hound::Error> {
    let mut resampler = output
        .as_ref()
        .map(|(_, rate, _)| Resampler::new(OPUS_RATE, *rate));
    let mut next = Instant::now();
    let mut recorded = 0;
    let mut out = Vec::new();
    let (mut mute, mut was_muted) = (Mute::default(), false);
    while !shared.done.load(Relaxed) {
        match &output {
            // the output device's clock paces playout: top the queue up with the next slice
            // once less than OUTPUT_AHEAD is left
            Some((samples, rate, _)) => {
                let ahead = *rate as usize * OUTPUT_AHEAD.as_millis() as usize / 1000;
                if samples.lock().unwrap().len() >= ahead {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
            }
            // no device: the system clock paces it
            None => {
                next += SLICE;
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        }
        let (mix, pulled) = {
            let mut mixer = shared.mixer.lock().unwrap();
            let mix = mixer.tick(Instant::now());
            (mix, std::mem::take(&mut mixer.pulled))
        };
        // half-duplex: silent speakers while PTT is held, so the microphone cannot pick the
        // others up; the mixer keeps pulling, so playback resumes in sync on release
        let muted = shared.half_duplex.load(Relaxed)
            && shared.ptt.load(Relaxed)
            && !shared.open_mic.load(Relaxed);
        let mut slice = [0.0; SLICE_SAMPLES];
        if let Some(mix) = &mix {
            slice.copy_from_slice(mix);
        }
        mute.apply(muted, &mut slice);
        let frame = &slice[..];
        shared.echo.far_end(frame);
        if let (Some(_), Some(writer)) = (&mix, wav.as_mut()) {
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
        // the frame's first sample sounds after everything the device took before plus what is
        // queued now; both are read under the queue lock, so the index does not depend on when
        // the device thread wakes (it counts only with the virtual device, else it is the queue
        // length)
        let mut slot = 0;
        if let (Some((samples, _, popped)), Some(resampler)) = (&output, resampler.as_mut()) {
            out.clear();
            resampler.process(frame, &mut out);
            let mut queue = samples.lock().unwrap();
            slot = popped.as_ref().map_or(0, |p| p.load(Relaxed)) + queue.len() as u64;
            queue.extend(&out);
        }
        if muted != was_muted {
            shared.trace(format_args!("mute {} {slot}", muted as u8));
            was_muted = muted;
        }
        for p in pulled {
            if let Some((seq, offset)) = p.shed {
                shared.trace(format_args!("shed {} {seq} {offset}", p.sender));
            }
            match p.slice {
                Some((seq, offset)) => shared.trace(format_args!(
                    "pull {} {seq} {offset} {} {slot}",
                    p.sender, p.queued
                )),
                None => shared.trace(format_args!("pull {} - 0 0 {slot}", p.sender)),
            }
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
    let rtt = if rtt.is_empty() {
        "rtt n/a (no own frames echoed)".to_string()
    } else {
        format!(
            "rtt p50 {:.0} ms p95 {:.0} ms max {:.0} ms (n={})",
            percentile(&rtt, 50),
            percentile(&rtt, 95),
            percentile(&rtt, 100),
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
        recorded as f64 * SLICE.as_secs_f64(),
        rx.ignored
    )
}
