//! trusted-game signaling schema; no ICE, Noise, authorization or key storage.

use super::Path;
use super::wire::{AUDIENCE, CodecError, Reader, ReceiverHandle, SIGNAL_IN, SIGNAL_OUT, bounded};
use crate::packet::{MAGIC, VERSION};

pub const PROTOCOL_VERSION: u16 = 2;
pub const CAP_MEDIA_V2: u32 = 1;
pub const CAP_UDP: u32 = 2;
pub const MAX_CANDIDATES: usize = 8;
pub const MAX_CANDIDATE_BYTES: usize = 512;
pub const MAX_CREDENTIAL_BYTES: usize = 256;
pub const MAX_NOISE_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Suite {
    Aes256Gcm = 1,
    ChaChaPoly = 2,
}

impl TryFrom<u8> for Suite {
    type Error = CodecError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Aes256Gcm),
            2 => Ok(Self::ChaChaPoly),
            _ => Err(CodecError::UnknownType),
        }
    }
}

/// negotiate once per pair; absence of a common suite prevents UDP readiness.
pub fn select_suite(local: &[Suite], remote: &[Suite]) -> Option<Suite> {
    [Suite::Aes256Gcm, Suite::ChaChaPoly]
        .into_iter()
        .find(|s| local.contains(s) && remote.contains(s))
}

/// the smaller player index initiates KK and controls ICE.
pub fn is_initiator(local_player: u16, remote_player: u16) -> bool {
    local_player < remote_player
}

// credential/grant types deliberately do not implement Debug: reports must not
// accidentally expose ICE passwords or relay secrets.
#[derive(Clone, PartialEq, Eq)]
pub struct IceCredentials {
    pub restart_generation: u32,
    /// supplied by the caller from an OS CSPRNG, fresh on every restart.
    pub ufrag: String,
    pub password: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Announce {
    pub version: u16,
    pub capabilities: u32,
    pub suites: Vec<Suite>,
    pub stream_generation: u64,
    pub static_key: [u8; 32],
    pub ice: IceCredentials,
    /// bounded SDP candidate attributes, converted by the ICE crate, not here.
    pub candidates: Vec<String>,
    pub candidates_complete: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Handshake {
    pub attempt: u32,
    pub session_generation: u64,
    /// locally allocated key slot, not a player index. sent to the peer for its
    /// outbound header; retained across old/new phase overlap, not reused early.
    pub receive_handle: ReceiverHandle,
    pub noise: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RelayGrant {
    pub session: u32,
    pub relay: u16,
    pub sender: u16,
    pub recipient: u16,
    pub hub_generation: u64,
    pub expires_at_ms: u64,
    pub secret: [u8; 16],
}

/// mod-authored game state, outside opaque peer SIGNAL forwarding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audience {
    pub revision: u64,
    pub send_to: Vec<u16>,
    pub receive_from: Vec<u16>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Signal {
    Announce(Announce),
    Noise1(Handshake),
    Noise2(Handshake),
    /// metadata for correlating the encrypted PING/PONG confirmation; receipt
    /// through signaling cannot activate a key. initiator sends PING first.
    KeyConfirmation {
        attempt: u32,
        session_generation: u64,
        token: u64,
    },
    Rtt {
        probe: u64,
        path: Path,
        rtt_us: u32,
    },
    RelayGrant(RelayGrant),
    /// replacement/trickle records only apply to the selected ICE restart.
    IceCandidates {
        stream_generation: u64,
        restart_generation: u32,
        candidates: Vec<String>,
        complete: bool,
    },
    RelayClose {
        relay: u16,
        session: u32,
        hub_generation: u64,
    },
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8], max: usize) -> Result<(), CodecError> {
    if bytes.is_empty() {
        return Err(CodecError::Invalid);
    }
    if bytes.len() > max {
        return Err(CodecError::Oversize);
    }
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn read_bytes<'a>(r: &mut Reader<'a>, max: usize) -> Result<&'a [u8], CodecError> {
    let len = usize::from(r.u16()?);
    if len == 0 {
        return Err(CodecError::Invalid);
    }
    if len > max {
        return Err(CodecError::Oversize);
    }
    r.take(len)
}

fn read_string(r: &mut Reader<'_>, max: usize) -> Result<String, CodecError> {
    String::from_utf8(read_bytes(r, max)?.to_vec()).map_err(|_| CodecError::Invalid)
}

fn write_candidates(
    out: &mut Vec<u8>,
    candidates: &[String],
    complete: bool,
) -> Result<(), CodecError> {
    if candidates.len() > MAX_CANDIDATES {
        return Err(CodecError::Oversize);
    }
    out.extend_from_slice(&[u8::from(complete), candidates.len() as u8]);
    for candidate in candidates {
        write_bytes(out, candidate.as_bytes(), MAX_CANDIDATE_BYTES)?;
    }
    Ok(())
}

fn read_candidates(r: &mut Reader<'_>) -> Result<(Vec<String>, bool), CodecError> {
    let complete = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(CodecError::Invalid),
    };
    let count = usize::from(r.u8()?);
    if count > MAX_CANDIDATES {
        return Err(CodecError::Oversize);
    }
    let candidates = (0..count)
        .map(|_| read_string(r, MAX_CANDIDATE_BYTES))
        .collect::<Result<_, _>>()?;
    Ok((candidates, complete))
}

