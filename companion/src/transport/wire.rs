//! bounded big-endian codecs; decoding a header or plaintext does not authenticate it.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use super::StreamId;
use super::signaling::{RelayGrant, write_grant_public};
use crate::packet::{MAGIC, VERSION};

pub const MAX_DATAGRAM: usize = 1200;
pub const UDP_HEADER: usize = 10;
pub const INNER_HEADER: usize = 5;
pub const TAG: usize = 16;
pub const MEDIA_OVERHEAD: usize = UDP_HEADER + INNER_HEADER + TAG;
pub const RELAY_HEADER: usize = 5;
pub const RELAY_MEDIA_OVERHEAD: usize = RELAY_HEADER + MEDIA_OVERHEAD;
pub const MAX_OPUS: usize = MAX_DATAGRAM - RELAY_MEDIA_OVERHEAD;
pub const POLICY_REVISION_BYTES: usize = 8;
pub const MAX_POLICY_OPUS: usize = MAX_OPUS - POLICY_REVISION_BYTES;
pub const GAME_START: u8 = 0x07;
pub const GAME_VOICE: u8 = 0x08;
pub const GAME_STOP: u8 = 0x09;
pub const GAME_POLICY_START: u8 = 0x0B;
pub const GAME_POLICY_VOICE: u8 = 0x0C;
pub const GAME_POLICY_STOP: u8 = 0x0D;
pub const SIGNAL_OUT: u8 = 0x06;
pub const SIGNAL_IN: u8 = 0x13;
pub const AUDIENCE: u8 = 0x14;
pub const POLICY: u8 = 0x15;
pub const GAME_OUT_HEADER: usize = 15;
pub const GAME_IN_HEADER: usize = GAME_OUT_HEADER + 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    Truncated,
    Oversize,
    UnknownType,
    Invalid,
    Trailing,
    UnsupportedVersion,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated packet",
            Self::Oversize => "packet or field exceeds bound",
            Self::UnknownType => "unknown packet type",
            Self::Invalid => "invalid packet field",
            Self::Trailing => "unexpected trailing bytes",
            Self::UnsupportedVersion => "unsupported protocol version",
        })
    }
}

impl std::error::Error for CodecError {}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Result<Self, CodecError> {
        if data.len() > MAX_DATAGRAM {
            return Err(CodecError::Oversize);
        }
        Ok(Self(data))
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let bytes = self.0.get(..n).ok_or(CodecError::Truncated)?;
        self.0 = &self.0[n..];
        Ok(bytes)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let mut bytes = [0; N];
        bytes.copy_from_slice(self.take(N)?);
        Ok(bytes)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    pub(crate) fn finish(self) -> Result<(), CodecError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(CodecError::Trailing)
        }
    }
}

