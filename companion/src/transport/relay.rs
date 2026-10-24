//! sans-I/O relay core shared by the hub binary and the companion's player relay
//! (DESIGN.md, "Relay hub"). it forwards `0x10` relay datagrams unchanged between the two
//! endpoints a grant names, after each endpoint proved its address with BIND. it never
//! decrypts media. time is injected as unix milliseconds, the clock of grant and
//! challenge expiry. secrets and challenges come from the OS CSPRNG.
//!
//! the hub runs it as player 0 (transport/hub.rs, src/bin/ptt-radio-hub.rs). a companion
//! acting as player relay for pairs that chose it runs the same `Relay` with its own
//! player index and `fresh_generation()`, `offer`s grants for those pairs, feeds game
//! SIGNAL to `on_signal`, and hands the datagrams `claims` accepts on its socket to `handle`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Weak};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::signaling::{RelayGrant, Signal, encode_game};
use super::wire::{
    Bind, BindChallenge, BindRequest, CodecError, Datagram, bind_mac_input, decode_datagram,
    encode_datagram,
};

/// how long an endpoint has to answer a challenge
pub const CHALLENGE_MS: u64 = 5_000;
/// outstanding challenges per endpoint slot, one per source address; the oldest is replaced
pub const MAX_PENDING: usize = 4;
/// grants in the whole table
pub const MAX_GRANTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// lifetime of an issued grant
    pub grant_ms: u64,
    /// live grants one player may be an endpoint of
    pub grants_per_player: usize,
    /// bytes per second one player may make the relay send: forwarded datagrams it sent
    /// plus the challenges and acks addressed to it; the burst is one second of it
    pub bytes_per_second: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            grant_ms: 30 * 60 * 1000,
            grants_per_player: 64,
            // ~20 listeners of one redundant voice stream, with headroom
            bytes_per_second: 64 * 1024,
        }
    }
}

/// why a datagram produced nothing; network callers drop silently
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    Malformed(CodecError),
    /// encrypted media without a wrapper, a challenge or an ack: not addressed to a relay
    NotForRelay,
    UnknownSession,
    WrongGeneration,
    Expired,
    WrongPlayer,
    /// the source is not the registered endpoint of either slot of that session
    UnregisteredSource,
    /// the other endpoint has not bound yet
    PeerUnbound,
    RateLimited,
    /// attempt not above the slot's last committed attempt
    StaleAttempt,
    /// no outstanding challenge equal to the one committed, at that source, in time
    NoChallenge,
    BadProof,
    Random,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// send the received bytes unchanged, wrapper included, to this endpoint
    Forward(SocketAddr),
    /// send these bytes (a challenge or an ack) back to the source
    Send(SocketAddr, Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueError {
    /// sender equals recipient, or either is the relay itself
    Invalid,
    /// a player has `grants_per_player` live grants, or the table is full
    Cap,
    Random,
}

#[derive(Default)]
struct Slot {
    endpoint: Option<SocketAddr>,
    /// the challenge of the commit that set `endpoint`, to re-ack a retransmitted commit
    committed: Option<BindChallenge>,
    attempt: Option<u64>,
    pending: Vec<BindChallenge>,
    active_ms: u64,
}

struct Entry {
    grant: RelayGrant,
    closed: bool,
    slots: [Slot; 2],
    /// the grant this one renews, and the grant that renews this one
    predecessor: Option<u32>,
    successor: Option<u32>,
}

impl Entry {
    fn slot(&self, player: u16) -> Option<usize> {
        if player == self.grant.sender {
            Some(0)
        } else if player == self.grant.recipient {
            Some(1)
        } else {
            None
        }
    }

    fn player(&self, slot: usize) -> u16 {
        if slot == 0 {
            self.grant.sender
        } else {
            self.grant.recipient
        }
    }

    fn live(&self, now_ms: u64) -> bool {
        !self.closed && now_ms < self.grant.expires_at_ms
    }
}

/// per-player byte budget, kept in thousandths of a byte so refills do not round away
struct Bucket {
    milli: u64,
    at_ms: u64,
}

/// held only while the hub retries a refused renewal; expiry does not revoke it
pub(super) struct RetryAuthorization(Arc<AtomicBool>);

impl RetryAuthorization {
    pub(super) fn valid(&self) -> bool {
        self.0.load(Relaxed)
    }
}

struct RetryWatch {
    sender: u16,
    recipient: u16,
    allowed: Weak<AtomicBool>,
}

pub struct Relay {
    relay: u16,
    generation: u64,
    limits: Limits,
    grants: HashMap<u32, Entry>,
    buckets: HashMap<u16, Bucket>,
    retry_watches: HashMap<u32, RetryWatch>,
}

fn mac(grant: &RelayGrant, challenge: BindChallenge, ack: bool) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&grant.secret).expect("hmac accepts any key length");
    mac.update(&bind_mac_input(grant, challenge, ack));
    mac
}