fn write_players(out: &mut Vec<u8>, players: &[u16]) -> Result<(), CodecError> {
    if players.len() > 256 {
        return Err(CodecError::Oversize);
    }
    if players.windows(2).any(|w| w[0] >= w[1]) {
        return Err(CodecError::Invalid);
    }
    out.extend_from_slice(&(players.len() as u16).to_be_bytes());
    for player in players {
        out.extend_from_slice(&player.to_be_bytes());
    }
    Ok(())
}

fn read_players(r: &mut Reader<'_>) -> Result<Vec<u16>, CodecError> {
    let count = usize::from(r.u16()?);
    if count > 256 {
        return Err(CodecError::Oversize);
    }
    let players = (0..count).map(|_| r.u16()).collect::<Result<Vec<_>, _>>()?;
    if players.windows(2).any(|w| w[0] >= w[1]) {
        return Err(CodecError::Invalid);
    }
    Ok(players)
}

fn write_path(out: &mut Vec<u8>, path: Path) {
    match path {
        Path::Game => out.push(0),
        Path::Direct => out.push(1),
        Path::Hub { session } => {
            out.push(2);
            out.extend_from_slice(&session.to_be_bytes());
        }
        Path::PlayerRelay { player, session } => {
            out.push(3);
            out.extend_from_slice(&player.to_be_bytes());
            out.extend_from_slice(&session.to_be_bytes());
        }
    }
}

fn read_path(r: &mut Reader<'_>) -> Result<Path, CodecError> {
    match r.u8()? {
        0 => Ok(Path::Game),
        1 => Ok(Path::Direct),
        2 => Ok(Path::Hub { session: r.u32()? }),
        3 => Ok(Path::PlayerRelay {
            player: r.u16()?,
            session: r.u32()?,
        }),
        _ => Err(CodecError::UnknownType),
    }
}

pub(crate) fn write_grant_public(out: &mut Vec<u8>, grant: &RelayGrant) {
    out.extend_from_slice(&grant.session.to_be_bytes());
    out.extend_from_slice(&grant.relay.to_be_bytes());
    out.extend_from_slice(&grant.sender.to_be_bytes());
    out.extend_from_slice(&grant.recipient.to_be_bytes());
    out.extend_from_slice(&grant.hub_generation.to_be_bytes());
    out.extend_from_slice(&grant.expires_at_ms.to_be_bytes());
}

/// signaling includes the secret; BIND HMAC input uses only the public prefix.
pub fn encode_grant(grant: &RelayGrant) -> Vec<u8> {
    let mut out = Vec::with_capacity(42);
    write_grant_public(&mut out, grant);
    out.extend_from_slice(&grant.secret);
    out
}

