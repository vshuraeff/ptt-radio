# Full ICE agent spike

Verdict: **adopt `is = "=0.11.1"` with named integration gaps**. Two full agents
complete authenticated STUN checks and nominate usable UDP paths through the NAT
model below. The dependency fits a single nominated direct path per peer; the
transport manager can compare that path's application RTT with independently
probed hub, player-relay and game routes. This experiment changes no companion
runtime code and supplies no general-purpose network lab.

## Reproduce

From this directory, with Rust 1.85 or newer:

```sh
cargo run --locked -j 1
cargo test --locked -j 1 -- --nocapture
cargo fmt -- --check
cargo clippy --locked --all-targets -j 1 -- -D warnings
cargo tree --locked -e features
```

The checked-in lockfile also pins transitive dependencies. The executed native
toolchain was Rust 1.99.1, `x86_64-apple-darwin`; the declared minimum toolchain was
not separately tested. No sockets, real servers, sleeps, audio or external
services are used by the executable. Each connection attempt has a 30-second
simulated deadline, driven in 5 ms steps. Network scheduling, fixtures and
relative outcomes are deterministic; the crate's internal STUN transaction IDs
remain random and are not compared.

## What the network actually does

Each peer has private host candidates behind a separate NAT. Outbound datagrams
allocate a binding and record their actual destination. Inbound datagrams must
match an allocated public endpoint and its filter before translation to a
private socket. Private remote candidates cannot bypass that lookup. Mapping
and filtering are separate axes:

- EIM: a private endpoint retains its public binding across destinations.
- EDM: the destination endpoint is part of the mapping key; each new destination
  receives a sequential, distinct public port.
- EIF accepts any source at an allocated binding. Address filtering permits only
  previously contacted IPs. Endpoint filtering requires the exact contacted
  source IP and port.

The small `gather` reflector models an outbound discovery request and its return
through the NAT before exposing the mapped address. It abstracts discovery
payloads; it is not a STUN-server implementation. Agent-to-agent ICE packets are
the crate's real serialized, authenticated STUN messages. Candidate strings and
credentials travel through an in-process signaling queue. No candidate is
automatically marked viable, nominated or deliverable by signaling.

Success requires **both** agents to reach a connected state and emit a nominated
source/destination tuple, followed by application probes delivered in both
directions through the same NAT rules. Application probes exclude the synthetic
loss rule, so this checks path usability rather than media loss resilience.
The filter unit test checks all six mapping/filter combinations, same-IP versus
different-port sources, foreign IPs and unallocated public ports.

## Executed matrix

Times include 2 seconds of additional checks and a 200 ms application-probe
window after initial nomination. They are simulated lab timings, not performance
measurements or predictions of real routers.

| Scenario | Result at both peers | Evidence |
| --- | --- | --- |
| EIM / EIF at both ends | Connected, 2,315 ms | Both nominate port 40000; probes delivered |
| EIM / address filtering at both | Connected, 2,315 ms | Both nominate port 40000; probes delivered |
| EIM / endpoint filtering at both | Connected, 2,315 ms | Exact-source pinholes required; probes delivered |
| Hidden A reflexive address; A endpoint filter, B EIF; both EIM | Connected, 2,315 ms | A gains a local peer-reflexive candidate; B gains a remote peer-reflexive candidate; neither was supplied by signaling |
| Same hidden A, both endpoint filters | Disconnected, 30,000 ms | All 27 packets filtered; no nomination |
| EIM / endpoint filtering, loss, reorder, restart | Connected after restart, 2,970 ms total | 6 packets lost, 2 deliveries reordered; fresh credentials, changed public ports 41001, second nomination on both agents; stale signaling dropped |
| EIM / EIF A against EDM / endpoint filter B | Connected, 2,365 ms | B's peer-specific port 40002 becomes peer-reflexive; proves EDM alone does not imply failure |
| EDM / endpoint filtering at both | Disconnected, 30,000 ms | All 36 packets filtered; no nomination; bounded relay/fallback decision belongs to caller |
| Two interfaces, EIM / endpoint filtering | Connected, 2,705 ms | Two remote addresses viable; first non-nominated pair for faster remote exposes 90 ms RTT and 4 successful responses, versus nominated 165 ms |

