# Channels and Admin

When you finish this page, you can pick the channel you talk on, add channels you listen to, save favourites as presets, read the speaker stack, and, as an administrator, set the radio rules for the server.

## Channels

A channel is a Factorio signal. Everyone starts on **ANY**.

- **ANY** is the shared default channel. It is a channel of its own, not "listen to everything".
- **Here** follows the surface you are physically on: players on the same planet or platform hear each other. Looking at another planet on the map does not change it.
- **Any other signal** is a fixed channel, for example the iron plate, water or the signal *A*. A planet's signal is a plain channel too, reachable from any surface. Parameter signals and the special signals *each*, *anything* and *everything* cannot be channels.

Each player has **one transmit channel (TX)**. You hear your TX channel, any extra **listening channels (RX)** you tick, and the mandatory admin channel if the server set one.

The map setting *Who hears the radio* stays the outer limit: with *Own force only* you hear only players of your own force, whatever channel you pick.

## The radio settings window

Open it with `ALT + SHIFT + V` or the radio-and-cog button on the shortcut bar. The radio button next to it toggles the open microphone. Close the window with `Esc`, with `ALT + SHIFT + V` again, or with the button in its title bar.

The **Companion** section comes first, followed by these radio controls from top to bottom:

1. **Radio off.** Turns the radio off completely: you receive nothing, the administrator channel and server messages included, and you cannot transmit. Administrator locks do not affect this switch.
2. **Transmit channel (TX).** Pick a signal, or press *ANY* or *Here*. To create a new channel, pick a signal nobody uses yet. A parameter, *each*, *anything* or *everything* is refused: the window says *Not changed: this signal cannot be a channel* and keeps your previous channel. If you clear the picker, the window puts your current channel back without a message. If an administrator muted you, the section says *Transmission is forbidden by the administrator.* If your TX channel is the admin channel and you are neither a publisher nor an administrator, it says *Only publishers and administrators can speak on the administrator channel; you can only listen.*
3. **Listening (RX).** A tick box for each known channel. Your TX channel is always on. The mandatory admin channel is always ticked and greyed out, whether the radio is on or off; you hear it only while the radio is on.
4. **Presets.** Up to 8 favourite channels: *Add current TX*, *Move up*, *Move down*, *Remove*, and *Next preset*, which moves your TX to the next allowed preset.
5. **Known channels.** Each entry shows the channel's icon with a number on it, the channel's name, and an *admin* mark on the admin channel. The number counts connected players with the radio on who have that channel as their TX channel or tick it for listening; a player who has it as both counts once. TX here means the selected channel, not someone talking right now. Under *Own force only* only players of your force are counted, and for *Here* only players on your surface. The list holds channels with at least one such player, pinned channels and the admin channel; the last two can show 0. A banned channel is not listed. If the list is empty, the window says *Nobody is on a channel yet.* Click a channel to transmit on it.

When the server's rules forbid something, the window shows it this way:

- A **lock** greys out the controls it covers, with hover text naming the lock: a TX lock the TX picker, *ANY*, *Here*, the Known channels buttons and *Next preset*; an RX lock the RX tick boxes; `all` both of these and the preset buttons.
- A **ban**, **deny** or **confine** greys out *ANY*, *Here* and the Known channels button of a channel you may not transmit on, and the RX tick box of a channel you may not listen to, with the reason as hover text. A forbidden channel you already listen to stays clickable, so you can untick it. The TX picker stays enabled: picking a forbidden signal shows *Not changed:* followed by the reason, and your previous channel stays. These rules never grey out the admin channel.
- A **mute** greys out nothing; it only adds the red note in the Transmit channel section.

### Companion setup and test

The Companion section explains why voice needs a separate app and provides selectable, read-only installer commands for macOS, Linux and Windows, Steam launch examples, and the source repository and wiki URLs. Copy a command into your system terminal to install the app. For Steam, use the exact launch option printed by the installer, especially if you chose a custom install directory. The window also notes the GPL-3.0 source licence and release checksums.

Status shows whether the companion is connected and compares the version in its greeting with the mod version: *Versions match.*, *The companion is newer than this mod.* or *The companion is older; update it.* When the version cannot be read, the status says *Companion: connected; version unknown.* These are only labels: nothing requires equal versions, and voice keeps working. Still, use the companion from the same release as the mod. A companion too old to support channels (its greeting has no `pol1`) cannot send or receive voice at all. The window then shows the red note *Update the companion to use radio channels. This companion cannot send or receive voice.*, and the microphone test refuses to start with *Connect an up-to-date companion before testing.*

If no greeting arrives within 10 seconds of joining, the window opens once for that session with a short chat reminder. The open-microphone button or `ALT + V` also opens it when no companion is connected.

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