pub fn encode(signal: &Signal) -> Result<Vec<u8>, CodecError> {
    let mut out = vec![PROTOCOL_VERSION as u8];
    match signal {
        Signal::Announce(a) => {
            if a.version != PROTOCOL_VERSION {
                return Err(CodecError::UnsupportedVersion);
            }
            if a.suites.is_empty()
                || a.suites.len() > 2
                || (a.suites.len() == 2 && a.suites[0] == a.suites[1])
            {
                return Err(CodecError::Invalid);
            }
            out.push(1);
            out.extend_from_slice(&a.version.to_be_bytes());
            out.extend_from_slice(&a.capabilities.to_be_bytes());
            out.push(a.suites.len() as u8);
            out.extend(a.suites.iter().map(|s| *s as u8));
            out.extend_from_slice(&a.stream_generation.to_be_bytes());
            out.extend_from_slice(&a.static_key);
            out.extend_from_slice(&a.ice.restart_generation.to_be_bytes());
            write_bytes(&mut out, a.ice.ufrag.as_bytes(), MAX_CREDENTIAL_BYTES)?;
            write_bytes(&mut out, a.ice.password.as_bytes(), MAX_CREDENTIAL_BYTES)?;
            write_candidates(&mut out, &a.candidates, a.candidates_complete)?;
        }
        Signal::Noise1(h) | Signal::Noise2(h) => {
            out.push(if matches!(signal, Signal::Noise1(_)) {
                2
            } else {
                3
            });
            out.extend_from_slice(&h.attempt.to_be_bytes());
            out.extend_from_slice(&h.session_generation.to_be_bytes());
            out.extend_from_slice(&h.receive_handle.0.to_be_bytes());
            write_bytes(&mut out, &h.noise, MAX_NOISE_BYTES)?;
        }
        Signal::KeyConfirmation {
            attempt,
            session_generation,
            token,
        } => {
            out.push(4);
            out.extend_from_slice(&attempt.to_be_bytes());
            out.extend_from_slice(&session_generation.to_be_bytes());
            out.extend_from_slice(&token.to_be_bytes());
        }
        Signal::Rtt {
            probe,
            path,
            rtt_us,
        } => {
            out.push(5);
            out.extend_from_slice(&probe.to_be_bytes());
            write_path(&mut out, *path);
            out.extend_from_slice(&rtt_us.to_be_bytes());
        }
        Signal::RelayGrant(g) => {
            out.push(6);
            out.extend_from_slice(&encode_grant(g));
        }
        Signal::IceCandidates {
            stream_generation,
            restart_generation,
            candidates,
            complete,
        } => {
            out.push(8);
            out.extend_from_slice(&stream_generation.to_be_bytes());
            out.extend_from_slice(&restart_generation.to_be_bytes());
            write_candidates(&mut out, candidates, *complete)?;
        }
        Signal::RelayClose {
            relay,
            session,
            hub_generation,
        } => {
            out.push(9);
            out.extend_from_slice(&relay.to_be_bytes());
            out.extend_from_slice(&session.to_be_bytes());
            out.extend_from_slice(&hub_generation.to_be_bytes());
        }
    }
    bounded(out)
}

