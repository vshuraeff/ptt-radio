//! ptt-radio companion: negotiated direct/relay UDP with the trusted game path
//! for signaling, compatibility and fallback. see ../DESIGN.md.
//!
//! threads: main (OS events for the PTT key, HELLO heartbeat, auto-ptt),
//! transmit (capture or tone -> opus -> runtime, which sends each frame on the
//! selected route: direct, hub or player-relay UDP, or the game as fallback),
//! receive (UDP sockets and game -> runtime ticks, signaling and media -> jitter
//! queues), playout (10 ms ticks: mix -> output device and/or wav).
//! the in-game echo test (self_test.rs) spans all four: receive files the
//! request, main starts and times it out, transmit records and sends, playout
//! plays its tone.
//! everything OS-specific is in ptt_key.rs; audio goes through cpal, or
//! through the virtual device (src/virtual_audio.rs) for tests.

mod launch;
mod ptt_key;
mod self_test;
#[cfg(feature = "self-update")]
mod self_uninstall;
#[cfg(feature = "self-update")]
mod self_update;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use clap::builder::{PossibleValuesParser, TypedValueParser};

use ptt_key::PttKey;
use ptt_radio_companion::audio::{self, OPUS_RATE, Resampler, Samples};
use ptt_radio_companion::echo::{self, EchoPath};
use ptt_radio_companion::jitter::SLICE;
use ptt_radio_companion::mix::{self, MediaOutcome, Mixer, Mute, SLICE_SAMPLES};
use ptt_radio_companion::packet::{
    self, ECHO_STATE, HELLO, Incoming, RosterEntry, START, SelfTestResult, VOICE,
};
use ptt_radio_companion::quality::Reporter;
use ptt_radio_companion::runtime::{Media, Output, Runtime, RuntimeConfig};
use ptt_radio_companion::runtime_io::{Network, Resolver};
use ptt_radio_companion::transport::wire::Kind;
use ptt_radio_companion::tx::{Codec, Transmitter};
use ptt_radio_companion::virtual_audio::{self, Coupling, VirtualDevice};
use ptt_radio_companion::{percentile, unix_us};
use self_test::{Fence, Frame, SelfTest};

/// audio kept queued ahead of the output device
const OUTPUT_AHEAD: Duration = Duration::from_millis(20);
const HELLO_EVERY: Duration = Duration::from_secs(5);
const ROSTER_WARN_AFTER: Duration = Duration::from_secs(3);
const AUTO_PTT_GIVE_UP: Duration = Duration::from_secs(10);
const DRAIN_MIN: Duration = Duration::from_millis(1500);
const DRAIN_MAX: Duration = Duration::from_secs(5);
const MIC_RETRY: Duration = Duration::from_secs(1);

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
    /// Opus bitrate in bit/s; the encoder keeps every packet within the transport's
    /// 1164-byte Opus cap, about 465 kbit/s at 20 ms frames
    #[arg(long, default_value_t = 16_000, value_parser = clap::value_parser!(u32).range(500..=512_000))]
    bitrate: u32,
    /// acoustic echo cancellation, selected only after an echo path is detected
    #[arg(long, value_parser = ["on", "off"], default_value = "on")]
    aec: String,
    /// Opus frame (packet) duration in ms; receivers play any mix of durations
    #[arg(long, default_value_t = 20, value_parser = PossibleValuesParser::new(["10", "20"]).map(|s| s.parse::<usize>().unwrap()))]
    frame_ms: usize,
    /// Opus encoder complexity, 0 (cheapest) ..= 10
    #[arg(long, default_value_t = 9, value_parser = clap::value_parser!(u8).range(0..=10))]
    complexity: u8,
    /// Opus in-band FEC (costs bits; only a lost packet uses it: UDP routes can lose
    /// packets, the game fallback loses none)
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
    /// UDP endpoint of the relay hub beside the game server
    #[arg(long, value_name = "ADDR")]
    hub: Option<SocketAddr>,
    /// local UDP interface and port (repeat for up to four sockets; default: default-route interfaces)
    #[arg(long, value_name = "ADDR")]
    udp_bind: Vec<SocketAddr>,
    /// public STUN address (repeat; default: stun.l.google.com:19302)
    #[arg(long, value_name = "HOST:PORT")]
    stun_server: Vec<String>,
    /// use the game transport only, retaining v2 framing and stream identity
    #[arg(long)]
    game_only: bool,
    /// test: carry actual UDP datagrams through this network-lab router
    #[arg(long, value_name = "ADDR")]
    netlab: Option<SocketAddr>,
    /// test: private address of the lab path, as registered with the router (default 127.0.0.1:0)
    #[arg(long, value_name = "ADDR", requires = "netlab")]
    netlab_bind: Option<SocketAddr>,
    /// do not update to the latest GitHub release at startup (also PTT_RADIO_NO_SELF_UPDATE=1)
    #[cfg(feature = "self-update")]
    #[arg(long)]
    no_self_update: bool,
    /// keep the install directory when the ptt-radio mod is gone (also PTT_RADIO_NO_SELF_UNINSTALL=1)
    #[cfg(feature = "self-update")]
    #[arg(long)]
    no_self_uninstall: bool,
    /// start the game with this command line and exit with it (Steam: --launch %command%);
    /// everything after it is the game's, and --enable-lua-udp <game-port> is appended
    /// unless present
    #[arg(
        long,
        value_name = "GAME",
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_parser = clap::value_parser!(OsString)
    )]
    launch: Vec<OsString>,
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
    real_mic: bool,
    mic_close: AtomicBool,
    mixer: Mutex<Mixer>,
    echo: EchoPath,
    // own VOICE frames by seq, to time their echo
    sent: Mutex<HashMap<u16, (Instant, Vec<u8>)>>,
    trace: Option<Mutex<BufWriter<File>>>,
    runtime: Mutex<(Runtime, Network)>,
    output: OnceLock<(Samples, u32)>,
    output_history: Mutex<OutputHistory>,
    output_reset: AtomicU64,
    // a leaf lock: never held while taking another
    self_test: Mutex<SelfTest>,
}

#[derive(Default)]
struct OutputHistory {
    queued: VecDeque<(usize, Vec<(u16, usize)>)>,
    carry: Vec<u16>,
}

impl OutputHistory {
    fn synchronize(&mut self, samples: usize) {
        let mut consumed = self
            .queued
            .iter()
            .map(|entry| entry.0)
            .sum::<usize>()
            .saturating_sub(samples);
        while consumed > 0 {
            let entry = self.queued.front_mut().expect("counted queued samples");
            let take = consumed.min(entry.0);
            entry.0 -= take;
            entry.1.retain_mut(|(_, until)| {
                *until = until.saturating_sub(take);
                *until > 0
            });
            consumed -= take;
            if entry.0 == 0 {
                self.queued.pop_front();
            }
        }
    }

