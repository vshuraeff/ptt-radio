# Troubleshooting

Each section starts with what you see, then gives the fix. Messages from the game appear in the chat; messages from the companion appear in its console window.

## "Radio: no companion"

You pressed `ALT + V` or the open microphone button and the chat says:

> Radio: no companion. Start the ptt-radio companion and launch the game with --enable-lua-udp &lt;port&gt;. Setup guide: https://github.com/vshuraeff/ptt-radio/wiki

The mod has not heard from your companion. It cannot tell a companion that is not running from a game started without the flag, so check both:

1. Is the companion running, and does its first line say `game at 127.0.0.1:34556`?
2. Was the game started with `--enable-lua-udp 34556` (the same number)? A flag added to the launch options only applies after a full restart of the game. See [Installation](Installation#3-add-the-launch-flag).
3. Does the per-player mod setting *Companion port* match the companion's `--port` (both `34555` by default)?

When it works, the chat says `Radio: companion connected.` within about five seconds.

## The companion keeps printing "no ROSTER"

```text
warning: no ROSTER from the game at 127.0.0.1:34556: Factorio not started with --enable-lua-udp 34556, a different port, not in a multiplayer game, or the ptt-radio mod is missing; still trying
```

This is normal in the main menu. If it is the last line while you are in a multiplayer game:

1. Check the launch flag and its port, as above.
2. Check that *PTT Radio* is enabled under *Mods*.
3. Make sure the game and the companion run on the same computer.

## "Radio: companion stopped answering"

> Radio: companion stopped answering, open microphone is off.

The game got no sign of life from the companion for 15 seconds, so it turned the open microphone off. The companion was closed, crashed or hung. Start it again; the game announces `Radio: companion connected.` and you can turn the open microphone back on.

## "Update the companion to use radio channels"

The settings window shows:

> Update the companion to use radio channels. This companion cannot send or receive voice.

Your companion is older than the mod. Download the companion from the same release as the mod from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases).

## Nobody hears me

Go through these in order:

1. **Does the other player run a companion?** Only players with their own companion hear voice.
2. **Does your key reach the companion?** Holding the key should print `ptt pressed`. If not, see [The push-to-talk key does nothing](#the-push-to-talk-key-does-nothing).
3. **Are you on the same channel?** The other player has to listen on your TX channel (as their TX or a ticked RX). Check both settings windows (`ALT + SHIFT + V`). With *Here*, you must be on the same surface.
4. **Did the server limit you?** If the window says *Transmission is forbidden by the administrator.*, an administrator muted you. On the admin channel, *Only publishers and administrators can speak on the administrator channel; you can only listen.*
5. **Is the right microphone the default one?** The companion records the system's default input. Test it alone: turn on the per-player mod setting *Echo my own voice* and listen.
6. **Is the radio off?** *Radio off* at the top of the settings window must be unticked.
7. **Does your force limit you?** With the map setting *Who hears the radio* set to *Own force only*, players of other forces do not hear you.

## Others hear themselves (echo)

The game shows you a panel:

> Your microphone hears your speakers: put on headphones or turn off the open microphone

and other players see a red *echo* mark next to your name. Your microphone picks up your speakers.

1. Put on headphones, or press *Turn off open microphone* on the panel and use push-to-talk.
2. Keep the per-player setting *Mute the radio while I talk* on: then your speakers are silent while you hold the key.

The panel disappears once the companion stops hearing the echo. Echo cancellation is on by default (`--aec on`); it starts working only after the echo is detected, which can take tens of seconds with quiet echo.

## The push-to-talk key does nothing

- **Linux:** the global key needs X11. On a pure Wayland session it does nothing, even though the companion prints `push-to-talk key: F8`. Use the open microphone, or log into an X11 session.
- **macOS laptop keyboards:** `F8` may be a media key; press `fn + F8`.
- **The console says** `warning: push-to-talk key F8 not registered: ...; only open mic works`: another program holds the key. Start the companion with another key, for example `--ptt-key F13` or `--ptt-key shift+F8`.
- **The companion stops with** `error: --ptt-key ...`: the key name is not valid. Use names like `F8`, `F13`, `shift+F8`.

## macOS will not open the companion

macOS says the program cannot be opened because the developer cannot be verified. The build is not signed. Open *System Settings → Privacy & Security*, click *Open Anyway* next to the message about `ptt-radio-companion`, and start it again. Or, in Terminal, in the companion's folder: `xattr -d com.apple.quarantine ptt-radio-companion`.

## macOS: the others hear silence

The companion starts and the key works, but nobody hears you. macOS gives an app without microphone access only silence.

1. Open *System Settings → Privacy & Security → Microphone*.
2. Turn on the app you started the companion from. When you start it from Terminal, that is Terminal (or your other terminal app), not `ptt-radio-companion`.
3. Restart the companion.

## Self-update messages

At start the companion may print a line beginning with `self-update:`, for example when there is no internet, GitHub does not answer, the checksum does not match, or the companion may not replace its own file. This is not an error that stops anything: the companion keeps running the version you have and tries again at the next start.

- If the line says the file cannot be replaced, the companion sits in a folder you cannot write to (for example *Program Files* or a system folder). Move it to a folder of your own, or update by hand from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases).
- `self-update: disabled (not a release build)` means you built the companion yourself; such builds never update.
- To turn the check off, start with `--no-self-update` or set `PTT_RADIO_NO_SELF_UPDATE=1`.

## The port is in use

The companion stops with:

```text
error: cannot bind 127.0.0.1:34555: ... (another companion or radio.py running?)
```

Another program holds the port, usually a second companion that is still running. Close it, or give this companion another port with `--port` and set the same number in the mod setting *Companion port*.

The hub stops with `error: cannot bind public ...` or `error: cannot bind receive ...`: another process, often a second hub, already holds that port on the server. Stop it or choose another `--public` or `--receive`; the mod learns the new receive port from the hub on its own.

## Firewall

- **Players' computers.** The companion talks to the game on `127.0.0.1`, which firewalls do not block. For direct routes it also sends UDP to other players and to a public STUN server, and at every start it asks GitHub over HTTPS for a new release (unless started with `--no-self-update`). If that is blocked, voice still goes through the game with more delay. On Windows, allow `ptt-radio-companion` when the firewall asks.
- **The server.** Open the game port `34197/udp` as usual. With the hub, also open `34560/udp` and forward it through NAT (UDP, not TCP). Do not open `34556` or `34557`: they stay on loopback. See [Hub](Hub#ports).

## The hub shows "signals 0"

The hub's summary line has `signals 0` even though players with companions are online. This and the other common hub problems are covered on the [Hub](Hub#if-it-does-not-work) page.
