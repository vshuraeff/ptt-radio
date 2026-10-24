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
| `--ptt-key <KEY>` | `F8` | The push-to-talk key, for example `F8`, `F13`, `shift+F8`. `none` registers no key; then only the open microphone works. |
| `--hub <IP:PORT>` | none | The relay hub beside the server, if the server runs one (see [Hub](Hub)). An IP address with a port, not a host name. |
| `--game-only` | off | Send voice only through the game. The companion opens no voice routes of its own. |
| `--aec on\|off` | `on` | Echo cancellation. It switches itself on only after the companion hears your speakers in your microphone. |
| `--bitrate <BIT/S>` | `16000` | Opus bitrate. |
| `--no-playback` | off | Do not open the output device: you talk but hear nothing. |
| `--record-received <FILE.wav>` | none | Also write what you hear to a WAV file. |
| `--udp-bind <ADDR>` | default-route interfaces | The local network interface and port for direct connections (repeat for up to four). |
| `--stun-server <HOST:PORT>` | `stun.l.google.com:19302` | The public STUN server used to find your external address (repeat to add more). |
| `--no-self-update` | off | Do not check for a new release at start (see [Updates](#updates)). |

The full list, test options included, is printed by `ptt-radio-companion --help`.

### Two players on one computer

Each copy of the game needs its own pair of ports. For the second player:

1. Start the second game with another port, for example `--enable-lua-udp 34566`.
2. In that game, set the per-player mod setting *Companion port* to `34565`.
3. Start the second companion with `--game-port 34566 --port 34565` and another key, for example `--ptt-key F9`: one key cannot be registered twice. With `--ptt-key none` that player uses the open microphone.

## Updates

The companion from the Releases page keeps itself up to date:

1. At every start it asks GitHub for the latest release of the project.
2. If that release is newer than the running version, it downloads the archive for your system and checks it against the release's `SHA256SUMS`.
3. It replaces its own program file with the new one and restarts itself with the same options.

It never installs an older or the same version. If anything goes wrong (no internet, GitHub unreachable, a checksum that does not match, a folder it may not write to), the companion prints one `self-update: ...` line and keeps running the version you have. See [Troubleshooting](Troubleshooting#self-update-messages).

To turn updates off, start the companion with `--no-self-update`, or set the environment variable `PTT_RADIO_NO_SELF_UPDATE=1`. An option to check only once a day is planned.

A companion you built yourself with `cargo build` never updates itself; it prints `self-update: disabled (not a release build)`.

## Devices

The companion has no device option: it opens the system's **default** microphone and the **default** output device when it starts.

- Windows: *Settings → System → Sound*, choose the input and the output.
- macOS: *System Settings → Sound*, the *Output* and *Input* tabs.
- Linux: your desktop's sound settings, or `pavucontrol` / `wpctl` under PipeWire.

After changing a default device, restart the companion.

Headphones are best. With speakers your microphone picks up the other players, and the companion has to cancel that echo (see [Echo](#echo-and-half-duplex)).

## Talking

There are two ways to transmit.

**Push-to-talk.** Hold `F8` (or your `--ptt-key`) and speak. The key is global: the companion sees it whichever window has focus, and other programs stop receiving it while the companion runs. That is why the default is a function key nobody types and not a letter.

**Open microphone.** The companion transmits all the time. Toggle it in the game with `ALT + V` (control *Toggle open microphone*) or the radio button on the shortcut bar. The companion prints `open mic on (you are player N)` or `open mic off ...`. The open microphone turns itself off when the companion stops answering.

`ALT + SHIFT + V` opens the radio settings window, where you choose your channel (see [Channels and Admin](Channels-and-Admin)). You can rebind both keys in the game's controls settings.

## Echo and half-duplex

- **Half-duplex** (per-player mod setting *Mute the radio while I talk*, on by default): while you hold the push-to-talk key, other players' voices are silent, so your speakers cannot feed them back into your microphone. With headphones you can turn it off. It has no effect on the open microphone.
- **Echo detection.** If your microphone hears your speakers, the game shows you a panel: *Your microphone hears your speakers: put on headphones or turn off the open microphone*, with a button that turns the open microphone off. Other players see a red *echo* mark next to your name. Detection can take tens of seconds when the echo is quiet.

## Console messages

| Message | Meaning |
| --- | --- |
| `ptt-radio-companion 0.4.0 (...), listening on 127.0.0.1:34555, game at 127.0.0.1:34556` | Started. The two ports are `--port` and `--game-port`. |
| `push-to-talk key: F8` | The key was registered. |
| `warning: push-to-talk key F8 not registered: ...; only open mic works` | The system refused the key, usually because another program holds it. Pick another `--ptt-key`. |
| `warning: no ROSTER from the game at 127.0.0.1:34556: ...; still trying` | No answer from the game after 3 seconds. Normal while you are still in the main menu; see [Troubleshooting](Troubleshooting) if it stays. |
| `roster:` and lines `player N  Name  rgb(...)` | The game answered; this is the player list. It is printed again when it changes. |
| `open mic on (you are player N)` / `open mic off (you are player N)` | The open microphone state from the game. |
| `half-duplex on` / `half-duplex off` | The *Mute the radio while I talk* setting reached the companion. |
| `ptt pressed` / `ptt released` | The push-to-talk key went down or up. |
| `sent START` / `sent STOP` | A transmission began or ended. |
| `stats: sent N frames \| received [...] \| ...` | Printed when the companion exits: what it sent and received. |

## Your system

### Windows

- **SmartScreen.** The first start may show *Windows protected your PC*. Click *More info*, then *Run anyway*.
- **Firewall.** If Windows asks whether `ptt-radio-companion` may use networks, allow it. If you refuse, voice still works through the game, only with more delay.
- **Updates.** Keep the companion in a folder you can write to, such as your *Downloads* or *Documents*, not under *Program Files*: otherwise it cannot replace itself.

### macOS

- **Gatekeeper.** If the release build is not signed, macOS refuses to open it the first time. Open *System Settings → Privacy & Security* and click *Open Anyway* next to the message about `ptt-radio-companion`, then start it again. Alternatively, remove the download mark in Terminal: `xattr -d com.apple.quarantine ptt-radio-companion`.
- **Microphone.** On the first start macOS asks for microphone access. When you start the companion from Terminal, the request is made for your terminal app. If you refused, allow it under *System Settings → Privacy & Security → Microphone* and restart the companion.
- **The key.** The companion registers `F8` as an ordinary global shortcut and does not ask for Accessibility or Input Monitoring. On many Mac keyboards `F8` is a media key; press `fn + F8`, or turn on *Use F1, F2, etc. keys as standard function keys* in the keyboard settings.

### Linux

- **Minimum.** The release build needs glibc 2.35 or newer: Ubuntu 22.04 or newer, Debian 12 or newer, or a distribution of the same age.
- **Sound.** The companion uses ALSA. On PipeWire or PulseAudio systems it reaches the sound server through the ALSA plugin (`pipewire-alsa` or the PulseAudio ALSA plugin), which most desktops install by default. The program needs the ALSA library (`libasound2` or `alsa-lib`).
- **The key works on X11 only.** On a pure Wayland session the global key does not work, even though the companion still prints `push-to-talk key: F8`. Use the open microphone, or log into an X11 session.