    fn purge(&mut self, queue: &mut VecDeque<f32>, all: bool, players: &[u16]) -> (usize, bool) {
        self.synchronize(queue.len());
        let queued = all
            || self
                .queued
                .iter()
                .any(|(_, senders)| senders.iter().any(|(sender, _)| players.contains(sender)));
        let carry = all || self.carry.iter().any(|sender| players.contains(sender));
        // silence keeps the queue's length: the echo reference already holds these samples,
        // and shrinking the queue would let playout refill ahead of the device timeline
        let dropped = if queued {
            queue.iter_mut().for_each(|sample| *sample = 0.0);
            for (_, senders) in &mut self.queued {
                senders.clear();
            }
            queue.len()
        } else {
            0
        };
        if queued || carry {
            self.carry.clear();
        }
        (dropped, queued || carry)
    }
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
    let args = Args::parse();
    // before any socket, audio device or hotkey: a newer release re-launches from here,
    // with the same arguments, --launch and the game's command line included
    #[cfg(feature = "self-update")]
    self_update::at_startup(args.no_self_update);
    #[cfg(feature = "self-update")]
    if self_uninstall::check(args.no_self_uninstall) {
        // the game the player started through --launch still starts, without the companion
        return if args.launch.is_empty() {
            ExitCode::SUCCESS
        } else {
            launch::run_alone(&args.launch, args.game_port)
        };
    }
    #[cfg(feature = "self-update")]
    let no_self_uninstall = args.no_self_uninstall;
    let (game_argv, game_port) = (args.launch.clone(), args.game_port);
    let mut game_attempted = false;
    match run(args, &mut game_attempted) {
        Ok(code) => {
            // the game has exited: the mod may have been removed from its own mod manager
            #[cfg(feature = "self-update")]
            if !game_argv.is_empty() {
                self_uninstall::check(no_self_uninstall);
            }
            code
        }
        Err(err) => {
            eprintln!("error: {err}");
            // a setup failure must not cost the player the game; once spawn was tried,
            // a second start would be a second instance or a retry of a broken command
            if game_argv.is_empty() || game_attempted {
                ExitCode::FAILURE
            } else {
                eprintln!("--launch: starting the game without the companion");
                launch::run_alone(&game_argv, game_port)
            }
        }
    }
}

