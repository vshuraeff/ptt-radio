//! pure pair sessions; callers own trusted signaling, handle allocation and timers.
//!
//! copy each returned encrypted datagram unchanged across routes. feed only the
//! selected trusted ANNOUNCE snapshots to `Session::new`; replace the session on
//! a snapshot change. no type here containing keys or credentials implements Debug.

pub mod replay;

use std::fmt;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use snow::{Builder, HandshakeState, StatelessTransportState};

use crate::transport::StreamId;
use crate::transport::signaling::{
    self, Announce, CAP_MEDIA_V2, CAP_UDP, Handshake, PROTOCOL_VERSION, Signal, Suite,
};
use crate::transport::wire::{
    self, CodecError, Datagram, Header, Inner, Kind, ReceiverHandle, SessionContext,
};
use replay::ReplayWindow;

pub const OLD_KEY_OVERLAP: Duration = Duration::from_secs(5);
/// leave room for confirmation/retries while asking the lower player to rekey.
pub const REKEY_AT: u32 = u32::MAX - 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Codec(CodecError),
    Crypto,
    Incompatible,
    NotInitiator,
    StaleAttempt,
    UnexpectedSignal,
    HandleInUse,
    PhaseInUse,
    Exhausted,
    NotReady,
    WrongReceiver,
    WrongPeer,
    Replay,
    InvalidConfirmation,
    ResetRequired,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "crypto session: {self:?}")
    }
}

impl std::error::Error for Error {}

impl From<CodecError> for Error {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<snow::Error> for Error {
    fn from(_: snow::Error) -> Self {
        Self::Crypto
    }
}

/// signaling may contain credentials elsewhere in Signal, so this has no Debug.
#[derive(Default)]
pub struct Actions {
    pub signals: Vec<Signal>,
    pub datagrams: Vec<Vec<u8>>,
    pub activated: bool,
}

/// authenticated media identity comes from the key, never an outer header.
#[derive(Debug, PartialEq, Eq)]
pub struct Received {
    pub context: SessionContext,
    pub kind: Kind,
    pub sequence: u16,
    pub talkspurt: u16,
    pub body: Vec<u8>,
}

pub struct Reception {
    pub packet: Received,
    pub actions: Actions,
}

pub struct StaticKeypair {
    pub private: Vec<u8>,
    pub public: [u8; 32],
}

pub fn generate_static_keypair() -> Result<StaticKeypair, Error> {
    let pair = Builder::new(noise_name(Suite::ChaChaPoly).parse()?).generate_keypair()?;
    Ok(StaticKeypair {
        private: pair.private,
        public: pair.public.try_into().map_err(|_| Error::Crypto)?,
    })
}

fn noise_name(suite: Suite) -> &'static str {
    match suite {
        Suite::Aes256Gcm => "Noise_KK_25519_AESGCM_SHA256",
        Suite::ChaChaPoly => "Noise_KK_25519_ChaChaPoly_SHA256",
    }
}

pub fn announce_hash(announce: &Announce) -> Result<[u8; 32], Error> {
    let snapshot = signaling::encode(&Signal::Announce(announce.clone()))?;
    let mut hash = Sha256::new();
    hash.update(b"ptt-radio-v2 announce\0");
    hash.update(snapshot);
    Ok(hash.finalize().into())
}

struct Initiating {
    noise: HandshakeState,
    first: Handshake,
}

struct Key {
    noise: StatelessTransportState,
    generation: u64,
    epoch: u32,
    local_handle: ReceiverHandle,
    remote_handle: ReceiverHandle,
    next_counter: Option<u32>,
    replay: ReplayWindow,
    token: u64,
    first: Handshake,
    second: Handshake,
}

impl Key {
    fn phase(&self) -> bool {
        self.epoch & 1 != 0
    }

    fn send(&mut self, inner: Inner<'_>) -> Result<Vec<u8>, Error> {
        let plaintext = wire::encode_inner(inner)?;
        let counter = self.next_counter.ok_or(Error::Exhausted)?;
        let mut ciphertext = vec![0; plaintext.len() + wire::TAG];
        let nonce = (u64::from(self.epoch) << 32) | u64::from(counter);
        let length = self
            .noise
            .write_message(nonce, &plaintext, &mut ciphertext)?;
        ciphertext.truncate(length);
        let packet = wire::encode_datagram(Datagram::Encrypted {
            header: Header {
                kind: inner.kind,
                phase: self.phase(),
                receiver: self.remote_handle,
                counter,
            },
            ciphertext: &ciphertext,
        })?;
        self.next_counter = counter.checked_add(1).filter(|next| *next < u32::MAX);
        Ok(packet)
    }