pub(crate) fn bounded(data: Vec<u8>) -> Result<Vec<u8>, CodecError> {
    if data.len() > MAX_DATAGRAM {
        Err(CodecError::Oversize)
    } else {
        Ok(data)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Voice = 4,
    Start = 5,
    Stop = 6,
    Ping = 7,
    Pong = 8,
    PolicyVoice = 9,
    PolicyStart = 10,
    PolicyStop = 11,
}

impl TryFrom<u8> for Kind {
    type Error = CodecError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            4 => Ok(Self::Voice),
            5 => Ok(Self::Start),
            6 => Ok(Self::Stop),
            7 => Ok(Self::Ping),
            8 => Ok(Self::Pong),
            9 => Ok(Self::PolicyVoice),
            10 => Ok(Self::PolicyStart),
            11 => Ok(Self::PolicyStop),
            _ => Err(CodecError::UnknownType),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverHandle(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub phase: bool,
    pub receiver: ReceiverHandle,
    pub counter: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inner<'a> {
    pub kind: Kind,
    pub sequence: u16,
    pub talkspurt: u16,
    /// policy media prefixes the ordinary media body with its eight-byte revision.
    /// ordinary voice is opus, ping/pong a probe token, start/stop empty.
    pub body: &'a [u8],
}

fn valid_inner(inner: Inner<'_>) -> Result<(), CodecError> {
    match inner.kind {
        Kind::Voice if !inner.body.is_empty() && inner.body.len() <= MAX_OPUS => Ok(()),
        Kind::Start | Kind::Stop if inner.body.is_empty() => Ok(()),
        Kind::Ping | Kind::Pong if inner.body.len() == 8 => Ok(()),
        Kind::PolicyVoice
            if inner.body.len() > POLICY_REVISION_BYTES && inner.body.len() <= MAX_OPUS =>
        {
            Ok(())
        }
        Kind::PolicyStart | Kind::PolicyStop if inner.body.len() == POLICY_REVISION_BYTES => Ok(()),
        _ => Err(CodecError::Invalid),
    }
}

pub fn encode_inner(inner: Inner<'_>) -> Result<Vec<u8>, CodecError> {
    valid_inner(inner)?;
    let mut out = vec![inner.kind as u8];
    out.extend_from_slice(&inner.sequence.to_be_bytes());
    out.extend_from_slice(&inner.talkspurt.to_be_bytes());
    out.extend_from_slice(inner.body);
    bounded(out)
}

/// call only after tag verification with the session selected by receiver/phase.
/// expected is the outer kind; its authenticated inner copy must match.
pub fn decode_inner(data: &[u8], expected: Kind) -> Result<Inner<'_>, CodecError> {
    let mut r = Reader::new(data)?;
    let kind = Kind::try_from(r.u8()?)?;
    if kind != expected {
        return Err(CodecError::Invalid);
    }
    let inner = Inner {
        kind,
        sequence: r.u16()?,
        talkspurt: r.u16()?,
        body: r.0,
    };
    valid_inner(inner)?;
    Ok(inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyInner<'a> {
    pub kind: Kind,
    pub sequence: u16,
    pub talkspurt: u16,
    pub revision: u64,
    pub opus: &'a [u8],
}

pub fn encode_policy_inner(media: PolicyInner<'_>) -> Result<Vec<u8>, CodecError> {
    if !matches!(
        media.kind,
        Kind::PolicyVoice | Kind::PolicyStart | Kind::PolicyStop
    ) || media.opus.len() > MAX_POLICY_OPUS
    {
        return Err(CodecError::Invalid);
    }
    let mut body = media.revision.to_be_bytes().to_vec();
    body.extend_from_slice(media.opus);
    encode_inner(Inner {
        kind: media.kind,
        sequence: media.sequence,
        talkspurt: media.talkspurt,
        body: &body,
    })
}

/// expected must come from the authenticated outer packet kind.
pub fn decode_policy_inner(data: &[u8], expected: Kind) -> Result<PolicyInner<'_>, CodecError> {
    let inner = decode_inner(data, expected)?;
    if !matches!(
        inner.kind,
        Kind::PolicyVoice | Kind::PolicyStart | Kind::PolicyStop
    ) {
        return Err(CodecError::Invalid);
    }
    let mut r = Reader::new(inner.body)?;
    Ok(PolicyInner {
        kind: inner.kind,
        sequence: inner.sequence,
        talkspurt: inner.talkspurt,
        revision: r.u64()?,
        opus: r.0,
    })
}

/// identity comes from the authenticated key context, never the raw UDP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionContext {
    pub sender: StreamId,
    pub receiver: StreamId,
    pub key_epoch: u32,
    pub announce_hash: [u8; 32],
}

