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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_attempt_floor_updates_the_exact_reflection_transcript() {
        let now = Instant::now();
        let source = "10.0.0.1:5000".parse().unwrap();
        let (mut discovery, initial) = Discovery::start(1, &[source], &config(), now).unwrap();
        let DiscoveryAction::BeginHubBinding { hub, request, .. } = initial[0] else {
            panic!("hub request")
        };
        let original = request;
        let request = discovery
            .hub_request(source, hub, original, u64::MAX)
            .unwrap();
        assert_eq!(request.attempt, u64::MAX);
        assert_eq!(request.client_nonce, original.client_nonce);
        let challenge = |request| BindChallenge {
            request,
            endpoint: "198.51.100.1:5000".parse().unwrap(),
            challenge: [1; 32],
            expires_at_ms: u64::MAX,
        };
        if original != request {
            assert!(
                discovery
                    .authenticated_hub_response(source, hub, challenge(original), now)
                    .is_none()
            );
        }
        assert!(
            discovery
                .authenticated_hub_response(source, hub, challenge(request), now)
                .is_some()
        );
        assert!(discovery.complete());
    }

    fn config() -> DiscoveryConfig {
        DiscoveryConfig {
            hub: Some(HubReflection {
                endpoint: "203.0.113.9:34560".parse().unwrap(),
                session: 7,
                hub_generation: 8,
            }),
            stun_servers: vec![
                "203.0.113.10:3478".parse().unwrap(),
                "[2001:db8::10]:3478".parse().unwrap(),
            ],
            timeout: Duration::from_millis(800),
        }
    }

    #[test]
    fn unreachable_hub_falls_back_for_every_affected_socket() {
        let now = Instant::now();
        let sockets = vec![
            "10.0.0.1:5000".parse().unwrap(),
            "[2001:db8::1]:5000".parse().unwrap(),
        ];
        let (mut discovery, initial) = Discovery::start(1, &sockets, &config(), now).unwrap();
        assert_eq!(initial.len(), 1);
        assert!(
            discovery
                .fallback(now + Duration::from_millis(799))
                .unwrap()
                .is_none()
        );
        let fallback = discovery
            .fallback(now + Duration::from_millis(800))
            .unwrap()
            .unwrap();
        assert_eq!(fallback.len(), 2);
        for action in fallback {
            assert!(matches!(action, DiscoveryAction::Send(_)));
        }
        assert!(!discovery.complete());
        assert!(discovery.expire(now + Duration::from_millis(1600)));
        assert!(discovery.complete());
    }

    #[test]
    fn successful_hub_leaves_public_fallback_only_for_the_unserved_family() {
        let now = Instant::now();
        let sockets = vec![
            "10.0.0.1:5000".parse().unwrap(),
            "[2001:db8::1]:5000".parse().unwrap(),
        ];
        let config = config();
        let (mut discovery, initial) = Discovery::start(1, &sockets, &config, now).unwrap();
        let DiscoveryAction::BeginHubBinding {
            source,
            hub,
            request,
        } = initial[0]
        else {
            panic!("hub request")
        };
        let mapped = "198.51.100.1:5000".parse().unwrap();
        assert_eq!(
            discovery.authenticated_hub_response(
                source,
                hub,
                BindChallenge {
                    request,
                    endpoint: mapped,
                    challenge: [1; 32],
                    expires_at_ms: u64::MAX
                },
                now
            ),
            Some(mapped)
        );
        let fallback = discovery.fallback(now + config.timeout).unwrap().unwrap();
        assert_eq!(fallback.len(), 1);
        assert!(
            matches!(&fallback[0],DiscoveryAction::Send(tx) if tx.source.is_ipv6() && tx.destination.is_ipv6())
        );
    }
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
    fallback: Option<(u16, Vec<SocketAddr>, DiscoveryConfig)>,
    reflected: Vec<SocketAddr>,
}

impl Discovery {
    pub(super) fn validate(config: &DiscoveryConfig) -> Result<(), ConnectivityError> {
        if config.timeout.is_zero()
            || config.timeout > Duration::from_secs(2)
            || config.stun_servers.len() > 4
            || config.stun_servers.iter().any(|s| !valid_socket(*s))
            || config.hub.is_some_and(|h| !valid_socket(h.endpoint))
        {
            return Err(ConnectivityError::Configuration);
        }
        Ok(())
    }

    pub(super) fn start(
        player: u16,
        sockets: &[SocketAddr],
        config: &DiscoveryConfig,
        now: Instant,
    ) -> Result<(Self, Vec<DiscoveryAction>), ConnectivityError> {
        Self::validate(config)?;
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
        let fallback = config.hub.map(|_| {
            let mut public = config.clone();
            public.hub = None;
            (player, sockets.to_vec(), public)
        });
        let complete = pending.is_empty() && fallback.is_none();
        Ok((
            Self {
                pending,
                deadline: now + config.timeout,
                complete,
                fallback,
                reflected: Vec::new(),
            },
            actions,
        ))
    }

    pub(super) fn complete(&self) -> bool {
        self.complete
    }

    pub(super) fn hub_request(
        &mut self,
        source: SocketAddr,
        hub: SocketAddr,
        original: BindRequest,
        minimum: u64,
    ) -> Option<BindRequest> {
        self.pending.iter_mut().find_map(|pending| match pending {
            Request::Hub {
                source: local,
                hub: server,
                request,
            } if *local == source && *server == hub && *request == original => {
                request.attempt = request.attempt.max(minimum);
                Some(*request)
            }
            _ => None,
        })
    }

    pub(super) fn public_servers(&mut self, servers: &[SocketAddr]) -> bool {
        if let Some((_, _, config)) = &mut self.fallback {
            config.stun_servers = servers.to_vec();
            true
        } else {
            false
        }
    }

    pub(super) fn missing_sockets(&self, sockets: &[SocketAddr]) -> Vec<SocketAddr> {
        sockets
            .iter()
            .filter(|s| !self.reflected.contains(s))
            .copied()
            .collect()
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

    pub(super) fn fallback(
        &mut self,
        now: Instant,
    ) -> Result<Option<Vec<DiscoveryAction>>, ConnectivityError> {
        if now < self.deadline {
            return Ok(None);
        }
        let Some((player, sockets, config)) = self.fallback.take() else {
            return Ok(None);
        };
        let missing = self.missing_sockets(&sockets);
        let (mut replacement, actions) = Self::start(player, &missing, &config, now)?;
        replacement.reflected.append(&mut self.reflected);
        *self = replacement;
        Ok(Some(actions))
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
        self.reflected.push(destination);
        self.complete = self.pending.is_empty()
            && self
                .fallback
                .as_ref()
                .is_none_or(|(_, sockets, _)| sockets.iter().all(|s| self.reflected.contains(s)));
        if self.complete {
            self.fallback = None;
        }
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
        self.reflected.push(source);
        self.complete = self.pending.is_empty()
            && self
                .fallback
                .as_ref()
                .is_none_or(|(_, sockets, _)| sockets.iter().all(|s| self.reflected.contains(s)));
        if self.complete {
            self.fallback = None;
        }
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
