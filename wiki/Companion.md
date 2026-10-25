# Companion

When you finish this page, the companion is running beside the game, it uses the microphone and speakers you chose, and you know how to talk with the push-to-talk key or the open microphone.

The companion is a console program. It talks to your game over UDP on `127.0.0.1`, so it must run on the same computer as the game. It keeps working while you play; close it with `Ctrl-C`.

## First run

1. Set the microphone and the speakers or headphones you want as the **default** input and output devices of your system (see [Devices](#devices)).
2. Open a terminal (on Windows, *Command Prompt* or *PowerShell*) in the folder where you unpacked the companion.
3. Start it:
   - Windows: `.\ptt-radio-companion.exe` (the `.\` is required in PowerShell and also works in Command Prompt)
   - macOS and Linux: `./ptt-radio-companion`
4. Among the first lines you see:

   ```text
   ptt-radio-companion 0.4.0 (libopus 1.6.1, static), listening on 127.0.0.1:34555, game at 127.0.0.1:34556
   push-to-talk key: F8
   ```

5. Start Factorio with `--enable-lua-udp 34556` (see [Installation](Installation)) and join a multiplayer game. The chat shows `Radio: companion connected.` and the companion prints the player list.
6. Hold `F8`, speak, release. The companion prints `ptt pressed` and `ptt released`.

To hear yourself, turn on the per-player mod setting *Echo my own voice*: the server sends your voice back to you, which tests the whole path on your own.

## Options

The options you may need as a player:

| Option | Default | What it does |
| --- | --- | --- |
| `--game-port <PORT>` | `34556` | The game's Lua UDP port: the number after `--enable-lua-udp`. |
| `--port <PORT>` | `34555` | The port the companion listens on. Must match the per-player mod setting *Companion port*. |
| `--ptt-key <KEY>` | `F8` | The push-to-talk key, for example `F8`, `F13`, `shift+F8`, `ctrl+alt+F9` (see below). `none` registers no key; then only the open microphone works. |
| `--hub <IP:PORT>` | none | The relay hub beside the server, if the server runs one (see [Hub](Hub)). An IP address with a port, not a host name. |
| `--game-only` | off | Send voice only through the game. The companion opens no voice routes of its own. |
| `--aec on\|off` | `on` | Echo detection and echo cancellation. With `on`, detection always runs, and cancellation works on your microphone only while the companion hears your speakers in it. With `off` there is neither (see [Echo](#echo-and-half-duplex)). |
| `--bitrate <BIT/S>` | `16000` | Opus bitrate. |
| `--no-playback` | off | Do not open the output device: you talk but hear nothing. |
| `--record-received <FILE.wav>` | none | Also write what you hear to a WAV file. |
| `--udp-bind <ADDR>` | default-route interfaces | The local network interface and port for direct connections (repeat for up to four). |
| `--stun-server <HOST:PORT>` | `stun.l.google.com:19302` | The public STUN server used to find your external address. Repeat it for more. The servers you give replace the default instead of adding to it. At most four addresses are used, and a host name that resolves to several addresses takes up several of the four. With `--game-only` no STUN server is used. |
| `--no-self-update` | off | Do not check for a new release at start (see [Updates](#updates)). |
| `--no-self-uninstall` | off | Keep the installed companion when the mod is removed (see [Removal with the mod](#removal-with-the-mod)). |
| `--launch <GAME COMMAND...>` | none | Start the game and run as long as it does (see [Starting the game with the companion](#starting-the-game-with-the-companion)). Must come last. |

The full list, test options included, is printed by `ptt-radio-companion --help`.

`--ptt-key` takes one main key with optional modifiers in front of it, joined by `+`. The modifiers are `shift`, `ctrl` (or `control`), `alt` (or `option`) and `cmd` (or `command`, `super`, `meta`). Key and modifier names are case-insensitive, but `none` must be written in lowercase: `None` is read as a key name and fails. A key name the companion cannot read, two main keys, or a modifier after the key stops the companion with `error: --ptt-key <KEY>: ...`; with `--launch` the game still starts, without the companion. A valid key that the system refuses, for example because another program holds it, only prints a warning, and the companion keeps running with only the open microphone.

### Two players on one computer

Each copy of the game needs its own pair of ports. For the second player:

1. Start the second game with another port, for example `--enable-lua-udp 34566`.
2. In that game, set the per-player mod setting *Companion port* to `34565`.
3. Start the second companion with `--game-port 34566 --port 34565` and another key, for example `--ptt-key F9`: one key cannot be registered twice. With `--ptt-key none` that player uses the open microphone.

## Updates

The companion from the Releases page keeps itself up to date:

1. At every start it asks GitHub for the latest release of the project.
2. If that release is newer than the running version, it downloads the archive for your system and checks it against the release's `SHA256SUMS`.
3. It replaces its own program file with the new one and restarts itself with the same options. Only the companion is replaced, never the mod.

It moves only to a newer stable release, never to an older or the same version. At every start it prints exactly one `self-update: ...` line, whether or not anything went wrong: `up to date (...)`, `updated to <version>` (printed by the restarted new version), `disabled (opted out)`, `disabled (not a release build)`, or the reason the check failed (no internet, GitHub unreachable, a checksum that does not match, a folder it may not write to). A failed check never stops the companion: it keeps running the version you have. See [Troubleshooting](Troubleshooting#self-update-messages).

The check runs before the companion opens its port and, with `--launch`, before it starts the game, so it holds up the game start. Without an update that takes a few seconds: the release lookup gives up after 5 seconds. While an update downloads, it can take up to about two minutes.

To turn updates off, start the companion with `--no-self-update`, or set the environment variable `PTT_RADIO_NO_SELF_UPDATE=1` (any value other than empty or `0` works). There is no setting to check less often: the companion checks at every start or not at all.

A companion you built yourself with `cargo build` never updates itself; it prints `self-update: disabled (not a release build)`.

## Starting the game with the companion

With `--launch`, the companion starts the game itself. Everything after `--launch` is the game's command line, so put the companion's own options before it. The companion appends `--enable-lua-udp` with its `--game-port` unless the game's command line already has that flag. It runs while the game runs; when the game exits, it ends the transmission in progress, shuts down and exits with the game's exit code. If that code is outside 0-255 (for example a Windows crash code), the game was killed, or the companion could not watch the game, it exits with 1.

If the companion's own setup fails before it starts the game, for example because its port is already taken by another companion, the `--ptt-key` name is invalid or there is no output device, it prints `error: ...` and then `--launch: starting the game without the companion`, and starts the game alone. In the game you then see only "Radio: no companion". If the game itself cannot be started, the companion prints `error: --launch: cannot start <program>: ...` and exits with an error. Once it has tried to start the game, it never starts it a second time: a later failure prints `error: ...` and exits with an error.

In Steam, open the game's *Properties*, and under *Launch options* paste what the installer printed after `Steam launch options:`. It has this form, with the Factorio user-data folder in place of `<user-data>` and `ptt-radio-companion.exe` at the end on Windows:

```text
"<user-data>/ptt-radio-companion/ptt-radio-companion" --launch %command%
```

Steam replaces `%command%` with the game's own command line. Other companion options go before `--launch`, for example `--hub 203.0.113.10:34560 --launch %command%`. An update found at start is installed first, before the game starts; the restarted companion keeps `--launch` and the game's command line.

## Removal with the mod

A companion put in place by the installer sits in the folder `ptt-radio-companion` inside the Factorio user-data folder, beside a file `install.json`. That file records the mods folder: the one given to the installer with `--mods-dir` (`-ModsDir` on Windows), otherwise `mods` inside the Factorio user-data folder, which is the platform default unless the installer was given `--factorio-dir` (`-FactorioDir`). The installer does not search for it. At every start, after the update check, and with `--launch` again after the game exits, the companion looks into that mods folder for the `ptt-radio` mod: any file, folder or link named exactly `ptt-radio` or starting with `ptt-radio_`. Neither the version nor the `.zip` extension is checked, so an old version left in the folder, such as `ptt-radio_0.3.0.zip`, still counts as the mod, and the companion stays.

The companion has to see the mod installed once before it can remove itself. The first check that finds the mod writes an empty file `mod-seen` beside `install.json`. Until then a missing mod means it is not installed yet, for example because the game will download it when you join a server, and the companion stays. Once `mod-seen` exists, a check that finds the mods folder with its `mod-list.json` but no `ptt-radio` mod deletes the companion's folder, prints one `self-uninstall: ...` line and exits. With `--launch`, the game still starts that one time, without the companion. Reinstalling the companion with the installer starts the folder over, without `mod-seen`.

It removes nothing when anything is unclear:

- there is no `install.json`, which is the case for a companion you unpacked or built yourself;
- `install.json` cannot be read or is not the installer's;
- the mods folder is missing or unreadable, or has no `mod-list.json`: the companion prints a warning instead;
- the installer was told to use a mods folder other than the default one;
- the folder holding the program is not named `ptt-radio-companion`, for example because you renamed it or moved the program out of it.

It deletes only its own folder, never the user-data folder or anything else in it. To keep it even without the mod, start it with `--no-self-uninstall` or set `PTT_RADIO_NO_SELF_UNINSTALL=1`. After a removal, delete the Steam launch option as well: it points at a program that is gone.

## Devices

The companion has no device option. It opens the system's **default** output device at startup. If there is none, it stops with `error: no default output device` (any other error while opening the output device stops it the same way); with `--launch` the game starts without it. With `--no-playback` it opens no output device. The microphone stays closed until the game permits transmission, and closes when the radio is off, you are muted or refused, or no permission update arrives from the game for one minute. On macOS, the microphone indicator goes away when the radio is off. Releasing PTT alone does not close the microphone: it keeps capturing for echo detection and for the last 120 ms of pre-roll, which goes out at the start of your next transmission.

A join, autosave or multiplayer catch-up keeps the microphone open unless the pause lasts a minute; voice over established direct and hub routes can continue while the game is frozen. After leaving the game, the microphone indicator can remain for up to a minute. Input reopens when the game next confirms permission to transmit. A missing or failing microphone does not stop the companion: it prints `mic open failed: ...; retrying in 1 s` and retries once a second while transmission is permitted.

- Windows: *Settings → System → Sound*, choose the input and the output.
- macOS: *System Settings → Sound*, the *Output* and *Input* tabs.
- Linux: your desktop's sound settings, or `pavucontrol` / `wpctl` under PipeWire.

After changing the default input, switch the radio off and on: each reopen selects the current default microphone. Restart the companion to change its output device.

Headphones are best. With speakers your microphone picks up the other players, and the companion has to cancel that echo (see [Echo](#echo-and-half-duplex)).

## Talking

There are two ways to transmit.

**Push-to-talk.** Hold `F8` (or your `--ptt-key`) and speak. The key is global: the companion sees it whichever window has focus, and other programs stop receiving it while the companion runs. That is why the default is a function key nobody types and not a letter.

**Open microphone.** The companion transmits all the time. Toggle it in the game with `ALT + V` (control *Toggle open microphone*) or the *Open microphone* button on the shortcut bar. The companion prints `open mic on (you are player N)` or `open mic off ...`. The open microphone turns itself off when the companion stops answering.

`ALT + SHIFT + V` opens the radio settings window, where you choose your channel (see [Channels and Admin](Channels-and-Admin)). You can rebind both keys in the game's controls settings.

## Echo and half-duplex

- **Half-duplex** (per-player mod setting *Mute the radio while I talk*, on by default): while you hold the push-to-talk key, other players' voices are silent, so your speakers cannot feed them back into your microphone. With headphones you can turn it off. It has no effect on the open microphone.
- **Echo detection.** If your microphone hears your speakers, the game shows you a panel: *Your microphone hears your speakers: put on headphones or turn off the open microphone*, with a button that turns the open microphone off. Other players see a red *echo* mark next to your name. Detection can take tens of seconds when the echo is quiet. It runs only with `--aec on`, the default: with `--aec off` neither the panel nor the mark ever appears.

## Testing the radio

The *Test microphone and game loopback* button in the *Companion* section of the radio settings window checks the whole path through the game:

1. The companion plays a short tone, half a second, on your speakers. The tone is not transmitted.
2. It records 3 seconds from your microphone: say something. The radio must be allowed to transmit, as for push-to-talk.
3. It transmits the recording through the game as one transmission. Players who hear you hear it too.
4. The game sends the recording back to you for this test only, and the companion plays it. Half-duplex does not mute it, because the test never holds the push-to-talk key.
5. Two seconds after the last frame, the window shows the result: whether the tone played (*Tone*), the loudest level of your microphone (*Microphone peak*), how many frames were sent and returned, and how long the first one took to come back (*First return*).

The outcome is one of: *completed* (everything worked); *transmission not permitted*; *could not open the microphone* (it did not open or stopped delivering sound); *no audio frames returned*; *companion busy transmitting*. The test answers *companion busy transmitting* while you hold the push-to-talk key or the open microphone is on, and pressing the key during the test ends it with the same outcome.

## Console messages

| Message | Meaning |
| --- | --- |
| `self-update: ...` | Printed once at every start: the result of the update check (see [Updates](#updates)). |
| `ptt-radio-companion 0.4.0 (...), listening on 127.0.0.1:34555, game at 127.0.0.1:34556` | Started. The two ports are `--port` and `--game-port`. |
| `output: <device> (<rate> Hz, <n> ch)` | The default output device was opened, with its own sample rate and channel count. Not printed with `--no-playback`. |
| `push-to-talk key: F8` | The key was registered. |
| `warning: push-to-talk key F8 not registered: ...; only open mic works` | The system refused the key, usually because another program holds it. Pick another `--ptt-key`. |
| `error: ...` | Setup failed and the companion stopped, for example `cannot bind 127.0.0.1:34555: ... (another companion or radio.py running?)`, `--ptt-key <KEY>: ...` or `no default output device`. |
| `--launch: starting the game without the companion` | With `--launch` only: the companion's own setup failed (the `error: ...` line just before says why), so it starts the game without itself. If that start fails too, it prints `error: --launch: cannot start <program>: ...` and exits with an error. |
| `warning: no ROSTER from the game at 127.0.0.1:34556: ...; still trying` | No answer from the game after 3 seconds; printed once per run. Normal while you are still in the main menu; if no `roster:` follows after you join a multiplayer game, see [Troubleshooting](Troubleshooting). |
| `roster:` and lines `player N  Name  rgb(...)` | The game answered; this is the player list. It is printed again when it changes. |
| `warning: ignoring a packet that is not ptt-radio protocol v1 (mod version mismatch?)` | The game sent a packet the companion does not understand, usually because the mod and the companion come from different releases. Printed once, at the first such packet; later ones are only counted. |
| `open mic on (you are player N)` / `open mic off (you are player N)` | The open microphone state from the game: printed for the first state the game sends, `off` included, then at each change. |
| `half-duplex on` / `half-duplex off` | The *Mute the radio while I talk* setting differs from what the companion holds. The companion starts with `on`, the default, so with the default nothing is printed. If you turned the setting off, `half-duplex off` appears when the game first reaches the companion, then a line each time you switch it. |
| `ptt pressed` / `ptt released` | The push-to-talk key went down or up. |
| `sent START` / `sent STOP` | A transmission began or ended. |
| `input: <device> (<rate> Hz, <n> ch)` | The companion is opening this input device. Printed on each attempt once the default input device has been found and its default configuration read, normally just before `mic opened` or `mic open failed`. With no default input device, or when its configuration cannot be read, only `mic open failed` appears. |
| `mic opened` / `mic closed (...)` | The real input device opened or was released. |
| `mic open failed: <error>; retrying in 1 s` | The microphone could not be opened. The companion tries again every second while transmission is permitted. |
| `game started (pid N)` / `game exited (...)` | With `--launch`: the game was started, or it has exited and the companion shuts down. |
| `self-test N: Ok, tone 1, mic peak -12 dBFS, sent 150 returned 150 frames, first return 80 ms` | A radio test finished; the same result goes to the game window. |
| `self-uninstall: ...` | The mod is gone and the companion removed its folder, or something was unclear and it removed nothing. |
| `stats: sent N frames \| received [...] \| ...` | Printed when the companion exits: what it sent and received. |

## Your system

### Windows

- **SmartScreen.** The first start may show *Windows protected your PC*. Click *More info*, then *Run anyway*.
- **Firewall.** If Windows asks whether `ptt-radio-companion` may use networks, allow it. If you refuse, voice still works through the game, only with more delay.
- **Updates.** Keep the companion in a folder you can write to, such as your *Downloads* or *Documents*, not under *Program Files*: otherwise it cannot replace itself.

### macOS

- **Gatekeeper.** If the release build is not signed, macOS refuses to open it the first time. Open *System Settings → Privacy & Security* and click *Open Anyway* next to the message about `ptt-radio-companion`, then start it again. Alternatively, remove the download mark in Terminal: `xattr -d com.apple.quarantine ptt-radio-companion`.
- **Microphone.** When the radio first permits transmission, macOS asks for microphone access. When you start the companion from Terminal, the request is made for your terminal app. If you refused, allow it under *System Settings → Privacy & Security → Microphone*. The companion retries while transmission is permitted; restart it if access is still refused after changing the permission.
- **The key.** The companion registers `F8` as an ordinary global shortcut and does not ask for Accessibility or Input Monitoring. On many Mac keyboards `F8` is a media key; press `fn + F8`, or turn on *Use F1, F2, etc. keys as standard function keys* in the keyboard settings.

### Linux

- **Minimum.** The release build needs glibc 2.35 or newer: Ubuntu 22.04 or newer, Debian 12 or newer, or a distribution of the same age.
- **Sound.** The companion uses ALSA. On PipeWire or PulseAudio systems it reaches the sound server through the ALSA plugin (`pipewire-alsa` or the PulseAudio ALSA plugin), which most desktops install by default. The program needs the ALSA library (`libasound2` or `alsa-lib`).
- **The key works on X11 only.** On a pure Wayland session the global key does not work, even though the companion still prints `push-to-talk key: F8`. Use the open microphone, or log into an X11 session.
