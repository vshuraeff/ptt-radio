//! stand-in for Factorio and the ptt-radio mod: lets companions talk to each
//! other without the game. see src/relay.rs for what it emulates.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Duration;

use clap::Parser;

use ptt_radio_companion::relay::{Config, Player, Relay, Stall};

#[derive(Parser)]
#[command(about = "Game relay emulator for ptt-radio companions (no Factorio needed)")]
struct Args {
    /// one simulated player: GAME_PORT:COMPANION_PORT (the companion's --game-port and --port);
    /// repeat for more players, numbered 1, 2, ... in order
    #[arg(long = "player", value_name = "GAME:COMPANION", required = true, value_parser = parse_player)]
    players: Vec<(u16, u16)>,
    /// player index whose own voice comes back (mod setting ptt-radio-echo); repeatable
    #[arg(long, value_name = "INDEX")]
    echo: Vec<u16>,
    /// player index whose open mic is on (sent as MIC_STATE on every HELLO); repeatable
    #[arg(long, value_name = "INDEX")]
    open_mic: Vec<u16>,
    /// fixed part of the client -> server -> client delay
    #[arg(long, default_value_t = 72)]
    delay_ms: u64,
    /// random extra delay, uniform in 0..JITTER (order is kept)
    #[arg(long, default_value_t = 10)]
    jitter_ms: u64,
    /// stall delivery (like an autosave) every this many seconds
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    stall_every: Option<f64>,
    /// length of each stall
    #[arg(long, default_value_t = 500)]
    stall_ms: u64,
}

fn parse_seconds(spec: &str) -> Result<f64, String> {
    let seconds: f64 = spec.parse().map_err(|err| format!("{err}"))?;
    match Duration::try_from_secs_f64(seconds) {
        Ok(d) if !d.is_zero() => Ok(seconds),
        _ => Err("must be a positive number of seconds".into()),
    }
}

fn parse_player(spec: &str) -> Result<(u16, u16), String> {
    let (game, companion) = spec
        .split_once(':')
        .ok_or("expected GAME_PORT:COMPANION_PORT")?;
    Ok((
        game.parse().map_err(|err| format!("game port: {err}"))?,
        companion
            .parse()
            .map_err(|err| format!("companion port: {err}"))?,
    ))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let players: Vec<(u16, Player)> = args
        .players
        .iter()
        .enumerate()
        .map(|(i, &(game, companion))| {
            let index = i as u16 + 1;
            (
                game,
                Player {
                    name: format!("player{index}"),
                    companion_port: companion,
                    echo: args.echo.contains(&index),
                    open_mic: args.open_mic.contains(&index),
                },
            )
        })
        .collect();
    let config = Config {
        delay: Duration::from_millis(args.delay_ms),
        jitter: Duration::from_millis(args.jitter_ms),
        stall: args.stall_every.map(|every| Stall {
            at: Duration::from_secs_f64(every),
            len: Duration::from_millis(args.stall_ms),
            every: Some(Duration::from_secs_f64(every)),
        }),
    };
    let relay = match Relay::start(config, players) {
        Ok(relay) => relay,
        Err(err) => {
            eprintln!("error: cannot bind the game ports: {err}");
            return ExitCode::FAILURE;
        }
    };
    for (i, (game, companion)) in args.players.iter().enumerate() {
        println!(
            "player{}: game 127.0.0.1:{game} -> companion 127.0.0.1:{companion}",
            i + 1
        );
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    if let Err(err) = ctrlc::set_handler(move || flag.store(true, Relaxed)) {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }
    while !interrupted.load(Relaxed) {
        thread::sleep(Duration::from_millis(50));
    }
    let report = relay.stop();
    let mut lag: Vec<f64> = report
        .deliveries
        .iter()
        .map(|d| (d.delivered_us - d.received_us) as f64 / 1000.0)
        .collect();
    lag.sort_by(f64::total_cmp);
    println!(
        "relay: hello {} | forwarded {} | ignored {} | stalls {} | delay p50 {:.0} ms p95 {:.0} ms",
        report.hellos,
        report.deliveries.len(),
        report.ignored,
        report.stalls.len(),
        ptt_radio_companion::percentile(&lag, 50),
        ptt_radio_companion::percentile(&lag, 95),
    );
    ExitCode::SUCCESS
}
