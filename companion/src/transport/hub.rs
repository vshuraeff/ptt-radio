//! the hub's game side: player 0 on the server's Lua UDP port. it registers its receive
//! port with HELLO, grants a relay session to every pair of announced UDP-capable
//! companions, renews grants a pair still uses and honours RelayClose. forwarding itself
//! is the shared `Relay` core. see DESIGN.md, "Relay hub".

use std::collections::HashMap;

use super::relay::{IssueError, Limits, MAX_GRANTS, Outgoing, Relay, RetryAuthorization};
use super::signaling::{CAP_UDP, Signal, decode_game};
use crate::packet::{self, HELLO};

/// the mod forgets a hub registration after 15 s without HELLO
pub const HELLO_EVERY_MS: u64 = 5_000;
/// announced players tracked at once; later ones get no grants
pub const MAX_PLAYERS: usize = 1024;
const RETRY_AFTER_EXPIRY_MS: u64 = 30_000;

/// `R,1,HELLO,port u16 BE`: registers the hub's local receive port with the mod
/// port 0 selects the mod's default 34555
pub fn registration(port: u16) -> Vec<u8> {
    packet::encode(HELLO, port, &[])
}

struct Presence {
    stream_generation: u64,
    at_ms: u64,
}

struct Retry {
    session: u32,
    expires_at_ms: u64,
    deadline_ms: u64,
    generations: (u64, u64),
    authorization: RetryAuthorization,
}

pub struct Hub {
    pub relay: Relay,
    announced: HashMap<u16, Presence>,
    retries: HashMap<(u16, u16), Retry>,
    /// grants and renewals the relay refused (cap, CSPRNG); a refused renewal is retried
    /// on later ticks, including a bounded interval after the old grant expires
    pub failed_offers: u64,
}

impl Hub {
    pub fn new(generation: u64, limits: Limits) -> Hub {
        Hub {
            relay: Relay::new(0, generation, limits),
            announced: HashMap::new(),
            retries: HashMap::new(),
            failed_offers: 0,
        }
    }

    fn counted(&mut self, offered: Result<Vec<Outgoing>, IssueError>) -> Vec<Outgoing> {
        offered.unwrap_or_else(|_| {
            self.failed_offers += 1;
            Vec::new()
        })
    }

    /// one packet from the game on the hub's receive port; anything but inbound SIGNAL
    /// (ROSTER, AUDIENCE, garbage) is ignored. returns SIGNAL_OUT frames for the game.
    pub fn on_game(&mut self, now_ms: u64, data: &[u8]) -> Vec<Outgoing> {
        let Ok((sender, signal)) = decode_game(data, true) else {
            return Vec::new();
        };
        match signal {
            Signal::Announce(a) if a.capabilities & CAP_UDP != 0 && sender != 0 => {
                self.announce(now_ms, sender, a.stream_generation)
            }
            Signal::Announce(_) if sender != 0 => {
                self.announced.remove(&sender);
                self.withdraw_player(sender, now_ms)
            }
            other => self.relay.on_signal(sender, &other),
        }
    }

    fn withdraw_player(&mut self, player: u16, now_ms: u64) -> Vec<Outgoing> {
        self.retries.retain(|&(a, b), _| a != player && b != player);
        let sessions: Vec<u32> = self
            .relay
            .live(now_ms)
            .filter(|g| g.sender == player || g.recipient == player)
            .map(|g| g.session)
            .collect();
        let mut frames = Vec::new();
        for session in sessions {
            frames.extend(self.relay.withdraw(session));
        }
        frames
    }

    fn announce(&mut self, now_ms: u64, player: u16, stream_generation: u64) -> Vec<Outgoing> {
        let mut frames = Vec::new();
        if !self.announced.contains_key(&player) && self.announced.len() >= MAX_PLAYERS {
            return frames;
        }
        let restarted = self
            .announced
            .get(&player)
            .is_some_and(|p| p.stream_generation != stream_generation);
        if restarted {
            // a restarted companion lost its grants: close them and start the pairs over
            frames.extend(self.withdraw_player(player, now_ms));
        }
        self.announced.insert(
            player,
            Presence {
                stream_generation,
                at_ms: now_ms,
            },
        );
        let mut peers: Vec<u16> = self
            .announced
            .keys()
            .copied()
            .filter(|&q| q != player)
            .collect();
        peers.sort_unstable();
        for peer in peers {
            let (sender, recipient) = (player.min(peer), player.max(peer));
            let existing = self
                .relay
                .live(now_ms)
                .filter(|g| (g.sender, g.recipient) == (sender, recipient))
                .max_by_key(|g| g.expires_at_ms)
                .cloned();
            match existing {
                // the same companion again: it may have missed the grant
                Some(grant) => frames.extend(Outgoing::signal(player, &Signal::RelayGrant(grant))),
                None => {
                    let offered = self.relay.offer(sender, recipient, now_ms);
                    frames.extend(self.counted(offered));
                }
            }
        }
        frames
    }

