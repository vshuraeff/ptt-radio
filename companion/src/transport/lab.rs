//! stand-only `Transport` for the p2p network lab (tests/netlab/router.rs): every datagram
//! goes through one local udp socket to the router's inside address. the router envelope is a
//! 6-byte public address (ipv4 + port, big-endian) in front of the payload: the destination
//! when sending, the source when receiving. enabled only by `--netlab`; see DESIGN.md.

use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Instant;

use super::wire::Kind;
use super::{Path, Received, Transport};

/// envelope prefix length, equal to the router's `PREFIX`
pub const PREFIX: usize = 6;
/// the router drops whole datagrams (prefix included) above this
pub const MAX_DATAGRAM: usize = PREFIX + 1500;

pub fn encode_addr(addr: SocketAddrV4) -> [u8; PREFIX] {
    let mut out = [0; PREFIX];
    out[..4].copy_from_slice(&addr.ip().octets());
    out[4..].copy_from_slice(&addr.port().to_be_bytes());
    out
}

pub fn decode_addr(bytes: &[u8]) -> Option<SocketAddrV4> {
    let head: &[u8; PREFIX] = bytes.get(..PREFIX)?.try_into().ok()?;
    let ip = Ipv4Addr::new(head[0], head[1], head[2], head[3]);
    Some(SocketAddrV4::new(
        ip,
        u16::from_be_bytes([head[4], head[5]]),
    ))
}

/// name of a datagram by its first byte, for router traces; the payload stays opaque.
/// 0..=3 are STUN (top two bits of a STUN message type are zero, RFC 8489), the rest are
/// the wire.rs types.
pub fn label(first: Option<u8>) -> &'static str {
    let Some(byte) = first else {
        return "EMPTY";
    };
    match byte {
        0..=3 => "STUN",
        0x10 => "RELAY",
        0x11 => "BIND",
        0x12 => "BIND-ACK",
        _ => match Kind::try_from(byte) {
            Ok(Kind::Voice) => "VOICE",
            Ok(Kind::Start) => "START",
            Ok(Kind::Stop) => "STOP",
            Ok(Kind::Ping) => "PING",
            Ok(Kind::Pong) => "PONG",
            Err(_) => "UNKNOWN",
        },
    }
}

pub struct LabPath {
    socket: UdpSocket,
    router: SocketAddr,
    routes: HashMap<(u16, Path), SocketAddrV4>,
    last_source: Option<SocketAddrV4>,
    buf: Vec<u8>,
}

impl LabPath {
    /// binds the companion's one lab socket (non-blocking); `local` must be the private
    /// address the router was told about, because it delivers there
    pub fn bind(local: SocketAddr, router: SocketAddr) -> std::io::Result<LabPath> {
        let socket = UdpSocket::bind(local)?;
        socket.set_nonblocking(true)?;
        Ok(LabPath {
            socket,
            router,
            routes: HashMap::new(),
            last_source: None,
            // one byte over the limit, so an oversize datagram is seen rather than truncated
            buf: vec![0; MAX_DATAGRAM + 1],
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// the public address a `send(peer, path, ..)` is addressed to
    pub fn set_route(&mut self, peer: u16, path: Path, public: SocketAddrV4) {
        self.routes.insert((peer, path), public);
    }

    /// public source of the datagram the last `receive` returned
    pub fn last_source(&self) -> Option<SocketAddrV4> {
        self.last_source
    }
}

impl Transport for LabPath {
    type Error = Error;

    fn send(&mut self, peer: u16, path: Path, packet: &[u8]) -> Result<(), Error> {
        let to = *self.routes.get(&(peer, path)).ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                format!("no lab route for peer {peer} on {path:?}"),
            )
        })?;
        let mut datagram = Vec::with_capacity(PREFIX + packet.len());
        datagram.extend_from_slice(&encode_addr(to));
        datagram.extend_from_slice(packet);
        self.socket.send_to(&datagram, self.router)?;
        Ok(())
    }

    /// reports the path whose route has the datagram's public source, `Direct` when none
    /// has; the source itself is `last_source`. datagrams not sent by the configured router,
    /// longer than `MAX_DATAGRAM` or without an envelope are skipped, leaving `last_source`
    /// untouched. when several routes share the source endpoint (two relay sessions at one
    /// hub address) the relay wrapper's session picks the route; if that does not single one
    /// out the path is `Direct`, never an arbitrary map entry.
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<Received>, Error> {
        loop {
            let (len, from) = match self.socket.recv_from(&mut self.buf) {
                Ok(got) => got,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            };
            if from != self.router || len > MAX_DATAGRAM {
                continue;
            }
            let Some(source) = decode_addr(&self.buf[..len]) else {
                continue;
            };
            let payload = &self.buf[PREFIX..len];
            let Some(target) = buffer.get_mut(..payload.len()) else {
                return Err(Error::new(ErrorKind::InvalidInput, "buffer too small"));
            };
            target.copy_from_slice(payload);
            self.last_source = Some(source);
            let path = resolve(&self.routes, source, payload);
            return Ok(Some(Received {
                path,
                length: payload.len(),
                received_at: Instant::now(),
            }));
        }
    }
}

/// the path of the one route at `source`; among several, the one whose relay session equals
/// the session in the payload's relay wrapper (0x10, u32 big-endian, see wire.rs)
fn resolve(
    routes: &HashMap<(u16, Path), SocketAddrV4>,
    source: SocketAddrV4,
    payload: &[u8],
) -> Path {
    let at_source: Vec<Path> = routes
        .iter()
        .filter(|(_, public)| **public == source)
        .map(|((_, path), _)| *path)
        .collect();
    if at_source.len() < 2 {
        return at_source.first().copied().unwrap_or(Path::Direct);
    }
    let &[0x10, a, b, c, d, ..] = payload else {
        return Path::Direct;
    };
    let wrapped = u32::from_be_bytes([a, b, c, d]);
    let named: Vec<Path> = at_source
        .into_iter()
        .filter(|path| {
            matches!(path, Path::Hub { session } | Path::PlayerRelay { session, .. } if *session == wrapped)
        })
        .collect();
    match named[..] {
        [path] => path,
        _ => Path::Direct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips() {
        let addr = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 40123);
        assert_eq!(decode_addr(&encode_addr(addr)), Some(addr));
        assert_eq!(decode_addr(&[1, 2, 3]), None);
    }

    #[test]
    fn send_without_a_route_fails() {
        let local = SocketAddr::from(([127, 0, 0, 1], 0));
        let mut path = LabPath::bind(local, local).unwrap();
        assert!(path.send(1, Path::Direct, b"x").is_err());
    }
}