impl SessionContext {
    /// counter is shared by every path in a direction; rekey before u32 exhaustion.
    pub fn nonce(&self, counter: u32) -> u64 {
        (u64::from(self.key_epoch) << 32) | u64::from(counter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindRequest {
    pub session: u32,
    pub hub_generation: u64,
    /// endpoint slot being registered; the relay checks membership in the grant.
    pub player: u16,
    pub attempt: u64,
    pub client_nonce: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindChallenge {
    pub request: BindRequest,
    /// relay-observed source, echoed and covered by the commit/ack HMAC.
    pub endpoint: SocketAddr,
    pub challenge: [u8; 32],
    pub expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    Request(BindRequest),
    Challenge(BindChallenge),
    Commit {
        challenge: BindChallenge,
        proof: [u8; 32],
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Datagram<'a> {
    Encrypted {
        header: Header,
        ciphertext: &'a [u8],
    },
    Relay {
        session: u32,
        datagram: &'a [u8],
    },
    Bind(Bind),
    BindAck {
        challenge: BindChallenge,
        proof: [u8; 32],
    },
}

fn write_request(out: &mut Vec<u8>, request: BindRequest) {
    out.extend_from_slice(&request.session.to_be_bytes());
    out.extend_from_slice(&request.hub_generation.to_be_bytes());
    out.extend_from_slice(&request.player.to_be_bytes());
    out.extend_from_slice(&request.attempt.to_be_bytes());
    out.extend_from_slice(&request.client_nonce);
}

fn read_request(r: &mut Reader<'_>) -> Result<BindRequest, CodecError> {
    Ok(BindRequest {
        session: r.u32()?,
        hub_generation: r.u64()?,
        player: r.u16()?,
        attempt: r.u64()?,
        client_nonce: r.array()?,
    })
}

fn write_challenge(out: &mut Vec<u8>, challenge: BindChallenge) {
    write_request(out, challenge.request);
    match challenge.endpoint.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&challenge.endpoint.port().to_be_bytes());
    if let SocketAddr::V6(endpoint) = challenge.endpoint {
        out.extend_from_slice(&endpoint.flowinfo().to_be_bytes());
        out.extend_from_slice(&endpoint.scope_id().to_be_bytes());
    }
    out.extend_from_slice(&challenge.challenge);
    out.extend_from_slice(&challenge.expires_at_ms.to_be_bytes());
}

fn read_challenge(r: &mut Reader<'_>) -> Result<BindChallenge, CodecError> {
    let request = read_request(r)?;
    let ip = match r.u8()? {
        4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>()?)),
        6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>()?)),
        _ => return Err(CodecError::Invalid),
    };
    let port = r.u16()?;
    let endpoint = match ip {
        IpAddr::V4(ip) => SocketAddr::from((ip, port)),
        IpAddr::V6(ip) => SocketAddr::V6(SocketAddrV6::new(ip, port, r.u32()?, r.u32()?)),
    };
    Ok(BindChallenge {
        request,
        endpoint,
        challenge: r.array()?,
        expires_at_ms: r.u64()?,
    })
}

/// canonical HMAC input: domain label, grant public fields, complete challenge.
/// the grant secret is the HMAC key, not part of this input. the relay checks the
/// requested player/session/hub generation against the grant and requires an
/// outstanding, unexpired, single-use challenge at this exact source endpoint.
pub fn bind_mac_input(
    grant: &RelayGrant,
    challenge: BindChallenge,
    acknowledgement: bool,
) -> Vec<u8> {
    let mut out = if acknowledgement {
        b"ptt-radio-v2 bind ack\0".to_vec()
    } else {
        b"ptt-radio-v2 bind commit\0".to_vec()
    };
    write_grant_public(&mut out, grant);
    write_challenge(&mut out, challenge);
    out
}