/// HMAC-SHA-256 keyed by the grant secret over `bind_mac_input`
pub fn bind_proof(grant: &RelayGrant, challenge: BindChallenge, ack: bool) -> [u8; 32] {
    mac(grant, challenge, ack).finalize().into_bytes().into()
}

/// constant-time check of a proof
pub fn verify_proof(grant: &RelayGrant, challenge: BindChallenge, ack: bool, proof: &[u8]) -> bool {
    mac(grant, challenge, ack).verify_slice(proof).is_ok()
}

/// a SIGNAL_OUT frame for the game (`to` is the destination player), already encoded
pub struct Outgoing {
    pub to: u16,
    pub frame: Vec<u8>,
}

impl Outgoing {
    pub fn signal(to: u16, signal: &Signal) -> Option<Outgoing> {
        let frame = encode_game(signal, to, false).ok()?;
        Some(Outgoing { to, frame })
    }
}

fn random<const N: usize>() -> Option<[u8; N]> {
    let mut out = [0; N];
    getrandom::fill(&mut out).ok()?;
    Some(out)
}

fn grant_frames(grant: RelayGrant) -> Vec<Outgoing> {
    let to = [grant.sender, grant.recipient];
    let signal = Signal::RelayGrant(grant);
    to.into_iter()
        .filter_map(|to| Outgoing::signal(to, &signal))
        .collect()
}

/// a fresh game-session generation for a relay, from the OS CSPRNG
pub fn fresh_generation() -> Option<u64> {
    random().map(u64::from_be_bytes)
}

