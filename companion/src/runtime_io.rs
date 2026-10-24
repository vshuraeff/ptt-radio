//! socket adapter shared by the normal runtime pump and the lab envelope.

use std::io::{self, ErrorKind};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::connectivity::DatagramTx;
use crate::transport::lab::LabPath;
use crate::transport::{Path, Transport};

const DNS_LIMIT: Duration = Duration::from_secs(2);

/// one optional lookup job per process. the OS resolver can outlive the result
/// deadline; no pump or shutdown path joins it, and process exit ends the worker.
pub struct Resolver {
    result: Receiver<io::Result<Vec<SocketAddr>>>,
    deadline: Instant,
    done: bool,
}

impl Resolver {
    pub fn start(names: Vec<String>, now: Instant) -> io::Result<Self> {
        let (resolver, _worker) = Self::spawn(now, move || {
            let mut addresses = Vec::new();
            for name in names.into_iter().take(4) {
                match name.to_socket_addrs() {
                    Ok(found) => addresses.extend(found.take(4 - addresses.len())),
                    Err(err) => eprintln!("STUN discovery unavailable for {name}: {err}"),
                }
                if addresses.len() == 4 {
                    break;
                }
            }
            Ok(addresses)
        })?;
        Ok(resolver)
    }

    fn spawn(
        now: Instant,
        lookup: impl FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
    ) -> io::Result<(Self, JoinHandle<()>)> {
        let (send, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("stun-dns".into())
            .spawn(move || {
                // the receiver may have timed out or the companion may be shutting down.
                let _ = send.send(lookup());
            })?;
        Ok((
            Self {
                result,
                deadline: now + DNS_LIMIT,
                done: false,
            },
            worker,
        ))
    }

    pub fn poll(&mut self, now: Instant) -> Option<io::Result<Vec<SocketAddr>>> {
        if self.done {
            return None;
        }
        // a result that already arrived is delivered even past the deadline, which
        // starts before audio startup; only an empty channel can time out.
        let result = match self.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) if now >= self.deadline => Err(io::Error::new(
                ErrorKind::TimedOut,
                "STUN DNS result deadline",
            )),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(io::Error::other("STUN DNS worker ended")),
        };
        self.done = true;
        Some(result)
    }
}

pub enum Network {
    Udp(Vec<UdpSocket>),
    Lab(LabPath),
}

impl Network {
    pub fn bind(
        mut addresses: Vec<SocketAddr>,
        lab: Option<(SocketAddr, SocketAddr)>,
    ) -> io::Result<Self> {
        if let Some((local, router)) = lab {
            return Ok(Self::Lab(LabPath::bind(local, router)?));
        }
        if addresses.is_empty() {
            for target in ["192.0.2.1:9", "[2001:db8::1]:9"] {
                let target: SocketAddr = target.parse().unwrap();
                let bind = if target.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                if let Ok(socket) = UdpSocket::bind(bind)
                    && socket.connect(target).is_ok()
                    && let Ok(local) = socket.local_addr()
                {
                    addresses.push(SocketAddr::new(local.ip(), 0));
                }
            }
            if addresses.is_empty() {
                addresses.push("127.0.0.1:0".parse().unwrap());
            }
        }
        if addresses.len() > 4 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "at most four UDP sockets",
            ));
        }
        let sockets = addresses
            .into_iter()
            .map(|address| {
                let socket = UdpSocket::bind(address)?;
                socket.set_nonblocking(true)?;
                Ok(socket)
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self::Udp(sockets))
    }

    pub fn addresses(&self) -> io::Result<Vec<SocketAddr>> {
        match self {
            Self::Udp(sockets) => sockets.iter().map(UdpSocket::local_addr).collect(),
            Self::Lab(path) => Ok(vec![path.local_addr()?]),
        }
    }

    pub fn send(&mut self, tx: &DatagramTx) -> io::Result<()> {
        match self {
            Self::Udp(sockets) => {
                let socket = sockets
                    .iter()
                    .find(|s| s.local_addr().ok() == Some(tx.source))
                    .ok_or_else(|| {
                        io::Error::new(ErrorKind::AddrNotAvailable, "UDP source not bound")
                    })?;
                socket.send_to(&tx.contents, tx.destination)?;
            }
            Self::Lab(path) => {
                let SocketAddr::V4(destination) = tx.destination else {
                    return Err(io::Error::new(ErrorKind::InvalidInput, "lab is IPv4"));
                };
                path.set_route(0, Path::Direct, destination);
                path.send(0, Path::Direct, &tx.contents)?;
            }
        }
        Ok(())
    }

    pub fn receive(
        &mut self,
        buffer: &mut [u8],
    ) -> io::Result<Option<(SocketAddr, SocketAddr, usize, Instant)>> {
        match self {
            Self::Udp(sockets) => {
                for socket in sockets {
                    match socket.recv_from(buffer) {
                        Ok((len, source)) => {
                            return Ok(Some((source, socket.local_addr()?, len, Instant::now())));
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(None)
            }
            Self::Lab(path) => {
                let Some(received) = path.receive(buffer)? else {
                    return Ok(None);
                };
                let source = path
                    .last_source()
                    .ok_or_else(|| io::Error::other("lab source missing"))?;
                Ok(Some((
                    source.into(),
                    path.local_addr()?,
                    received.length,
                    received.received_at,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        packet,
        runtime::{Output, Runtime, RuntimeConfig},
        transport::wire,
    };

    #[test]
    fn blocked_resolver_does_not_block_game_deadline_or_shutdown() {
        let now = Instant::now();
        let (release, blocked) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        let (mut resolver, worker) = Resolver::spawn(now, move || {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(vec!["127.0.0.1:3478".parse().unwrap()])
        })
        .unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(resolver.poll(now).is_none());
        let mut runtime = Runtime::new(
            vec!["127.0.0.1:41001".parse().unwrap()],
            RuntimeConfig::default(),
        )
        .unwrap();
        runtime.transmit(&packet::encode(packet::VOICE, 1, &[1]), now);
        assert!(
            std::iter::from_fn(|| runtime.poll())
                .any(|o| matches!(o, Output::Game(bytes) if bytes[2] == wire::GAME_VOICE))
        );
        assert_eq!(
            resolver.poll(now + DNS_LIMIT).unwrap().unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        assert!(resolver.poll(now + DNS_LIMIT).is_none());
        drop(resolver);
        release.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn resolver_delivers_one_bounded_result() {
        let now = Instant::now();
        let address = "127.0.0.1:3478".parse().unwrap();
        let (mut resolver, worker) = Resolver::spawn(now, move || Ok(vec![address])).unwrap();
        worker.join().unwrap();
        assert_eq!(resolver.poll(now).unwrap().unwrap(), vec![address]);
        assert!(resolver.poll(now).is_none());
    }

    #[test]
    fn ready_result_is_delivered_after_the_deadline() {
        let now = Instant::now();
        let (mut resolver, worker) =
            Resolver::spawn(now, || Ok("127.0.0.1:3478".to_socket_addrs()?.collect())).unwrap();
        worker.join().unwrap();
        let found = resolver.poll(now + DNS_LIMIT * 2).unwrap().unwrap();
        assert_eq!(found, vec!["127.0.0.1:3478".parse().unwrap()]);
        assert!(resolver.poll(now + DNS_LIMIT * 2).is_none());
    }
}