pub fn encode_datagram(packet: Datagram<'_>) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    match packet {
        Datagram::Encrypted { header, ciphertext } => {
            if ciphertext.len() > MAX_DATAGRAM - UDP_HEADER {
                return Err(CodecError::Oversize);
            }
            if ciphertext.len() < INNER_HEADER + TAG {
                return Err(CodecError::Truncated);
            }
            out.extend_from_slice(&[header.kind as u8, u8::from(header.phase)]);
            out.extend_from_slice(&header.receiver.0.to_be_bytes());
            out.extend_from_slice(&header.counter.to_be_bytes());
            out.extend_from_slice(ciphertext);
        }
        Datagram::Relay { session, datagram } => {
            if !matches!(decode_datagram(datagram)?, Datagram::Encrypted { .. }) {
                return Err(CodecError::Invalid);
            }
            out.push(0x10);
            out.extend_from_slice(&session.to_be_bytes());
            out.extend_from_slice(datagram);
        }
        Datagram::Bind(bind) => {
            out.push(0x11);
            match bind {
                Bind::Request(request) => {
                    out.push(0);
                    write_request(&mut out, request);
                }
                Bind::Challenge(challenge) => {
                    out.push(1);
                    write_challenge(&mut out, challenge);
                }
                Bind::Commit { challenge, proof } => {
                    out.push(2);
                    write_challenge(&mut out, challenge);
                    out.extend_from_slice(&proof);
                }
            }
        }
        Datagram::BindAck { challenge, proof } => {
            out.push(0x12);
            write_challenge(&mut out, challenge);
            out.extend_from_slice(&proof);
        }
    }
    bounded(out)
}

pub fn decode_datagram(data: &[u8]) -> Result<Datagram<'_>, CodecError> {
    let mut r = Reader::new(data)?;
    let kind = r.u8()?;
    match kind {
        4..=11 => {
            let flags = r.u8()?;
            if flags & !1 != 0 {
                return Err(CodecError::Invalid);
            }
            let header = Header {
                kind: Kind::try_from(kind)?,
                phase: flags == 1,
                receiver: ReceiverHandle(r.u32()?),
                counter: r.u32()?,
            };
            if r.0.len() < INNER_HEADER + TAG {
                return Err(CodecError::Truncated);
            }
            Ok(Datagram::Encrypted {
                header,
                ciphertext: r.0,
            })
        }
        0x10 => {
            let session = r.u32()?;
            // no recursive wrappers: inspect the child type before decoding it.
            if !matches!(r.0.first(), Some(4..=11)) {
                return Err(CodecError::Invalid);
            }
            decode_datagram(r.0)?;
            Ok(Datagram::Relay {
                session,
                datagram: r.0,
            })
        }
        0x11 => {
            let bind = match r.u8()? {
                0 => Bind::Request(read_request(&mut r)?),
                1 => Bind::Challenge(read_challenge(&mut r)?),
                2 => Bind::Commit {
                    challenge: read_challenge(&mut r)?,
                    proof: r.array()?,
                },
                _ => return Err(CodecError::UnknownType),
            };
            r.finish()?;
            Ok(Datagram::Bind(bind))
        }
        0x12 => {
            let challenge = read_challenge(&mut r)?;
            let proof = r.array()?;
            r.finish()?;
            Ok(Datagram::BindAck { challenge, proof })
        }
        _ => Err(CodecError::UnknownType),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameMedia<'a> {
    pub kind: Kind,
    pub stream: StreamId,
    pub sequence: u16,
    pub talkspurt: u16,
    pub opus: &'a [u8],
}

/// sender is omitted outbound; the mod supplies authoritative event.player_index.
pub fn encode_game(media: GameMedia<'_>, forwarded: bool) -> Result<Vec<u8>, CodecError> {
    let kind = match media.kind {
        Kind::Start => GAME_START,
        Kind::Voice => GAME_VOICE,
        Kind::Stop => GAME_STOP,
        _ => return Err(CodecError::Invalid),
    };
    valid_inner(Inner {
        kind: media.kind,
        sequence: media.sequence,
        talkspurt: media.talkspurt,
        body: media.opus,
    })?;
    let mut out = vec![MAGIC, VERSION, kind];
    if forwarded {
        out.extend_from_slice(&media.stream.player.to_be_bytes());
    }
    out.extend_from_slice(&media.sequence.to_be_bytes());
    out.extend_from_slice(&media.stream.generation.to_be_bytes());
    out.extend_from_slice(&media.talkspurt.to_be_bytes());
    out.extend_from_slice(media.opus);
    bounded(out)
}

