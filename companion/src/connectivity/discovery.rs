use super::{ConnectivityError, DatagramTx, valid_socket};
use crate::transport::wire::{BindChallenge, BindRequest};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubReflection {
    pub endpoint: SocketAddr,
    pub session: u32,
    pub hub_generation: u64,
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// when present, BIND is used instead of public STUN. the relay owner
    /// authenticates the existing wire protocol, including the final ACK.
    pub hub: Option<HubReflection>,
    pub stun_servers: Vec<SocketAddr>,
    pub timeout: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            hub: None,
            stun_servers: Vec::new(),
            timeout: Duration::from_millis(800),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryAction {
    Send(DatagramTx),
    /// delegate the authenticated BIND exchange to the relay implementation;
    /// do not send a second reflection protocol or trust an unauthenticated ACK.
    BeginHubBinding {
        source: SocketAddr,
        hub: SocketAddr,
        request: BindRequest,
    },
}

enum Request {
    Stun {
        source: SocketAddr,
        server: SocketAddr,
        transaction: [u8; 12],
    },
    Hub {
        source: SocketAddr,
        hub: SocketAddr,
        request: BindRequest,
    },
}

pub(super) struct Discovery {
    pending: Vec<Request>,
    deadline: Instant,
    complete: bool,
}

impl Discovery {
    pub(super) fn start(
        player: u16,
        sockets: &[SocketAddr],
        config: &DiscoveryConfig,
        now: Instant,
    ) -> Result<(Self, Vec<DiscoveryAction>), ConnectivityError> {
        if config.timeout.is_zero()
            || config.timeout > Duration::from_secs(2)
            || config.stun_servers.len() > 4
        {
            return Err(ConnectivityError::Configuration);
        }
        if config.stun_servers.iter().any(|s| !valid_socket(*s))
            || config.hub.is_some_and(|h| !valid_socket(h.endpoint))
        {
            return Err(ConnectivityError::Configuration);
        }
        let mut pending = Vec::new();
        let mut actions = Vec::new();
        for &source in sockets {
            if let Some(hub) = config.hub {
                if source.is_ipv4() != hub.endpoint.is_ipv4() {
                    continue;
                }
                let mut nonce = [0; 32];
                getrandom::fill(&mut nonce)?;
                let mut attempt = [0; 8];
                getrandom::fill(&mut attempt)?;
                let request = BindRequest {
                    session: hub.session,
                    hub_generation: hub.hub_generation,
                    player,
                    attempt: u64::from_be_bytes(attempt),
                    client_nonce: nonce,
                };
                pending.push(Request::Hub {
                    source,
                    hub: hub.endpoint,
                    request,
                });
                actions.push(DiscoveryAction::BeginHubBinding {
                    source,
                    hub: hub.endpoint,
                    request,
                });
            } else {
                for &server in &config.stun_servers {
                    if source.is_ipv4() != server.is_ipv4() {
                        continue;
                    }
                    let mut transaction = [0; 12];
                    getrandom::fill(&mut transaction)?;
                    let mut contents = vec![0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42];
                    contents.extend_from_slice(&transaction);
                    pending.push(Request::Stun {
                        source,
                        server,
                        transaction,
                    });
                    actions.push(DiscoveryAction::Send(DatagramTx {
                        source,
                        destination: server,
                        contents,
                    }));
                }
            }
        }
        let complete = pending.is_empty();
        Ok((
            Self {
                pending,
                deadline: now + config.timeout,
                complete,
            },
            actions,
        ))
    }

    pub(super) fn complete(&self) -> bool {
        self.complete
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        (!self.complete).then_some(self.deadline)
    }

    pub(super) fn expire(&mut self, now: Instant) -> bool {
        if self.complete || now < self.deadline {
            return false;
        }
        self.pending.clear();
        self.complete = true;
        true
    }

    pub(super) fn stun_response(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
        now: Instant,
    ) -> Option<SocketAddr> {
        if self.complete || now >= self.deadline {
            return None;
        }
        let mapped = parse_reflection(bytes)?;
        if !valid_socket(mapped) || mapped.is_ipv4() != destination.is_ipv4() {
            return None;
        }
        let index = self.pending.iter().position(|r| matches!(r, Request::Stun { source: local, server, transaction } if *local == destination && *server == source && bytes.get(8..20) == Some(transaction.as_slice())))?;
        self.pending.remove(index);
        self.complete = self.pending.is_empty();
        Some(mapped)
    }

    /// the relay owner has verified the ACK HMAC, grant, freshness and source
    /// challenge. this matches its transcript to the outstanding local request.
    pub(super) fn authenticated_hub_response(
        &mut self,
        source: SocketAddr,
        hub: SocketAddr,
        challenge: BindChallenge,
        now: Instant,
    ) -> Option<SocketAddr> {
        if self.complete
            || now >= self.deadline
            || !valid_socket(challenge.endpoint)
            || challenge.endpoint.is_ipv4() != source.is_ipv4()
        {
            return None;
        }
        let index = self.pending.iter().position(|r| matches!(r, Request::Hub { source: local, hub: server, request } if *local == source && *server == hub && *request == challenge.request))?;
        self.pending.remove(index);
        self.complete = self.pending.is_empty();
        Some(challenge.endpoint)
    }
}

// the pinned ICE parser requires message-integrity even on binding replies;
// public STUN does not. discovery only decodes a correlated xor-mapped address,
// which is untrusted candidate data until the separate ICE checks succeed.
fn parse_reflection(bytes: &[u8]) -> Option<SocketAddr> {
    if bytes.get(..2)? != [1, 1] {
        return None;
    }
    let transaction = bytes.get(8..20)?;
    let mut attributes = bytes.get(20..)?;
    let mut mapped = None;
    while !attributes.is_empty() {
        let header = attributes.get(..4)?;
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let value = attributes.get(4..4 + len)?;
        attributes = attributes.get(4 + len.div_ceil(4) * 4..)?;
        if kind != 0x0020 {
            continue;
        }
        if mapped.is_some() || value.first() != Some(&0) {
            return None;
        }
        let port = u16::from_be_bytes(value.get(2..4)?.try_into().ok()?) ^ 0x2112;
        let ip = match value.get(1)? {
            1 if value.len() == 8 => {
                let encoded = u32::from_be_bytes(value[4..8].try_into().ok()?) ^ 0x2112_a442;
                IpAddr::V4(Ipv4Addr::from(encoded))
            }
            2 if value.len() == 20 => {
                let mut octets: [u8; 16] = value[4..20].try_into().ok()?;
                let mask = [0x21, 0x12, 0xa4, 0x42]
                    .into_iter()
                    .chain(transaction.iter().copied());
                for (byte, mask) in octets.iter_mut().zip(mask) {
                    *byte ^= mask;
                }
                IpAddr::V6(Ipv6Addr::from(octets))
            }
            _ => return None,
        };
        mapped = Some(SocketAddr::new(ip, port));
    }
    mapped
}