Hover over the three bars to see the route (*direct*, *through the hub*, *through a player relay* or *through the game*) and the delay, for example *Link: through the hub, delay unknown*. The companion does not report the delay, so it always reads *delay unknown*. While the bars are grey because the first estimate is not ready, the tooltip reads *Link quality unknown:* followed by the route and *delay unknown*. When your companion has sent no report for that speaker, none yet or the last one more than 300 ticks ago, the tooltip says only *Link quality unknown*. Hovering the channel icon names the channel: *ANY*, *Here*, or the channel key such as `item/iron-plate`. The row and the name have no tooltip. A red *echo* mark instead of the bars means that player's microphone hears their speakers; its tooltip is *echo from speakers*, and such a row shows no route.

A new speaker joins at the bottom; a row disappears when that player stops and the rest move up. When the rows no longer fit, the last visible one turns into *N more*. You only see rows for speakers you can hear by channel and by the *Who hears the radio* setting. Players without a companion see the stack and hear the radio clicks too, but no voice.

## For administrators

The mod has no admin list of its own: it uses Factorio administrators, and the server console is always allowed. A fresh dedicated server has none: add them to the server's admin list, `server-adminlist.json`, or run `/promote <player>` from the server console. Administrators see an **Administration** section at the bottom of the settings window. The same actions are available as the `/radio-admin` command; in the server console, type it without the slash. Anyone else who runs the command gets *Only an administrator can change radio policy.* The rules are stored in the save.

The command has no way to list the current rules. They are shown at the bottom of the Administration section: the admin channel, the banned and pinned channels, and one row for each player who has a rule (muted, publisher, lock, allowed channels, denials).

**Writing channels and players in commands.**

- A channel is `ANY`, `HERE` or `type/name`: `item/iron-plate`, `fluid/water`, `virtual/signal-A`, `space-location/vulcanus`. The types `entity`, `recipe`, `asteroid-chunk` and `quality` work too, and the name must be an existing prototype of that type. A name without a type means `item`. Write `ANY` and `HERE` in capitals (the internal keys `ptt/any` and `ptt/here` also work): `any` is read as `item/any` and refused as an invalid channel. Parameter signals and `signal-each`, `signal-anything` and `signal-everything` are refused.
- A player is an index from 1 to 65535 or an exact name. Put a name with spaces in double quotes: `"Player Two"`. There is no way to escape a quote, so use the index for a name with one. A name made only of digits is read as an index, so a number outside that range is refused, not looked up as a name. In the window's text field, type the name without quotes.

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

**What the rules do.**

- A ban works both ways for everyone: nobody can speak on or hear a banned channel, and it is left off the Known channels list. A ban moves nobody: a player whose TX or RX is already on that channel keeps it selected but can no longer speak or hear there. The admin channel can never be banned.
- A player has one deny mode per channel. A new `deny` replaces the earlier mode instead of adding to it: `deny P ch tx` followed by `deny P ch rx` leaves only `rx`. Use `both` to deny speaking and listening. `allow` removes the player's whole denial for that channel.
- Confinement never blocks the admin channel: a confined player with the radio on still hears it and may pick it as TX, though speaking there still takes publisher or administrator rights, as for everyone. Confinement moves nobody either: a player whose TX is outside the list stays on it and is silent there, and does not hear that channel, until they switch to an allowed channel or are released. If that player's TX is locked (`lock tx` or `lock all`), the `confine` is refused instead.

For an administrator or the console, `/radio-admin` with no arguments prints the syntax. Every change is checked against all the current rules and refused as a whole, with the reason, if it would create a contradiction:

- the admin channel banned, or denied to a player;
- the same channel banned and pinned;
- a player confined to a banned channel, or to a channel denied to them `both` ways;
- a player whose lock covers TX (`lock tx` or `lock all`) left on a TX channel they may not use; a ban, deny, confine or lock can each cause this.

The check works in either order: `deny <player> <channel> both` is refused while that player is confined to that channel, and `channel <X>` is refused while X is banned or denied to someone. Repeating a value that is already set, or `allow` when there is no denial, changes nothing and replies *Radio policy already has that value.*

**How the admin channel behaves.**

- Everyone with the radio on hears the admin channel, within the *Who hears the radio* limit. Only administrators and players you added with `publisher add` can speak on it, and not while they are muted or have their own radio off. The server can always speak on it.
- You may make ANY the admin channel, but then ordinary players on ANY go silent until they are made publishers.
- *Here* cannot be the admin channel: it means a different audience on every surface, so it cannot be one channel that everyone must hear.
- Even under `lock ... all` a player can still switch the whole radio off, which also silences the admin channel for them.
- With the player setting *Mute the radio while I talk* on (the default), all incoming voice goes silent while that player holds the push-to-talk key, the admin channel included. It has no effect on the open microphone, and with headphones you can turn it off; see [Companion](Companion#echo-and-half-duplex).

**Privacy.** Channels decide what honest clients play. They do not hide voice from a modified client: the game sends every voice packet to every player, and the server hears everything.