pub fn decode(data: &[u8]) -> Result<Signal, CodecError> {
    let mut r = Reader::new(data)?;
    if r.u8()? != PROTOCOL_VERSION as u8 {
        return Err(CodecError::UnsupportedVersion);
    }
    let signal = match r.u8()? {
        1 => {
            let version = r.u16()?;
            if version != PROTOCOL_VERSION {
                return Err(CodecError::UnsupportedVersion);
            }
            let capabilities = r.u32()?;
            let count = r.u8()?;
            if !(1..=2).contains(&count) {
                return Err(CodecError::Invalid);
            }
            let suites = (0..count)
                .map(|_| Suite::try_from(r.u8()?))
                .collect::<Result<Vec<_>, _>>()?;
            if suites.len() == 2 && suites[0] == suites[1] {
                return Err(CodecError::Invalid);
            }
            let stream_generation = r.u64()?;
            let static_key = r.array()?;
            let ice = IceCredentials {
                restart_generation: r.u32()?,
                ufrag: read_string(&mut r, MAX_CREDENTIAL_BYTES)?,
                password: read_string(&mut r, MAX_CREDENTIAL_BYTES)?,
            };
            let (candidates, candidates_complete) = read_candidates(&mut r)?;
            Signal::Announce(Announce {
                version,
                capabilities,
                suites,
                stream_generation,
                static_key,
                ice,
                candidates,
                candidates_complete,
            })
        }
        kind @ (2 | 3) => {
            let h = Handshake {
                attempt: r.u32()?,
                session_generation: r.u64()?,
                receive_handle: ReceiverHandle(r.u32()?),
                noise: read_bytes(&mut r, MAX_NOISE_BYTES)?.to_vec(),
            };
            if kind == 2 {
                Signal::Noise1(h)
            } else {
                Signal::Noise2(h)
            }
        }
        4 => Signal::KeyConfirmation {
            attempt: r.u32()?,
            session_generation: r.u64()?,
            token: r.u64()?,
        },
        5 => Signal::Rtt {
            probe: r.u64()?,
            path: read_path(&mut r)?,
            rtt_us: r.u32()?,
        },
        6 => Signal::RelayGrant(RelayGrant {
            session: r.u32()?,
            relay: r.u16()?,
            sender: r.u16()?,
            recipient: r.u16()?,
            hub_generation: r.u64()?,
            expires_at_ms: r.u64()?,
            secret: r.array()?,
        }),
        8 => {
            let stream_generation = r.u64()?;
            let restart_generation = r.u32()?;
            let (candidates, complete) = read_candidates(&mut r)?;
            Signal::IceCandidates {
                stream_generation,
                restart_generation,
                candidates,
                complete,
            }
        }
        9 => Signal::RelayClose {
            relay: r.u16()?,
            session: r.u32()?,
            hub_generation: r.u64()?,
        },
        _ => return Err(CodecError::UnknownType),
    };
    r.finish()?;
    Ok(signal)
}

/// outbound peer is the destination; inbound peer is the game-derived sender.
pub fn encode_game(signal: &Signal, peer: u16, inbound: bool) -> Result<Vec<u8>, CodecError> {
    let mut out = vec![MAGIC, VERSION, if inbound { SIGNAL_IN } else { SIGNAL_OUT }];
    out.extend_from_slice(&peer.to_be_bytes());
    out.extend_from_slice(&encode(signal)?);
    bounded(out)
}

pub fn decode_game(data: &[u8], inbound: bool) -> Result<(u16, Signal), CodecError> {
    let mut r = Reader::new(data)?;
    if r.u8()? != MAGIC {
        return Err(CodecError::Invalid);
    }
    if r.u8()? != VERSION {
        return Err(CodecError::UnsupportedVersion);
    }
    if r.u8()? != if inbound { SIGNAL_IN } else { SIGNAL_OUT } {
        return Err(CodecError::UnknownType);
    }
    let peer = r.u16()?;
    Ok((peer, decode(r.0)?))
}

/// only the mod emits this game kind; companion input is never forwarded as it.
/// the reserved sender field is zero, matching other mod-originated headers.
pub fn encode_audience(audience: &Audience) -> Result<Vec<u8>, CodecError> {
    let mut out = vec![MAGIC, VERSION, AUDIENCE, 0, 0];
    out.extend_from_slice(&audience.revision.to_be_bytes());
    write_players(&mut out, &audience.send_to)?;
    write_players(&mut out, &audience.receive_from)?;
    bounded(out)
}

/// no player SIGNAL envelope can decode as this mod-authored game state.
pub fn decode_audience(data: &[u8]) -> Result<Audience, CodecError> {
    let mut r = Reader::new(data)?;
    if r.u8()? != MAGIC {
        return Err(CodecError::Invalid);
    }
    if r.u8()? != VERSION {
        return Err(CodecError::UnsupportedVersion);
    }
    if r.u8()? != AUDIENCE {
        return Err(CodecError::UnknownType);
    }
    if r.u16()? != 0 {
        return Err(CodecError::Invalid);
    }
    let audience = Audience {
        revision: r.u64()?,
        send_to: read_players(&mut r)?,
        receive_from: read_players(&mut r)?,
    };
    r.finish()?;
    Ok(audience)
}
