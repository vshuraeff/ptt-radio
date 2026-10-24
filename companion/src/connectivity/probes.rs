use crate::transport::Path;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const TALK_INTERVAL: Duration = Duration::from_millis(250);
pub const TALK_DEAD: Duration = Duration::from_secs(1);
pub const IDLE_INTERVAL: Duration = Duration::from_secs(1);
pub const IDLE_DEAD: Duration = Duration::from_secs(3);

fn thresholds(active: Path, talking: bool, path: Path) -> (Duration, Duration) {
    if talking && path == active {
        (TALK_INTERVAL, TALK_DEAD)
    } else {
        (IDLE_INTERVAL, IDLE_DEAD)
    }
}

/// changes whenever a route's underlying endpoint or registration changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RouteId {
    pub path: Path,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeEvent {
    SendPing {
        route: RouteId,
        token: u64,
    },
    SendPong {
        route: RouteId,
        token: u64,
    },
    Available {
        route: RouteId,
    },
    Unavailable {
        route: RouteId,
    },
    /// full-route network RTT, never an own-voice echo measurement.
    NetworkRtt {
        route: RouteId,
        token: u64,
        rtt: Duration,
    },
}

impl ProbeEvent {
    pub(super) fn route(self) -> RouteId {
        match self {
            Self::SendPing { route, .. }
            | Self::SendPong { route, .. }
            | Self::Available { route }
            | Self::Unavailable { route }
            | Self::NetworkRtt { route, .. } => route,
        }
    }
}

struct Route {
    id: RouteId,
    // registration starts the initial budget; only an authenticated pong advances it.
    last_response: Instant,
    dead_from: Instant,
    next_ping: Instant,
    live: bool,
    expired: bool,
    pending: VecDeque<(u64, Instant)>,
    rtt: Option<Duration>,
}

/// only authenticated PONGs matching a current route's outstanding token count.
/// the caller authenticates the remote peer and, for UDP, the encrypted inner kind.
pub struct ProbeScheduler {
    routes: HashMap<Path, Route>,
    revision: u64,
    active: Path,
    talking: bool,
}

impl Default for ProbeScheduler {
    fn default() -> Self {
        Self {
            routes: HashMap::new(),
            revision: 0,
            active: Path::Game,
            talking: false,
        }
    }
}

impl ProbeScheduler {
    pub fn register(&mut self, path: Path, now: Instant) -> Option<RouteId> {
        if self.routes.len() >= 16 && !self.routes.contains_key(&path) {
            return None;
        }
        self.revision = self.revision.checked_add(1)?;
        let id = RouteId {
            path,
            revision: self.revision,
        };
        self.routes.insert(
            path,
            Route {
                id,
                last_response: now,
                dead_from: now,
                next_ping: now,
                live: false,
                expired: false,
                pending: VecDeque::new(),
                rtt: None,
            },
        );
        Some(id)
    }

    pub fn remove(&mut self, path: Path) -> Option<RouteId> {
        self.routes.remove(&path).map(|r| r.id)
    }

    pub(super) fn clear(&mut self) -> Vec<RouteId> {
        self.routes.drain().map(|(_, route)| route.id).collect()
    }

    pub fn route(&self, path: Path) -> Option<RouteId> {
        self.routes.get(&path).map(|r| r.id)
    }

    pub fn is_live(&self, route: RouteId) -> bool {
        self.routes
            .get(&route.path)
            .is_some_and(|r| r.id == route && r.live)
    }

    pub fn rtt(&self, route: RouteId) -> Option<Duration> {
        self.routes.get(&route.path).filter(|r| r.id == route)?.rtt
    }

    pub fn set_activity(&mut self, active: Path, talking: bool, now: Instant) {
        for route in self.routes.values_mut() {
            let (_, previous_dead) = thresholds(self.active, self.talking, route.id.path);
            let (_, dead) = thresholds(active, talking, route.id.path);
            if dead < previous_dead {
                // idle history cannot consume the new talk-rate probe window,
                // but an overdue idle deadline must not gain transition grace.
                route.dead_from = if !route.expired && now < route.dead_from + previous_dead {
                    route.last_response.max(now)
                } else {
                    route.last_response
                };
                route.next_ping = now;
            } else if dead > previous_dead {
                route.dead_from = route.last_response;
            }
        }
        self.active = active;
        self.talking = talking;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.routes
            .values()
            .map(|r| {
                let (_, dead) = thresholds(self.active, self.talking, r.id.path);
                if r.expired {
                    r.next_ping
                } else {
                    r.next_ping.min(r.dead_from + dead)
                }
            })
            .min()
    }

    /// actions are send intents: the caller must frame/authenticate each one and
    /// send it on precisely the named route. polling at the deadline keeps NAT
    /// mappings alive at <=1 s, including backup paths and dead-route recovery.
    pub fn poll(&mut self, now: Instant) -> Result<Vec<ProbeEvent>, getrandom::Error> {
        let mut events = Vec::new();
        let mut paths: Vec<_> = self.routes.keys().copied().collect();
        paths.sort_by_key(|p| match p {
            Path::Game => (0, 0, 0),
            Path::Direct => (1, 0, 0),
            Path::Hub { session } => (2, 0, *session),
            Path::PlayerRelay { player, session } => (3, *player, *session),
        });
        for path in paths {
            let (interval, dead) = thresholds(self.active, self.talking, path);
            let route = self.routes.get_mut(&path).expect("registered route");
            if !route.expired && now >= route.dead_from + dead {
                route.live = false;
                route.expired = true;
                route.rtt = None;
                events.push(ProbeEvent::Unavailable { route: route.id });
            }
            route
                .pending
                .retain(|(_, sent)| now.saturating_duration_since(*sent) < dead);
            if now >= route.next_ping {
                let mut bytes = [0; 8];
                getrandom::fill(&mut bytes)?;
                let token = u64::from_be_bytes(bytes);
                route.pending.push_back((token, now));
                while route.pending.len() > 16 {
                    route.pending.pop_front();
                }
                route.next_ping = now + interval;
                events.push(ProbeEvent::SendPing {
                    route: route.id,
                    token,
                });
            }
        }
        Ok(events)
    }

    /// inbound PING must already be authenticated for this pair and route.
    /// receiving a PING never changes liveness or RTT.
    pub fn authenticated_ping(&self, route: RouteId, token: u64) -> Option<ProbeEvent> {
        (self.route(route.path) == Some(route)).then_some(ProbeEvent::SendPong { route, token })
    }

    /// inbound PONG must already be authenticated. endpoint similarity or a
    /// SIGNAL RTT record is insufficient. tokens are one-shot and route scoped.
    pub fn authenticated_pong(&mut self, id: RouteId, token: u64, now: Instant) -> Vec<ProbeEvent> {
        let (_, dead) = thresholds(self.active, self.talking, id.path);
        let Some(route) = self.routes.get_mut(&id.path).filter(|r| r.id == id) else {
            return Vec::new();
        };
        let Some(index) = route
            .pending
            .iter()
            .position(|&(t, sent)| t == token && now >= sent && now.duration_since(sent) < dead)
        else {
            return Vec::new();
        };
        let (_, sent) = route.pending.remove(index).expect("matched token");
        let rtt = now.duration_since(sent);
        let mut events = Vec::new();
        if !route.live {
            events.push(ProbeEvent::Available { route: id });
        }
        route.live = true;
        route.expired = false;
        route.last_response = now;
        route.dead_from = now;
        route.rtt = Some(rtt);
        events.push(ProbeEvent::NetworkRtt {
            route: id,
            token,
            rtt,
        });
        events
    }
}
