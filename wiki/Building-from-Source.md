# Building from Source

When you finish this page, you have built the companion and the hub yourself and run the mod's tests.

## What you need

- **Rust** 1.91 or newer, installed with [rustup](https://rustup.rs). The companion's audio libraries (`sonora` 0.2.0) and the test dependency `tract-onnx` 0.23.9 require it, so the default build and every `cargo test` need 1.91. The hub alone builds on Rust 1.88 or newer (see [Build only the hub](#build-only-the-hub)).
- **cmake** and a **C compiler**: the Opus codec (libopus 1.6.1) is built from bundled sources and linked statically, so no system libopus is needed.
- **Linux only:** the ALSA development package and `pkg-config`, for example on Debian or Ubuntu:

  ```sh
  sudo apt-get install build-essential cmake pkg-config libasound2-dev
  ```

- **macOS:** the Xcode command line tools (`xcode-select --install`) and cmake, for example from Homebrew.
- **Windows:** the Visual Studio C++ build tools and cmake.
- For the mod's tests: `lua` and `luac` in your `PATH`.

## Build the companion

1. Get the source:

   ```sh
   git clone https://github.com/vshuraeff/ptt-radio.git
   cd ptt-radio/companion
   ```

2. Build:

   ```sh
   cargo build --release
   ```

3. The programs are in `target/release/`: `ptt-radio-companion`, `ptt-radio-hub`, and `relay`, a test emulator of the game. Check with `./target/release/ptt-radio-companion --help`. A companion you build yourself never replaces itself with a release; only the builds on the Releases page update themselves.

## Build only the hub

The hub needs no audio. The `audio` feature, on by default, carries everything that deals with sound and the keyboard (the audio library, the global key, the macOS frameworks). Turn it off to build the hub without them, for example on a server without ALSA headers:

```sh
cargo build --release --no-default-features --bin ptt-radio-hub
```

The result is `target/release/ptt-radio-hub`; the command builds no other program. This build needs only Rust (1.88 or newer) and a linker: no cmake, no C compiler for Opus, no ALSA headers and no `pkg-config`. `--no-default-features` also turns off the `self-update` feature, so its download and archive libraries are not compiled. The hub has no self-update in any build; only the companion updates itself. The companion itself cannot be built without the `audio` feature.

## Run the tests

From `companion/`:

```sh
cargo test --lib --bins
```

The integration suites run one at a time, also from `companion/` (CI runs them on Linux only):

```sh
for t in bridge_repr connectivity crypto_sessions game_probe multiparty_lab netlab policy_contract policy_playout receive_order relay_hub transport_contract; do cargo test --test "$t" -- --test-threads 1; done
```

This skips the tests marked ignored: one in `multiparty_lab` and five in `policy_playout`. The `scenarios` and `aec_gate` suites are not in the loop; they are described below.

From the repository root, the mod:

```sh
luac -p mod/ptt-radio/*.lua mod/tests/*.lua
lua mod/tests/control_test.lua mod/ptt-radio/control.lua
lua mod/tests/channels_test.lua
lua mod/tests/admin_test.lua
lua mod/tests/settings_gui_test.lua
```

These run the mod's event handlers without Factorio. They do not replace a test in the real game.

### End-to-end scenarios

These run several companions with virtual audio against the `relay` emulator, with no game, and measure real time. From `companion/`:

```sh
cargo test --release --test scenarios -- --nocapture --test-threads=1   # every transport, about 8.5 minutes
cargo test --release --test scenarios direct:: -- --nocapture           # only direct UDP
PTT_E2E_VOICE=talk.wav cargo test --test scenarios -- --nocapture       # your own voice instead of the synthetic one for player A
```

The scenarios cover mouth-to-ear delay, echo from speakers, double talk, half-duplex (the *Mute the radio while I talk* setting) and a 500 ms stall in mid-sentence. The modules `game`, `direct`, `hub`, `player_relay` and `fallback` run the same checks over each route.

`PTT_E2E_VOICE` takes any WAV file: it is mixed down to mono, resampled to 48 kHz, and then cut or padded with silence to each scenario's talk length (3 s for latency and echo, 3.5 s for double talk, 7 s for the stall, 8 s for hard-cut). The assertions pin a baseline measured with the synthetic voice, so a different voice may not meet the same thresholds.

The last run's files are in `$CARGO_TARGET_TMPDIR/scenarios/<mode>/<scenario>/aec-on/` or `aec-off/`; with the default target directory that is `target/tmp/scenarios/...`.

- `<mode>` is `Game`, `Direct`, `Hub`, `Player` or `Fallback`, after the modules above (`player_relay` writes `Player`). The separate `hard-cut` test uses `CutHub`, `CutGame` and `HubDown` instead.
- `<scenario>` is `latency`, `echo`, `double-talk`, `half-duplex-on`, `half-duplex-off`, `stall`, `hard-cut` or `netlab`.
- `echo` and `double-talk` write both `aec-on/` and `aec-off/`, `hard-cut` writes only `aec-off/`, and `latency`, `half-duplex-*` and `stall` write only `aec-on/`. `netlab` writes `Game/netlab/aec-on/`, and the ignored `quality_sweep` test writes `Game/sweep-<settings>/aec-off/`.
- Each run directory holds `room-ir.wav`, `session.trace` and, in every mode except `Game`, `network.trace`. Each player gets `<p>-mic.wav` (what the player said), `<p>-speaker.wav` (what the player heard), `<p>.trace` and `<p>.log`. The players are `a` and `b`; a third player `c` appears only in `Player` mode, which adds a listener, and in `netlab`, which has three players.

### Echo labs

The offline echo gate lab needs two independent mono 48 kHz 16-bit WAV files:

```sh
PTT_LAB_A=a.wav PTT_LAB_B=b.wav cargo test --release --test aec_gate -- --ignored --nocapture
```

Each row of the results table is computed over 60 seconds of sound by default; `PTT_LAB_SECONDS` sets that length in seconds for the main table (at least 20). The WAV files go, unnormalised, to `target/tmp/aec_gate/`.

The multi-player echo lab is `cargo test --release --test multiparty_lab -- --ignored --nocapture`, with voices in `PTT_LAB_A`, `PTT_LAB_B` and `PTT_LAB_C`. `PTT_LAB_PATH` picks the delay model: `game` (the default), `direct` (one-way delays A–B, A–C, B–C of 15, 35 and 25 ms) or `hub` (the two legs to the server, without game ticks). A full run takes about 5 minutes in release.

Each voice is a path to a WAV file of speech. The voices in `PTT_LAB_A` and `PTT_LAB_B` must each be at least 10 seconds long, or the test panics; voice C may be shorter. 48 kHz mono with 30–40 seconds of speech and pauses is recommended, but any sample rate and channel count is accepted: the channels are averaged to mono and the sound is resampled to 48 kHz. A voice shorter than a scenario is looped. The lab brings each voice to a fixed level measured over its active speech, so the input loudness does not matter. The listening WAV files go to `PTT_LAB_OUT`, by default `target/tmp/multiparty_lab/` (the `multiparty_lab` subdirectory of `$CARGO_TARGET_TMPDIR`).

### Two companions without the game

The `relay` emulator stands in for the game and the mod. In three terminals, from `companion/`: first the emulator for two players, with open microphone on for player 2; then two companions with virtual audio, the second one "with speakers":

```sh
./target/release/relay --player 34556:34555 --player 34566:34565 --open-mic 2
./target/release/ptt-radio-companion --ptt-key none --mic-wav talk.wav --speaker-wav a-heard.wav --auto-ptt 4 --exit-after 8
./target/release/ptt-radio-companion --game-port 34566 --port 34565 --ptt-key none \
  --speaker-wav b-heard.wav --couple-gain-db -6 --couple-delay-ms 2 --exit-after 8
```

Start the emulator first and wait for its `playerN: ...` lines: a HELLO sent before that is lost, and the player list (and with it `--auto-ptt`) only arrives with the next heartbeat, 5 seconds later. `a-heard.wav` is A's voice coming back through B's speakers. The emulator also takes `--echo N`, `--half-duplex N`, `--delay-ms`, `--jitter-ms` and `--stall-every S`. Without `--mic-wav` and `--speaker-wav` the companions use the real devices, so this also checks two live companions on one machine.

### The full gate

```sh
tools/gate.sh [--twice] [log directory]   # from the repository root
```

It runs `cargo fmt --check`, clippy on every target, the debug `--lib --bins` tests and the eleven integration suites above, the release `--lib` tests, the scenarios, the `aec_gate` step, the policy timing measurements (the ignored `policy_playout` tests), `luac -p` over the mod and the four Lua tests. The `aec_gate` step runs without `--ignored`, so only the metric's self-test runs there: the echo labs need your own WAV files and run only by hand, as shown above. The scenarios alone take about 8.5 minutes, so one gate run takes longer than that. `--twice` runs the scenario suite a second time to catch flaky checks, which adds roughly another 8.5 minutes.

- Logs go to `<target>/gate/<time>/` by default; `summary.txt` there has the exit code and time of every step. The script exits non-zero if any step failed.
- A real-time test that fails is re-run once by its exact name; if the re-run passes, the step is marked `flaky` and does not fail the gate.
- The gate's real-time steps (`scenarios-N`, `aec_gate` and `policy_playout`) hold a `flock` on a shared lock file, so two gate runs on one machine do not measure at once. Nothing else takes this lock: a `cargo test` you run by hand is not serialized against a running gate. `PTT_REALTIME_LOCK` sets the file (by default `ptt-realtime.lock` in the git common directory, shared by every worktree), and `PTT_REALTIME_WAIT` limits the wait for it (3600 seconds by default). A step that does not get the lock in time fails with `lock wait over`, and the gate goes on to the next step.
- Outside a git checkout, set `PTT_REALTIME_LOCK`: the default lock path then cannot be opened, and every real-time step fails with `cannot open lock`.
- `tools/gate.sh` is a bash script. It needs `cargo`, `flock`, `lua` and `luac` in `PATH` and exits with code 2 before running any step if one is missing; on macOS `flock` comes from Homebrew.

## Use the mod from source

Copy or link the `mod/ptt-radio` folder into your Factorio `mods` folder under the name `ptt-radio`. In a multiplayer game, the server and every player need the same version of the mod.