/// only parses forwarded game media. the caller must match stream against the
/// generation selected by trusted ANNOUNCE before extending seq/deduplicating.
pub fn decode_game(data: &[u8]) -> Result<GameMedia<'_>, CodecError> {
    let mut r = Reader::new(data)?;
    if r.u8()? != MAGIC {
        return Err(CodecError::Invalid);
    }
    if r.u8()? != VERSION {
        return Err(CodecError::UnsupportedVersion);
    }
    let kind = match r.u8()? {
        GAME_START => Kind::Start,
        GAME_VOICE => Kind::Voice,
        GAME_STOP => Kind::Stop,
        _ => return Err(CodecError::UnknownType),
    };
    let player = r.u16()?;
    let sequence = r.u16()?;
    let generation = r.u64()?;
    let talkspurt = r.u16()?;
    valid_inner(Inner {
        kind,
        sequence,
        talkspurt,
        body: r.0,
    })?;
    Ok(GameMedia {
        kind,
        stream: StreamId { player, generation },
        sequence,
        talkspurt,
        opus: r.0,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyGameMedia<'a> {
    /// kind is a policy media kind.
    pub media: GameMedia<'a>,
    pub revision: u64,
}

pub fn encode_policy_game(
    packet: PolicyGameMedia<'_>,
    forwarded: bool,
) -> Result<Vec<u8>, CodecError> {
    let media = packet.media;
    let kind = match media.kind {
        Kind::PolicyStart => GAME_POLICY_START,
        Kind::PolicyVoice => GAME_POLICY_VOICE,
        Kind::PolicyStop => GAME_POLICY_STOP,
        _ => return Err(CodecError::Invalid),
    };
    let inner = encode_policy_inner(PolicyInner {
        kind: media.kind,
        sequence: media.sequence,
        talkspurt: media.talkspurt,
        revision: packet.revision,
        opus: media.opus,
    })?;
    let mut out = vec![MAGIC, VERSION, kind];
    if forwarded {
        out.extend_from_slice(&media.stream.player.to_be_bytes());
    }
    out.extend_from_slice(&media.sequence.to_be_bytes());
    out.extend_from_slice(&media.stream.generation.to_be_bytes());
    out.extend_from_slice(&media.talkspurt.to_be_bytes());
    out.extend_from_slice(&inner[INNER_HEADER..]);
    bounded(out)
}

/// parses only forwarded policy game media; legacy media is a separate codec.
pub fn decode_policy_game(data: &[u8]) -> Result<PolicyGameMedia<'_>, CodecError> {
    let mut r = Reader::new(data)?;
    if r.u8()? != MAGIC {
        return Err(CodecError::Invalid);
    }
    if r.u8()? != VERSION {
        return Err(CodecError::UnsupportedVersion);
    }
    let kind = match r.u8()? {
        GAME_POLICY_START => Kind::PolicyStart,
        GAME_POLICY_VOICE => Kind::PolicyVoice,
        GAME_POLICY_STOP => Kind::PolicyStop,
        _ => return Err(CodecError::UnknownType),
    };
    let player = r.u16()?;
    let sequence = r.u16()?;
    let generation = r.u64()?;
    let talkspurt = r.u16()?;
    valid_inner(Inner {
        kind,
        sequence,
        talkspurt,
        body: r.0,
    })?;
    let revision = r.u64()?;
    Ok(PolicyGameMedia {
        media: GameMedia {
            kind,
            stream: StreamId { player, generation },
            sequence,
            talkspurt,
            opus: r.0,
        },
        revision,
    })
}