    fn probe(&mut self, kind: Kind) -> Result<Vec<u8>, Error> {
        self.send(Inner {
            kind,
            sequence: 0,
            talkspurt: 0,
            body: &self.token.to_be_bytes(),
        })
    }

    fn metadata(&self) -> Signal {
        Signal::KeyConfirmation {
            attempt: self.epoch,
            session_generation: self.generation,
            token: self.token,
        }
    }
}

/// one direction pair, one selected immutable pair of ANNOUNCE snapshots.
///
/// `begin` is lower-player only. its full u64 generation must grow on each fresh
/// attempt; checked attempt is also the u32 key epoch, with no cast or truncation.
/// handles must be globally allocated by the caller and remain reserved through
/// pending/active/old lifetime. call retry on a timer until activation; it repeats
/// Noise bytes but encrypts confirmation with a fresh counter on every call.
/// the caller must set a finite retry deadline (for example ten seconds), then
/// call `abort_pending`. ResetRequired means both peers must select fresh ANNOUNCE
/// snapshots/credentials and construct new sessions, preserving stream generations.
/// this module owns no timer; game fallback handles that terminal UDP failure.
pub struct Session {
    local: StreamId,
    remote: StreamId,
    local_private: Vec<u8>,
    remote_static: [u8; 32],
    suite: Suite,
    lower_hash: [u8; 32],
    higher_hash: [u8; 32],
    pair_hash: [u8; 32],
    last_attempt: u32,
    last_generation: u64,
    initiating: Option<Initiating>,
    pending: Option<Key>,
    active: Option<Key>,
    old: Option<(Key, Instant)>,
    rekey_requested: Option<u64>,
    reset_required: bool,
}

impl Session {
    pub fn new(
        local_player: u16,
        local_announce: &Announce,
        local_private: Vec<u8>,
        remote_player: u16,
        remote_announce: &Announce,
    ) -> Result<Self, Error> {
        let required = CAP_MEDIA_V2 | CAP_UDP;
        if local_player == remote_player
            || local_private.len() != 32
            || [local_announce, remote_announce]
                .iter()
                .any(|a| a.version != PROTOCOL_VERSION || a.capabilities & required != required)
        {
            return Err(Error::Incompatible);
        }
        let suite = signaling::select_suite(&local_announce.suites, &remote_announce.suites)
            .ok_or(Error::Incompatible)?;
        let local_hash = announce_hash(local_announce)?;
        let remote_hash = announce_hash(remote_announce)?;
        let (lower, lower_hash, higher, higher_hash) = if local_player < remote_player {
            (local_player, local_hash, remote_player, remote_hash)
        } else {
            (remote_player, remote_hash, local_player, local_hash)
        };
        let mut hash = Sha256::new();
        hash.update(b"ptt-radio-v2 pair\0");
        hash.update(lower.to_be_bytes());
        hash.update(lower_hash);
        hash.update(higher.to_be_bytes());
        hash.update(higher_hash);
        Ok(Self {
            local: StreamId {
                player: local_player,
                generation: local_announce.stream_generation,
            },
            remote: StreamId {
                player: remote_player,
                generation: remote_announce.stream_generation,
            },
            local_private,
            remote_static: remote_announce.static_key,
            suite,
            lower_hash,
            higher_hash,
            pair_hash: hash.finalize().into(),
            last_attempt: 0,
            last_generation: 0,
            initiating: None,
            pending: None,
            active: None,
            old: None,
            rekey_requested: None,
            reset_required: false,
        })
    }

