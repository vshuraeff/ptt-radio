# ptt-radio architecture

This is a map of the ptt-radio tree at version 0.4.0 (`mod/ptt-radio/info.json`, `companion/Cargo.toml`): the components, how voice and control data move between them, where trust ends, and how the code is built, released and tested. Wire formats, protocol contracts and measurements live in [DESIGN.md](DESIGN.md); this file links to its sections instead of repeating them. Paths are relative to this directory.

## Components

```text
     player A's machine             game server machine             player B's machine
+-----------------------+     +-------------------------+     +-----------------------+
| ptt-radio-companion A |     | ptt-radio-hub (optional)|     | ptt-radio-companion B |
|  mic, PTT key, Opus,  |     |  relay only             |     |                       |
|  AEC, mixer           |     +-------------------------+     |                       |
+-----------------------+          | Lua UDP, 127.0.0.1       +-----------------------+
     | Lua UDP, 127.0.0.1          | as player 0                   | Lua UDP, 127.0.0.1
+-----------------------+     +-------------------------+     +-----------------------+
| Factorio A + mod      |<===>| Factorio server + mod   |<===>| Factorio B + mod      |
+-----------------------+     +-------------------------+     +-----------------------+
                     game multiplayer (lockstep input actions)

 optional UDP routes between companion A and companion B, carrying the same frames encrypted:
   direct          A <--------------------------------------------------> B
   server hub      A <-----------------> ptt-radio-hub <-----------------> B
   player relay    A <------------> a third player's companion <---------> B
```

Each companion talks only to its own local game over loopback Lua UDP; the game multiplayer carries the frames between machines. That game path is always present. The UDP routes are optional and carry the same voice frames, encrypted.

### The mod

`mod/ptt-radio/` is plain Lua, required on the server and on every client. Factorio syncs the server's copy to clients, so every game instance runs the same version.

- `control.lua` wires every event and owns the packet dispatch from `on_udp_packet_received`: HELLO and companion presence, POLICY snapshots (`policy_snapshot`, `audience_changed`), media acceptance (`accept_policy`) and forwarding (`fan_out`), opaque SIGNAL routing (`forward_signal`), `HUD_STATUS` reports, `ECHO_STATE`, open-mic state and the timeouts in `on_nth_tick(15)` and `on_nth_tick(60)`. All timeouts count `game.tick`, so they stand still during saves and pauses.
- `channels.lua` is the channel model with no event wiring: the per-player radio record, the server policy (`storage.radio_policy`: mute, ban, pin, deny, lock, confine, publishers, admin channel), `channels.eligible(sender, listener)` and `channels.tx_permitted(sender)`.
- `admin.lua` is the `/radio-admin` command and the administration section of the settings window; every change goes through `channels.policy_*`.
- `settings_gui.lua` is the per-player settings window (radio switch, TX channel, RX list, presets).
- `hud.lua` is the speaker list in `gui.screen` near the left edge, with the echo marker and the quality bars from `HUD_STATUS`.
- `data.lua` declares the three key bindings and the two shortcut-bar buttons (open mic, settings); the radio clicks are built-in `utility/` sounds. `settings.lua` declares the companion port, echo, half-duplex and broadcast-scope settings.

### The companion crate

`companion/` is one Rust crate, edition 2024, with a library in `src/lib.rs` and three binaries.

| Binary | Source | Role |
| --- | --- | --- |
| `ptt-radio-companion` | `src/main.rs` | the player's program: devices, PTT key, threads, self-update |
| `ptt-radio-hub` | `src/bin/ptt-radio-hub.rs` | the relay beside the server; game side in `src/transport/hub.rs` |
| `relay` | `src/bin/relay.rs`, `src/relay.rs` | stand-in for Factorio and the mod in end-to-end tests |

The library modules fall into layers:

- **I/O owners.** `main.rs` (not in the library) owns the sockets, devices and threads. `runtime_io.rs` binds the UDP sockets, sends from each exact source and runs the STUN name lookup. `audio.rs` opens the cpal input and output and resamples to and from 48 kHz. `ptt_key.rs` is the global hotkey and the per-OS event pump. `virtual_audio.rs` replaces both devices with WAV files for tests (`--mic-wav`, `--speaker-wav`).
- **Connection owner.** `runtime.rs` (`Runtime`) drives everything below it: route choice, sessions, relay registration, POLICY and AUDIENCE state, the outgoing media copies. Time and datagrams are injected, so the socket pump and the network lab run the same code. `policy.rs` (`PolicyGate`) holds the current POLICY and one adjacent previous revision.
- **Sans-I/O cores.** `connectivity/` runs one ICE agent per pair (the `is` crate), discovery and route probes. `crypto/` holds the Noise pair sessions and replay windows. `transport/wire.rs` and `transport/signaling.rs` are the bounded codecs for UDP datagrams, game media and SIGNAL bodies. `transport/relay.rs` is the forwarding core shared by the hub and a player relay. `transport/lab.rs` is the `--netlab` transport for the test router.
- **Audio.** `tx.rs` is the transmit state machine (START, pre-roll, VOICE, STOP). `echo.rs` and `echo/detector.rs` are the gated echo canceller. `reorder.rs` orders compressed frames before the decoder. `jitter.rs` is the per-speaker queue of 10 ms slices. `mix.rs` owns one stream per speaker and mixes them. `quality.rs` measures playout continuity and builds `HUD_STATUS`.
- **Legacy framing.** `packet.rs` encodes HELLO and `ECHO_STATE` and parses ROSTER, `MIC_STATE`, `HALF_DUPLEX` and the v1 media kinds.

`experiments/dtln-runtime` is a dev-dependency used only by the offline echo lab; `experiments/ice-spike` is a separate crate. Neither ships.

### Stand and tools

