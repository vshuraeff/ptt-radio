# Building from Source

When you finish this page, you have built the companion and the hub yourself and run the mod's tests.

## What you need

- **Rust** 1.88 or newer (the crate uses edition 2024), installed with [rustup](https://rustup.rs).
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

The result is `target/release/ptt-radio-hub`. The companion itself cannot be built without the `audio` feature.

## Run the tests

From `companion/`:

```sh
cargo test --lib --bins
```

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
cargo test --release --test scenarios -- --nocapture --test-threads=1   # every transport, about 8 minutes
cargo test --release --test scenarios direct:: -- --nocapture           # only direct UDP
PTT_E2E_VOICE=talk.wav cargo test --test scenarios -- --nocapture       # your own voice instead of the synthetic one for player A
```

The scenarios cover mouth-to-ear delay, echo from speakers, double talk, half-duplex and a 500 ms stall in mid-sentence. The modules `game`, `direct`, `hub`, `player_relay` and `fallback` run the same checks over each route. The last run's files are in `target/tmp/scenarios/<transport>/<scenario>/aec-on/` and `aec-off/`: what each player said and heard as WAV, traces and companion logs.

### Echo labs

The offline echo gate lab needs two independent mono 48 kHz 16-bit WAV files:

```sh
PTT_LAB_A=a.wav PTT_LAB_B=b.wav cargo test --release --test aec_gate -- --ignored --nocapture
```

Each row takes 60 seconds by default; `PTT_LAB_SECONDS` changes the length of the main table (at least 20). The WAV files go, unnormalised, to `target/tmp/aec_gate/`.

The multi-player echo lab is `cargo test --release --test multiparty_lab -- --ignored --nocapture`, with voices in `PTT_LAB_A`, `PTT_LAB_B` and `PTT_LAB_C`. `PTT_LAB_PATH` picks the delay model: `game` (the default), `direct` (one-way delays A–B, A–C, B–C of 15, 35 and 25 ms) or `hub` (the two legs to the server, without game ticks). A full run takes about 5 minutes in release.

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

It runs `cargo fmt --check`, clippy on every target, the debug `--lib --bins` tests and every integration suite, the release `--lib` tests, the scenarios, `aec_gate`, the policy timing measurements, `luac -p` over the mod and the four Lua tests. One run takes about 8.5 minutes with a warm cache. `--twice` adds a second full pass of the scenarios to catch flaky checks.

- Logs go to `<target>/gate/<time>/` by default; `summary.txt` there has the exit code and time of every step. The script exits non-zero if any step failed.
- A real-time test that fails is re-run once by its exact name; if the re-run passes, the step is marked `flaky` and does not fail the gate.
- Real-time steps hold a `flock` on a shared lock file, so two jobs on one machine do not measure at once. `PTT_REALTIME_LOCK` sets the file (by default `ptt-realtime.lock` in the git common directory, shared by every worktree), and `PTT_REALTIME_WAIT` limits the wait for it (3600 seconds by default).
- It needs `flock`, `lua` and `luac` in `PATH`; on macOS `flock` comes from Homebrew.

## Use the mod from source

Copy or link the `mod/ptt-radio` folder into your Factorio `mods` folder under the name `ptt-radio`. In a multiplayer game, the server and every player need the same version of the mod.
