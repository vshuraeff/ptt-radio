# Installation

When you finish this page, the mod is installed in your game, the companion is unpacked on your computer, and the game starts with the flag the radio needs.

Three things have to be in place:

- the mod on the server **and** in every player's game: it is not a client-only mod, and Factorio will not let you join with a different mod set;
- the companion on every player's computer, running on the same computer as the game;
- the game started with `--enable-lua-udp <port>`, so the mod can talk to the companion.

The mod needs Factorio 2.1. For the server side, see [Server Setup](Server-Setup).

## 1. Install the mod

**From the mod portal.** In the game, open *Mods*, search for *PTT Radio*, install it and restart the game. If the server already runs the mod, the game also offers to download it when you join.

**From the zip.**

1. Download `ptt-radio_<version>.zip` from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases).
2. Put the zip, unchanged, into your Factorio `mods` folder:
   - Windows: `%APPDATA%\Factorio\mods`
   - macOS: `~/Library/Application Support/factorio/mods`
   - Linux: `~/.factorio/mods`
3. Start the game and check that *PTT Radio* is enabled under *Mods*.

## 2. Install the companion

On macOS or Linux, run in Terminal:

```sh
curl -fsSL https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh | sh
```

On Windows x86_64, run in PowerShell 5.1 or later:

```powershell
irm https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 | iex
```

The installer downloads the latest release, verifies its SHA-256 checksum, and installs it into `ptt-radio-companion` inside the Factorio user-data directory listed above (the parent of `mods`). It prints the executable's start command and a Steam launch option. It needs no administrator access and does not change Steam settings. Run the command again to update; a failed download, checksum or archive check keeps the previous installation.

For custom directories or a pinned release, download the installer first and pass arguments:

```sh
curl -fsSLo install.sh https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh
sh install.sh --factorio-dir "/path/to/Factorio data" --mods-dir "/path/to/custom mods" --version v0.4.0
```

```powershell
Invoke-WebRequest -UseBasicParsing https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 -OutFile install.ps1
& .\install.ps1 -FactorioDir 'D:\Games\Factorio data' -ModsDir 'D:\Games\custom mods' -Version v0.4.0
```

`--mods-dir` / `-ModsDir` records the absolute mod directory in `install.json`; the installer never edits that directory or `mod-list.json`. Omit it to use `<user-data>/mods`. The installed companion removes its own folder after the mod is gone only when the recorded directory is `<user-data>/mods` itself; naming that folder, or a link to it, with `--mods-dir` still counts. With any other folder it never removes itself and stays until you delete it. The other conditions are in [Removal with the mod](Companion#removal-with-the-mod).

`--version` / `-Version` only chooses the release the installer puts in place. Companion builds from the Releases page check GitHub's latest release at every start. When that release is stable and strictly newer than the running companion, the companion downloads and verifies it, replaces its own executable (never the mod) and restarts with the same arguments, so it no longer matches a pinned mod. A failed check keeps the current version. To stay on the pinned version, start the companion with `--no-self-update` or set `PTT_RADIO_NO_SELF_UPDATE=1` (any value other than empty or `0`). The Steam launch option the installer prints has no opt-out; put the flag before `--launch`, because everything after `--launch` goes to the game: `"<user-data>/ptt-radio-companion/ptt-radio-companion" --no-self-update --launch %command%`.

Linux archives currently support x86_64 with glibc 2.35 or newer; macOS supports Apple silicon and Intel. On other platforms the installer stops with an error saying no companion release archive is published for them.

### Manual download

1. Open the [Releases page](https://github.com/vshuraeff/ptt-radio/releases) and download the file for your system:

   | System | File |
   | --- | --- |
   | Windows (64-bit) | `ptt-radio-companion-<version>-windows-x86_64.zip` |
   | macOS (Apple silicon and Intel) | `ptt-radio-companion-<version>-macos-universal.zip` |
   | Linux (x86_64, glibc 2.35 or newer: Ubuntu 22.04+, Debian 12+) | `ptt-radio-companion-<version>-linux-x86_64.tar.gz` |

2. Optional: check the download against `SHA256SUMS` from the same release, for example `sha256sum --check --ignore-missing SHA256SUMS` on Linux or `shasum -a 256 <file>` on macOS.
3. Unpack it into a folder you can write to, so the companion can update itself later. Inside is one program, `ptt-radio-companion` (`ptt-radio-companion.exe` on Windows), plus `LICENSE` and `README.md`.

Use the companion from the same release as the mod. Nothing enforces this: the *Companion* section of the *Radio settings* window only labels the companion as older, newer or matching. A companion without radio-channel support cannot send or receive voice at all; the window then shows the red note "Update the companion to use radio channels. This companion cannot send or receive voice." The first start, the push-to-talk key and the OS permission prompts are covered in [Companion](Companion).

## 3. Add the launch flag

The game must start with `--enable-lua-udp 34556`. `34556` is the port the companion expects by default (its `--game-port`). If you pick another port, start the companion with `--game-port <the same port>`.

**Steam (Windows, macOS, Linux).**

1. In the Steam library, right-click Factorio and choose *Properties*.
2. On the *General* tab, paste the line the installer printed after `Steam launch options:` into *Launch Options*. It looks like `"<user-data>/ptt-radio-companion/ptt-radio-companion" --launch %command%` (`.exe` on Windows). This starts the companion with the game and adds the Lua UDP flag. If you start the companion yourself, put only `--enable-lua-udp 34556` in *Launch Options* instead.
3. Close the window and start the game as usual.

**Standalone version (factorio.com).** Start the game binary with the flag, or add the flag to your shortcut:

- Windows: `bin\x64\factorio.exe --enable-lua-udp 34556`, run from the Factorio folder. In a desktop shortcut, append the flag to *Target*.
- Linux: `./bin/x64/factorio --enable-lua-udp 34556`, run from the Factorio folder.

**macOS app (not Steam).** In Terminal:

```sh
open -a factorio --args --enable-lua-udp 34556
```

If Factorio is already running, quit it first: `open` passes the flag only to a new start.

## 4. Check that it works

1. Run the installer's start command, or start Factorio through the printed Steam option you set in step 3 (see [Companion](Companion)).
2. Join a multiplayer game.
3. Within a few seconds the chat shows `Radio: companion connected.` and the companion window prints a `roster:` list with the players.

If the message does not appear, see [Troubleshooting](Troubleshooting).
