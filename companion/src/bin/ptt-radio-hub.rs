//! ptt-radio-hub: the relay beside the game server. it speaks to the game as player 0
//! over the server's Lua UDP port and forwards relay datagrams between companions that
//! bound to a grant. see DESIGN.md, "Relay hub".

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use clap::Parser;

use ptt_radio_companion::transport::hub::{HELLO_EVERY_MS, Hub, registration};
use ptt_radio_companion::transport::relay::{Limits, Reply, fresh_generation};
use ptt_radio_companion::unix_us;

#[derive(Parser)]
#[command(about = "Relay hub for ptt-radio companions, beside the Factorio server")]
struct Args {
    /// public UDP address companions bind to and relay through
    #[arg(long, default_value = "0.0.0.0:34560")]
    public: SocketAddr,
    /// the server's Lua UDP address (its --enable-lua-udp port)
    #[arg(long, default_value = "127.0.0.1:34556")]
    game: SocketAddr,
    /// local address the mod delivers hub SIGNAL to; its port is registered by HELLO
    #[arg(long, default_value = "127.0.0.1:34557")]
    receive: SocketAddr,
    /// lifetime of a relay grant
    #[arg(long, default_value_t = 1800)]
    grant_ttl_s: u64,
    /// live grants one player may hold
    #[arg(long, default_value_t = 64)]
    player_grants: usize,
    /// bytes per second the hub sends on behalf of one player
    #[arg(long, default_value_t = 64 * 1024)]
    player_rate: u64,
}

fn now_ms() -> u64 {
    unix_us() / 1000
}

fn main() -> ExitCode {
    let args = Args::parse();
    let limits = Limits {
        grant_ms: args.grant_ttl_s.saturating_mul(1000),
        grants_per_player: args.player_grants,
        bytes_per_second: args.player_rate,
    };
    let Some(generation) = fresh_generation() else {
        eprintln!("error: no OS random source");
        return ExitCode::FAILURE;
    };
    let public = match UdpSocket::bind(args.public) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("error: cannot bind public {}: {err}", args.public);
            return ExitCode::FAILURE;
        }
    };
    let game = match UdpSocket::bind(args.receive) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("error: cannot bind receive {}: {err}", args.receive);
            return ExitCode::FAILURE;
        }
    };
    // the public socket paces the loop; the game link is drained between datagrams
    if let Err(err) = public
        .set_read_timeout(Some(Duration::from_millis(5)))
        .and_then(|()| game.set_nonblocking(true))
    {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }
    let hello = registration(game.local_addr().map(|a| a.port()).unwrap_or(0));
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    if let Err(err) = ctrlc::set_handler(move || flag.store(true, Relaxed)) {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }
    println!(
        "hub: public {} | game {} via {}",
        args.public, args.game, args.receive
    );

    let mut hub = Hub::new(generation, limits);
    let mut forwarded = 0u64;
    let mut replies = 0u64;
    let mut signals = 0u64;
    let mut dropped: BTreeMap<String, u64> = BTreeMap::new();
    let mut next_hello = 0;
    let mut next_tick = 0;
    // larger than any valid datagram, so a truncated oversize one still fails to decode
    let mut buf = [0u8; 2048];
    // udp sends below are best effort: a failed send is a lost datagram, which every
    // sender already retries or tolerates
    while !interrupted.load(Relaxed) {
        match public.recv_from(&mut buf) {
            Ok((len, source)) => {
                let data = &buf[..len];
                match hub.relay.handle(now_ms(), source, data) {
                    Ok(Reply::Forward(to)) => {
                        forwarded += 1;
                        let _ = public.send_to(data, to);
                    }
                    Ok(Reply::Send(to, bytes)) => {
                        replies += 1;
                        let _ = public.send_to(&bytes, to);
                    }
                    Err(why) => *dropped.entry(format!("{why:?}")).or_default() += 1,
                }
            }
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) => {
                // e.g. icmp port unreachable surfacing on some platforms; keep serving
                *dropped.entry(format!("{:?}", err.kind())).or_default() += 1;
            }
        }
        let now = now_ms();
        let mut frames = Vec::new();
        for _ in 0..64 {
            match game.recv_from(&mut buf) {
                // the mod's send_udp arrives from the server's own address
                Ok((len, from)) if from.ip() == args.game.ip() => {
                    frames.extend(hub.on_game(now, &buf[..len]))
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if now >= next_tick {
            frames.extend(hub.tick(now));
            next_tick = now + 1000;
        }
        for frame in frames {
            signals += 1;
            let _ = game.send_to(&frame.frame, args.game);
        }
        if now >= next_hello {
            let _ = game.send_to(&hello, args.game);
            next_hello = now + HELLO_EVERY_MS;
        }
    }
    let drops: Vec<String> = dropped.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!(
        "hub: forwarded {forwarded} | bind replies {replies} | signals {signals} | refused grants {} | dropped: {}",
        hub.failed_offers,
        if drops.is_empty() {
            "none".into()
        } else {
            drops.join(", ")
        }
    );
    ExitCode::SUCCESS
}
