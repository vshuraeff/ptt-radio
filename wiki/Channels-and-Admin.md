# Channels and Admin

When you finish this page, you can pick the channel you talk on, add channels you listen to, save favourites as presets, read the speaker stack, and, as an administrator, set the radio rules for the server.

## Channels

A channel is a Factorio signal. Everyone starts on **ANY**.

- **ANY** is the shared default channel. It is a channel of its own, not "listen to everything".
- **Here** follows the surface you are physically on: players on the same planet or platform hear each other. Looking at another planet on the map does not change it.
- **Any other signal** is a fixed channel, for example the iron plate, water or the signal *A*. A planet's signal is a plain channel too, reachable from any surface. Empty, parameter and special signals (*each*, *anything*, *everything*) cannot be channels.

Each player has **one transmit channel (TX)**. You hear your TX channel, any extra **listening channels (RX)** you tick, and the mandatory admin channel if the server set one.

The map setting *Who hears the radio* stays the outer limit: with *Own force only* you hear only your own force, whatever channel you pick.

## The radio settings window

Open it with `ALT + SHIFT + V` or the radio-and-cog button on the shortcut bar. The radio button next to it toggles the open microphone. Close the window with `Esc`, the same key again, or the button in its title bar.

The **Companion** section comes first, followed by these radio controls from top to bottom:

1. **Radio off.** Turns the radio off completely: you receive nothing, the administrator channel and server messages included, and you cannot transmit. Administrator locks do not affect this switch.
2. **Transmit channel (TX).** Pick a signal, or press *ANY* or *Here*. An empty signal, a parameter, *each*, *anything* or *everything* is refused: the window says why and keeps your previous channel. To create a new channel, pick a signal nobody uses yet. If an administrator muted you, the section says *Transmission is forbidden by the administrator.*
3. **Listening (RX).** A tick box for each known channel. Your TX channel is always on; the mandatory admin channel is ticked and locked while the radio is on.
4. **Presets.** Up to 8 favourite channels: *Add current TX*, *Move up*, *Move down*, *Remove*, and *Next preset*, which moves your TX to the next allowed preset.
5. **Known channels.** Each channel with its icon, the number of listeners, and an *admin* mark on the admin channel. Click one to transmit on it.

When the server's rules forbid something, the control is greyed out and its hover text names the rule.

### Companion setup and test

The Companion section explains why voice needs a separate app and provides selectable, read-only installer commands for macOS, Linux and Windows, Steam launch examples, and the source repository and wiki URLs. Copy a command into your system terminal to install the app. For Steam, use the exact launch option printed by the installer, especially if you chose a custom install directory. The window also notes the GPL-3.0 source licence and release checksums.

Status shows whether the companion is connected and compares the version in its greeting with the mod version. An older or unrecognised companion may need updating. If no greeting arrives within 10 seconds of joining, the window opens once for that session with a short chat reminder. The open-microphone shortcut also opens it when no companion is connected.

**Test microphone and game loopback** plays a local tone, records three seconds from your microphone, sends the recording through the game, and plays back the returned audio. Your channel listeners can hear the recording; the usual transmission rules still apply. The result reports the tone, microphone peak in dBFS, frames sent and returned, and time to the first returned frame, or explains a permission, microphone, missing-return or busy failure.

The test temporarily enables your own audio return for at most 15 seconds of game time. The permission ends on a result, timeout or disconnect. It never changes your **Echo my own voice** setting, and a second click while a test is pending does not start another one. Save and pause stop the game clock, so resume the game to finish a pending test.

### Keys

| Control in the game settings | Default |
| --- | --- |
| *Open radio settings* | `ALT + SHIFT + V` |
| *Toggle open microphone* | `ALT + V` |
| *Cycle radio presets* | not assigned, bind it yourself (`ALT + C` is already taken in vanilla) |

The push-to-talk key (`F8` by default) belongs to the companion, not to the game's controls; see [Companion](Companion).

## The speaker stack

While someone talks, a row appears near the left edge of the screen, starting at 40 % of its height:

- the icon of the speaker's TX channel;
- the speaker's name in their colour;
- three link quality bars, measured by your own companion over the last two seconds of sound you played:
  - three green: less than 1 % lost;
  - two yellow: 1 to 5 %;
  - one red: more than 5 %;
  - grey: not measured yet (the first estimate needs half a second of sound).

Hover over a row to see the route (*direct*, *through the hub*, *through a player relay* or *through the game*) and the delay class. A red *echo* mark instead of the bars means that player's microphone hears their speakers.

A new speaker joins at the bottom; a row disappears when that player stops and the rest move up. When the rows no longer fit, the last visible one turns into *N more*. You only see rows for speakers you can hear by channel and by the *Who hears the radio* setting. Players without a companion see the stack and hear the radio clicks too, but no voice.

## For administrators

Factorio administrators (`/promote <player>`) see an **Administration** section at the bottom of the settings window. The same actions are available as the `/radio-admin` command; in the server console, type it without the slash. The rules are stored in the save.

**Writing channels and players in commands.**

- A channel is `ANY`, `HERE` or `type/name`: `item/iron-plate`, `fluid/water`, `virtual/signal-A`, `space-location/vulcanus`. A name without a type means `item`.
- A player is a positive index or an exact name. Put a name with spaces in double quotes: `"Player Two"`. There is no way to escape a quote, so use the index for a name with one. A name made only of digits is read as an index. In the window's text field, type the name without quotes.

| After `/radio-admin` | What it does |
| --- | --- |
| `channel <channel>` / `channel none` | Set or clear the mandatory admin channel |
| `publisher add <player>` / `publisher remove <player>` | Allow or revoke speaking on the admin channel |
| `ban <channel>` / `unban <channel>` | Ban a channel for everyone, or lift the ban |
| `pin <channel>` / `unpin <channel>` | Pin a channel to the known list, or unpin it |
| `deny <player> <channel> tx\|rx\|both` | Deny a player speaking, listening or both on a channel |
| `allow <player> <channel>` | Lift that player's denial on the channel |
| `lock <player> tx\|rx\|all` / `unlock <player>` | Lock the player's TX choice, RX choices or both (`all` also locks presets) |
| `confine <player> <channels...>` / `release <player>` | Limit a player to the listed channels, or lift the limit |
| `mute <player>` / `unmute <player>` | Forbid or allow the player to transmit |

Examples:

```text
/radio-admin deny "Player Two" item/iron-plate tx
/radio-admin confine 2 ANY HERE item/copper-plate
```

`/radio-admin` with no arguments prints the syntax. A command that contradicts the current rules, such as banning the admin channel or banning and pinning the same channel, is refused as a whole with the reason. Repeating a value that is already set changes nothing.

**How the admin channel behaves.**

- Everyone with the radio on hears the admin channel, within the *Who hears the radio* limit. Only administrators and players you added with `publisher add` can speak on it.
- You may make ANY the admin channel, but then ordinary players on ANY go silent until they are made publishers.
- *Here* cannot be the admin channel: it means a different audience on every surface, so it cannot be one channel that everyone must hear.
- Even under `lock ... all` a player can still switch the whole radio off, which also silences the admin channel for them.
- Half-duplex silences everything while a player holds the key, the admin channel included.

**Privacy.** Channels decide what honest clients play. They do not hide voice from a modified client: the game sends every voice packet to every player, and the server hears everything.
