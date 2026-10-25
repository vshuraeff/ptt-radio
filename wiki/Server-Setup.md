# Server Setup

When you finish this page, your dedicated or rented server runs the mod, you know whether it needs the Lua UDP flag, and you know which settings and admin tools the server owner controls.

## What the server needs

- **The mod.** The server must run `ptt-radio`, the same version the players have. The mod does the routing: it decides who hears whom and shows the speaker stack. It is not client-only.
- **No companion.** Nobody listens on the server, so it needs no companion, microphone or sound card.
- **`--enable-lua-udp` only with the hub.** Voice from players travels inside the game, so a server without the hub needs no extra flag. Add `--enable-lua-udp <port>` to the server only when you run the [Hub](Hub) beside it, because the hub talks to the mod through that port.

## Install the mod on a headless server

1. Download `ptt-radio_<version>.zip` from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases).
2. Copy the zip, unchanged, into the server's `mods` folder.
3. Enable it: add this entry to the `mods` list in `mods/mod-list.json`.

   ```json
   { "name": "ptt-radio", "enabled": true }
   ```

4. Restart the server. Players who join with the mod missing are offered a download if the mod is on the mod portal; otherwise they install it as in [Installation](Installation).

With the hub, the start command looks like this (keep your own save and settings arguments):

```sh
./bin/x64/factorio --start-server <save.zip> --enable-lua-udp 34556
```

The docker example in [Hub](Hub) (`docker/compose.example.yaml`) builds its server from `stand/server`, whose `server-settings.json` is a local test profile: visible on the LAN, user verification off, no game password, 4 players, autosave every minute, no admins (see [Administrators](#administrators) for how to add one). For a public server, write your own `server-settings.json` (start from `data/server-settings.example.json` in the Factorio folder) and mount it over the test one by adding this line to the `factorio` service's `volumes`:

```yaml
      - ./server-settings.json:/config/server-settings.json:ro
```

While the game is saving or paused, voice that travels through the game stops; voice sent directly between players or through the hub keeps going. When the game resumes, the delayed packets arrive together, and the receiver drops the backlog and covers the gaps instead of playing the stalled speech back in a burst. Choose the autosave interval with this in mind.

On a rented server, put the zip wherever the provider's panel uploads mods and add the flag in its *launch parameters* field if you want the hub. The hub needs a process or container running on the same machine as the server, which many providers do not allow. Without it, voice still works through the game and directly between players.

## Settings

**Map setting** (*Settings → Mod settings → Map* in game, administrators only):

| Setting | Values | Default |
| --- | --- | --- |
| *Who hears the radio* | *Everyone*, *Own force only* | *Everyone* |

This is the outer limit for every channel: with *Own force only*, players hear only their own force, whatever channel they pick.

**Per-player settings** (each player sets their own under *Settings → Mod settings → Per player*):

| Setting | Default | What it does |
| --- | --- | --- |
| *Companion port* | `34555` | The UDP port on the player's computer where the companion listens. The game accepts values from 1024 to 65535. |
| *Echo my own voice* | off | The server sends the player's voice back to them, for testing alone. |
| *Mute the radio while I talk* | on | Half-duplex: others are silent while the player holds the radio key. It has no effect while the player's open microphone is on. |

## Administrators

The mod keeps no admin list of its own: the server's Factorio administrators control the radio policy, and the server console is always allowed. A fresh server built from the test profile above has no administrators. To make someone one, add their name to the server's admin list, `server-adminlist.json` (Factorio no longer reads an `admins` key in `server-settings.json`), or run `/promote <player>` from the server console (or ask an existing administrator to run it). Promoting or demoting a player refreshes the open radio windows straight away.

Administrators use:

- the **Administration** section at the bottom of the radio settings window (`ALT + SHIFT + V`);
- the chat command `/radio-admin`. In the server console the same command works without the slash: `radio-admin channel none`. The console has administrator rights.

They can set a mandatory admin channel, allow or revoke a player's right to speak on it (`publisher add|remove <player>`), ban or pin channels, mute a player, deny a player a channel, lock a player's choices, or confine a player to a list of channels. Once an admin channel is set, every player whose radio is on hears it, within the *Who hears the radio* limit, even without tuning to it; only administrators and publishers can speak on it, and the rest can only listen. The policy is stored in the save. See [Channels and Admin](Channels-and-Admin) for the full command list.

Channels decide what honest clients play. They are not a privacy feature: every game copy receives all voice data, so the server and a modified client can hear everything.
