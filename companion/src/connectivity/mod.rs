//! sans-i/o full ICE per selected peer, discovery and authenticated route probes.
//! the socket owner sends each datagram from its exact source, drives deadline(),
//! and authenticates application traffic before calling the probe methods.

mod discovery;
mod probes;

pub use discovery::{DiscoveryAction, DiscoveryConfig, HubReflection};
pub use probes::{
    IDLE_DEAD, IDLE_INTERVAL, ProbeEvent, ProbeScheduler, RouteId, TALK_DEAD, TALK_INTERVAL,
};

use crate::transport::signaling::{
    self, Announce, IceCredentials, MAX_CANDIDATE_BYTES, MAX_CANDIDATES, MAX_CREDENTIAL_BYTES,
    Signal,
};
use crate::transport::wire::{BindChallenge, MAX_DATAGRAM};
use crate::transport::{Path, StreamId};
use discovery::Discovery;
use is::stun::{StunMessage, StunPacket};
use is::{
    Candidate, CandidateKind, IceAgent, IceAgentEvent, IceConnectionState, IceCreds, Protocol,
};
use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

const SIGNAL_RETRY_INITIAL: Duration = Duration::from_secs(1);
const SIGNAL_RETRY_CAP: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityError {
    Configuration,
    Candidate,
    Credentials,
    GenerationExhausted,
    Random,
}

impl fmt::Display for ConnectivityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Configuration => "invalid connectivity configuration",
            Self::Candidate => "invalid ICE candidate",
            Self::Credentials => "invalid ICE credentials",
            Self::GenerationExhausted => "ICE restart generation exhausted",
            Self::Random => "OS random source failed",
        })
    }
}
impl std::error::Error for ConnectivityError {}
impl From<getrandom::Error> for ConnectivityError {
    fn from(_: getrandom::Error) -> Self {
        Self::Random
    }
}