    pub fn suite(&self) -> Suite {
        self.suite
    }
    pub fn generation(&self) -> u64 {
        self.last_generation
    }
    pub fn active_epoch(&self) -> Option<u32> {
        self.active.as_ref().map(|k| k.epoch)
    }
    pub fn needs_rekey(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|k| k.next_counter.is_none_or(|n| n >= REKEY_AT))
    }

    /// either endpoint can request rekey; only the lower endpoint starts KK.
    /// `new_handle` is consumed only when this call starts a lower-side attempt.
    pub fn request_rekey(
        &mut self,
        new_handle: ReceiverHandle,
        now: Instant,
    ) -> Result<Actions, Error> {
        let generation = self.active.as_ref().ok_or(Error::NotReady)?.generation;
        if self.local.player < self.remote.player {
            if self.initiating.is_some() || self.pending.is_some() {
                return self.retry();
            }
            self.begin(
                self.last_generation
                    .checked_add(1)
                    .ok_or(Error::Exhausted)?,
                new_handle,
                now,
            )
        } else {
            self.rekey_requested = Some(generation);
            Ok(Actions {
                signals: vec![Signal::RekeyRequest {
                    session_generation: generation,
                }],
                ..Actions::default()
            })
        }
    }

    pub fn expire(&mut self, now: Instant) {
        if self.old.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.old = None;
        }
    }

    /// call at the caller's bounded retry deadline. active/old keys and replay
    /// windows survive; attempt and generation high-water marks stay burnt.
    /// after sending confirmation, the initiator cannot know whether the peer
    /// activated. abort then requires a new trusted snapshot context on both ends.
    pub fn abort_pending(&mut self) -> Result<(), Error> {
        self.reset_required |= self.local.player < self.remote.player && self.pending.is_some();
        self.initiating = None;
        self.pending = None;
        self.rekey_requested = None;
        if self.reset_required {
            Err(Error::ResetRequired)
        } else {
            Ok(())
        }
    }

    fn available(&self, epoch: u32, handle: ReceiverHandle) -> Result<(), Error> {
        for key in self
            .active
            .iter()
            .chain(self.pending.iter())
            .chain(self.old.iter().map(|(k, _)| k))
        {
            if key.local_handle == handle {
                return Err(Error::HandleInUse);
            }
        }
        if self
            .initiating
            .as_ref()
            .is_some_and(|i| i.first.receive_handle == handle)
        {
            return Err(Error::HandleInUse);
        }
        for key in self.active.iter().chain(self.old.iter().map(|(k, _)| k)) {
            if key.phase() == (epoch & 1 != 0) {
                return Err(Error::PhaseInUse);
            }
        }
        Ok(())
    }

    fn prologue(&self, generation: u64, attempt: u32) -> Vec<u8> {
        let (lower, higher) = if self.local.player < self.remote.player {
            (self.local, self.remote)
        } else {
            (self.remote, self.local)
        };
        let mut out = b"ptt-radio-v2 noise\0".to_vec();
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        for stream in [lower, higher] {
            out.extend_from_slice(&stream.player.to_be_bytes());
            out.extend_from_slice(&stream.generation.to_be_bytes());
        }
        out.extend_from_slice(&self.lower_hash);
        out.extend_from_slice(&self.higher_hash);
        let name = noise_name(self.suite).as_bytes();
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&generation.to_be_bytes());
        out.extend_from_slice(&attempt.to_be_bytes());
        out
    }

    fn handshake(&self, generation: u64, attempt: u32) -> Result<HandshakeState, Error> {
        let prologue = self.prologue(generation, attempt);
        let builder = Builder::new(noise_name(self.suite).parse()?)
            .local_private_key(&self.local_private)?
            .remote_public_key(&self.remote_static)?
            .prologue(&prologue)?;
        Ok(if self.local.player < self.remote.player {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        })
    }

    /// starts/replaces a pending attempt using fresh Noise ephemeral material.
    pub fn begin(
        &mut self,
        generation: u64,
        handle: ReceiverHandle,
        now: Instant,
    ) -> Result<Actions, Error> {
        if self.reset_required {
            return Err(Error::ResetRequired);
        }
        if self.local.player > self.remote.player {
            return Err(Error::NotInitiator);
        }
        if self.pending.is_some() {
            return Err(Error::NotReady);
        }
        self.expire(now);
        if generation <= self.last_generation {
            return Err(Error::StaleAttempt);
        }
        let mut attempt = self.last_attempt.checked_add(1).ok_or(Error::Exhausted)?;
        if self
            .active
            .as_ref()
            .is_some_and(|k| k.phase() == (attempt & 1 != 0))
        {
            attempt = attempt.checked_add(1).ok_or(Error::Exhausted)?;
        }
        self.available(attempt, handle)?;
        let mut noise = self.handshake(generation, attempt)?;
        let mut bytes = vec![0; signaling::MAX_NOISE_BYTES];
        let length = noise.write_message(&handle.0.to_be_bytes(), &mut bytes)?;
        bytes.truncate(length);
        let first = Handshake {
            attempt,
            session_generation: generation,
            receive_handle: handle,
            noise: bytes,
        };
        self.last_attempt = attempt;
        self.last_generation = generation;
        self.pending = None;
        self.initiating = Some(Initiating {
            noise,
            first: first.clone(),
        });
        Ok(Actions {
            signals: vec![Signal::Noise1(first)],
            ..Actions::default()
        })
    }

    fn key(
        noise: HandshakeState,
        first: Handshake,
        second: Handshake,
        initiator: bool,
    ) -> Result<Key, Error> {
        let mut hash = Sha256::new();
        hash.update(b"ptt-radio-v2 confirm\0");
        hash.update(noise.get_handshake_hash());
        let digest: [u8; 32] = hash.finalize().into();
        let token = u64::from_be_bytes(digest[..8].try_into().map_err(|_| Error::Crypto)?);
        Ok(Key {
            noise: noise.into_stateless_transport_mode()?,
            generation: first.session_generation,
            epoch: first.attempt,
            local_handle: if initiator {
                first.receive_handle
            } else {
                second.receive_handle
            },
            remote_handle: if initiator {
                second.receive_handle
            } else {
                first.receive_handle
            },
            next_counter: Some(0),
            replay: ReplayWindow::default(),
            token,
            first,
            second,
        })
    }

    /// Noise1 and a new lower-side rekey consume `new_handle`; reserve it globally.
    /// sender must come from the trusted game's envelope, not the opaque body.
    pub fn on_signal(
        &mut self,
        sender: u16,
        signal: &Signal,
        new_handle: ReceiverHandle,
        now: Instant,
    ) -> Result<Actions, Error> {
        if sender != self.remote.player {
            return Err(Error::WrongPeer);
        }
        if self.reset_required {
            return Err(Error::ResetRequired);
        }
        self.expire(now);
        match signal {
            Signal::Noise1(first) => {
                if self.local.player < self.remote.player {
                    return Err(Error::NotInitiator);
                }
                for key in self
                    .pending
                    .iter()
                    .chain(self.active.iter())
                    .chain(self.old.iter().map(|(k, _)| k))
                {
                    if first == &key.first {
                        return Ok(Actions {
                            signals: vec![Signal::Noise2(key.second.clone())],
                            ..Actions::default()
                        });
                    }
                }
                if first.attempt == 0
                    || first.attempt <= self.last_attempt
                    || first.session_generation <= self.last_generation
                {
                    return Err(Error::StaleAttempt);
                }
                self.available(first.attempt, new_handle)?;
                let mut noise = self.handshake(first.session_generation, first.attempt)?;
                let mut payload = [0; signaling::MAX_NOISE_BYTES];
                let length = noise.read_message(&first.noise, &mut payload)?;
                if payload[..length] != first.receive_handle.0.to_be_bytes() {
                    return Err(Error::WrongReceiver);
                }
                let mut bytes = vec![0; signaling::MAX_NOISE_BYTES];
                let length = noise.write_message(&new_handle.0.to_be_bytes(), &mut bytes)?;
                bytes.truncate(length);
                let second = Handshake {
                    attempt: first.attempt,
                    session_generation: first.session_generation,
                    receive_handle: new_handle,
                    noise: bytes,
                };
                let key = Self::key(noise, first.clone(), second.clone(), false)?;
                self.last_attempt = first.attempt;
                self.last_generation = first.session_generation;
                self.pending = Some(key);
                Ok(Actions {
                    signals: vec![Signal::Noise2(second)],
                    ..Actions::default()
                })
            }
            Signal::Noise2(second) => {
                if self.local.player > self.remote.player {
                    return Err(Error::NotInitiator);
                }
                for key in self.pending.iter_mut().chain(self.active.iter_mut()) {
                    if second == &key.second {
                        return Ok(Actions {
                            signals: vec![key.metadata()],
                            datagrams: vec![key.probe(Kind::Ping)?],
                            activated: false,
                        });
                    }
                }
                let initiating = self.initiating.as_ref().ok_or(Error::StaleAttempt)?;
                if second.attempt != initiating.first.attempt
                    || second.session_generation != initiating.first.session_generation
                {
                    return Err(Error::StaleAttempt);
                }
                // a failed read may mutate a Noise handshake; consume it and require a fresh attempt.
                let mut initiating = self
                    .initiating
                    .take()
                    .expect("initiating handshake checked");
                let mut payload = [0; signaling::MAX_NOISE_BYTES];
                let length = initiating.noise.read_message(&second.noise, &mut payload)?;
                if payload[..length] != second.receive_handle.0.to_be_bytes() {
                    return Err(Error::WrongReceiver);
                }
                let mut key = Self::key(initiating.noise, initiating.first, second.clone(), true)?;
                let actions = Actions {
                    signals: vec![key.metadata()],
                    datagrams: vec![key.probe(Kind::Ping)?],
                    activated: false,
                };
                self.pending = Some(key);
                Ok(actions)
            }
            Signal::KeyConfirmation {
                attempt,
                session_generation,
                token,
            } => {
                if !self.pending.iter().chain(self.active.iter()).any(|k| {
                    k.epoch == *attempt && k.generation == *session_generation && k.token == *token
                }) {
                    return Err(Error::InvalidConfirmation);
                }
                Ok(Actions::default())
            }
            Signal::RekeyRequest { session_generation } => {
                if self.local.player > self.remote.player {
                    return Err(Error::UnexpectedSignal);
                }
                if self
                    .active
                    .as_ref()
                    .is_none_or(|k| k.generation != *session_generation)
                {
                    return Err(Error::StaleAttempt);
                }
                self.request_rekey(new_handle, now)
            }
            _ => Err(Error::UnexpectedSignal),
        }
    }

    pub fn retry(&mut self) -> Result<Actions, Error> {
        if let Some(initiating) = &self.initiating {
            return Ok(Actions {
                signals: vec![Signal::Noise1(initiating.first.clone())],
                ..Actions::default()
            });
        }
        if let Some(key) = &mut self.pending {
            return if self.local.player < self.remote.player {
                Ok(Actions {
                    signals: vec![key.metadata()],
                    datagrams: vec![key.probe(Kind::Ping)?],
                    activated: false,
                })
            } else {
                Ok(Actions {
                    signals: vec![Signal::Noise2(key.second.clone())],
                    ..Actions::default()
                })
            };
        }
        if let Some(generation) = self.rekey_requested {
            return Ok(Actions {
                signals: vec![Signal::RekeyRequest {
                    session_generation: generation,
                }],
                ..Actions::default()
            });
        }
        Ok(Actions::default())
    }

    pub fn encrypt(&mut self, inner: Inner<'_>) -> Result<Vec<u8>, Error> {
        self.active.as_mut().ok_or(Error::NotReady)?.send(inner)
    }

    /// accepts only an already unwrapped encrypted datagram. replay is committed
    /// after both authentication and complete inner/confirmation validation.
    pub fn receive(&mut self, bytes: &[u8], now: Instant) -> Result<Reception, Error> {
        self.expire(now);
        let Datagram::Encrypted { header, ciphertext } = wire::decode_datagram(bytes)? else {
            return Err(Error::WrongReceiver);
        };
        let pending = self
            .pending
            .as_ref()
            .is_some_and(|k| k.local_handle == header.receiver && k.phase() == header.phase);
        let key = if pending {
            self.pending.as_mut()
        } else if self
            .active
            .as_ref()
            .is_some_and(|k| k.local_handle == header.receiver && k.phase() == header.phase)
        {
            self.active.as_mut()
        } else {
            self.old
                .as_mut()
                .map(|(k, _)| k)
                .filter(|k| k.local_handle == header.receiver && k.phase() == header.phase)
        }
        .ok_or(Error::WrongReceiver)?;
        if !key.replay.would_accept(header.counter) {
            return Err(Error::Replay);
        }
        let context = SessionContext {
            sender: self.remote,
            receiver: self.local,
            key_epoch: key.epoch,
            announce_hash: self.pair_hash,
        };
        let mut plaintext = vec![0; ciphertext.len()];
        let length =
            key.noise
                .read_message(context.nonce(header.counter), ciphertext, &mut plaintext)?;
        let inner = wire::decode_inner(&plaintext[..length], header.kind)?;
        if pending {
            let expected = if self.local.player < self.remote.player {
                Kind::Pong
            } else {
                Kind::Ping
            };
            if inner.kind != expected
                || inner.sequence != 0
                || inner.talkspurt != 0
                || inner.body != key.token.to_be_bytes()
            {
                return Err(Error::InvalidConfirmation);
            }
        }
        if !key.replay.commit_authenticated(header.counter) {
            return Err(Error::Replay);
        }
        let packet = Received {
            context,
            kind: inner.kind,
            sequence: inner.sequence,
            talkspurt: inner.talkspurt,
            body: inner.body.to_vec(),
        };
        let mut actions = Actions::default();
        if inner.kind == Kind::Ping {
            actions.datagrams.push(key.send(Inner {
                kind: Kind::Pong,
                ..inner
            })?);
        }
        if pending {
            let key = self.pending.take().expect("pending key selected");
            self.old = self.active.take().map(|old| (old, now + OLD_KEY_OVERLAP));
            self.active = Some(key);
            self.rekey_requested = None;
            actions.activated = true;
        }
        Ok(Reception { packet, actions })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signaling::IceCredentials;

    fn endpoint(generation: u64) -> (Announce, Vec<u8>) {
        let keys = generate_static_keypair().unwrap();
        (
            Announce {
                version: PROTOCOL_VERSION,
                capabilities: CAP_MEDIA_V2 | CAP_UDP,
                suites: vec![Suite::ChaChaPoly],
                stream_generation: generation,
                static_key: keys.public,
                ice: IceCredentials {
                    restart_generation: 0,
                    ufrag: "test".into(),
                    password: "test".into(),
                },
                candidates: vec![],
                candidates_complete: true,
            },
            keys.private,
        )
    }

    fn pair() -> (Session, Session) {
        let (a, ak) = endpoint(100);
        let (b, bk) = endpoint(200);
        (
            Session::new(1, &a, ak, 2, &b).unwrap(),
            Session::new(2, &b, bk, 1, &a).unwrap(),
        )
    }

    fn pending(a: &mut Session, b: &mut Session, now: Instant) -> Actions {
        let first = a.begin(1, ReceiverHandle(100), now).unwrap();
        let second = b
            .on_signal(1, &first.signals[0], ReceiverHandle(101), now)
            .unwrap();
        a.on_signal(2, &second.signals[0], ReceiverHandle(0), now)
            .unwrap()
    }

    fn connected(a: &mut Session, b: &mut Session, now: Instant) {
        let ping = pending(a, b, now);
        let pong = b.receive(&ping.datagrams[0], now).unwrap();
        a.receive(&pong.actions.datagrams[0], now).unwrap();
    }

    fn start() -> Inner<'static> {
        Inner {
            kind: Kind::Start,
            sequence: 1,
            talkspurt: 1,
            body: &[],
        }
    }

    fn forged(key: &Key, plaintext: &[u8], kind: Kind, counter: u32) -> Vec<u8> {
        let mut ciphertext = vec![0; plaintext.len() + wire::TAG];
        let nonce = (u64::from(key.epoch) << 32) | u64::from(counter);
        let length = key
            .noise
            .write_message(nonce, plaintext, &mut ciphertext)
            .unwrap();
        wire::encode_datagram(Datagram::Encrypted {
            header: Header {
                kind,
                phase: key.phase(),
                receiver: key.remote_handle,
                counter,
            },
            ciphertext: &ciphertext[..length],
        })
        .unwrap()
    }

    #[test]
    fn authenticated_invalid_inner_and_confirmation_do_not_commit_or_activate() {
        let now = Instant::now();
        let (mut a, mut b) = pair();
        let ping = pending(&mut a, &mut b, now);
        let key = a.pending.as_ref().unwrap();
        let mut wrong = key.token.to_be_bytes();
        wrong[0] ^= 1;
        let invalid = wire::encode_inner(Inner {
            kind: Kind::Ping,
            sequence: 0,
            talkspurt: 0,
            body: &wrong,
        })
        .unwrap();
        assert!(matches!(
            b.receive(&forged(key, &invalid, Kind::Ping, 0), now),
            Err(Error::InvalidConfirmation)
        ));
        let premature = wire::encode_inner(start()).unwrap();
        assert!(matches!(
            b.receive(&forged(key, &premature, Kind::Start, 0), now),
            Err(Error::InvalidConfirmation)
        ));
        assert_eq!(b.active_epoch(), None);
        let pong = b.receive(&ping.datagrams[0], now).unwrap();
        let key = b.active.as_ref().unwrap();
        let wrong_ack = wire::encode_inner(Inner {
            kind: Kind::Pong,
            sequence: 1,
            talkspurt: 0,
            body: &key.token.to_be_bytes(),
        })
        .unwrap();
        assert!(matches!(
            a.receive(&forged(key, &wrong_ack, Kind::Pong, 0), now),
            Err(Error::InvalidConfirmation)
        ));
        a.receive(&pong.actions.datagrams[0], now).unwrap();
        let key = a.active.as_ref().unwrap();
        // authenticated outer/inner type mismatch must not consume the counter.
        let wrong_kind = forged(key, &wire::encode_inner(start()).unwrap(), Kind::Stop, 1);
        assert!(matches!(
            b.receive(&wrong_kind, now),
            Err(Error::Codec(CodecError::Invalid))
        ));
        let invalid_body = forged(key, &[Kind::Start as u8, 0, 1, 0, 1, 9], Kind::Start, 1);
        assert!(matches!(
            b.receive(&invalid_body, now),
            Err(Error::Codec(CodecError::Invalid))
        ));
        b.receive(&a.encrypt(start()).unwrap(), now).unwrap();
    }

    #[test]
    fn both_directions_exhaust_without_wrap_and_higher_rekeys_without_media() {
        let now = Instant::now();
        let (mut a, mut b) = pair();
        connected(&mut a, &mut b, now);
        for sender in [&mut a, &mut b] {
            sender.active.as_mut().unwrap().next_counter = Some(REKEY_AT - 1);
            assert!(!sender.needs_rekey());
            sender.encrypt(start()).unwrap();
            assert!(sender.needs_rekey());
            sender.active.as_mut().unwrap().next_counter = Some(u32::MAX - 1);
            let last = sender.encrypt(start()).unwrap();
            let Datagram::Encrypted { header, .. } = wire::decode_datagram(&last).unwrap() else {
                panic!("encrypted")
            };
            assert_eq!(header.counter, u32::MAX - 1);
            assert!(matches!(sender.encrypt(start()), Err(Error::Exhausted)));
            assert!(sender.needs_rekey());
        }
        let request = b.request_rekey(ReceiverHandle(0), now).unwrap();
        assert!(request.signals == b.retry().unwrap().signals);
        let first = a
            .on_signal(2, &request.signals[0], ReceiverHandle(200), now)
            .unwrap();
        let duplicate = a
            .on_signal(2, &request.signals[0], ReceiverHandle(201), now)
            .unwrap();
        assert!(first.signals == duplicate.signals);
        assert!(first.signals == a.request_rekey(ReceiverHandle(202), now).unwrap().signals);
        let second = b
            .on_signal(1, &first.signals[0], ReceiverHandle(203), now)
            .unwrap();
        let ping = a
            .on_signal(2, &second.signals[0], ReceiverHandle(0), now)
            .unwrap();
        let pong = b.receive(&ping.datagrams[0], now).unwrap();
        a.receive(&pong.actions.datagrams[0], now).unwrap();
        assert_eq!(a.active_epoch(), Some(2));
        assert_eq!(b.active_epoch(), Some(2));
        assert!(!a.needs_rekey());
        assert!(!b.needs_rekey());
        assert!(b.retry().unwrap().signals.is_empty());
        assert!(matches!(
            a.on_signal(2, &request.signals[0], ReceiverHandle(204), now),
            Err(Error::StaleAttempt)
        ));
        let received = a.receive(&b.encrypt(start()).unwrap(), now).unwrap().packet;
        assert_eq!(received.context.sender.generation, 200);
    }

    #[test]
    fn epoch_generation_and_handle_exhaustion_do_not_replace_active_key() {
        let now = Instant::now();
        let (mut a, mut b) = pair();
        connected(&mut a, &mut b, now);
        assert!(matches!(
            a.begin(2, ReceiverHandle(100), now),
            Err(Error::HandleInUse)
        ));
        assert_eq!(a.active_epoch(), Some(1));
        a.last_attempt = u32::MAX;
        assert!(matches!(
            a.begin(2, ReceiverHandle(200), now),
            Err(Error::Exhausted)
        ));
        a.last_attempt = 1;
        a.last_generation = u64::MAX;
        assert!(matches!(
            a.request_rekey(ReceiverHandle(200), now),
            Err(Error::Exhausted)
        ));
        assert_eq!(a.active_epoch(), Some(1));
        b.receive(&a.encrypt(start()).unwrap(), now).unwrap();
    }
}