impl Relay {
    /// `relay` is this relay's player index (0 for the hub); `generation` scopes every grant
    pub fn new(relay: u16, generation: u64, limits: Limits) -> Relay {
        Relay {
            relay,
            generation,
            limits,
            grants: HashMap::new(),
            buckets: HashMap::new(),
            retry_watches: HashMap::new(),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// a new game session: every grant of the old generation stops at once
    pub fn set_generation(&mut self, generation: u64) {
        for watch in self.retry_watches.values() {
            if let Some(allowed) = watch.allowed.upgrade() {
                allowed.store(false, Relaxed);
            }
        }
        self.retry_watches.clear();
        self.generation = generation;
        self.grants.clear();
        self.buckets.clear();
    }

    /// a new grant for the pair with a fresh session number and secret. the caller sends it
    /// as `Signal::RelayGrant` to both endpoints over the game.
    pub fn issue(
        &mut self,
        sender: u16,
        recipient: u16,
        now_ms: u64,
    ) -> Result<RelayGrant, IssueError> {
        if sender == recipient || sender == self.relay || recipient == self.relay {
            return Err(IssueError::Invalid);
        }
        self.expire(now_ms);
        if self.grants.len() >= MAX_GRANTS
            || self.live_grants(sender) >= self.limits.grants_per_player
            || self.live_grants(recipient) >= self.limits.grants_per_player
        {
            return Err(IssueError::Cap);
        }
        self.mint(sender, recipient, now_ms, None)
    }

    /// a replacement for the live grant `session`, for the same pair. it takes over that
    /// grant's place under `grants_per_player`, so a pair at the cap can renew; the old
    /// grant forwards until it expires. a pair holds at most the old grant and one
    /// replacement: renewing a grant that already has a live successor or predecessor is
    /// `Cap`, as is a full table. an unknown or closed session is `Invalid`.
    pub fn renew(&mut self, session: u32, now_ms: u64) -> Result<RelayGrant, IssueError> {
        self.expire(now_ms);
        let entry = self.grants.get(&session).ok_or(IssueError::Invalid)?;
        let overlapping = [entry.predecessor, entry.successor]
            .into_iter()
            .flatten()
            .any(|s| self.grants.get(&s).is_some_and(|e| !e.closed));
        if self.grants.len() >= MAX_GRANTS || overlapping {
            return Err(IssueError::Cap);
        }
        let (sender, recipient) = (entry.grant.sender, entry.grant.recipient);
        let grant = self.mint(sender, recipient, now_ms, Some(session))?;
        if let Some(old) = self.grants.get_mut(&session) {
            old.successor = Some(grant.session);
        }
        Ok(grant)
    }

    pub(super) fn watch_renewal(&mut self, session: u32) -> Option<RetryAuthorization> {
        let entry = self.grants.get(&session)?;
        if entry.closed {
            return None;
        }
        if let Some(allowed) = self
            .retry_watches
            .get(&session)
            .and_then(|w| w.allowed.upgrade())
        {
            return Some(RetryAuthorization(allowed));
        }
        self.retry_watches
            .retain(|_, w| w.allowed.strong_count() > 0);
        if self.retry_watches.len() >= MAX_GRANTS {
            return None;
        }
        let allowed = Arc::new(AtomicBool::new(true));
        self.retry_watches.insert(
            session,
            RetryWatch {
                sender: entry.grant.sender,
                recipient: entry.grant.recipient,
                allowed: Arc::downgrade(&allowed),
            },
        );
        Some(RetryAuthorization(allowed))
    }

    fn mint(
        &mut self,
        sender: u16,
        recipient: u16,
        now_ms: u64,
        predecessor: Option<u32>,
    ) -> Result<RelayGrant, IssueError> {
        let session = loop {
            let session = u32::from_be_bytes(random().ok_or(IssueError::Random)?);
            if session != 0
                && !self.grants.contains_key(&session)
                && !self.retry_watches.contains_key(&session)
            {
                break session;
            }
        };
        let grant = RelayGrant {
            session,
            relay: self.relay,
            sender,
            recipient,
            hub_generation: self.generation,
            expires_at_ms: now_ms.saturating_add(self.limits.grant_ms),
            secret: random().ok_or(IssueError::Random)?,
        };
        self.grants.insert(
            session,
            Entry {
                grant: grant.clone(),
                closed: false,
                slots: Default::default(),
                predecessor,
                successor: None,
            },
        );
        Ok(grant)
    }

    /// every live grant
    pub fn live(&self, now_ms: u64) -> impl Iterator<Item = &RelayGrant> {
        self.grants
            .values()
            .filter(move |e| e.live(now_ms))
            .map(|e| &e.grant)
    }

    /// grants counted against `grants_per_player`: a grant whose live replacement exists
    /// is not counted, the replacement holds its place
    fn live_grants(&self, player: u16) -> usize {
        let superseded = |e: &Entry| {
            e.successor
                .and_then(|s| self.grants.get(&s))
                .is_some_and(|n| !n.closed)
        };
        self.grants
            .values()
            .filter(|e| !e.closed && !superseded(e) && e.slot(player).is_some())
            .count()
    }

    /// last time a slot bound or forwarded, 0 if never
    pub fn active_ms(&self, session: u32, player: u16) -> u64 {
        self.grants
            .get(&session)
            .and_then(|e| Some(e.slots[e.slot(player)?].active_ms))
            .unwrap_or(0)
    }

    /// explicit close by an endpoint or by the relay itself (`by` is the game-derived
    /// sender of a RelayClose, or the relay's own index). returns the closed grant's
    /// endpoints.
    fn close(&mut self, session: u32, hub_generation: u64, by: u16) -> Option<(u16, u16)> {
        if hub_generation != self.generation {
            return None;
        }
        let (sender, recipient) = if let Some(entry) = self.grants.get(&session) {
            if entry.closed {
                return None;
            }
            (entry.grant.sender, entry.grant.recipient)
        } else {
            let watch = self.retry_watches.get(&session)?;
            if !watch.allowed.upgrade()?.load(Relaxed) {
                return None;
            }
            (watch.sender, watch.recipient)
        };
        if by != self.relay && by != sender && by != recipient {
            return None;
        }
        if let Some(entry) = self.grants.get_mut(&session) {
            entry.closed = true;
        }
        if let Some(allowed) = self
            .retry_watches
            .remove(&session)
            .and_then(|w| w.allowed.upgrade())
        {
            allowed.store(false, Relaxed);
        }
        Some((sender, recipient))
    }

    /// `issue` plus the RelayGrant frames for both endpoints
    pub fn offer(
        &mut self,
        sender: u16,
        recipient: u16,
        now_ms: u64,
    ) -> Result<Vec<Outgoing>, IssueError> {
        Ok(grant_frames(self.issue(sender, recipient, now_ms)?))
    }

    /// `renew` plus the RelayGrant frames for both endpoints
    pub fn offer_renewal(
        &mut self,
        session: u32,
        now_ms: u64,
    ) -> Result<Vec<Outgoing>, IssueError> {
        Ok(grant_frames(self.renew(session, now_ms)?))
    }

    /// relay-initiated close (the pair no longer uses this relay, or an endpoint
    /// restarted); RelayClose frames go to both endpoints
    pub fn withdraw(&mut self, session: u32) -> Vec<Outgoing> {
        let Some((a, b)) = self.close(session, self.generation, self.relay) else {
            return Vec::new();
        };
        let signal = self.close_signal(session);
        [a, b]
            .into_iter()
            .filter_map(|to| Outgoing::signal(to, &signal))
            .collect()
    }

    /// game signaling every relay handles: RelayClose for one of its sessions from an
    /// endpoint (`sender` is game-derived) closes it and tells the other endpoint
    pub fn on_signal(&mut self, sender: u16, signal: &Signal) -> Vec<Outgoing> {
        let &Signal::RelayClose {
            relay,
            session,
            hub_generation,
        } = signal
        else {
            return Vec::new();
        };
        if relay != self.relay || sender == self.relay {
            return Vec::new();
        }
        let Some((a, b)) = self.close(session, hub_generation, sender) else {
            return Vec::new();
        };
        let other = if sender == a { b } else { a };
        Outgoing::signal(other, &self.close_signal(session))
            .into_iter()
            .collect()
    }

    fn close_signal(&self, session: u32) -> Signal {
        Signal::RelayClose {
            relay: self.relay,
            session,
            hub_generation: self.generation,
        }
    }

    /// demux on a socket shared with the endpoint role (player relay): BIND requests and
    /// commits, and wrapped datagrams from an endpoint registered on one of our sessions,
    /// are for the relay; challenges, acks and other wrapped datagrams are the endpoint's
    pub fn claims(&self, source: SocketAddr, data: &[u8]) -> bool {
        match decode_datagram(data) {
            Ok(Datagram::Bind(Bind::Request(_) | Bind::Commit { .. })) => true,
            Ok(Datagram::Relay { session, .. }) => self
                .grants
                .get(&session)
                .is_some_and(|e| e.slots.iter().any(|slot| slot.endpoint == Some(source))),
            _ => false,
        }
    }

    /// drops expired and closed grants, expired challenges and idle budgets
    pub fn expire(&mut self, now_ms: u64) {
        self.grants.retain(|_, e| e.live(now_ms));
        self.retry_watches
            .retain(|_, w| w.allowed.strong_count() > 0);
        for entry in self.grants.values_mut() {
            for slot in &mut entry.slots {
                slot.pending.retain(|c| now_ms <= c.expires_at_ms);
            }
        }
        let grants = &self.grants;
        self.buckets
            .retain(|player, _| grants.values().any(|e| e.slot(*player).is_some()));
    }

    fn charge(&mut self, player: u16, bytes: usize, now_ms: u64) -> bool {
        let cap = self.limits.bytes_per_second.saturating_mul(1000);
        let bucket = self.buckets.entry(player).or_insert(Bucket {
            milli: cap,
            at_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(bucket.at_ms);
        bucket.milli = bucket
            .milli
            .saturating_add(elapsed.saturating_mul(self.limits.bytes_per_second))
            .min(cap);
        bucket.at_ms = bucket.at_ms.max(now_ms);
        let cost = (bytes as u64).saturating_mul(1000);
        if bucket.milli < cost {
            return false;
        }
        bucket.milli -= cost;
        true
    }

    /// one datagram from `source` on the relay socket
    pub fn handle(
        &mut self,
        now_ms: u64,
        source: SocketAddr,
        data: &[u8],
    ) -> Result<Reply, Dropped> {
        match decode_datagram(data).map_err(Dropped::Malformed)? {
            Datagram::Relay { session, .. } => self.forward(now_ms, source, session, data.len()),
            Datagram::Bind(Bind::Request(request)) => self.request(now_ms, source, request),
            Datagram::Bind(Bind::Commit { challenge, proof }) => {
                self.commit(now_ms, source, challenge, &proof)
            }
            Datagram::Bind(Bind::Challenge(_)) | Datagram::BindAck { .. } => {
                Err(Dropped::NotForRelay)
            }
            Datagram::Encrypted { .. } => Err(Dropped::NotForRelay),
        }
    }

    fn entry(&self, session: u32, now_ms: u64) -> Result<&Entry, Dropped> {
        let entry = self.grants.get(&session).ok_or(Dropped::UnknownSession)?;
        if !entry.live(now_ms) {
            return Err(Dropped::Expired);
        }
        Ok(entry)
    }

    fn forward(
        &mut self,
        now_ms: u64,
        source: SocketAddr,
        session: u32,
        len: usize,
    ) -> Result<Reply, Dropped> {
        let entry = self.entry(session, now_ms)?;
        // the session number only selects the grant; the source address authorizes
        let from = (0..2)
            .find(|&i| entry.slots[i].endpoint == Some(source))
            .ok_or(Dropped::UnregisteredSource)?;
        let to = entry.slots[1 - from].endpoint.ok_or(Dropped::PeerUnbound)?;
        let player = entry.player(from);
        if !self.charge(player, len, now_ms) {
            return Err(Dropped::RateLimited);
        }
        if let Some(entry) = self.grants.get_mut(&session) {
            entry.slots[from].active_ms = now_ms;
        }
        Ok(Reply::Forward(to))
    }

    fn request(
        &mut self,
        now_ms: u64,
        source: SocketAddr,
        request: BindRequest,
    ) -> Result<Reply, Dropped> {
        let entry = self.entry(request.session, now_ms)?;
        if request.hub_generation != self.generation
            || request.hub_generation != entry.grant.hub_generation
        {
            return Err(Dropped::WrongGeneration);
        }
        let slot = entry.slot(request.player).ok_or(Dropped::WrongPlayer)?;
        if entry.slots[slot]
            .attempt
            .is_some_and(|committed| request.attempt <= committed)
        {
            return Err(Dropped::StaleAttempt);
        }
        // a retransmitted request gets the same challenge back, so a late first challenge
        // still commits
        let outstanding = entry.slots[slot]
            .pending
            .iter()
            .find(|c| c.endpoint == source && c.request == request && now_ms <= c.expires_at_ms)
            .copied();
        let challenge = match outstanding {
            Some(challenge) => challenge,
            None => BindChallenge {
                request,
                endpoint: source,
                challenge: random().ok_or(Dropped::Random)?,
                expires_at_ms: now_ms
                    .saturating_add(CHALLENGE_MS)
                    .min(entry.grant.expires_at_ms),
            },
        };
        let bytes = encode_datagram(Datagram::Bind(Bind::Challenge(challenge)))
            .map_err(Dropped::Malformed)?;
        if !self.charge(request.player, bytes.len(), now_ms) {
            return Err(Dropped::RateLimited);
        }
        let Some(entry) = self.grants.get_mut(&request.session) else {
            return Err(Dropped::UnknownSession);
        };
        let pending = &mut entry.slots[slot].pending;
        pending.retain(|c| c.endpoint != source && now_ms <= c.expires_at_ms);
        if pending.len() >= MAX_PENDING {
            pending.remove(0);
        }
        pending.push(challenge);
        Ok(Reply::Send(source, bytes))
    }

    fn commit(
        &mut self,
        now_ms: u64,
        source: SocketAddr,
        challenge: BindChallenge,
        proof: &[u8],
    ) -> Result<Reply, Dropped> {
        let request = challenge.request;
        let entry = self.entry(request.session, now_ms)?;
        if request.hub_generation != self.generation
            || request.hub_generation != entry.grant.hub_generation
        {
            return Err(Dropped::WrongGeneration);
        }
        let slot = entry.slot(request.player).ok_or(Dropped::WrongPlayer)?;
        if source != challenge.endpoint || now_ms > challenge.expires_at_ms {
            return Err(Dropped::NoChallenge);
        }
        let state = &entry.slots[slot];
        // a retransmitted commit whose ack was lost: ack again, change nothing
        let repeat = state.committed == Some(challenge) && state.endpoint == Some(source);
        if !repeat && !state.pending.contains(&challenge) {
            return Err(Dropped::NoChallenge);
        }
        if !repeat
            && state
                .attempt
                .is_some_and(|committed| request.attempt <= committed)
        {
            return Err(Dropped::StaleAttempt);
        }
        if !verify_proof(&entry.grant, challenge, false, proof) {
            return Err(Dropped::BadProof);
        }
        let ack = Datagram::BindAck {
            challenge,
            proof: bind_proof(&entry.grant, challenge, true),
        };
        let bytes = encode_datagram(ack).map_err(Dropped::Malformed)?;
        if !self.charge(request.player, bytes.len(), now_ms) {
            return Err(Dropped::RateLimited);
        }
        let Some(entry) = self.grants.get_mut(&request.session) else {
            return Err(Dropped::UnknownSession);
        };
        let state = &mut entry.slots[slot];
        if !repeat {
            state.pending.clear();
            state.endpoint = Some(source);
            state.committed = Some(challenge);
            state.attempt = Some(request.attempt);
        }
        state.active_ms = now_ms;
        Ok(Reply::Send(source, bytes))
    }
}

/// endpoint side of BIND for one grant slot: request, commit, ack. a fresh client (with a
/// larger attempt) rebinds after a NAT change or on another socket.
pub struct BindClient {
    grant: RelayGrant,
    request: BindRequest,
    challenge: Option<BindChallenge>,
}

impl BindClient {
    /// None when `player` is not an endpoint of the grant or the CSPRNG fails. `attempt`
    /// must exceed the attempt of this slot's previous successful bind on this grant.
    pub fn new(grant: RelayGrant, player: u16, attempt: u64) -> Option<BindClient> {
        if player != grant.sender && player != grant.recipient {
            return None;
        }
        let request = BindRequest {
            session: grant.session,
            hub_generation: grant.hub_generation,
            player,
            attempt,
            client_nonce: random()?,
        };
        Self::from_request(grant, request)
    }

    /// uses the connectivity owner's reflection request without changing its transcript.
    pub fn from_request(grant: RelayGrant, request: BindRequest) -> Option<BindClient> {
        if request.session != grant.session
            || request.hub_generation != grant.hub_generation
            || (request.player != grant.sender && request.player != grant.recipient)
        {
            return None;
        }
        Some(BindClient {
            grant,
            request,
            challenge: None,
        })
    }

    /// stage 0, resent unchanged until a challenge comes back
    pub fn request(&self) -> Vec<u8> {
        encode_datagram(Datagram::Bind(Bind::Request(self.request)))
            .expect("a bind request is far below the datagram cap")
    }

    /// stage 2 for a challenge to our own request; the challenge names our address as the
    /// relay sees it. resend the returned commit until the ack arrives.
    pub fn on_challenge(&mut self, challenge: BindChallenge) -> Option<Vec<u8>> {
        if challenge.request != self.request {
            return None;
        }
        self.challenge = Some(challenge);
        let proof = bind_proof(&self.grant, challenge, false);
        encode_datagram(Datagram::Bind(Bind::Commit { challenge, proof })).ok()
    }

    /// the relay-observed endpoint once the ack for our commit verifies
    pub fn on_ack(&self, challenge: BindChallenge, proof: &[u8]) -> Option<SocketAddr> {
        (self.challenge == Some(challenge) && verify_proof(&self.grant, challenge, true, proof))
            .then_some(challenge.endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> RelayGrant {
        RelayGrant {
            session: 7,
            relay: 0,
            sender: 1,
            recipient: 2,
            hub_generation: 9,
            expires_at_ms: 1000,
            secret: [5; 16],
        }
    }

    fn challenge() -> BindChallenge {
        BindChallenge {
            request: BindRequest {
                session: 7,
                hub_generation: 9,
                player: 1,
                attempt: 1,
                client_nonce: [1; 32],
            },
            endpoint: SocketAddr::from(([203, 0, 113, 1], 40000)),
            challenge: [2; 32],
            expires_at_ms: 500,
        }
    }

    #[test]
    fn proof_depends_on_secret_domain_and_challenge() {
        let (g, c) = (grant(), challenge());
        let commit = bind_proof(&g, c, false);
        assert!(verify_proof(&g, c, false, &commit));
        assert!(!verify_proof(&g, c, true, &commit));
        assert!(!verify_proof(&g, c, false, &commit[..31]));
        let mut other = g.clone();
        other.secret[0] ^= 1;
        assert!(!verify_proof(&other, c, false, &commit));
        let mut moved = c;
        moved.endpoint = SocketAddr::from(([203, 0, 113, 1], 40001));
        assert!(!verify_proof(&g, moved, false, &commit));
    }

    #[test]
    fn bucket_refills_at_the_configured_rate() {
        let limits = Limits {
            bytes_per_second: 1000,
            ..Limits::default()
        };
        let mut relay = Relay::new(0, 1, limits);
        assert!(relay.charge(1, 1000, 0));
        assert!(!relay.charge(1, 1, 0));
        assert!(relay.charge(1, 100, 100));
        assert!(!relay.charge(1, 1, 100));
        // a clock step backwards never mints budget
        assert!(!relay.charge(1, 1, 50));
        assert!(relay.charge(1, 1000, 5000));
    }

    #[test]
    fn issue_rejects_self_and_relay_pairs() {
        let mut relay = Relay::new(3, 1, Limits::default());
        assert_eq!(relay.issue(1, 1, 0).err(), Some(IssueError::Invalid));
        assert_eq!(relay.issue(1, 3, 0).err(), Some(IssueError::Invalid));
        let grant = relay.issue(1, 2, 0).unwrap();
        assert_eq!((grant.relay, grant.hub_generation), (3, 1));
        assert_ne!(grant.session, 0);
    }

    #[test]
    fn renewal_replaces_one_grant_and_frees_nothing_for_other_pairs() {
        let limits = Limits {
            grants_per_player: 1,
            ..Limits::default()
        };
        let mut relay = Relay::new(0, 1, limits);
        let old = relay.issue(1, 2, 0).unwrap();
        assert_eq!(relay.issue(1, 3, 0).err(), Some(IssueError::Cap));
        let new = relay.renew(old.session, 10).unwrap();
        assert_eq!((new.sender, new.recipient), (1, 2));
        assert_eq!(relay.live(10).count(), 2);
        // at most the old grant and one replacement
        assert_eq!(relay.renew(old.session, 10).err(), Some(IssueError::Cap));
        assert_eq!(relay.renew(new.session, 10).err(), Some(IssueError::Cap));
        assert_eq!(relay.issue(1, 3, 10).err(), Some(IssueError::Cap));
        assert_eq!(relay.issue(3, 2, 10).err(), Some(IssueError::Cap));
        assert_eq!(relay.renew(12345, 10).err(), Some(IssueError::Invalid));
        // once the old grant is gone the replacement renews in turn
        assert!(!relay.withdraw(old.session).is_empty());
        assert!(relay.renew(new.session, 20).is_ok());
    }

    #[test]
    fn retry_watches_expire_when_the_hub_drops_its_authorization() {
        let mut relay = Relay::new(0, 1, Limits::default());
        let grant = relay.issue(1, 2, 0).unwrap();
        let authorization = relay.watch_renewal(grant.session).unwrap();
        relay.expire(grant.expires_at_ms);
        assert!(authorization.valid());
        assert_eq!(relay.retry_watches.len(), 1);
        drop(authorization);
        relay.expire(grant.expires_at_ms);
        assert!(relay.retry_watches.is_empty());
    }
}