/// `game_attempted` is set right before the --launch spawn, so an error after it
/// never starts the game again
fn run(args: Args, game_attempted: &mut bool) -> Result<ExitCode, Box<dyn Error>> {
    let socket = UdpSocket::bind(("127.0.0.1", args.port)).map_err(|err| {
        format!(
            "cannot bind 127.0.0.1:{}: {err} (another companion or radio.py running?)",
            args.port
        )
    })?;
    socket.set_nonblocking(true)?;
    let network = Network::bind(
        args.udp_bind.clone(),
        args.netlab.map(|router| {
            (
                args.netlab_bind
                    .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0))),
                router,
            )
        }),
    )?;
    let mut config = RuntimeConfig {
        hub: args.hub,
        game_only: args.game_only,
        ..Default::default()
    };
    let mut lookup_names = Vec::new();
    if !args.game_only {
        let servers = if args.stun_server.is_empty() && args.netlab.is_none() {
            vec!["stun.l.google.com:19302".to_string()]
        } else {
            args.stun_server.clone()
        };
        for server in servers.into_iter().take(4) {
            match server.parse::<SocketAddr>() {
                Ok(address) => config.connectivity.discovery.stun_servers.push(address),
                Err(_) => lookup_names.push(server),
            }
        }
        config.connectivity.discovery.stun_servers.truncate(4);
    }
    let runtime = Runtime::new(network.addresses()?, config).map_err(|e| e.to_string())?;
    if let Some(router) = args.netlab {
        println!(
            "netlab: path {} via router {router}",
            network.addresses()?[0]
        );
    }
    let trace = match &args.trace {
        Some(path) => Some(Mutex::new(BufWriter::new(File::create(path)?))),
        None => None,
    };
    let aec = args.aec == "on";
    let shared = Arc::new(Shared {
        socket,
        game: SocketAddr::from(([127, 0, 0, 1], args.game_port)),
        done: AtomicBool::new(false),
        ptt: AtomicBool::new(false),
        open_mic: AtomicBool::new(false),
        half_duplex: AtomicBool::new(true),
        roster_seen: AtomicBool::new(false),
        real_mic: args.tone.is_none() && args.mic_wav.is_none() && args.speaker_wav.is_none(),
        mic_close: AtomicBool::new(false),
        mixer: Mutex::new(Mixer::default()),
        echo: EchoPath::new(aec),
        sent: Mutex::new(HashMap::new()),
        trace,
        runtime: Mutex::new((runtime, network)),
        output: OnceLock::new(),
        output_history: Mutex::new(OutputHistory::default()),
        output_reset: AtomicU64::new(0),
        self_test: Mutex::default(),
    });
    let hello = packet::encode(
        HELLO,
        0,
        format!(
            "ptt-radio-companion {} hud1 pol1 v2",
            env!("CARGO_PKG_VERSION")
        )
        .as_bytes(),
    );
    send(&shared, &hello);
    send_echo_state(&shared, shared.echo.gate_state().engaged);
    let resolver = if lookup_names.is_empty() {
        None
    } else {
        match Resolver::start(lookup_names, Instant::now()) {
            Ok(resolver) => Some(resolver),
            Err(err) => {
                eprintln!("STUN discovery unavailable: {err}");
                None
            }
        }
    };
    shared.trace(format_args!(
        "canceller {} 0",
        if aec { echo::CANCELLER } else { "none" }
    ));
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

    // cpal streams are not send: the real input stays owned by this main thread
    let (source, mut microphone) = match (args.tone, &virtual_device) {
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
            let microphone = Microphone::default();
            (Source::Gated(microphone.capture.clone()), Some(microphone))
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
    if let Some((samples, rate, _)) = &output {
        shared
            .output
            .set((samples.clone(), *rate))
            .expect("output configured once");
    }

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
    // the companion is ready before the game starts; a spawn failure ends it here
    let mut game = if args.launch.is_empty() {
        None
    } else {
        *game_attempted = true;
        let child = launch::spawn(&args.launch, args.game_port)?;
        println!("game started (pid {})", child.id());
        Some(child)
    };
    let tx_thread = {
        let shared = shared.clone();
        thread::spawn(move || transmit(&shared, source, codec))
    };
    let rx_thread = {
        let shared = shared.clone();
        thread::spawn(move || receive(&shared, resolver))
    };
    let playout_thread = {
        let shared = shared.clone();
        thread::spawn(move || playout(&shared, output, wav))
    };

    let started = Instant::now();
    let mut next_hello = started + HELLO_EVERY;
    let mut roster_warned = false;
    let mut auto = args.auto_ptt.map(|_| Auto::WaitRoster);
    let mut code = ExitCode::SUCCESS;
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
        // the usual shutdown below sends STOP before the device closes
        match game.as_mut().map(Child::try_wait) {
            None | Some(Ok(None)) => {}
            Some(Ok(Some(status))) => {
                println!("game exited ({status})");
                code = launch::exit_code(status);
                break;
            }
            Some(Err(err)) => {
                eprintln!("--launch: cannot watch the game: {err}");
                code = ExitCode::FAILURE;
                break;
            }
        }
        if let Some(microphone) = &mut microphone {
            microphone.update(&shared, now, audio::open_input);
        }
        drive_self_test(
            &shared,
            now,
            microphone.as_ref().is_none_or(|m| m.stream.is_some()),
        );
        // heartbeat for the whole lifetime: the mod marks the companion offline after 15 s without HELLO
        if now >= next_hello {
            send(&shared, &hello);
            send_echo_state(&shared, shared.echo.gate_state().engaged);
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

    if let Some(microphone) = &mut microphone {
        microphone.close(&shared, "shutdown");
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

fn send_echo_state(shared: &Shared, engaged: bool) {
    send(shared, &packet::encode(ECHO_STATE, 0, &[engaged as u8]));
    shared.trace(format_args!("echo_state {}", engaged as u8));
}

/// starts a requested self-test and ends a running one on time (main thread)
fn drive_self_test(shared: &Shared, now: Instant, mic_open: bool) {
    if shared.self_test.lock().unwrap().idle() {
        return;
    }
    // the gate the transmit path applies to this source
    let (permitted, reset) = {
        let runtime = &shared.runtime.lock().unwrap().0;
        let (policy_permitted, _, reset) = runtime.capture_policy();
        let permitted = if shared.real_mic {
            runtime.microphone_permitted(now)
        } else {
            policy_permitted
        };
        (permitted, reset)
    };
    let status = self_test::Status {
        busy: shared.ptt.load(Relaxed) || shared.open_mic.load(Relaxed),
        permitted,
        reset,
        mic_open,
        playback: shared.output.get().is_some(),
    };
    let result = shared.self_test.lock().unwrap().poll(now, status);
    if let Some(result) = result {
        send_self_test_result(shared, &result);
    }
}

fn send_self_test_result(shared: &Shared, result: &SelfTestResult) {
    send(shared, &packet::encode_self_test_result(result));
    println!(
        "self-test {}: {:?}, tone {}, mic peak {} dBFS, sent {} returned {} frames, first return {} ms",
        result.test_id,
        result.outcome,
        result.tone,
        result.mic_peak_dbfs,
        result.frames_sent,
        result.frames_returned,
        result.first_return_ms
    );
    shared.trace(format_args!(
        "self_test {} {} {} {} {}",
        result.test_id,
        result.outcome as u8,
        result.tone,
        result.frames_sent,
        result.frames_returned
    ));
}

enum Source {
    Tone { hz: f32, phase: f32, next: Instant },
    Mic(Capture),
    Gated(Arc<Mutex<MicCapture>>),
}

struct Capture {
    samples: Samples,
    resampler: Resampler,
    pending: Vec<f32>,
}

impl Capture {
    fn new(samples: Samples, rate: u32) -> Self {
        Self {
            samples,
            resampler: Resampler::new(rate, OPUS_RATE),
            pending: Vec::new(),
        }
    }

    fn try_frame(&mut self, n: usize) -> Option<Vec<f32>> {
        if self.pending.len() < n {
            let captured: Vec<f32> = self.samples.lock().unwrap().drain(..).collect();
            self.resampler.process(&captured, &mut self.pending);
        }
        (self.pending.len() >= n).then(|| self.pending.drain(..n).collect())
    }
}

#[derive(Default)]
struct MicCapture {
    input: Option<Capture>,
    generation: u64,
}

impl MicCapture {
    fn accepts(&self, generation: Option<u64>) -> bool {
        self.input.is_some() && generation == Some(self.generation)
    }
}

// generic over the owned device handle so tests can observe its drop without a real microphone
struct Microphone<S = cpal::Stream> {
    stream: Option<S>,
    capture: Arc<Mutex<MicCapture>>,
    retry_at: Option<Instant>,
}

impl<S> Default for Microphone<S> {
    fn default() -> Self {
        Self {
            stream: None,
            capture: Arc::default(),
            retry_at: None,
        }
    }
}

impl<S> Microphone<S> {
    fn update(
        &mut self,
        shared: &Shared,
        now: Instant,
        open: impl FnOnce(Samples) -> Result<(S, u32), Box<dyn Error>>,
    ) {
        if shared.mic_close.swap(false, Relaxed) {
            self.close(shared, "transmit policy changed");
        }
        if !shared.runtime.lock().unwrap().0.microphone_permitted(now) {
            self.retry_at = None;
            self.close(shared, "radio off, transmit not permitted or policy lost");
            return;
        }
        if self.stream.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        let samples = Samples::default();
        match open(samples.clone()) {
            Ok((stream, rate)) => {
                let mut capture = self.capture.lock().unwrap();
                // permission may have changed while the device was opening
                let runtime = shared.runtime.lock().unwrap();
                if !runtime.0.microphone_permitted(Instant::now()) || shared.mic_close.load(Relaxed)
                {
                    return;
                }
                shared.echo.reset();
                capture.generation = capture.generation.wrapping_add(1);
                capture.input = Some(Capture::new(samples, rate));
                self.stream = Some(stream);
                self.retry_at = None;
                println!("mic opened");
            }
            Err(err) => {
                eprintln!("mic open failed: {err}; retrying in 1 s");
                self.retry_at = Some(now + MIC_RETRY);
            }
        }
    }

    fn close(&mut self, shared: &Shared, reason: &str) {
        if self.stream.is_none() {
            return;
        }
        // the same fence spans the transmit worker's final generation check and send
        let mut capture = self.capture.lock().unwrap();
        let (runtime, network) = &mut *shared.runtime.lock().unwrap();
        runtime.close_transmit();
        drain_runtime(shared, runtime, network, None);
        capture.input = None;
        capture.generation = capture.generation.wrapping_add(1);
        drop(self.stream.take());
        shared.echo.reset();
        send_echo_state(shared, false);
        println!("mic closed ({reason})");
    }
}

struct Captured {
    pcm: Vec<f32>,
    generation: Option<u64>,
}

impl Source {
    fn mic(samples: Samples, rate: u32) -> Self {
        Source::Mic(Capture::new(samples, rate))
    }

    /// blocks until the next frame of `n` samples at 48 kHz is ready; None once shutdown starts
    fn next_frame(&mut self, n: usize, done: &AtomicBool) -> Option<Captured> {
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
                (!done.load(Relaxed)).then_some(Captured {
                    pcm: frame,
                    generation: None,
                })
            }
            Source::Mic(input) => loop {
                if done.load(Relaxed) {
                    return None;
                }
                if let Some(pcm) = input.try_frame(n) {
                    return Some(Captured {
                        pcm,
                        generation: None,
                    });
                }
                thread::sleep(Duration::from_millis(1));
            },
            Source::Gated(capture) => loop {
                if done.load(Relaxed) {
                    return None;
                }
                let delay = {
                    let mut capture = capture.lock().unwrap();
                    let generation = capture.generation;
                    if let Some(input) = &mut capture.input {
                        if let Some(pcm) = input.try_frame(n) {
                            return Some(Captured {
                                pcm,
                                generation: Some(generation),
                            });
                        }
                        Duration::from_millis(1)
                    } else {
                        Duration::from_millis(10)
                    }
                };
                thread::sleep(delay);
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
    let mut gate = false;
    let mut generation = None;
    let mut test_on_air = false;
    while let Some(Captured {
        mut pcm,
        generation: captured_generation,
    }) = source.next_frame(tx.frame_samples(), &shared.done)
    {
        let capture = match &source {
            Source::Gated(capture) => Some(capture.lock().unwrap()),
            _ => None,
        };
        if let Some(capture) = &capture {
            if !capture.accepts(captured_generation) {
                continue;
            }
            if generation != captured_generation {
                tx.set_policy(false, None, 0);
                generation = captured_generation;
                frame = 0;
                gate = false;
            }
        }
        let ready_us = unix_us();
        shared.echo.near_end(&mut pcm);
        let state = shared.echo.gate_state();
        if state.engaged != gate {
            send_echo_state(shared, state.engaged);
            shared.trace(format_args!(
                "aec_gate {} {} {} {}",
                state.engaged as u8, state.score, state.lag_ms, state.capture_sample
            ));
            shared.trace(format_args!(
                "canceller {} {}",
                echo::CANCELLER,
                if state.engaged {
                    echo::ALGORITHMIC_DELAY_MS
                } else {
                    0.0
                }
            ));
            gate = state.engaged;
        }
        tx.set_ptt(shared.ptt.load(Relaxed));
        tx.set_open_mic(shared.open_mic.load(Relaxed));
        let (runtime, network) = &mut *shared.runtime.lock().unwrap();
        if capture.is_some()
            && (!runtime.microphone_permitted(Instant::now()) || shared.mic_close.load(Relaxed))
        {
            continue;
        }
        let (permitted, revision, reset) = runtime.capture_policy();
        tx.set_policy(permitted, revision, reset);
        // a self-test records through this same gate, then sends its recording in place of
        // the input as one talkspurt of this transmitter; shared.ptt stays untouched. it sees
        // the fence this frame passed, so a deny and allow between polls drops its recording.
        // lock order: capture, runtime, then self_test as the leaf
        let now = Instant::now();
        let fence = Fence {
            reset,
            generation: captured_generation,
        };
        let test = shared
            .self_test
            .lock()
            .unwrap()
            .frame(&pcm, now, permitted, fence);
        let testing = matches!(test, Frame::Send { .. });
        let packets = match test {
            Frame::Send { pcm, last } => {
                if !test_on_air {
                    tx.clear_preroll();
                }
                test_on_air = !last;
                tx.set_ptt(!last);
                tx.set_open_mic(false);
                tx.frame(&pcm)
            }
            Frame::Live => {
                // a test that ended early still closes its talkspurt first
                let mut packets = if std::mem::take(&mut test_on_air) {
                    tx.close()
                } else {
                    Vec::new()
                };
                packets.extend(tx.frame(&pcm));
                packets
            }
        };
        let voices = send_packets(shared, runtime, network, packets, frame, ready_us);
        if testing {
            shared.self_test.lock().unwrap().sent(voices, now);
        }
        sent += voices;
        frame += 1;
    }
    if matches!(source, Source::Gated(_)) {
        // the main thread closed and flushed the talk before dropping the device
        return sent;
    }
    let (runtime, network) = &mut *shared.runtime.lock().unwrap();
    sent + send_packets(shared, runtime, network, tx.close(), frame, unix_us())
}

/// sends what the transmitter made of captured frame `frame`; returns the VOICE count
fn send_packets(
    shared: &Shared,
    runtime: &mut Runtime,
    network: &mut Network,
    packets: Vec<Vec<u8>>,
    frame: u64,
    ready_us: u64,
) -> u64 {
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
                runtime.transmit(&p, Instant::now());
                drain_runtime(shared, runtime, network, None);
                shared.trace(format_args!(
                    "tx {seq} {voice_frame} {} {ready_us} {}",
                    (voice_frame < frame) as u8,
                    p.len() - 5
                ));
                voice_frame += 1;
            }
            kind => {
                println!("sent {}", if kind == START { "START" } else { "STOP" });
                runtime.transmit(&p, Instant::now());
                drain_runtime(shared, runtime, network, None);
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

fn drain_runtime(
    shared: &Shared,
    runtime: &mut Runtime,
    network: &mut Network,
    mut stats: Option<&mut RxStats>,
) {
    while let Some(output) = runtime.poll() {
        match output {
            Output::Game(bytes) => send(shared, &bytes),
            Output::Udp(tx) => {
                if let Err(err) = network.send(&tx) {
                    shared.trace(format_args!("drop 0 socket-send"));
                    eprintln!("UDP send failed: {err}");
                }
            }
            Output::Select(stream) => {
                let mut mixer = shared.mixer.lock().unwrap();
                mixer.enforce(|sender, revision| runtime.accepts_media(sender, revision));
                let received = mixer.select(stream, Instant::now());
                drop(mixer);
                for (sender, sequence) in received {
                    shared.trace(format_args!("rx {sender} {sequence}"));
                    if let Some(stats) = stats.as_deref_mut() {
                        stats.senders.entry(sender).or_default().voice += 1;
                    }
                }
            }
            Output::Reorder(player, wait) => {
                shared.mixer.lock().unwrap().reorder_wait(player, wait)
            }
            Output::Purge { all, players } => {
                shared.mixer.lock().unwrap().purge(all, &players);
                let dropped = shared.output.get().map_or(0, |(samples, _)| {
                    let mut queue = samples.lock().unwrap();
                    let (dropped, reset) = shared
                        .output_history
                        .lock()
                        .unwrap()
                        .purge(&mut queue, all, &players);
                    if reset {
                        shared.output_reset.fetch_add(1, Relaxed);
                    }
                    dropped
                });
                let enforced_us = unix_us();
                shared.trace(format_args!(
                    "policy_purge {enforced_us} {} {dropped}",
                    runtime.playout_reset()
                ));
            }
            Output::ResetStreams => shared.mixer.lock().unwrap().reset_streams(),
            Output::Trace(text) => shared.trace(format_args!("{text}")),
            Output::Media(media) => {
                accept_media(shared, runtime, media, true, stats.as_deref_mut())
            }
            Output::UdpMedia(media) => {
                accept_media(shared, runtime, media, false, stats.as_deref_mut())
            }
        }
    }
}

fn accept_media(
    shared: &Shared,
    runtime: &Runtime,
    media: Media,
    game: bool,
    stats: Option<&mut RxStats>,
) {
    let now = Instant::now();
    let outcome = {
        let mut mixer = shared.mixer.lock().unwrap();
        mixer.enforce(|sender, revision| runtime.accepts_media(sender, revision));
        if game {
            mixer.receive_game_at(media.borrowed(), media.revision, now)
        } else {
            mixer.receive_authenticated_at(media.borrowed(), media.revision, now)
        }
    };
    let taken = outcome == MediaOutcome::Accepted;
    // only a self-monitor POLICY lets the own stream back in
    if taken && media.kind == Kind::Voice && runtime.local_player() == Some(media.stream.player) {
        shared.self_test.lock().unwrap().returned(now);
    }
    if media.kind == Kind::Voice {
        if taken {
            shared.trace(format_args!(
                "rx {} {}",
                media.stream.player, media.sequence
            ));
        } else if outcome == MediaOutcome::Held {
            shared.trace(format_args!(
                "hold {} media {}",
                media.stream.player, media.sequence
            ));
        } else if outcome == MediaOutcome::Duplicate {
            shared.trace(format_args!(
                "duplicate {} media {}",
                media.stream.player, media.sequence
            ));
        } else if let MediaOutcome::Dropped(reason) = outcome {
            shared.trace(format_args!(
                "drop {} media {} {reason}",
                media.stream.player, media.sequence
            ));
        }
    }
    if let Some(stats) = stats {
        let s = stats.senders.entry(media.stream.player).or_default();
        match media.kind {
            Kind::Start => s.starts += u64::from(taken),
            Kind::Stop => s.stops += u64::from(taken),
            Kind::Voice => {
                s.voice += u64::from(taken);
                let mut sent = shared.sent.lock().unwrap();
                if sent
                    .get(&media.sequence)
                    .is_some_and(|(_, body)| *body == media.opus)
                {
                    let (at, _) = sent.remove(&media.sequence).unwrap();
                    stats.rtt_ms.push((now - at).as_secs_f64() * 1000.0);
                }
            }
            _ => {}
        }
    }
}

fn game_signal(
    shared: &Shared,
    runtime: &mut Runtime,
    network: &mut Network,
    bytes: &[u8],
    now: Instant,
    mut stats: Option<&mut RxStats>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    runtime.game_with_capture_close(bytes, now, |runtime, permitted| {
        if shared.real_mic {
            if !permitted {
                shared.mic_close.store(true, Relaxed);
            }
            runtime.close_transmit();
            drain_runtime(shared, runtime, network, stats.as_deref_mut());
        }
    })
}

fn receive(shared: &Shared, mut resolver: Option<Resolver>) -> RxStats {
    let mut stats = RxStats::default();
    let mut hud = Reporter::default();
    let mut open_mic: Option<bool> = None;
    let mut buf = [0u8; 65536];
    while !shared.done.load(Relaxed) {
        if let Some(result) = resolver.as_mut().and_then(|r| r.poll(Instant::now())) {
            resolver = None;
            match result {
                Ok(servers) => {
                    if let Err(err) = shared
                        .runtime
                        .lock()
                        .unwrap()
                        .0
                        .stun_servers(servers, Instant::now())
                    {
                        eprintln!("STUN discovery: {err}");
                    }
                }
                Err(err) => eprintln!("STUN discovery unavailable: {err}"),
            }
        }
        {
            let (runtime, network) = &mut *shared.runtime.lock().unwrap();
            for _ in 0..256 {
                match network.receive(&mut buf) {
                    Ok(Some((source, destination, len, now))) => {
                        if let Err(err) = runtime.udp(source, destination, &buf[..len], now) {
                            shared.trace(format_args!("drop 0 udp-runtime"));
                            eprintln!("UDP runtime: {err}");
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        eprintln!("UDP receive: {err}");
                        break;
                    }
                }
            }
            if let Err(err) = runtime.tick(
                Instant::now(),
                shared.ptt.load(Relaxed) || shared.open_mic.load(Relaxed),
            ) {
                eprintln!("transport tick: {err}");
            }
            drain_runtime(shared, runtime, network, Some(&mut stats));
            let now = Instant::now();
            let mut active = shared.mixer.lock().unwrap().hud_status(now);
            for record in &mut active {
                record.selected_path(runtime.selected_path(record.sender));
            }
            if let Some(packet) = hud.batch(&active, now) {
                send(shared, &packet);
            }
        }
        let len = match shared.socket.recv_from(&mut buf) {
            Ok((len, sender)) if sender.ip() == shared.game.ip() => len,
            Ok(_) => {
                shared.trace(format_args!("drop 0 game-source"));
                continue;
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => {
                eprintln!("receive failed: {err}");
                continue;
            }
        };
        let now = Instant::now();
        {
            let (runtime, network) = &mut *shared.runtime.lock().unwrap();
            let consumed =
                match game_signal(shared, runtime, network, &buf[..len], now, Some(&mut stats)) {
                    Ok(consumed) => consumed,
                    Err(err) => {
                        eprintln!("game signaling: {err}");
                        true
                    }
                };
            drain_runtime(shared, runtime, network, Some(&mut stats));
            if consumed {
                continue;
            }
        }
        // the generic parser does not know SELF_TEST; the main thread starts the run
        if let Some(id) = packet::parse_self_test(&buf[..len]) {
            let busy = shared.self_test.lock().unwrap().request(id);
            if let Some(result) = busy {
                send_self_test_result(shared, &result);
            }
            continue;
        }
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
                let (runtime, network) = &mut *shared.runtime.lock().unwrap();
                if let Err(err) = runtime.roster(roster.iter().map(|p| p.index).collect(), now) {
                    eprintln!("transport roster: {err}");
                }
                drain_runtime(shared, runtime, network, Some(&mut stats));
                if roster != stats.roster {
                    println!("roster:");
                    for p in &roster {
                        println!("  player {:>3}  {:<24} rgb({})", p.index, p.name, p.color);
                    }
                    stats.roster = roster;
                }
            }
            Incoming::MicState { player, open } => {
                let (runtime, network) = &mut *shared.runtime.lock().unwrap();
                if let Err(err) = runtime.identify(player, now) {
                    eprintln!("transport identity: {err}");
                }
                drain_runtime(shared, runtime, network, Some(&mut stats));
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
                let runtime = shared.runtime.lock().unwrap();
                if !runtime.0.allows_receive(sender) {
                    continue;
                }
                let s = stats.senders.entry(sender).or_default();
                s.starts += 1;
                s.last_seq = Some(seq);
                shared.mixer.lock().unwrap().start(sender, now);
            }
            Incoming::Voice { sender, seq, frame } => {
                let runtime = shared.runtime.lock().unwrap();
                if !runtime.0.allows_receive(sender) {
                    continue;
                }
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
                if shared.mixer.lock().unwrap().voice(sender, seq, frame, now) {
                    shared.trace(format_args!("rx {sender} {seq}"));
                }
            }
            Incoming::Stop { sender, .. } => {
                let runtime = shared.runtime.lock().unwrap();
                if !runtime.0.allows_receive(sender) {
                    continue;
                }
                stats.senders.entry(sender).or_default().stops += 1;
                shared.mixer.lock().unwrap().stop(sender);
            }
        }
    }
    stats
}

type Playback = (Samples, u32, Option<Arc<AtomicU64>>);

struct PlayoutSlice {
    audible: bool,
    pcm: [f32; SLICE_SAMPLES],
    pulled: Vec<mix::Pulled>,
    slot: u64,
    muted: bool,
}

fn pull_playout(
    shared: &Shared,
    output: &Option<Playback>,
    resampler: &mut Option<Resampler>,
    reset: &mut u64,
    mute: &mut Mute,
) -> PlayoutSlice {
    // admission and rendering share this fence through the final queue append.
    let runtime = shared.runtime.lock().unwrap();
    let epoch = shared.output_reset.load(Relaxed);
    if *reset != epoch {
        *reset = epoch;
        *resampler = output
            .as_ref()
            .map(|(_, rate, _)| Resampler::new(OPUS_RATE, *rate));
    }
    let muted = shared.half_duplex.load(Relaxed)
        && shared.ptt.load(Relaxed)
        && !shared.open_mic.load(Relaxed);
    let mut gains = [1.0; SLICE_SAMPLES];
    mute.apply(muted, &mut gains);
    let audible_samples = gains
        .iter()
        .position(|gain| *gain == 0.0)
        .unwrap_or(SLICE_SAMPLES);
    let (mix, contributions) = {
        let mut mixer = shared.mixer.lock().unwrap();
        let mix = mixer.tick_checked(Instant::now(), audible_samples, |sender, revision| {
            runtime.0.accepts_media(sender, revision)
        });
        let contributions: Vec<_> = mixer
            .pulled
            .iter()
            .filter(|pull| pull.audible_until > 0)
            .map(|pull| (pull.sender, pull.audible_until))
            .collect();
        (mix, contributions)
    };
    let mut pcm = [0.0; SLICE_SAMPLES];
    if let Some(mix) = &mix {
        pcm.copy_from_slice(mix);
    }
    for (sample, gain) in pcm.iter_mut().zip(gains) {
        *sample *= gain;
    }
    // the self-test tone sounds here, so the echo reference below includes it
    shared.self_test.lock().unwrap().tone(&mut pcm);
    shared.echo.far_end(&pcm);
    let mut slot = 0;
    if let (Some((samples, _, popped)), Some(resampler)) = (output, resampler.as_mut()) {
        let mut out = Vec::new();
        let carry_until = resampler.prefix_samples(1);
        let endpoints: Vec<_> = contributions
            .iter()
            .map(|(sender, until)| (*sender, resampler.prefix_samples(until + 1)))
            .collect();
        resampler.process(&pcm, &mut out);
        let mut queue = samples.lock().unwrap();
        let mut history = shared.output_history.lock().unwrap();
        history.synchronize(queue.len());
        let mut contributors: BTreeMap<_, _> = history
            .carry
            .iter()
            .map(|sender| (*sender, carry_until.min(out.len())))
            .collect();
        if pcm.iter().any(|sample| *sample != 0.0) {
            for (sender, until) in endpoints {
                let end = contributors.entry(sender).or_default();
                *end = (*end).max(until.min(out.len()));
            }
        }
        let contributors = contributors
            .into_iter()
            .filter(|(_, until)| *until > 0)
            .collect();
        history.queued.push_back((out.len(), contributors));
        history.carry = if pcm[SLICE_SAMPLES - 1] != 0.0 {
            contributions
                .iter()
                .filter(|(_, until)| *until == SLICE_SAMPLES)
                .map(|(sender, _)| *sender)
                .collect()
        } else {
            Vec::new()
        };
        slot = popped.as_ref().map_or(0, |p| p.load(Relaxed)) + queue.len() as u64;
        queue.extend(&out);
    }
    let pulled = std::mem::take(&mut shared.mixer.lock().unwrap().pulled);
    drop(runtime);
    PlayoutSlice {
        audible: mix.is_some(),
        pcm,
        pulled,
        slot,
        muted,
    }
}

/// returns the number of slices written to the wav
fn playout(
    shared: &Shared,
    output: Option<Playback>,
    mut wav: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
) -> Result<u64, hound::Error> {
    let mut resampler = output
        .as_ref()
        .map(|(_, rate, _)| Resampler::new(OPUS_RATE, *rate));
    let mut next = Instant::now();
    let mut recorded = 0;
    let (mut mute, mut was_muted) = (Mute::default(), false);
    let mut reset = 0;
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
        let PlayoutSlice {
            audible,
            pcm,
            pulled,
            slot,
            muted,
        } = pull_playout(shared, &output, &mut resampler, &mut reset, &mut mute);
        if let (true, Some(writer)) = (audible, wav.as_mut()) {
            let frame = &pcm[..];
            let written = frame.iter().try_for_each(|s| {
                writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            });
            match written {
                Ok(()) => recorded += 1,
                Err(err) => {
                    eprintln!("--record-received: write failed, recording stopped: {err}");
                    wav = None;
                }
            }
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
        "stats: sent {frames_sent} frames | received [{}] | discarded {} missing {} decoded {} decode-errors {} \
         dropped-late {} underruns {} | recorded {:.2} s | {rtt} | ignored packets {}",
        if senders.is_empty() {
            "nothing".to_string()
        } else {
            senders.join("; ")
        },
        mix.discarded,
        mix.missing,
        mix.decoded,
        mix.decode_errors,
        mix.dropped_late,
        mix.underruns,
        recorded as f64 * SLICE.as_secs_f64(),
        rx.ignored
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic_policy(shared: &Shared, revision: u64, permitted: bool, now: Instant) {
        let bytes = signaling::encode_policy(&signaling::Policy {
            audience: Audience {
                revision,
                send_to: vec![],
                receive_from: vec![],
            },
            tx_permitted: permitted,
            self_monitor: false,
        })
        .unwrap();
        let (runtime, network) = &mut *shared.runtime.lock().unwrap();
        game_signal(shared, runtime, network, &bytes, now, None).unwrap();
    }

    #[test]
    fn launch_takes_every_later_argument_as_the_game_command_line() {
        let args = Args::try_parse_from([
            "ptt-radio-companion",
            "--port",
            "1",
            "--launch",
            "/games/factorio",
            "--mp-connect",
            "host",
            "--port",
            "9",
            "-v",
            "--launch",
        ])
        .unwrap();
        assert_eq!(args.port, 1);
        assert_eq!(
            args.launch,
            [
                "/games/factorio",
                "--mp-connect",
                "host",
                "--port",
                "9",
                "-v",
                "--launch"
            ]
        );
        let plain = Args::try_parse_from(["ptt-radio-companion", "--game-port", "7"]).unwrap();
        assert!(plain.launch.is_empty());
        assert!(Args::try_parse_from(["ptt-radio-companion", "--launch"]).is_err());
    }

    #[test]
    fn microphone_starts_closed_retries_nonfatally_and_reopens_with_fresh_capture() {
        let (shared, _) = shared();
        let mut mic = Microphone::<()>::default();
        let now = Instant::now();
        mic.update(&shared, now, |_| panic!("no policy must not open input"));
        mic_policy(&shared, 1, false, now);
        mic.update(&shared, now, |_| panic!("denial must not open input"));
        mic_policy(&shared, 2, true, now);
        mic.update(&shared, now, |_| Err("no input available".into()));
        assert!(mic.stream.is_none());
        mic.update(&shared, now + MIC_RETRY / 2, |_| {
            panic!("retry must back off")
        });
        mic.update(&shared, now + MIC_RETRY, |samples| {
            samples.lock().unwrap().extend([0.5; 2000]);
            Ok(((), 44_100))
        });
        assert!(mic.stream.is_some());
        let mut source = Source::Gated(mic.capture.clone());
        let stale = source.next_frame(960, &shared.done).unwrap();
        assert!(mic.capture.lock().unwrap().accepts(stale.generation));
        assert!(
            !mic.capture
                .lock()
                .unwrap()
                .input
                .as_ref()
                .unwrap()
                .pending
                .is_empty()
        );
        mic_policy(&shared, 3, false, now + MIC_RETRY);
        mic.update(&shared, now + MIC_RETRY, |_| panic!("denial must close"));
        assert!(mic.capture.lock().unwrap().input.is_none());
        assert!(!mic.capture.lock().unwrap().accepts(stale.generation));
        mic_policy(&shared, 4, true, now + MIC_RETRY);
        mic.update(&shared, now + MIC_RETRY, |samples| {
            samples.lock().unwrap().extend([-0.25; 960]);
            Ok(((), OPUS_RATE))
        });
        let capture = mic.capture.lock().unwrap();
        assert!(
            !capture.accepts(stale.generation),
            "already-read old frames are fenced out"
        );
        drop(capture);
        let fresh = source.next_frame(960, &shared.done).unwrap();
        assert_ne!(stale.generation, fresh.generation);
        assert!(
            fresh.pcm.iter().all(|sample| *sample <= 0.0),
            "no old input or resampler carry survives"
        );
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        tx.frame(&stale.pcm);
        tx.set_policy(false, None, 0);
        tx.set_policy(true, Some(4), 4);
        tx.set_ptt(true);
        let packets = tx.frame(&fresh.pcm);
        assert_eq!(
            packets.iter().map(|p| p[2]).collect::<Vec<_>>(),
            [START, VOICE]
        );
    }

    struct StopBeforeDrop {
        game: UdpSocket,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for StopBeforeDrop {
        fn drop(&mut self) {
            let mut buf = [0; 4096];
            let mut stops = Vec::new();
            // loopback delivery is asynchronous on macOS: wait a bounded time for both stops,
            // then drain whatever else already arrived. nothing sends while this drop runs
            self.game.set_nonblocking(false).unwrap();
            self.game
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            loop {
                if stops.len() == 2 {
                    self.game.set_nonblocking(true).unwrap();
                }
                let Ok(len) = self.game.recv(&mut buf) else {
                    break;
                };
                if len >= 3 && buf[2] == ptt_radio_companion::transport::wire::GAME_POLICY_STOP {
                    let mut forwarded = buf[..3].to_vec();
                    forwarded.extend_from_slice(&1u16.to_be_bytes());
                    forwarded.extend_from_slice(&buf[3..len]);
                    let media =
                        ptt_radio_companion::transport::wire::decode_policy_game(&forwarded)
                            .unwrap();
                    stops.push((media.revision, media.media.sequence));
                }
            }
            assert_eq!(
                stops,
                [(1, 17), (1, 17)],
                "wire stops must arrive before stream drop"
            );
            self.dropped.store(true, Relaxed);
        }
    }

    #[test]
    fn denial_and_watchdog_send_stop_before_dropping_the_stream() {
        for watchdog in [false, true] {
            let (mut shared, _) = shared();
            let game = UdpSocket::bind("127.0.0.1:0").unwrap();
            game.set_nonblocking(true).unwrap();
            let owned = Arc::get_mut(&mut shared).unwrap();
            owned.game = game.local_addr().unwrap();
            owned.real_mic = true;
            let now = Instant::now();
            mic_policy(&shared, 1, true, now);
            let dropped = Arc::new(AtomicBool::new(false));
            let mut mic = Microphone::default();
            mic.update(&shared, now, |_| {
                Ok((
                    StopBeforeDrop {
                        game,
                        dropped: dropped.clone(),
                    },
                    OPUS_RATE,
                ))
            });
            {
                let (runtime, network) = &mut *shared.runtime.lock().unwrap();
                runtime.transmit(&packet::encode(START, 17, &[]), now);
                drain_runtime(&shared, runtime, network, None);
            }
            if watchdog {
                let generation = mic.capture.lock().unwrap().generation;
                mic.update(&shared, now + Duration::from_secs(45), |_| {
                    panic!("the open input survives a join freeze")
                });
                assert!(mic.stream.is_some());
                assert_eq!(mic.capture.lock().unwrap().generation, generation);
                assert!(!dropped.load(Relaxed));
                mic.update(&shared, now + Duration::from_secs(60), |_| {
                    panic!("expired policy must close")
                });
            } else {
                mic_policy(&shared, 2, false, now);
                assert!(!shared.runtime.lock().unwrap().0.capture_policy().0);
                mic.update(&shared, now, |_| panic!("denied policy must close"));
            }
            assert!(dropped.load(Relaxed));
            assert!(mic.capture.lock().unwrap().input.is_none());
        }
    }

    #[test]
    fn a_brief_deny_then_allow_still_releases_the_device() {
        let (mut shared, _) = shared();
        Arc::get_mut(&mut shared).unwrap().real_mic = true;
        let mut mic = Microphone::<()>::default();
        let now = Instant::now();
        mic_policy(&shared, 1, true, now);
        mic.update(&shared, now, |_| Ok(((), OPUS_RATE)));
        let generation = mic.capture.lock().unwrap().generation;
        mic_policy(&shared, 2, false, now);
        mic_policy(&shared, 3, true, now);
        mic.update(&shared, now, |_| Ok(((), OPUS_RATE)));
        assert!(mic.capture.lock().unwrap().generation >= generation + 2);
    }

    #[test]
    fn permitted_monitor_and_epoch_changes_keep_the_device_and_capture_timeline() {
        struct Observed(Arc<AtomicBool>);
        impl Drop for Observed {
            fn drop(&mut self) {
                self.0.store(true, Relaxed);
            }
        }
        let (mut shared, _) = shared();
        Arc::get_mut(&mut shared).unwrap().real_mic = true;
        let now = Instant::now();
        mic_policy(&shared, 7, true, now);
        let dropped = Arc::new(AtomicBool::new(false));
        let mut mic = Microphone::default();
        mic.update(&shared, now, |_| Ok((Observed(dropped.clone()), OPUS_RATE)));
        let generation = mic.capture.lock().unwrap().generation;
        let monitor = signaling::encode_policy(&signaling::Policy {
            audience: Audience {
                revision: 8,
                send_to: vec![],
                receive_from: vec![],
            },
            tx_permitted: true,
            self_monitor: true,
        })
        .unwrap();
        {
            let (runtime, network) = &mut *shared.runtime.lock().unwrap();
            game_signal(&shared, runtime, network, &monitor, now, None).unwrap();
        }
        mic.update(&shared, now, |_| {
            panic!("monitor-only change must keep the device")
        });
        assert_eq!(mic.capture.lock().unwrap().generation, generation);
        assert!(!dropped.load(Relaxed));
        shared
            .runtime
            .lock()
            .unwrap()
            .0
            .roster(vec![1], now)
            .unwrap();
        mic_policy(&shared, 1, true, now);
        mic.update(&shared, now, |_| {
            panic!("permitted epoch reset must keep the device")
        });
        assert_eq!(mic.capture.lock().unwrap().generation, generation);
        assert!(!dropped.load(Relaxed));
        mic.close(&shared, "shutdown");
        assert!(dropped.load(Relaxed));
    }

    #[test]
    fn closed_or_empty_real_capture_does_not_block_transmit_shutdown() {
        for open in [false, true] {
            let (shared, _) = shared();
            let capture = Arc::new(Mutex::new(MicCapture {
                input: open.then(|| Capture::new(Samples::default(), OPUS_RATE)),
                generation: 1,
            }));
            let (ready, started) = std::sync::mpsc::channel();
            let (finished, result) = std::sync::mpsc::channel();
            let worker = {
                let shared = shared.clone();
                thread::spawn(move || {
                    ready.send(()).unwrap();
                    finished
                        .send(transmit(&shared, Source::Gated(capture), Codec::default()))
                        .unwrap();
                })
            };
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            thread::sleep(Duration::from_millis(20));
            shared.done.store(true, Relaxed);
            assert_eq!(result.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
            worker.join().unwrap();
        }
    }
    use ptt_radio_companion::transport::signaling::{self, Audience};

    fn shared() -> (Arc<Shared>, Samples) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let game = socket.local_addr().unwrap();
        let network = Network::bind(vec!["127.0.0.1:0".parse().unwrap()], None).unwrap();
        let mut runtime = Runtime::new(
            network.addresses().unwrap(),
            RuntimeConfig {
                game_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        runtime.identify(1, Instant::now()).unwrap();
        runtime
            .game(
                &signaling::encode_audience(&Audience {
                    revision: 1,
                    send_to: vec![],
                    receive_from: vec![2, 3],
                })
                .unwrap(),
                Instant::now(),
            )
            .unwrap();
        while runtime.poll().is_some() {}
        let shared = Arc::new(Shared {
            socket,
            game,
            done: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            open_mic: AtomicBool::new(false),
            half_duplex: AtomicBool::new(false),
            roster_seen: AtomicBool::new(false),
            real_mic: false,
            mic_close: AtomicBool::new(false),
            mixer: Mutex::new(Mixer::default()),
            echo: EchoPath::new(false),
            sent: Mutex::new(HashMap::new()),
            trace: None,
            runtime: Mutex::new((runtime, network)),
            output: OnceLock::new(),
            output_history: Mutex::new(OutputHistory::default()),
            output_reset: AtomicU64::new(0),
            self_test: Mutex::default(),
        });
        let samples = Samples::default();
        shared.output.set((samples.clone(), OPUS_RATE)).unwrap();
        (shared, samples)
    }

    fn remove(shared: &Shared, players: &[u16]) {
        let (runtime, network) = &mut *shared.runtime.lock().unwrap();
        runtime
            .game(
                &signaling::encode_audience(&Audience {
                    revision: 2,
                    send_to: vec![],
                    receive_from: players.to_vec(),
                })
                .unwrap(),
                Instant::now(),
            )
            .unwrap();
        drain_runtime(shared, runtime, network, None);
    }

    fn queue_voice(shared: &Shared, sender: u16, level: f32) {
        let mut tx = Transmitter::new(Codec {
            frame_ms: 10,
            ..Codec::default()
        })
        .unwrap();
        tx.set_ptt(true);
        let packets = tx.frame(&[level; SLICE_SAMPLES]);
        let voice = packets.iter().find(|packet| packet[2] == VOICE).unwrap();
        let now = Instant::now() - Duration::from_millis(100);
        let mut mixer = shared.mixer.lock().unwrap();
        mixer.start(sender, now);
        assert!(mixer.voice(sender, 1, &voice[5..], now));
    }

    #[test]
    fn a_silent_removed_player_preserves_unaffected_queued_audio_and_resampler_carry() {
        let (shared, samples) = shared();
        queue_voice(&shared, 2, 0.25);
        {
            // the existing malformed-opus rule produces exactly zero pcm.
            let now = Instant::now() - Duration::from_millis(100);
            let mut mixer = shared.mixer.lock().unwrap();
            mixer.start(3, now);
            assert!(mixer.voice(3, 1, &[255], now));
            assert_eq!(mixer.stats.decode_errors, 1);
        }
        let rendered = pull_playout(
            &shared,
            &Some((samples.clone(), OPUS_RATE, None)),
            &mut Some(Resampler::new(OPUS_RATE, OPUS_RATE)),
            &mut 0,
            &mut Mute::default(),
        );
        assert!(
            rendered
                .pulled
                .iter()
                .any(|pull| pull.sender == 3 && pull.audible_until == 0)
        );
        let before = samples.lock().unwrap().clone();
        assert!(before.iter().any(|sample| *sample != 0.0));
        remove(&shared, &[2]);
        assert_eq!(*samples.lock().unwrap(), before);
        assert_eq!(shared.output_history.lock().unwrap().carry, [2]);
        assert_eq!(shared.output_reset.load(Relaxed), 0);
    }

    #[test]
    fn consumed_resampler_carry_no_longer_tags_the_remaining_other_speaker_samples() {
        for rate in [44_100, 48_000, 96_000] {
            let (shared, samples) = shared();
            let output = Some((samples.clone(), rate, None));
            let mut resampler = Some(Resampler::new(OPUS_RATE, rate));
            let mut reset = 0;
            let mut mute = Mute::default();
            queue_voice(&shared, 3, 0.25);
            pull_playout(&shared, &output, &mut resampler, &mut reset, &mut mute);
            assert_eq!(shared.output_history.lock().unwrap().carry, [3]);
            samples.lock().unwrap().clear();
            shared.mixer.lock().unwrap().stop(3);
            queue_voice(&shared, 2, 0.3);
            let carry_samples = resampler.as_ref().unwrap().prefix_samples(1);
            if rate >= OPUS_RATE {
                assert!(carry_samples > 0);
            }
            pull_playout(&shared, &output, &mut resampler, &mut reset, &mut mute);
            samples.lock().unwrap().drain(..carry_samples);
            let before = samples.lock().unwrap().clone();
            assert!(before.iter().any(|sample| *sample != 0.0));
            remove(&shared, &[2]);
            assert_eq!(*samples.lock().unwrap(), before, "rate {rate}");
            assert_eq!(shared.output_reset.load(Relaxed), 0, "rate {rate}");
            assert_eq!(shared.output_history.lock().unwrap().carry, [2]);
        }
    }

    #[test]
    fn output_ledger_distinguishes_the_mute_fade_from_a_fully_zero_muted_slice() {
        let (shared, samples) = shared();
        shared.half_duplex.store(true, Relaxed);
        shared.ptt.store(true, Relaxed);
        queue_voice(&shared, 2, 0.25);
        let output = Some((samples.clone(), OPUS_RATE, None));
        let mut resampler = Some(Resampler::new(OPUS_RATE, OPUS_RATE));
        let mut mute = Mute::default();
        pull_playout(&shared, &output, &mut resampler, &mut 0, &mut mute);
        assert!(
            !shared
                .output_history
                .lock()
                .unwrap()
                .queued
                .back()
                .unwrap()
                .1
                .is_empty()
        );
        samples.lock().unwrap().clear();
        pull_playout(&shared, &output, &mut resampler, &mut 0, &mut mute);
        assert!(
            shared
                .output_history
                .lock()
                .unwrap()
                .queued
                .back()
                .unwrap()
                .1
                .is_empty()
        );
        assert!(shared.output_history.lock().unwrap().carry.is_empty());
        assert!(samples.lock().unwrap().iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn revoking_a_contributor_clears_remaining_mixed_audio_and_resampler_carry() {
        let (shared, samples) = shared();
        let queued =
            OPUS_RATE as usize * OUTPUT_AHEAD.as_millis() as usize / 1000 - 1 + SLICE_SAMPLES;
        samples.lock().unwrap().extend(vec![0.25; queued]);
        *shared.output_history.lock().unwrap() = OutputHistory {
            queued: VecDeque::from([
                (SLICE_SAMPLES, vec![(2, SLICE_SAMPLES)]),
                (
                    queued - SLICE_SAMPLES,
                    vec![(2, queued - SLICE_SAMPLES), (3, queued - SLICE_SAMPLES)],
                ),
            ]),
            carry: vec![2, 3],
        };
        assert_eq!(queued, 1439);
        remove(&shared, &[2]);
        // silenced in place: the device timeline the echo reference follows keeps its length
        assert_eq!(samples.lock().unwrap().len(), queued);
        assert!(samples.lock().unwrap().iter().all(|sample| *sample == 0.0));
        let history = shared.output_history.lock().unwrap();
        assert!(history.carry.is_empty());
        assert!(history.queued.iter().all(|(_, senders)| senders.is_empty()));
        assert_eq!(
            history.queued.iter().map(|entry| entry.0).sum::<usize>(),
            queued
        );
        drop(history);
        assert_eq!(shared.output_reset.load(Relaxed), 1);
        println!(
            "mixed output silenced: {queued} samples ({:.6} ms at {OPUS_RATE} Hz)",
            queued as f64 * 1000.0 / OPUS_RATE as f64
        );
        let mut resampler = Some(Resampler::new(OPUS_RATE, OPUS_RATE));
        resampler.as_mut().unwrap().process(&[0.5], &mut Vec::new());
        let slice = pull_playout(
            &shared,
            &Some((samples.clone(), OPUS_RATE, None)),
            &mut resampler,
            &mut 0,
            &mut Mute::default(),
        );
        assert!(!slice.audible);
        assert!(samples.lock().unwrap().iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_pre_purge_pull_cannot_append_after_the_revocation_fence() {
        let (shared, samples) = shared();
        let mut tx = Transmitter::new(Codec::default()).unwrap();
        tx.set_ptt(true);
        let packets = tx.frame(&[0.25; 960]);
        let voice = packets.iter().find(|packet| packet[2] == VOICE).unwrap();
        let now = Instant::now() - Duration::from_millis(100);
        shared.mixer.lock().unwrap().start(2, now);
        assert!(shared.mixer.lock().unwrap().voice(2, 1, &voice[5..], now));
        let queue = samples.lock().unwrap();
        let renderer = {
            let shared = shared.clone();
            let samples = samples.clone();
            thread::spawn(move || {
                pull_playout(
                    &shared,
                    &Some((samples, OPUS_RATE, None)),
                    &mut Some(Resampler::new(OPUS_RATE, OPUS_RATE)),
                    &mut 0,
                    &mut Mute::default(),
                )
            })
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.mixer.lock().unwrap().pulled.is_empty() {
            assert!(Instant::now() < deadline, "renderer never pulled");
            thread::yield_now();
        }
        assert!(
            shared.runtime.try_lock().is_err(),
            "policy fence released before sample append"
        );
        let revoker = {
            let shared = shared.clone();
            thread::spawn(move || remove(&shared, &[]))
        };
        drop(queue);
        assert!(renderer.join().unwrap().audible);
        revoker.join().unwrap();
        let queue = samples.lock().unwrap();
        assert!(!queue.is_empty() && queue.iter().all(|sample| *sample == 0.0));
    }
}