    /// renew active pairs and retry refused renewals through a fixed post-expiry deadline.
    /// retries retain only public pair identity and revocable authorization, never secrets.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Outgoing> {
        let mut frames = Vec::new();
        let grant_ms = self.relay.limits().grant_ms;
        let quarter = grant_ms / 4;
        let mut newest: HashMap<(u16, u16), (u32, u64)> = HashMap::new();
        for g in self.relay.live(now_ms) {
            let current = newest
                .entry((g.sender, g.recipient))
                .or_insert((g.session, g.expires_at_ms));
            if g.expires_at_ms > current.1 {
                *current = (g.session, g.expires_at_ms);
            }
        }
        let announced = &self.announced;
        self.retries.retain(|&(a, b), retry| {
            now_ms <= retry.deadline_ms
                && retry.authorization.valid()
                && announced.get(&a).map(|p| p.stream_generation) == Some(retry.generations.0)
                && announced.get(&b).map(|p| p.stream_generation) == Some(retry.generations.1)
                && newest
                    .get(&(a, b))
                    .is_none_or(|&(session, _)| session == retry.session)
        });
        let retry_pairs: Vec<_> = self.retries.keys().copied().collect();
        let used = |session, player| self.relay.active_ms(session, player) + quarter >= now_ms;
        let due: Vec<_> = self
            .relay
            .live(now_ms)
            .filter(|g| {
                newest.get(&(g.sender, g.recipient)) == Some(&(g.session, g.expires_at_ms))
                    && !self.retries.contains_key(&(g.sender, g.recipient))
                    && g.expires_at_ms.saturating_sub(now_ms) < quarter
                    && used(g.session, g.sender)
                    && used(g.session, g.recipient)
            })
            .map(|g| (g.sender, g.recipient, g.session, g.expires_at_ms))
            .collect();
        for (sender, recipient, session, expires_at_ms) in due {
            let offered = self.relay.offer_renewal(session, now_ms);
            if offered.is_err()
                && self.retries.len() < MAX_GRANTS
                && let (Some(a), Some(b), Some(authorization)) = (
                    self.announced.get(&sender),
                    self.announced.get(&recipient),
                    self.relay.watch_renewal(session),
                )
            {
                self.retries.insert(
                    (sender, recipient),
                    Retry {
                        session,
                        expires_at_ms,
                        deadline_ms: expires_at_ms.saturating_add(RETRY_AFTER_EXPIRY_MS),
                        generations: (a.stream_generation, b.stream_generation),
                        authorization,
                    },
                );
            }
            frames.extend(self.counted(offered));
        }
        for pair in retry_pairs {
            let retry = self.retries.remove(&pair).expect("collected retry exists");
            let offered = if now_ms < retry.expires_at_ms {
                self.relay.offer_renewal(retry.session, now_ms)
            } else {
                self.relay.offer(pair.0, pair.1, now_ms)
            };
            if offered.is_err() {
                self.retries.insert(pair, retry);
            }
            frames.extend(self.counted(offered));
        }
        let relay = &self.relay;
        let retries = &self.retries;
        self.announced.retain(|player, p| {
            p.at_ms.saturating_add(grant_ms) >= now_ms
                || relay
                    .live(now_ms)
                    .any(|g| g.sender == *player || g.recipient == *player)
                || retries.keys().any(|&(a, b)| a == *player || b == *player)
        });
        self.relay.expire(now_ms);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::signaling::{
        Announce, IceCredentials, PROTOCOL_VERSION, Suite, encode_game,
    };

    #[test]
    fn registration_is_the_five_byte_hello() {
        assert_eq!(registration(34555), vec![0x52, 1, 0x01, 0x86, 0xfb]);
        assert_eq!(registration(0), vec![0x52, 1, 0x01, 0, 0]);
    }

    #[test]
    fn refused_offers_are_counted() {
        let limits = Limits {
            grants_per_player: 1,
            ..Limits::default()
        };
        let mut hub = Hub::new(1, limits);
        hub.relay.issue(1, 9, 0).unwrap();
        hub.relay.issue(2, 8, 0).unwrap();
        // both players are at their cap: the hub's grant for the pair is refused
        let announce = |player| {
            encode_game(
                &Signal::Announce(Announce {
                    version: PROTOCOL_VERSION,
                    capabilities: CAP_UDP,
                    suites: vec![Suite::ChaChaPoly],
                    stream_generation: 1,
                    static_key: [0; 32],
                    ice: IceCredentials {
                        restart_generation: 1,
                        ufrag: "u".into(),
                        password: "p".into(),
                    },
                    candidates: Vec::new(),
                    candidates_complete: true,
                }),
                player,
                true,
            )
            .unwrap()
        };
        hub.on_game(0, &announce(1));
        assert!(hub.on_game(0, &announce(2)).is_empty());
        assert_eq!(hub.failed_offers, 1);
    }
}