- `stand/` builds a headless Factorio 2.1.23 server with the mod mounted read-only (`compose.yaml`). `compose.hub.yaml` adds the hub in the server's network namespace, so the hub reaches the server's Lua UDP port on `127.0.0.1` as player 0. `stand/ACCEPTANCE.md` is the live checklist for two real clients.
- `client-mods/` is a mod directory for a test client (`--mod-directory`); its `ptt-radio` entry links to `mod/ptt-radio`.
- `tools/gate.sh` is the full local gate (see [Tests](#tests)). `tools/radio.py` is an audio-less companion stand-in that speaks the v1 framing: `hello` and `listen` exercise presence and roster, while the current mod does not forward its v1 `talk` media. `tools/check-version.sh`, `tools/package-mod.sh` and `tools/archive-readme.sh` serve the release. `tools/public-audit.sh` checks the tree and history before publication.
- `docker/hub.Dockerfile` builds the static hub and its image; `docker/compose.example.yaml` is an example deployment.

## Runtime data flows

### The game path

1. The companion sends to its own game at `127.0.0.1:<--game-port>` (default 34556, the client's `--enable-lua-udp` port).
2. `helpers.recv_udp()` runs in `on_tick`. The engine turns each datagram into an input action, and every game instance raises `on_udp_packet_received` with `event.player_index` set by the engine to the player whose instance received it. The mod takes the sender from that field, never from the payload.
3. `fan_out` calls `helpers.send_udp(port, data, listener.index)` for each permitted listener. The call takes effect only on that listener's own instance, so each machine sends one local copy to its own companion on the `ptt-radio-companion-port` setting (default 34555), with the sender index inserted after the kind byte.
4. The companion's receive thread drops game datagrams whose source IP is not the configured game IP.

At this version `control.lua` forwards only the policy media kinds START3, VOICE3 and STOP3 (`0x0B`–`0x0D`) and authors only POLICY (`0x15`). It has no branch for the v1 kinds `0x02`–`0x04`, the v2 kinds `0x07`–`0x09` or AUDIENCE (`0x14`). The `relay` emulator still speaks AUDIENCE and the v1/v2 bridge, so `tests/scenarios.rs` runs the companion's pre-POLICY branch; `tests/policy_playout.rs` feeds POLICY bytes to the companions itself. The packet layouts are in [Формат пакетов](DESIGN.md#формат-пакетов) and [Companion policy transitions](DESIGN.md#companion-policy-transitions).

The game path stops whenever the game stops ticking: a pause, an autosave, or a player joining with `auto_pause_when_players_connect`. See [Ограничения](DESIGN.md#ограничения).

### Signalling through the mod

Every UDP route is negotiated over the game path. A companion sends SIGNAL `0x06` with a target index; `forward_signal` checks the body bound (1200 bytes), never parses it, and delivers it as `0x13` with the engine-supplied sender to the target player, to the registered hub (target 0), or to all of them (target `0xffff`). Delivery to the hub uses `send_udp(port, data, 0)`, which only the server executes. The hub registers by sending a 5-byte HELLO as player 0 every 5 s.

The SIGNAL bodies are ANNOUNCE (static key, stream generation, ICE credentials and candidates), the Noise handshake, key confirmation, rekey, ICE candidate updates, relay grants and closes, RTT reports and game probes. Once it has a pair, `Runtime::tick` also sends its ANNOUNCE to player 0 every 5 s; the mod delivers it only when a hub has registered. Schema and trust rules: [Signaling schema and trust](DESIGN.md#signaling-schema-and-trust).

### Routes and fallback

Each pair of companions has up to four routes, ranked in `runtime.rs` `rank()`: direct UDP, a server hub session, a player-relay session, and the game.

- **Direct.** One ICE agent per pair in `connectivity/`; the lower player index controls ICE and initiates Noise. Candidates come from the bound sockets, public STUN (`stun.l.google.com:19302` unless `--stun-server` is given) and, with `--hub`, the hub's authenticated BIND reflection.
- **Hub.** The hub grants a relay session to every pair of announced UDP-capable companions; a companion started with `--hub ADDR` binds to it. Each endpoint proves its address with BIND before the hub forwards anything for it.
- **Player relay.** Each companion runs the same `Relay` core under its own player index and offers grants to every pair of other players it has a live direct route to (`Runtime::player_relays`).
- **Game.** Always available while the game ticks.

`Choice::update` picks the route. A pair starts on the game and moves to the first validated UDP route at once. Losing the active route switches at once. Otherwise a route must be at least 10 ms faster by full-route RTT for 3 s before it takes over, and equal RTT prefers direct, then hub, then player relay. The backup is another UDP route when one is live, otherwise the game.

`Runtime::send_media` encrypts each logical packet once per listener and sends the same ciphertext on the primary and backup UDP routes. It also sends one plaintext game copy for every packet, whatever routes are selected. The receiver merges all copies in one ordering queue per stream, so when the primary fails the backup copies are already arriving; the measured gaps are in [Network lab](DESIGN.md#network-lab). Probe intervals and liveness budgets are in [Connectivity and route probes](DESIGN.md#connectivity-and-route-probes); selection rules in [Companion runtime](DESIGN.md#companion-runtime); grants and BIND in [Relay hub](DESIGN.md#relay-hub).

### POLICY and the audience

The mod alone decides who may send to whom. `policy_snapshot()` builds one row per connected player and player 0:

- `tx` is true when the player's companion is live and announced `pol1` in HELLO, and `channels.tx_permitted` allows transmission;
- `send_to` and `receive_from` are the other policy-capable players for which `channels.eligible` holds in each direction; a row whose sets would exceed 256 players is emptied and loses `tx`;
- the self-monitor flag needs `tx`, the `ptt-radio-echo` setting and self-eligibility;
- `hears` additionally covers viewers without a capable companion, so they still see the speaker list and hear the clicks.

`audience_changed()` runs on HELLO capability changes, joins and leaves, force changes, surface changes, setting changes, promotions and channel or admin edits. When any row changed, the revision (eight bytes) advances and POLICY goes to every connected capable companion, including players whose own row is unchanged. Every HELLO from a capable companion also gets the current POLICY back. The hub gets ROSTER but no POLICY: it never announces `pol1`, so its row has no media rights, and media permission is not a forwarding permission.

Both ends enforce it. The mod accepts game media only from a sender with `tx` whose revision is current or the immediately previous one, and forwards a copy only to listeners allowed under the current policy and, for a previous-revision packet, under that one too (`accept_policy`, `fan_out`). The companion sends UDP media only to `send_to`, and accepts game or UDP media only when `PolicyGate::accepts` allows the sender under the packet's revision. A skipped revision purges queued audio. Before the first POLICY, the companion falls back to the AUDIENCE sets; once a POLICY is installed, AUDIENCE is ignored. Details: [Companion policy transitions](DESIGN.md#companion-policy-transitions) and [Каналы и политика воспроизведения](DESIGN.md#каналы-и-политика-воспроизведения).

### The audio pipeline

Transmit, on the transmit thread:

1. The cpal input callback appends mono samples to a shared queue (`audio.rs`).
2. `Capture::try_frame` resamples to 48 kHz and cuts 10 or 20 ms frames (`--frame-ms`).
3. `EchoPath::near_end` runs the sonora AEC3 canceller; its output replaces the microphone only while the detector has found a speaker-to-microphone path. A change of that state sends `ECHO_STATE` to the mod. See [AEC только при обнаруженном эхо](DESIGN.md#aec-только-при-обнаруженном-эхо).
4. `Transmitter` (`tx.rs`) encodes Opus and emits START, the pre-roll (`PREROLL_MS`) and VOICE while PTT is held or the open mic is on, then STOP twice.
5. `Runtime::transmit` stamps the POLICY revision, encrypts and queues the copies; `drain_runtime` sends them.

Receive, on the receive and playout threads:

1. Game copies arrive as `Output::Media` and authenticated UDP copies as `Output::UdpMedia`. Game media for a generation that no ANNOUNCE has selected yet waits in a small bounded hold.
2. `Mixer` keeps one stream per speaker: `reorder.rs` deduplicates and orders compressed frames across all routes and conceals a gap once (FEC from the next frame or PLC), then one Opus decoder, then the `jitter.rs` queue of 10 ms slices with its start, overflow and backlog-shedding rules. See [Приём](DESIGN.md#приём) and [Media identity and ordering](DESIGN.md#media-identity-and-ordering).
3. The playout thread calls `Mixer::tick_checked` every 10 ms slice, rechecking each slice's revision. Half-duplex mutes the slice with a ramp while PTT is held and the open mic is off.
4. `EchoPath::far_end` receives exactly the slice that will sound, as the canceller's reference.
5. The slice is resampled to the device rate and appended to the output queue, which the cpal output callback drains. The thread keeps about 20 ms (`OUTPUT_AHEAD`) queued.

`quality.rs` measures each speaker's playout continuity and the receive thread sends `HUD_STATUS` batches to the mod, which paints the reporter's own bars ([Listener HUD quality](DESIGN.md#listener-hud-quality)).

## Trust boundaries

- **The game server and the mod are trusted.** The engine authors the sender index of every input action, and the mod authors POLICY, ROSTER and the routing of SIGNAL. Every connected player's game instance sees every SIGNAL, including ICE credentials and relay grant secrets. A hostile connected player is outside the protection model: it can, for example, use a grant secret it has seen to claim an endpoint slot and drop that pair's relayed traffic.
- **Channels separate listeners only among honest clients.** The game copy of every voice frame is a plaintext input action replicated to every client, with or without a companion, whatever the channel or the `ptt-radio-scope` setting. A modified client or the server can play any channel.
- **UDP sessions are end-to-end between two companions.** `crypto/mod.rs` uses `snow` 0.10.0 with `Noise_KK_25519_AESGCM_SHA256`, or `Noise_KK_25519_ChaChaPoly_SHA256` when either side does not offer AES. Each companion generates its static key and stream generation at process start (`Runtime::new`) and announces them through the game, which is what makes KK's pre-known keys trustworthy. The prologue binds both players, both stream generations and both ANNOUNCE hashes. A hub or player relay forwards ciphertext without the keys; BIND proofs (HMAC-SHA-256 keyed by the grant secret) stop off-game network attackers from redirecting a session. See [Noise pair sessions](DESIGN.md#noise-pair-sessions) and [UDP and relay wire](DESIGN.md#udp-and-relay-wire).
- **The local machine is trusted.** Any local process can write to the game's Lua UDP port and thereby speak as that player; the companion's localhost socket accepts game datagrams from the game IP only.
- **Self-update trusts GitHub over TLS.** The archive is checked against `SHA256SUMS` from the same release; there is no separate signature.

## Companion process and threads

`main.rs` builds one `Arc<Shared>` and starts three worker threads; the main thread keeps the objects that must stay on it.

| Thread | Owns or does |
| --- | --- |
| main | self-update check before anything else; registers `PttKey` and pumps its OS events every 10 ms (macOS `NSApplication` queue, Windows message queue); owns the cpal input stream (`Microphone`) and the cpal output stream, because cpal streams are not `Send`; sends HELLO and `ECHO_STATE` every 5 s; shutdown and joins |
| transmit | `Source::next_frame`, `EchoPath::near_end`, `Transmitter`, `Runtime::transmit` |
| receive | polls the UDP sockets (up to 256 datagrams per pass), `Runtime::udp`, `Runtime::tick`, the game socket, ROSTER, `MIC_STATE` and `HALF_DUPLEX`, `HUD_STATUS` batches |
| playout | paced by the output queue (or the clock without a device); mixes, mutes, feeds `EchoPath::far_end`, appends to the output queue |
| resolver | one worker for the STUN name lookup; may block until exit and is never joined |
| others | cpal callback threads touch only the sample queues; the virtual device runs one thread in test mode; global-hotkey has its own thread on X11; the Ctrl-C handler only sets a flag |

Shared state in `Shared`: atomics for `ptt`, `open_mic`, `half_duplex`, `mic_close`, `done` and the output reset epoch; `Mutex<(Runtime, Network)>`; `Mutex<Mixer>`; `EchoPath` with its own internal lock; the output sample queue and `Mutex<OutputHistory>`, which records which speaker contributed to queued output so a revocation can silence it in place. `Microphone` shares `Arc<Mutex<MicCapture>>` with the transmit thread.

Locks are taken in one order: `MicCapture`, then `Runtime`, then `Mixer`, then the output sample queue, then `OutputHistory`. The trace file, the sent-frame map and the canceller lock are leaves. `pull_playout` holds the `Runtime` lock from the mixer pull through the append to the output queue, so a slice pulled before a POLICY purge cannot be appended after it.

## Microphone lifecycle

Only a real microphone is gated this way; tone and WAV sources keep the pre-POLICY behaviour.

1. Each main-loop pass calls `Microphone::update`. It opens the cpal input only when `Runtime::microphone_permitted` holds: a POLICY was received or refreshed within `MIC_POLICY_LIFETIME` (60 s) and the current POLICY permits TX. The mod answers every 5-second HELLO with POLICY, so the watchdog fires only after about a minute without answers from the game, for example while it is paused.
2. After the device opens, the permission is checked again under the `MicCapture` and `Runtime` locks; then the echo state is reset and the capture generation advances. A failed open retries after 1 s.
3. When an accepted POLICY is the first one, a reset, or changes TX or self-monitor permission, the receive thread ends the current talk (`close_transmit`) while the old policy and routes are still installed. If the new POLICY denies TX, it also sets `mic_close` for the main thread.
4. `Microphone::close` runs under the same two locks: `close_transmit` sends STOP twice if on air, `drain_runtime` flushes the queued packets, the capture is detached and its generation advanced, the stream is dropped, the echo state is reset and `ECHO_STATE 0` goes to the mod. The transmit thread discards frames whose generation does not match and rechecks the permission before every send.

Releasing PTT does not close the microphone; it stays open for pre-roll and echo detection while TX is permitted.

## Self-update

`self_update.rs` runs from `main` before any socket, audio device or hotkey exists.

1. Skip when `PTT_RADIO_UPDATED` is set (this process is the re-launched one), when the binary was not compiled with `PTT_RADIO_RELEASE_BUILD=1` (a compile-time `option_env!`), or when the user opted out with `--no-self-update` or `PTT_RADIO_NO_SELF_UPDATE`.
2. GET `releases/latest` of the GitHub repository over rustls (`ureq`), and continue only for a strictly newer stable semver.
3. Download `SHA256SUMS` and the platform archive, verify the digest, and take only the companion executable out of the tar.gz or zip in memory, under size caps.
4. Write the new binary beside the current one, swap it in with `self-replace`, keeping a recovery copy for Windows.
5. Re-launch with the same arguments: `exec` on Unix, spawn and exit on Windows.

Every outcome prints one `self-update:` line, and every failure keeps the running binary. Release procedure and the "never delete a release" rule: [RELEASING.md](RELEASING.md#self-update).

## Cargo features

`default = ["audio", "self-update"]`.

| Binary | Required features | How it is built for release |
| --- | --- | --- |
| `ptt-radio-companion` | `audio` | default features, `PTT_RADIO_RELEASE_BUILD=1` |
| `relay` | `audio` | not shipped; built by the tests |
| `ptt-radio-hub` | none | `--no-default-features`, static musl, in `docker/hub.Dockerfile` |

- `audio` pulls cpal, global-hotkey, hound, `is` (ICE), opusic-c (bundled libopus built with cmake and linked statically), sonora, snow, and the macOS `objc2` and Windows `windows-sys` crates for the hotkey event pump. Without it the library compiles only `packet` and `transport` without `transport::lab`, which is all the hub needs.
- `self-update` pulls flate2, self-replace, semver, serde_json, tar, ureq and zip, and compiles `self_update.rs` and the `--no-self-update` flag into the companion binary.

## Build, CI and release

Workflows live in `.github/workflows/`.

### `ci.yml`

Runs on pushes to `master` and on pull requests, except changes limited to `wiki/`, `docs/` or Markdown files. The jobs are independent of each other.

| Job | Runs |
| --- | --- |
| `lint` | `cargo fmt --check`, clippy on all targets in release with `-D warnings`, the hub built with `--no-default-features` |
| `test` | `cargo test --lib --bins` on Linux, macOS and Windows; on Linux also eleven integration suites (all but `scenarios` and `aec_gate`) without their ignored tests |
| `test-release` | release-profile library tests on Linux |
| `release-build-linux` | the release companion build inside `ubuntu:22.04` with the glibc 2.35 check |
| `release-build` | the release companion builds for Windows MSVC and both macOS targets in one Cargo invocation |
| `hub-cache` | master pushes only: builds the hub's `binary` stage for both architectures to warm the BuildKit cache |
| `lua` | `luac5.4 -p` over the mod and its tests, then the four Lua suites |
| `version` | `tools/check-version.sh` |

### Caches

- `Swatinem/rust-cache` in every Rust job. `release-build-linux` and `release-build` use the same `shared-key` (`companion-linux`, `companion-windows`, `companion-macos`) as the matching `release.yml` jobs, so a tag build restores the dependencies master CI compiled. Caches are saved only on `master` pushes; pull requests and tags restore them.
- BuildKit `type=gha` caches with scope `hub-x86_64` and `hub-aarch64`, written by `ci.yml` `hub-cache` and by `hub.yml` `binaries`, and read by the image build. The Dockerfile compiles dependencies against stub sources first, so a source-only change keeps the dependency layer.

### `release.yml` and `hub.yml`

A `v*` tag starts `release.yml`:

1. `version` checks the tag against `info.json` and `Cargo.toml`; `mod` waits for it and packages the mod zip.
2. `companion-linux`, `companion-windows` and `companion-macos` start at once, without waiting for `version`. The macOS job builds both targets, joins them with `lipo`, and signs and notarizes only when the signing secrets are set.
3. `hub` calls `hub.yml`, which checks the version itself, builds the static hub for `linux/amd64` and `linux/arm64` from the Dockerfile's `binary` stage and packages the two tarballs. Its `image` job then pushes the multi-architecture image `ghcr.io/vshuraeff/ptt-radio-hub` (distroless static, non-root) tagged `<ver>`, `<major>.<minor>` and `latest`; it runs only when the ref is a `v*` tag.
4. `release` needs all of the above. It requires exactly six archives (the mod, three companions, two hubs), writes `SHA256SUMS`, takes the notes from the version's section of `mod/ptt-radio/changelog.txt` and publishes a non-draft, non-prerelease release marked latest, which is what the self-updater reads.

`wiki.yml` mirrors `wiki/` into the repository's GitHub wiki on master pushes that touch it. The procedure, asset contents and signing setup are in [RELEASING.md](RELEASING.md).

## Tests

| Suite | Where | What it covers | Runs in |
| --- | --- | --- | --- |
| Lua: `control`, `channels`, `admin`, `settings_gui` | `mod/tests/*_test.lua` | the real handlers and modules against a small Factorio API fake | CI `lua`, gate |
| Rust unit tests | `src/**` (`--lib --bins`) | codecs, reorder, jitter, mixer, runtime, policy, self-update, and in `main.rs` the microphone gate and the output purge | CI `test`, gate |
| `bridge_repr` | `tests/` | the emulator's one-representation rule for v1/v2 listeners | CI, gate |
| `connectivity`, `netlab` | `tests/`, `tests/netlab/` | ICE pairs, discovery and probes; the NAT and link router core | CI, gate |
| `crypto_sessions` | `tests/` | Noise pairs, key confirmation, rekey, replay | CI, gate |
| `transport_contract`, `policy_contract`, `game_probe` | `tests/` | exact wire layouts of media, BIND, POLICY and game probes | CI, gate |
| `receive_order` | `tests/` | the mixer's receive rules through its public API | CI, gate |
| `relay_hub` | `tests/` | relay core, BIND, hub policy, one forward through the lab router | CI, gate |
| `multiparty_lab` | `tests/` | offline closed-loop echo lab for several players; by default only its metric controls run, the lab itself is `--ignored` and needs `PTT_LAB_*` WAV inputs | CI, gate; the lab by hand |
| `aec_gate` | `tests/`, `tests/aec_gate/` | offline gated-AEC lab; by default only the metric control runs, the labs are `--ignored` and need WAV inputs | gate (release, real-time lock); the labs by hand |
| `scenarios` | `tests/scenarios.rs` | real companion processes with virtual audio against the `relay` emulator and the NAT lab: latency, echo, double talk, stalls, route cuts; `quality_sweep` is `--ignored` | gate only, real-time |
| `policy_playout` | `tests/policy_playout.rs` | real companion processes: revocation, open-mic mute, retune during double talk, channel-group playback; all four tests are `--ignored` | CI compiles only; gate runs them with `--ignored`, real-time |

`tools/gate.sh [--twice] [log dir]` runs every step even after a failure and writes one log per step plus `summary.txt`. The real-time steps (`scenarios`, `aec_gate`, the ignored `policy_playout` tests) hold an `flock` on a shared lock file (`PTT_REALTIME_LOCK`, wait `PTT_REALTIME_WAIT`), because two measurements on one machine disturb each other. A failed real-time test is rerun once by exact name and reported as flaky when the rerun passes. The gate needs `cargo`, `flock`, `lua` and `luac` on `PATH`. How the end-to-end stand and network lab work: [Сквозной стенд без игры](DESIGN.md#сквозной-стенд-без-игры) and [Network lab](DESIGN.md#network-lab).

None of these run Factorio. The live check with the stand server, two clients and their companions is `stand/ACCEPTANCE.md`, which has a results table to fill in during such a session.
