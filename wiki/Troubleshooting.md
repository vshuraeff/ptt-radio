# Troubleshooting

Each section starts with what you see, then gives the fix. Messages from the game appear in the chat; messages from the companion appear in its console window.

## "Radio: no companion"

You pressed `ALT + V` or the open microphone button and the chat says:

> Radio: no companion. Start the ptt-radio companion and launch the game with --enable-lua-udp &lt;port&gt;. Setup guide: https://github.com/vshuraeff/ptt-radio/wiki

About 10 seconds after you join a game, the mod also checks on its own. If it has not heard from your companion by then, the chat says:

> Radio: open the Companion section in Radio settings for installation and a microphone test.

and the settings window opens. If the companion connects before then, nothing appears.

Both messages mean the mod has not heard from your companion. It cannot tell a companion that is not running from a game started without the flag, so check both:

1. Is the companion running? Its startup line, `ptt-radio-companion ... listening on 127.0.0.1:34555, game at 127.0.0.1:34556`, shows the game port it sends to.
2. Was the game started with `--enable-lua-udp 34556` (the same number as `game at`)? A flag added to the launch options only applies after a full restart of the game. See [Installation](Installation#3-add-the-launch-flag). If the game uses another port, change its flag to `34556`, or start the companion with `--game-port <that port>`. `--launch` adds the flag only when the game's command line does not have one already, so it does not fix a mismatch.

When it works, the chat says `Radio: companion connected.` within about five seconds.

If you start the game through `--launch` and the companion's own setup fails (the port is in use, the `--ptt-key` name is not valid, there is no output device), the companion prints `error: ...` and `--launch: starting the game without the companion`, then starts the game alone. In game you see only the messages above, not the error. To read the error, run the companion once from a terminal. If the game itself cannot be started, the companion prints `error: --launch: cannot start ...`, exits with an error and does not try again.

## The companion prints "no ROSTER"

```text
warning: no ROSTER from the game at 127.0.0.1:34556: Factorio not started with --enable-lua-udp 34556, a different port, not in a multiplayer game, or the ptt-radio mod is missing; still trying
```

The companion prints this once, three seconds after it starts, if the game has not sent it the player list yet, and keeps listening. This is normal in the main menu. Once the game reaches the companion, it prints `roster:` followed by one `player N  <name> rgb(...)` line per player. If no `roster:` list follows while you are in a multiplayer game:

1. Check the launch flag and its port, as above.
2. Check that *PTT Radio* is enabled under *Mods*.
3. Make sure the game and the companion run on the same computer.
4. Check that the per-player mod setting *Companion port* matches the companion's `--port` (both `34555` by default). The game sends the player list, voice and everything else to that port. With a mismatch, the chat still says `Radio: companion connected.`, but nothing reaches the companion and the radio does not work.

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
4. **Did the server limit you?** If the window says *Transmission is forbidden by the administrator.*, an administrator muted you. On the admin channel, *Only publishers and administrators can speak on the administrator channel; you can only listen.* means you need an administrator to make you a publisher. An administrator can also ban the channel you transmit on, deny it to you, or confine you to other channels: then nobody hears you, and the window shows no note about it. If you try to switch to such a channel, the window says *Not changed: the administrator banned this channel.* (or *denied you this channel*, *confined you to other channels*, *locked this setting*) and your TX stays where it was; the buttons for such channels are greyed out, with the reason in their tooltip. The same limits on the listener's side keep a channel silent for them even when it is ticked. The admin channel is never banned, denied or confined away.
5. **Is the right microphone the default one?** The companion records the system's default input. Test it alone: turn on the per-player mod setting *Echo my own voice* and listen. Or press *Test microphone and game loopback* at the end of the *Companion* section of the settings window. It plays a local tone, records 3 seconds and sends them through the game and back to you; listeners on your channel hear it too, and the normal transmission rules apply. The result line gives the outcome (completed, transmission not permitted, could not open the microphone, no audio frames returned, or companion busy transmitting), whether the tone played, the microphone peak in dBFS (or *no capture*), the frames sent and returned, and how long the first return took. The test needs a connected, up-to-date companion; otherwise the window says *Connect an up-to-date companion before testing.*
6. **Is the radio off?** *Radio off*, the checkbox right after the *Companion* section of the settings window (above *Transmit channel (TX)*), must be unticked.
7. **Does your force limit you?** With the map setting *Who hears the radio* set to *Own force only*, players of other forces do not hear you.

## Others hear themselves (echo)

The game shows you a panel:

> Your microphone hears your speakers: put on headphones or turn off the open microphone

and other players see a red *echo* mark next to your name. Your microphone picks up your speakers.

1. Put on headphones, or press *Turn off open microphone* on the panel and use push-to-talk.
2. Keep the per-player setting *Mute the radio while I talk* on: then your speakers are silent while you hold the key. It has no effect on the open microphone, even when you also hold the key, so against echo from the open microphone only headphones or turning the open microphone off help.

The panel disappears once the companion stops hearing the echo. Echo cancellation is on by default (`--aec on`); it starts working only after the echo is detected, which can take tens of seconds with quiet echo.

## The push-to-talk key does nothing

- **Linux:** the global key needs X11. On a pure Wayland session it does nothing, even though the companion prints `push-to-talk key: F8`. Use the open microphone, or log into an X11 session.
- **macOS laptop keyboards:** `F8` may be a media key; press `fn + F8`.
- **The console says** `warning: push-to-talk key F8 not registered: ...; only open mic works`: another program holds the key. Start the companion with another key, for example `--ptt-key F13` or `--ptt-key shift+F8`.
- **The companion stops with** `error: --ptt-key ...`: the key name is not valid. Use names like `F8`, `F13`, `shift+F8`. With `--launch`, the game still starts, without the companion; see ["Radio: no companion"](#radio-no-companion).

## macOS will not open the companion

macOS says the program cannot be opened because the developer cannot be verified. This happens with a release build that is not signed. Open *System Settings → Privacy & Security*, click *Open Anyway* next to the message about `ptt-radio-companion`, and start it again. Or, in Terminal, in the companion's folder: `xattr -d com.apple.quarantine ptt-radio-companion`.

## macOS: the others hear silence

The companion starts and the key works, but nobody hears you. macOS gives an app without microphone access only silence.

1. Open *System Settings → Privacy & Security → Microphone*.
2. Turn on the app you started the companion from. When you start it from Terminal, that is Terminal (or your other terminal app), not `ptt-radio-companion`.
3. Restart the companion.

## No sound device

The companion uses the system's default output and input devices.

- **The companion stops with** `error: no default output device`: there is no default output device. With `--launch`, it also prints `--launch: starting the game without the companion`, and the game starts without it. Runs with `--no-playback` or with the virtual device (`--mic-wav`, `--speaker-wav`) do not open an output device.
- **The console says** `mic open failed: no default input device; retrying in 1 s`: the companion opens the microphone only while the game lets you transmit, and finds no default input. It tries again every second for as long as you may transmit.

Connect the device, make it the default in the system sound settings, and restart the companion (or the game, if you start it through `--launch`).

## Self-update messages

At every start the companion prints one line beginning with `self-update:`. The check runs before the companion opens its port and before `--launch` starts the game, so it delays the start: a few seconds when there is nothing to update (at most about 5 seconds without internet), up to about two minutes while an update downloads. A failed check never stops the companion: it keeps running the version you have and tries again at the next start.

- `self-update: up to date (0.4.0, latest release 0.4.0)`: nothing to do.
- `self-update: updated to 0.4.1`: the new version has just replaced the old one and restarted.
- `self-update: updated to 0.4.1, but the re-launch failed: ...`: the new version is installed but did not start. Start the companion again.
- `self-update: GET https://...: ...`: no internet, GitHub did not answer in time, or GitHub refused the request (the line then ends in `http status: <code>`, for example when its rate limit is reached).
- `self-update: checksum mismatch for ..., update refused`: the download was damaged or tampered with; nothing was installed.
- `self-update: SHA256SUMS has no line for ..., update refused`, `SHA256SUMS lists ... twice`, `SHA256SUMS: malformed line for ...`, `the release has no asset ...`, `...: no ptt-radio-companion inside` (`ptt-radio-companion.exe` on Windows) or a size-limit error: the release is broken or incomplete; nothing was installed.
- `self-update: replace ...: ...` or `self-update: recovery copy of ...: ...`: the companion sits in a folder you cannot write to (for example *Program Files* or a system folder). Move it to a folder of your own, or update by hand from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases).
- `self-update: replace failed (...), previous version restored`: the swap failed, and the companion put its saved copy back. The old version is in place and keeps running.
- `self-update: replace failed (...), restore failed: ...; the previous version is ...`: the swap broke halfway. The old program is kept under the name the line shows; rename it back to `ptt-radio-companion` (`ptt-radio-companion.exe` on Windows) or download the release again.
- `self-update: disabled (opted out)`: the companion was started with `--no-self-update` or with `PTT_RADIO_NO_SELF_UPDATE` set; nothing was checked.
- `self-update: disabled (not a release build)` means you built the companion yourself; such builds never update.
- To turn the check off, start with `--no-self-update` or set `PTT_RADIO_NO_SELF_UPDATE=1` (any value other than empty or `0` works).

## The companion folder disappeared

A companion put in place by the installer removes its own folder (`ptt-radio-companion` in the Factorio user-data folder) once the ptt-radio mod is gone from the `mods` folder. It does so only if an earlier check saw the mod there, and only when the installer recorded the default mods folder, `<user-data>/mods`; a custom `--mods-dir` given to the installer turns removal off. The companion checks when it starts and, with `--launch`, again after the game exits. On removal it prints:

```text
self-uninstall: the ptt-radio mod is no longer in <mods folder>, removed <companion folder>
```

and exits. With `--launch`, the game still starts that one time, without the companion. When something about the folders is unclear, it prints `self-uninstall: ...; nothing removed` and keeps everything.

- To get the companion back, put the mod back and run the installer again.
- To keep the companion without the mod, start it with `--no-self-uninstall` or set `PTT_RADIO_NO_SELF_UNINSTALL=1`.
- If you leave it removed, also delete the companion from the Steam launch options.

## The port is in use

The companion stops with:

```text
error: cannot bind 127.0.0.1:34555: ... (another companion or radio.py running?)
```

Another program holds the port, usually a second companion that is still running. Close it, or give this companion another port with `--port` and set the same number in the mod setting *Companion port*. With `--launch`, the game still starts, without the companion; see ["Radio: no companion"](#radio-no-companion).

The hub stops with `error: cannot bind public ...` or `error: cannot bind receive ...`: another process, often a second hub, already holds that port on the server. Stop it or choose another `--public` or `--receive`; the mod learns the new receive port from the hub on its own.

## Firewall

- **Players' computers.** The companion talks to the game on `127.0.0.1`, which firewalls do not block. For direct routes it also sends UDP to other players and to a public STUN server, and at every start it asks GitHub over HTTPS for a new release (unless started with `--no-self-update` or with `PTT_RADIO_NO_SELF_UPDATE=1`). If that is blocked, voice still goes through the game with more delay. On Windows, allow `ptt-radio-companion` when the firewall asks.
- **The server.** Open the game port `34197/udp` as usual. With the hub, also open `34560/udp` and forward it through NAT (UDP, not TCP). Do not open `34556` or `34557`: they stay on loopback. See [Hub](Hub#ports).

## The hub shows "signals 0"

The hub's summary line has `signals 0` even though players with companions are online. This and the other common hub problems are covered on the [Hub](Hub#if-it-does-not-work) page.