Loss drops every seventh emitted ICE datagram, starting with the first; every
third datagram receives 60 ms additional delay. A counted reorder is an actual
delivered serial older than a serial already delivered in the same peer-to-peer
direction; opposite-direction scheduling cannot increment it. The restart replaces
NAT bindings, clears candidates via `ice_restart(..., false)`, regathers and exchanges
new credentials/candidates. The lab's caller-side generation filter rejects a
delayed old-generation signal; this is not a crate signaling feature. A
separate test takes a real authenticated request emitted before restart and
checks that `accepts_message` rejects it after credential replacement. Old packets
in the remapped network can be filtered before reaching the agent; the matrix
does not prove old-generation rejection through `handle_packet`. This
does not claim signaling retries, NAT binding expiration, concurrent asymmetric
restarts or application media continuity were tested.

## Nomination and the transport manager

The two-interface scenario gives interface 1 an 80 ms one-way delay and higher
ICE preference; interface 2 has 5 ms one-way delay. After checks, both remote
addresses satisfy `has_viable_remote_candidate`. Nevertheless, both agents
nominate interface 1, and the measured nominated RTT is 165 ms including the
lab's event-loop quantization. ICE nomination therefore does not implement the
project's RTT optimization policy.

Per-remote-candidate diagnostics are also available: iterate `remote_candidates`
and call `find_pair_for_equivalent_remote_candidate`. On its inferred return
value, the experiment successfully calls `state`, `last_successful_rtt`,
`responses_received`, `total_round_trip_time` and `is_nominated` from outside the
crate. On both peers, the first pair for the faster remote is succeeded and
non-nominated, reports 90 ms RTT and four successful responses (360 ms total),
while the nominated pair reports 165 ms. Thus several validated pairs remain
available and have measurable RTT; visibility is not limited to the nominated
pair. The scheduler source continues checks on non-nominated pairs too. The
experiment observes their continued success during its 2-second check window,
not a long-duration NAT-expiry or repair test.

The lookup returns only the **first** pair matching an equivalent remote
candidate, without filtering its current state. With multiple local candidates,
it cannot enumerate all pairs or select a pair by its local/remote tuple. This
also explains the observed 90 ms first-pair RTT: it need not represent the
fastest combination of the two interfaces. `CandidatePair` and its state type
are public inside a private module, so their methods work through inferred
values but the types cannot be named through the crate's public paths. Depending
on this surface deserves care when upgrading the pinned dependency.

`invalidate_candidate` can remove choices; setting local preference before
candidate addition affects priority, and `recreate_candidate_pairs` rebuilds the
checklist. These offer potential indirect steering, which this lab has not
exercised, with rechecking and candidate-lifecycle effects, rather than a direct
"send using this validated pair" or nomination-selection API. `DiscoveredRecv`
is a possible receive address, not a validated bidirectional path by itself.
The documented send contract uses the latest `NominatedSend` tuple. The lab does
not claim application-supported sending through arbitrary non-nominated pairs.

Adopt one ICE-selected direct route, then use the project's authenticated PING /
PONG policy to compare it against independent relay/game routes. A replacement
nomination must replace the direct-route tuple and invalidate its old route
measurement. Restart in this lab proves that changed endpoints are observable
through second nomination events and deliver application probes. Per-remote RTT
can inform a future experiment with indirect steering; clean arbitrary
local/remote-pair selection would need a supported upstream API. No `rice-proto`
experiment was needed: the required direct-versus-relay policy is compatible
with this boundary.

## U17 API and responsibilities