#[derive(Clone, Debug)]
pub struct ConnectivityConfig {
    pub discovery: DiscoveryConfig,
    /// a bounded attempt emits a fallback notification, while ICE continues
    /// checking for later recovery. game/relay media policy belongs to U20.
    pub attempt_timeout: Duration,
}
impl Default for ConnectivityConfig {
    fn default() -> Self {
        Self {
            discovery: DiscoveryConfig::default(),
            attempt_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatagramTx {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub contents: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectRoute {
    pub id: RouteId,
    pub source: SocketAddr,
    pub destination: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Send(DatagramTx),
    Discovery(DiscoveryAction),
    /// retry the identical cached ANNOUNCE for this local restart generation,
    /// built from local_credentials(), initial_candidates() and
    /// initial_gathering_complete(). never replace its key-bound snapshot with
    /// the growing candidate set. retries use 1/2/4/8 s backoff until connected.
    PublishDescription,
    /// send candidate_signals(), independently of the frozen ANNOUNCE snapshot.
    PublishCandidates,
    DirectNominated(DirectRoute),
    DirectInvalidated(RouteId),
    AttemptTimedOut,
    DiscoveryComplete,
    Probe(ProbeEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalResult {
    Applied,
    Duplicate,
    Stale,
    WrongStream,
    Unsupported,
    Unrelated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramClass {
    Stun,
    Application,
    Invalid,
}

/// RFC 7983 range plus cookie, header and aligned exact body length. malformed
/// reserved-range packets never fall through to the encrypted media decoder.
pub fn classify_datagram(bytes: &[u8]) -> DatagramClass {
    let Some(&first) = bytes.first() else {
        return DatagramClass::Invalid;
    };
    if bytes.len() > MAX_DATAGRAM {
        return DatagramClass::Invalid;
    }
    if first > 3 {
        return DatagramClass::Application;
    }
    if bytes.len() < 20 || bytes[4..8] != [0x21, 0x12, 0xa4, 0x42] {
        return DatagramClass::Invalid;
    }
    let body = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    if body % 4 != 0 || bytes.len() != 20 + body {
        return DatagramClass::Invalid;
    }
    DatagramClass::Stun
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveResult {
    Dropped,
    Stun,
    Reflection,
    /// endpoint consent only. bytes remain untrusted until crypto/audience
    /// validation; this result must never be treated as authenticated media.
    UntrustedApplication(DirectRoute),
}

struct RemoteDescription {
    credentials: IceCredentials,
    candidates: Vec<Candidate>,
    complete: bool,
    announced_candidates: Vec<String>,
    announced_complete: bool,
}

/// one full agent per pair; only the latest nominated tuple is a direct route.
/// no arbitrary non-nominated-pair send contract is inferred from diagnostics.
pub struct PairConnectivity {
    local: StreamId,
    remote: StreamId,
    sockets: Vec<SocketAddr>,
    config: ConnectivityConfig,
    agent: IceAgent,
    restart_generation: u32,
    initial_candidates: Vec<String>,
    initial_complete: bool,
    remote_description: Option<RemoteDescription>,
    discovery: Discovery,
    direct: Option<DirectRoute>,
    probes: ProbeScheduler,
    actions: VecDeque<Action>,
    attempt_deadline: Instant,
    attempt_expired: bool,
    next_publish: Instant,
    publish_backoff: Duration,
}

fn valid_socket(addr: SocketAddr) -> bool {
    addr.port() != 0
        && !addr.ip().is_unspecified()
        && !addr.ip().is_multicast()
        && !matches!(addr.ip(), IpAddr::V4(ip) if ip.is_broadcast() || ip.is_link_local())
}

fn secure_credentials() -> Result<IceCreds, ConnectivityError> {
    let mut bytes = [0; 48];
    getrandom::fill(&mut bytes)?;
    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        bytes
            .iter()
            .flat_map(|b| {
                [
                    DIGITS[usize::from(b >> 4)] as char,
                    DIGITS[usize::from(b & 15)] as char,
                ]
            })
            .collect()
    }
    Ok(IceCreds {
        ufrag: hex(&bytes[..16]),
        pass: hex(&bytes[16..]),
    })
}

fn parsed_candidates(
    strings: &[String],
    credentials: &IceCredentials,
) -> Result<Vec<Candidate>, ConnectivityError> {
    if strings.len() > MAX_CANDIDATES {
        return Err(ConnectivityError::Candidate);
    }
    strings
        .iter()
        .map(|s| {
            if s.len() > MAX_CANDIDATE_BYTES {
                return Err(ConnectivityError::Candidate);
            }
            let candidate =
                Candidate::from_sdp_string(s).map_err(|_| ConnectivityError::Candidate)?;
            if candidate.proto() != Protocol::Udp
                || s.split_ascii_whitespace().nth(1) != Some("1")
                || !valid_socket(candidate.addr())
                || candidate.ufrag().is_some_and(|u| u != credentials.ufrag)
            {
                return Err(ConnectivityError::Candidate);
            }
            Ok(candidate)
        })
        .collect()
}

fn valid_credentials(credentials: &IceCredentials) -> bool {
    let valid = |s: &str, min| {
        s.len() >= min
            && s.len() <= MAX_CREDENTIAL_BYTES
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    };
    valid(&credentials.ufrag, 4) && valid(&credentials.password, 22)
}

impl PairConnectivity {
    /// selected_remote is supplied by the trusted ANNOUNCE owner. a delayed
    /// packet cannot select a new stream; call select_remote explicitly instead.
    pub fn new(
        local: StreamId,
        selected_remote: StreamId,
        sockets: Vec<SocketAddr>,
        config: ConnectivityConfig,
        now: Instant,
    ) -> Result<Self, ConnectivityError> {
        if local.player == selected_remote.player
            || sockets.is_empty()
            || sockets.len() > 4
            || sockets.iter().any(|s| !valid_socket(*s))
            || sockets
                .iter()
                .enumerate()
                .any(|(i, s)| sockets[..i].contains(s))
            || config.attempt_timeout.is_zero()
            || config.attempt_timeout > Duration::from_secs(30)
        {
            return Err(ConnectivityError::Configuration);
        }
        let mut agent = Self::agent(local, selected_remote, secure_credentials()?);
        for &socket in &sockets {
            agent.add_local_candidate(
                Candidate::host(socket, "udp").map_err(|_| ConnectivityError::Candidate)?,
            );
        }
        let (discovery, actions) =
            Discovery::start(local.player, &sockets, &config.discovery, now)?;
        let complete = discovery.complete();
        let initial_candidates = agent
            .local_candidates()
            .map(|c| c.to_sdp_string())
            .collect();
        let mut this = Self {
            local,
            remote: selected_remote,
            sockets,
            config: config.clone(),
            agent,
            restart_generation: 0,
            initial_candidates,
            initial_complete: complete,
            remote_description: None,
            discovery,
            direct: None,
            probes: ProbeScheduler::default(),
            actions: actions.into_iter().map(Action::Discovery).collect(),
            attempt_deadline: now + config.attempt_timeout,
            attempt_expired: false,
            next_publish: now + SIGNAL_RETRY_INITIAL,
            publish_backoff: SIGNAL_RETRY_INITIAL,
        };
        this.actions.push_back(Action::PublishDescription);
        if complete {
            this.actions.push_back(Action::DiscoveryComplete);
        }
        this.agent.handle_timeout(now);
        Ok(this)
    }

    fn agent(local: StreamId, remote: StreamId, credentials: IceCreds) -> IceAgent {
        let mut agent = IceAgent::new(credentials);
        agent.set_controlling(signaling::is_initiator(local.player, remote.player));
        agent.set_max_candidate_pairs(64);
        agent.set_mtu(MAX_DATAGRAM..=MAX_DATAGRAM);
        agent
    }

    pub fn local_credentials(&self) -> IceCredentials {
        let credentials = self.agent.local_credentials();
        IceCredentials {
            restart_generation: self.restart_generation,
            ufrag: credentials.ufrag.clone(),
            password: credentials.pass.clone(),
        }
    }

    pub fn local_candidates(&self) -> Vec<String> {
        self.agent
            .local_candidates()
            .take(MAX_CANDIDATES)
            .map(|c| c.to_sdp_string())
            .collect()
    }

    pub fn initial_candidates(&self) -> &[String] {
        &self.initial_candidates
    }

    pub fn initial_gathering_complete(&self) -> bool {
        self.initial_complete
    }

    /// chunk through the existing codec, including the game envelope's bytes.
    /// only the final chunk declares gathering complete; aggregate count stays
    /// bounded even when full IPv6 candidates need more than one SIGNAL.
    pub fn candidate_signals(&self) -> Result<Vec<Signal>, ConnectivityError> {
        let make_signal = |candidates, complete| Signal::IceCandidates {
            stream_generation: self.local.generation,
            restart_generation: self.restart_generation,
            candidates,
            complete,
        };
        let mut signals = Vec::new();
        let mut chunk = Vec::new();
        for candidate in self.local_candidates() {
            chunk.push(candidate);
            if signaling::encode_game(
                &make_signal(chunk.clone(), false),
                self.remote.player,
                false,
            )
            .is_err()
            {
                let candidate = chunk.pop().expect("candidate just added");
                if chunk.is_empty() {
                    return Err(ConnectivityError::Candidate);
                }
                signals.push(make_signal(std::mem::take(&mut chunk), false));
                chunk.push(candidate);
            }
        }
        let last = make_signal(chunk, self.gathering_complete());
        signaling::encode_game(&last, self.remote.player, false)
            .map_err(|_| ConnectivityError::Candidate)?;
        signals.push(last);
        Ok(signals)
    }

    pub fn gathering_complete(&self) -> bool {
        self.discovery.complete()
    }

    pub fn direct(&self) -> Option<DirectRoute> {
        self.direct
    }

    pub fn probes(&self) -> &ProbeScheduler {
        &self.probes
    }

    pub fn probes_mut(&mut self) -> &mut ProbeScheduler {
        &mut self.probes
    }

    /// diagnostic counts include real ICE-learned peer-reflexive candidates.
    pub fn candidate_count(&self, local: bool, kind: CandidateKind) -> usize {
        if local {
            self.agent
                .local_candidates()
                .filter(|c| c.kind() == kind)
                .count()
        } else {
            self.agent
                .remote_candidates()
                .filter(|c| c.kind() == kind)
                .count()
        }
    }

    pub fn poll_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    pub fn deadline(&mut self) -> Option<Instant> {
        let mut deadlines = vec![
            self.agent.poll_timeout(),
            self.discovery.deadline(),
            self.probes.deadline(),
        ];
        if !self.attempt_expired && self.direct.is_none() {
            deadlines.push(Some(self.attempt_deadline));
        }
        if self.direct.is_none() {
            deadlines.push(Some(self.next_publish));
        }
        deadlines.into_iter().flatten().min()
    }

    fn invalidate_direct(&mut self) {
        if let Some(route) = self.direct.take() {
            self.probes.remove(Path::Direct);
            self.actions.push_back(Action::DirectInvalidated(route.id));
        }
        self.actions.retain(|a| match a {
            Action::Send(_) | Action::DirectNominated(_) => false,
            Action::Probe(event) => event.route().path != Path::Direct,
            _ => true,
        });
    }

    fn reset_checklist(&mut self, credentials: IceCreds, keep_reflexive: bool, now: Instant) {
        let candidates: Vec<_> = self
            .agent
            .local_candidates()
            .filter(|c| keep_reflexive || c.kind() == CandidateKind::Host)
            .collect();
        self.invalidate_direct();
        self.agent = Self::agent(self.local, self.remote, credentials);
        for candidate in candidates {
            self.agent.add_local_candidate(candidate);
        }
        if let Some(remote) = &self.remote_description {
            self.agent.set_remote_credentials(IceCreds {
                ufrag: remote.credentials.ufrag.clone(),
                pass: remote.credentials.password.clone(),
            });
            for candidate in &remote.candidates {
                self.agent.add_remote_candidate(candidate.clone());
            }
        }
        self.attempt_deadline = now + self.config.attempt_timeout;
        self.attempt_expired = false;
        self.reset_publication(now);
        self.agent.handle_timeout(now);
    }

    fn reset_publication(&mut self, now: Instant) {
        self.publish_backoff = SIGNAL_RETRY_INITIAL;
        self.next_publish = now + SIGNAL_RETRY_INITIAL;
    }

    /// local interface/NAT restart; credentials and discovery transactions are
    /// replaced, and late previous-generation probes cannot restore old routes.
    pub fn restart(&mut self, now: Instant) -> Result<(), ConnectivityError> {
        let generation = self
            .restart_generation
            .checked_add(1)
            .ok_or(ConnectivityError::GenerationExhausted)?;
        let credentials = secure_credentials()?;
        let (discovery, actions) = Discovery::start(
            self.local.player,
            &self.sockets,
            &self.config.discovery,
            now,
        )?;
        self.reset_checklist(credentials, false, now);
        self.restart_generation = generation;
        self.discovery = discovery;
        self.initial_candidates = self.local_candidates();
        self.initial_complete = self.discovery.complete();
        self.actions.retain(|a| {
            !matches!(
                a,
                Action::Discovery(_)
                    | Action::DiscoveryComplete
                    | Action::PublishDescription
                    | Action::PublishCandidates
            )
        });
        self.actions
            .extend(actions.into_iter().map(Action::Discovery));
        self.actions.push_back(Action::PublishDescription);
        if self.discovery.complete() {
            self.actions.push_back(Action::DiscoveryComplete);
        }
        Ok(())
    }

    /// only the trusted stream-selection owner calls this after accepting a
    /// companion restart. receiving an old ANNOUNCE here never performs it.
    pub fn select_remote(
        &mut self,
        remote: StreamId,
        now: Instant,
    ) -> Result<(), ConnectivityError> {
        if remote.player != self.remote.player {
            return Err(ConnectivityError::Configuration);
        }
        if remote == self.remote {
            return Ok(());
        }
        self.remote = remote;
        self.remote_description = None;
        self.actions.retain(|a| !matches!(a, Action::Probe(_)));
        for route in self.probes.clear() {
            self.actions
                .push_back(Action::Probe(ProbeEvent::Unavailable { route }));
        }
        self.reset_checklist(self.agent.local_credentials().clone(), true, now);
        Ok(())
    }

    /// the caller binds `sender` to the game-derived SIGNAL sender, not an
    /// asserted player field in an untrusted UDP packet.
    pub fn handle_signal(
        &mut self,
        sender: u16,
        signal: &Signal,
        now: Instant,
    ) -> Result<SignalResult, ConnectivityError> {
        if sender != self.remote.player {
            return Ok(SignalResult::WrongStream);
        }
        match signal {
            Signal::Announce(a) => self.handle_announce(a, now),
            Signal::IceCandidates {
                stream_generation,
                restart_generation,
                candidates,
                complete,
            } => {
                if *stream_generation != self.remote.generation {
                    return Ok(SignalResult::WrongStream);
                }
                let Some(remote) = &self.remote_description else {
                    return Ok(SignalResult::Stale);
                };
                if *restart_generation != remote.credentials.restart_generation {
                    return Ok(SignalResult::Stale);
                }
                let parsed = parsed_candidates(candidates, &remote.credentials)?;
                let mut added = Vec::new();
                for candidate in parsed {
                    if !remote.candidates.contains(&candidate) && !added.contains(&candidate) {
                        added.push(candidate);
                    }
                }
                if remote.candidates.len() + added.len() > MAX_CANDIDATES {
                    return Err(ConnectivityError::Candidate);
                }
                let changed = !added.is_empty() || (*complete && !remote.complete);
                let remote = self
                    .remote_description
                    .as_mut()
                    .expect("selected description");
                remote.complete |= *complete;
                for candidate in added {
                    remote.candidates.push(candidate.clone());
                    self.agent.add_remote_candidate(candidate);
                }
                Ok(if changed {
                    SignalResult::Applied
                } else {
                    SignalResult::Duplicate
                })
            }
            _ => Ok(SignalResult::Unrelated),
        }
    }

    fn handle_announce(
        &mut self,
        announce: &Announce,
        now: Instant,
    ) -> Result<SignalResult, ConnectivityError> {
        if announce.stream_generation != self.remote.generation {
            return Ok(SignalResult::WrongStream);
        }
        if announce.version != signaling::PROTOCOL_VERSION
            || announce.capabilities & signaling::CAP_UDP == 0
        {
            return Ok(SignalResult::Unsupported);
        }
        if !valid_credentials(&announce.ice) {
            return Err(ConnectivityError::Credentials);
        }
        let candidates = parsed_candidates(&announce.candidates, &announce.ice)?;
        if let Some(current) = &self.remote_description {
            if announce.ice.restart_generation < current.credentials.restart_generation {
                return Ok(SignalResult::Stale);
            }
            if announce.ice.restart_generation == current.credentials.restart_generation {
                if announce.ice != current.credentials {
                    return Err(ConnectivityError::Credentials);
                }
                if announce.candidates != current.announced_candidates
                    || announce.candidates_complete != current.announced_complete
                {
                    return Err(ConnectivityError::Candidate);
                }
                return Ok(SignalResult::Duplicate);
            }
        }
        self.remote_description = Some(RemoteDescription {
            credentials: announce.ice.clone(),
            candidates,
            complete: announce.candidates_complete,
            announced_candidates: announce.candidates.clone(),
            announced_complete: announce.candidates_complete,
        });
        self.reset_checklist(self.agent.local_credentials().clone(), true, now);
        Ok(SignalResult::Applied)
    }

    fn reflected(
        &mut self,
        source: SocketAddr,
        mapped: SocketAddr,
        now: Instant,
    ) -> Result<(), ConnectivityError> {
        self.reset_publication(now);
        if self.agent.local_candidates().count() < MAX_CANDIDATES {
            self.agent.add_local_candidate(
                Candidate::server_reflexive(mapped, source, "udp")
                    .map_err(|_| ConnectivityError::Candidate)?,
            );
            self.actions.push_back(Action::PublishCandidates);
        }
        if self.discovery.complete() {
            self.actions.push_back(Action::DiscoveryComplete);
            self.actions.push_back(Action::PublishCandidates);
        }
        Ok(())
    }

    /// U18 supplies this only after authenticating the complete BIND ACK and
    /// grant. raw BIND ACK packets passed to receive() never reach this method.
    pub fn authenticated_hub_reflection(
        &mut self,
        source: SocketAddr,
        hub: SocketAddr,
        challenge: BindChallenge,
        now: Instant,
    ) -> Result<bool, ConnectivityError> {
        let Some(mapped) = self
            .discovery
            .authenticated_hub_response(source, hub, challenge, now)
        else {
            return Ok(false);
        };
        self.reflected(source, mapped, now)?;
        Ok(true)
    }

    pub fn receive(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
        now: Instant,
    ) -> Result<ReceiveResult, ConnectivityError> {
        if !self.sockets.contains(&destination) {
            return Ok(ReceiveResult::Dropped);
        }
        match classify_datagram(bytes) {
            DatagramClass::Invalid => Ok(ReceiveResult::Dropped),
            DatagramClass::Stun => {
                if let Some(mapped) = self
                    .discovery
                    .stun_response(source, destination, bytes, now)
                {
                    self.reflected(destination, mapped, now)?;
                    return Ok(ReceiveResult::Reflection);
                }
                if self.remote_description.is_none() {
                    return Ok(ReceiveResult::Dropped);
                }
                let Ok(message) = StunMessage::parse(bytes) else {
                    return Ok(ReceiveResult::Dropped);
                };
                let packet = StunPacket {
                    proto: Protocol::Udp,
                    source,
                    destination,
                    message,
                };
                if !self.agent.accepts_message(&packet.message)
                    || !self.agent.handle_packet(now, packet)
                {
                    return Ok(ReceiveResult::Dropped);
                }
                self.drain_agent(now);
                Ok(ReceiveResult::Stun)
            }
            DatagramClass::Application => Ok(match self.direct {
                Some(route)
                    if self.agent.state().is_connected()
                        && route.source == destination
                        && route.destination == source =>
                {
                    ReceiveResult::UntrustedApplication(route)
                }
                _ => ReceiveResult::Dropped,
            }),
        }
    }

    fn drain_agent(&mut self, now: Instant) {
        while let Some(event) = self.agent.poll_event() {
            match event {
                IceAgentEvent::NominatedSend {
                    proto: Protocol::Udp,
                    source,
                    destination,
                } => {
                    if self
                        .direct
                        .is_some_and(|r| r.source == source && r.destination == destination)
                    {
                        continue;
                    }
                    self.invalidate_direct();
                    if let Some(id) = self.probes.register(Path::Direct, now) {
                        let route = DirectRoute {
                            id,
                            source,
                            destination,
                        };
                        self.direct = Some(route);
                        self.actions.push_back(Action::DirectNominated(route));
                    }
                }
                IceAgentEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    self.invalidate_direct()
                }
                _ => {}
            }
        }
        while let Some(tx) = self.agent.poll_transmit() {
            if tx.proto == Protocol::Udp && self.sockets.contains(&tx.source) {
                self.actions.push_back(Action::Send(DatagramTx {
                    source: tx.source,
                    destination: tx.destination,
                    contents: tx.contents.to_vec(),
                }));
            }
        }
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), ConnectivityError> {
        if self.discovery.expire(now) {
            self.reset_publication(now);
            self.actions.push_back(Action::DiscoveryComplete);
            self.actions.push_back(Action::PublishCandidates);
        }
        if self.agent.poll_timeout().is_none_or(|d| now >= d) {
            self.agent.handle_timeout(now);
        }
        self.drain_agent(now);
        if self.direct.is_none() && !self.attempt_expired && now >= self.attempt_deadline {
            self.attempt_expired = true;
            self.actions.push_back(Action::AttemptTimedOut);
        }
        if self.direct.is_none() && now >= self.next_publish {
            self.publish_backoff = (self.publish_backoff * 2).min(SIGNAL_RETRY_CAP);
            self.next_publish = now + self.publish_backoff;
            self.actions.push_back(Action::PublishDescription);
            self.actions.push_back(Action::PublishCandidates);
        }
        self.actions
            .extend(self.probes.poll(now)?.into_iter().map(Action::Probe));
        Ok(())
    }
}