| Stage | API / data | Caller responsibility |
| --- | --- | --- |
| Per peer | `IceAgent::new(IceCreds { ufrag, pass })`; `set_controlling(bool)` | One full agent per peer; deterministic role agreement; supply credentials from an OS CSPRNG |
| Gather | `Candidate::host`, `Candidate::server_reflexive`; `add_local_candidate` | Enumerate/bind sockets and obtain reflector/STUN mappings externally; no TURN allocation is included |
| Signal | `local_credentials`, `local_candidates`, `Candidate::to_sdp_string` / `from_sdp_string`, `set_remote_credentials`, `add_remote_candidate` | Send ufrag **and password**, candidates and explicit ICE generation over trusted game signaling; reject stale generations; bound candidate/body counts |
| Drive | `poll_timeout`, `handle_timeout`, `poll_transmit` | Prime with `handle_timeout`; schedule monotonic timers; send each returned datagram from its exact local socket |
| Receive | `StunMessage::parse`, `StunPacket`, `accepts_message`, `handle_packet` | Demultiplex STUN from application packets; supply actual source/destination; verify peer/session binding before media admission |
| Events | `IceRestart`, `IceConnectionStateChange`, `DiscoveredRecv`, `NominatedSend` | Exchange restart state; track diagnostics/possible receive addresses; publish and replace nominated direct route |
| Measure | `nominated_pair_rtt`, `nominated_pair_total_rtt`, `nominated_pair_responses_received`, `has_viable_remote_candidate`; `remote_candidates` + `find_pair_for_equivalent_remote_candidate` and inferred pair methods | Inspect first-pair-per-equivalent-remote RTT/state/count diagnostics; do not infer per-local-tuple selection; apply full-route application PING/PONG to manager route choices |
| Restart / remove | `ice_restart(new_creds, keep_local_candidates)`, `invalidate_candidate` | Fresh credentials and signaling generation; re-exchange remote candidates; preserve/fallback media policy separately |
| Bound | `set_max_candidate_pairs`, `set_mtu`, RTO setters, `ice_timeout` | Enforce application deadlines and aggregate multi-peer pacing/resource limits; the crate has no final failed/closed state |

The published `IceCreds::new()` delegates through `Id::random()` to
`NonCryptographicRng` in the pinned `str0m-proto`. U17 must construct the public
credential fields from the project's secure RNG instead of relying on that
convenience generator. The lab intentionally uses public, fixed fixture values.

## Build evidence and remaining gaps

The native macOS build, test and clippy commands pass without OpenSSL, cargo-c,
`librice`, pkg-config or any installed third-party system library. `is` enables
its Rust `hmac` / `sha1` implementation by default and depends on
`str0m-proto = 0.7.0` with default features disabled. The resolved feature tree
has no DTLS/OpenSSL feature. `otool -L target/debug/ice-spike` reports only the
macOS `libSystem.B.dylib`. Rust ICE dependencies are linked into the executable;
this is not a claim of a fully static macOS executable.

Windows and Linux are **source/metadata assessed only**: the agent and proto
sources use portable Rust networking/time types with no OS-only gate or native
link requirement on this configuration. Resolved metadata has no `links`
packages. CPU-feature detection has target-specific libc dependencies on some
Linux/Apple architectures, which are ordinary platform support rather than an
extra ICE library. No cross-compilation or execution on those platforms was run.

Named gaps for adoption are external address discovery, secure credential
generation, generation-aware reliable signaling, timer/socket multiplexing and
global pacing across peers, authenticated application probes/media, and the
manager's fallback/liveness deadlines. U19 still owns the general network/NAT
lab. Real routers, binding lifetimes/hairpinning, IPv6/TCP, TURN, role conflicts,
multiple concurrent peers, asymmetric restarts and long-duration path repair
remain unexercised here. The small matrix supplies no direct-connectivity rate
estimate.

## Primary sources inspected

- [`is` crate documentation](https://docs.rs/is/latest/is/) (reports 0.11.1).
- [Pinned `is` agent source](https://github.com/algesten/str0m/blob/95e5038c6f45de6646d8e3f4c2bca6207a5bedde/crates/is/src/agent.rs), matched to the downloaded package's VCS metadata: events, nomination, restart and viability.
- [Pinned proto ID generator](https://github.com/algesten/str0m/blob/9b159720be773693dd561dec0fb20c6d7e4aa323/crates/proto/src/id.rs), matched to `str0m-proto` 0.7.0 package metadata.
- [Upstream ICE design notes](https://github.com/algesten/str0m/blob/main/docs/ice.md), background only; the pinned source is authoritative where design notes differ.

Downloaded crates.io package sources, `Cargo.toml` metadata and the resolved
feature tree were also inspected. Research date: 2026-10-24.
