# PTT Radio

<img src="mod/ptt-radio/thumbnail.png" alt="PTT Radio icon" width="96" align="right">

Push-to-talk voice radio for Factorio 2.1 multiplayer. You hold a key and talk, and the other players hear you. A list at the left edge of the screen shows who is talking, in their player colour. The voice travels through the multiplayer game itself, or over direct UDP between players when their networks allow it.

Factorio mods cannot use the microphone or the speakers, so the radio has two parts:

- **the mod**, `ptt-radio`, which is required on the server and on every client;
- **the companion**, a small program that every player runs next to the game. It records the microphone, plays the voices and talks to the mod over the game's Lua UDP port.

Only players who run their own companion can talk and hear voices. Everyone else still sees who is speaking.

[Русская версия ниже](#быстрый-старт).

## Download

Everything is on the [releases page](https://github.com/vshuraeff/ptt-radio/releases). `<ver>` is the release version, for example `0.4.0`.

| File | What it is |
| --- | --- |
| `ptt-radio_<ver>.zip` | the mod |
| `ptt-radio-companion-<ver>-windows-x86_64.zip` | companion for Windows x86_64 |
| `ptt-radio-companion-<ver>-macos-universal.zip` | companion for macOS (one build for Apple silicon and Intel) |
| `ptt-radio-companion-<ver>-linux-x86_64.tar.gz` | companion for Linux x86_64 with glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and later) |
| `ptt-radio-hub-<ver>-linux-x86_64.tar.gz`, `ptt-radio-hub-<ver>-linux-aarch64.tar.gz` | optional relay hub for the game server |
| `install.sh`, `install.ps1` | companion installers for macOS/Linux and Windows |
| `SHA256SUMS` | checksums of all the files above |

There is no companion for Linux on ARM or Windows on ARM, and the installers stop there. Only the hub has a Linux aarch64 build. The hub is also published as a container image, `ghcr.io/vshuraeff/ptt-radio-hub`.

## Quick start

1. Install the mod. Put `ptt-radio_<ver>.zip` into your Factorio `mods` folder, or install PTT Radio from the mod portal. The server needs it too.
2. Install the companion with the command below. On macOS, allow microphone access when asked.
3. Start the game with the Lua UDP port open. In Steam: right-click Factorio, then Properties, and paste the line the installer printed after `Steam launch options:` into Launch options. The companion then starts with the game and adds `--enable-lua-udp 34556` itself, so you do not type the flag. Without that line, run the `Start:` command the installer printed and add `--enable-lua-udp 34556` to the game's launch options yourself. With no arguments the companion uses port 34556 for the game and listens on 34555, which is the mod's default.
4. Join the game. The chat shows "Radio: companion connected."
5. Hold **F8** to talk, or press **ALT + V** to toggle the open microphone.

macOS / Linux (Terminal):

```sh
curl -fsSL https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh | sh
```

Windows (PowerShell 5.1 or later):

```powershell
irm https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 | iex
```

The installer verifies the release archive against `SHA256SUMS` and puts the companion in `ptt-radio-companion` inside Factorio's user-data folder. Run it again to update. It needs no administrator access and does not edit Steam settings. Companion options such as `--ptt-key` go before `--launch` in the Steam line, because everything after `--launch` is the game's command line. For custom folders, pinned versions or manual downloads, see [Installation](https://github.com/vshuraeff/ptt-radio/wiki/Installation).

If the chat says "Radio: no companion", the companion is not running or the game was started without `--enable-lua-udp`. The mod cannot tell these two apart. See [Troubleshooting](https://github.com/vshuraeff/ptt-radio/wiki/Troubleshooting).

Server admins: a headless server needs the mod, the same version as the players, but no companion. It needs `--enable-lua-udp <port>` only when the relay hub runs beside it. The [Server Setup](https://github.com/vshuraeff/ptt-radio/wiki/Server-Setup) and [Hub](https://github.com/vshuraeff/ptt-radio/wiki/Hub) pages cover it.

## Controls

| Action | Default |
| --- | --- |
| Push to talk (a global key, held in the companion) | `F8`, change it with `--ptt-key` |
| Open microphone on or off | `ALT + V`, or the radio button on the shortcut bar |
| Radio settings window | `ALT + SHIFT + V`, or the radio-and-cog button on the shortcut bar |
| Next channel preset | not assigned, bind "Cycle radio presets" in the controls |

The settings window picks your transmit channel and the channels you listen to, holds up to 8 presets, and has a switch that turns the radio off completely. By default everyone talks on the shared `ANY` channel. Administrators also get a policy section there and the `/radio-admin` command, see [Channels and Admin](https://github.com/vshuraeff/ptt-radio/wiki/Channels-and-Admin).

## Good to know

- **The microphone follows the radio switch.** It opens only after the game permits transmission and closes when the radio is off, you are muted or refused, or the game stops responding for a minute. On macOS the microphone indicator then goes away; releasing PTT alone keeps the microphone ready for pre-roll and echo detection.
- **Half-duplex is on by default.** While you hold the push-to-talk key, other voices go quiet, the admin channel included, so your speakers do not feed back into your microphone. It has no effect on the open microphone. With headphones you can turn it off in the mod settings: "Mute the radio while I talk".
- **Echo warning.** If the companion hears your speakers in your microphone, your line in the speaker list says "echo", and while the open microphone is on you get a button to turn it off. Headphones fix it.
- **Who hears whom is a map setting.** "Who hears the radio" is "Everyone" by default. With "Own force only", players hear only players of their own force, on the admin channel too; the server's own voice still reaches everyone. It can be changed during a game.
- **Channels are not private.** They separate listeners only among honest clients. The server and a modified client can hear every channel.
- **A paused game pauses the voice** that goes through the game. Autosaves do too.
- **The companion keeps itself up to date.** Release builds check the latest GitHub release at every start, before the companion opens its port and before `--launch` starts the game, so the start takes a few seconds longer, up to about two minutes while an update downloads. They move only to a strictly newer stable release, verify its checksum, replace only the companion (never the mod) and restart. A failed check never stops the companion. Turn it off with `--no-self-update` or `PTT_RADIO_NO_SELF_UPDATE=1`.
- **The installed companion removes itself after the mod.** A companion put in place by the installer deletes its `ptt-radio-companion` folder once it has seen the ptt-radio mod in the mods folder and the mod is later gone from there. It checks at every start and, with `--launch`, again after the game exits. It leaves a manual install alone, and one whose installer was given a mods folder other than the default. Turn it off with `--no-self-uninstall` or `PTT_RADIO_NO_SELF_UNINSTALL=1`.
- **Linux:** the push-to-talk key needs an X11 session. A pure Wayland session has no global key for the companion yet.

## Documentation

The [wiki](https://github.com/vshuraeff/ptt-radio/wiki) has the full guides in English and Russian:

| English | Русский |
| --- | --- |
| [Installation](https://github.com/vshuraeff/ptt-radio/wiki/Installation) | [Установка](https://github.com/vshuraeff/ptt-radio/wiki/RU-Installation) |
| [Companion](https://github.com/vshuraeff/ptt-radio/wiki/Companion) | [Компаньон](https://github.com/vshuraeff/ptt-radio/wiki/RU-Companion) |
| [Server Setup](https://github.com/vshuraeff/ptt-radio/wiki/Server-Setup) | [Настройка сервера](https://github.com/vshuraeff/ptt-radio/wiki/RU-Server-Setup) |
| [Hub](https://github.com/vshuraeff/ptt-radio/wiki/Hub) | [Хаб](https://github.com/vshuraeff/ptt-radio/wiki/RU-Hub) |
| [Channels and Admin](https://github.com/vshuraeff/ptt-radio/wiki/Channels-and-Admin) | [Каналы и администрирование](https://github.com/vshuraeff/ptt-radio/wiki/RU-Channels-and-Admin) |
| [Troubleshooting](https://github.com/vshuraeff/ptt-radio/wiki/Troubleshooting) | [Если не работает](https://github.com/vshuraeff/ptt-radio/wiki/RU-Troubleshooting) |
| [Building from Source](https://github.com/vshuraeff/ptt-radio/wiki/Building-from-Source) | [Сборка из исходников](https://github.com/vshuraeff/ptt-radio/wiki/RU-Building-from-Source) |

## Быстрый старт

PTT Radio — рация для мультиплеера Factorio 2.1: держите клавишу и говорите, а остальные вас слышат. Слева на экране видно, кто сейчас говорит, ник — в цвете игрока. Голос идёт через саму игру, а если сети игроков позволяют — напрямую между ними по UDP.

Моды Factorio не могут работать ни с микрофоном, ни с динамиками, поэтому рация состоит из двух частей:

- **мод** `ptt-radio` — нужен на сервере и у каждого клиента;
- **компаньон** — небольшая программа, которую каждый игрок запускает рядом с игрой. Она записывает микрофон, воспроизводит голоса и обменивается данными с модом через Lua UDP-порт игры.

Говорить и слышать других могут только те, у кого запущен свой компаньон. Остальные всё равно видят, кто говорит.

### Файлы релиза

Всё лежит на [странице релизов](https://github.com/vshuraeff/ptt-radio/releases). `<версия>` — номер релиза, например `0.4.0`.

| Файл | Что это |
| --- | --- |
| `ptt-radio_<версия>.zip` | мод |
| `ptt-radio-companion-<версия>-windows-x86_64.zip` | компаньон для Windows x86_64 |
| `ptt-radio-companion-<версия>-macos-universal.zip` | компаньон для macOS (одна сборка для Apple silicon и Intel) |
| `ptt-radio-companion-<версия>-linux-x86_64.tar.gz` | компаньон для Linux x86_64 с glibc 2.35 или новее (Ubuntu 22.04, Debian 12 и новее) |
| `ptt-radio-hub-<версия>-linux-x86_64.tar.gz`, `ptt-radio-hub-<версия>-linux-aarch64.tar.gz` | необязательный хаб-ретранслятор для игрового сервера |
| `install.sh`, `install.ps1` | установщики компаньона для macOS/Linux и для Windows |
| `SHA256SUMS` | контрольные суммы всех файлов выше |

Для Linux и Windows на ARM компаньона нет, установщики на таких системах останавливаются. Под Linux на aarch64 собран только хаб. Хаб также выложен как образ контейнера `ghcr.io/vshuraeff/ptt-radio-hub`.

### Установка

1. Установите мод: положите `ptt-radio_<версия>.zip` в папку `mods` игры или установите PTT Radio с мод-портала. На сервере мод тоже нужен.
2. Установите компаньон командой ниже: в терминале на macOS и Linux, в PowerShell на Windows. На macOS разрешите доступ к микрофону, когда система спросит.
3. Запускайте игру с открытым Lua UDP-портом. В Steam щёлкните по Factorio правой кнопкой, выберите «Свойства» и вставьте в поле «Параметры запуска» строку, которую установщик напечатал после `Steam launch options:`. Тогда компаньон запускается вместе с игрой и сам добавляет `--enable-lua-udp 34556`, вводить флаг не нужно. Без этой строки запустите команду, которую установщик напечатал после `Start:`, и сами добавьте `--enable-lua-udp 34556` в параметры запуска игры. Без аргументов компаньон работает с игрой на порту 34556 и слушает порт 34555 — это значение по умолчанию в настройках мода.
4. Зайдите в игру. В чате появится «Рация: компаньон подключён.»
5. Держите **F8**, чтобы говорить, или нажмите **ALT + V**, чтобы включить или выключить открытый микрофон.

macOS / Linux (терминал):

```sh
curl -fsSL https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh | sh
```

Windows (PowerShell 5.1 или новее):

```powershell
irm https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 | iex
```

Установщик сверяет архив релиза с `SHA256SUMS` и кладёт компаньон в папку `ptt-radio-companion` внутри папки пользовательских данных Factorio. Чтобы обновить компаньон, запустите установщик ещё раз. Права администратора ему не нужны, настройки Steam он не трогает. Параметры самого компаньона, например `--ptt-key`, ставьте в строке для Steam перед `--launch`: всё, что идёт после `--launch`, считается командной строкой игры. Как указать свои папки игры и модов, поставить конкретную версию или скачать файлы вручную, рассказано на странице [Установка](https://github.com/vshuraeff/ptt-radio/wiki/RU-Installation).

Если в чате написано «Рация: компаньон не найден», значит, компаньон не запущен или игра запущена без `--enable-lua-udp`; мод не может отличить одно от другого. Что делать — на странице [Если не работает](https://github.com/vshuraeff/ptt-radio/wiki/RU-Troubleshooting).

Администраторам сервера: выделенному серверу нужен мод той же версии, что у игроков, а компаньон не нужен. Флаг `--enable-lua-udp <порт>` серверу нужен, только если рядом с ним работает хаб. Подробнее — на страницах [Настройка сервера](https://github.com/vshuraeff/ptt-radio/wiki/RU-Server-Setup) и [Хаб](https://github.com/vshuraeff/ptt-radio/wiki/RU-Hub).

### Управление

| Действие | По умолчанию |
| --- | --- |
| Клавиша рации (глобальная, её перехватывает компаньон, а не игра) | `F8`, меняется параметром `--ptt-key` |
| Открытый микрофон вкл/выкл | `ALT + V` или кнопка «Открытый микрофон» на панели ярлыков |
| Окно «Настройки рации» | `ALT + SHIFT + V` или кнопка «Настройки рации» на панели ярлыков |
| Следующий пресет | клавиша не назначена: назначьте «Переключать пресеты рации» в настройках управления |

В окне настроек вы выбираете канал передачи и каналы, которые слушаете, сохраняете до 8 пресетов, а переключателем «Рация выключена» можете полностью выключить рацию. По умолчанию все говорят на общем канале `ANY`. Администраторы видят в том же окне раздел «Администрирование» и могут пользоваться командой `/radio-admin`, см. [Каналы и администрирование](https://github.com/vshuraeff/ptt-radio/wiki/RU-Channels-and-Admin).

### Полезно знать

- **Микрофон подчиняется рации.** Он открывается только после того, как игра разрешит передачу, и закрывается, когда рация выключена, вас заглушили или вам отказали в передаче, а также если игра не отвечает минуту. Тогда на macOS гаснет индикатор микрофона. Если просто отпустить клавишу рации, компаньон микрофон не закрывает: так в передачу попадает начало фразы и работает обнаружение эха.
- **Настройка «Глушить эфир, пока говорю» включена по умолчанию.** Пока вы держите клавишу рации, голоса других игроков, в том числе на канале администратора, не звучат, поэтому звук из динамиков не попадает обратно в микрофон. На открытый микрофон настройка не действует. В наушниках её можно выключить в настройках мода.
- **Предупреждение об эхе.** Если компаньон слышит ваши динамики в микрофоне, в списке говорящих у вашей строки появляется «эхо», а при включённом открытом микрофоне — кнопка «Выключить открытый микрофон». Помогают наушники.
- **Кто кого слышит, задаёт настройка карты.** «Кто слышит рацию» по умолчанию стоит на «Все». При значении «Только своя команда» игроки слышат только свою команду, и на канале администратора тоже; голос самого сервера по-прежнему слышат все. Настройку можно менять посреди игры.
- **Каналы не защищают от подслушивания.** Они разделяют слушателей только среди честных клиентов. Сервер и модифицированный клиент слышат все каналы.
- **На паузе голос через игру замолкает.** То же происходит во время автосохранения.
- **Компаньон обновляется сам.** Сборка из релиза при каждом запуске проверяет последний релиз на GitHub. Проверка идёт до того, как компаньон откроет порт и `--launch` запустит игру, поэтому запуск затягивается на несколько секунд, а пока скачивается обновление — примерно до двух минут. Компаньон обновляется только до более новой стабильной версии и только после сверки контрольной суммы, заменяет лишь себя (мод не трогает) и перезапускается. Если проверка не удалась, компаньон всё равно работает дальше. Отключить: `--no-self-update` или `PTT_RADIO_NO_SELF_UPDATE=1`.
- **Компаньон удаляет себя вслед за модом.** Компаньон, поставленный установщиком, удаляет свою папку `ptt-radio-companion`, если раньше видел мод ptt-radio в папке модов, а потом мод оттуда пропал. Он проверяет это при каждом запуске, а с `--launch` — ещё и после выхода из игры. Установку вручную он не трогает, как и установку, при которой установщику указали папку модов не по умолчанию. Отключить: `--no-self-uninstall` или `PTT_RADIO_NO_SELF_UNINSTALL=1`.
- **Linux:** клавиша рации работает только в сеансе X11. В чистом Wayland глобальной клавиши для компаньона пока нет.

Полное руководство на русском — в [вики](https://github.com/vshuraeff/ptt-radio/wiki), ссылки на все её русские страницы — в таблице выше.

## For developers

The design, the wire formats, the trust boundaries and the measurements are in [DESIGN.md](DESIGN.md) (in Russian, with English sections for the v2 transport contracts). [ARCHITECTURE.md](ARCHITECTURE.md) maps the components, data flows, threads, build and tests. Build instructions are on the wiki's [Building from Source](https://github.com/vshuraeff/ptt-radio/wiki/Building-from-Source) page.

- `mod/ptt-radio/` is the mod (Lua), `mod/tests/` its tests, which run without Factorio.
- `companion/` is the Rust crate with three binaries: the companion `ptt-radio-companion`, the relay hub `ptt-radio-hub` and the `relay` game emulator for end-to-end tests. Building needs Rust 1.91 or newer (the hub alone builds on 1.88), cmake and a C compiler; libopus is built from source and linked statically.
- `tools/gate.sh [--twice] [log dir]` is the full acceptance gate: `cargo fmt --check`, clippy, the debug and release test suites, the real-time voice scenarios, `aec_gate`, `luac -p` and the four Lua suites. It needs `cargo`, `flock`, `lua` and `luac` on `PATH`; on macOS `flock` comes from Homebrew. Real-time steps hold a shared lock file (`PTT_REALTIME_LOCK`, wait limit `PTT_REALTIME_WAIT`) so two runs on one machine do not measure at once, and a real-time test that fails is rerun once by name and reported as flaky if the rerun passes.
- The Lua checks alone, from this directory: `luac -p mod/ptt-radio/*.lua mod/tests/*.lua`, then `lua mod/tests/<name>_test.lua mod/ptt-radio/control.lua` for `control`, `channels`, `admin` and `settings_gui`.
- `tools/radio.py` emulates a companion without audio: `uv run -q tools/radio.py {hello|talk|listen}`; `talk --no-stop` imitates a crashed companion.
- `stand/` runs a headless 2.1 server with the mod in Docker, optionally with the hub beside it (`compose.hub.yaml`). The live acceptance checklist is `stand/ACCEPTANCE.md`. `client-mods/` is a mod directory for a test client (`--mod-directory`), so your main installation stays untouched.

### Measured so far

Recorded on 2026-10-24 on one macOS x86_64 machine. These are the numbers of those runs, not measurements of the current release and not a passed quality acceptance.

- Through the game, with an early mod version, one macOS client and a headless Linux server in a virtual machine on the same computer: frame round trip p50 75–77 ms, p95 87 ms or less; STOP 76–84 ms after the key is released. A companion that dies mid-phrase is closed by the mod 587 ms after its last frame. A repeat later that day, in another state of the stand, gave p50 105–110 ms (288–293 ms in yet another state), and 76 ms did not reproduce.
- Mouth-to-ear in the network lab, without Factorio (a game emulator with 72 ms plus 0–10 ms delay and 60 Hz ticks, virtual audio devices), by route: game 153 ms, direct 64 ms, hub 95 ms, player relay 85 ms, game fallback 154 ms. After the direct path is cut mid-phrase, the gap is 20 ms when it falls back to the hub and 70 ms to the game; after the hub fails, 60 ms. An earlier run, before the two-leg metric repair, gave direct 65 ms and gaps of 30, 70 and 50 ms.
- Double-talk through the hub, in the same lab, is a known flaky case: the other player's voice measured from −4.2 to −2.4 dB against a −4 dB threshold, 6 passes and 5 failures out of 11 known runs.
- In the offline multi-party echo model, warmed-up echo cancellation keeps your own returned voice below −50 dB, but in a three-way conversation the 1–4 kHz band lost up to 12.1 dB, and before the echo is detected on a cold start the return is audible.
- Not yet checked live: the push-to-talk key with the Factorio window focused, real speakers, and the macOS permission prompts.

The tables and the conditions behind these numbers are in [DESIGN.md](DESIGN.md#network-lab).

## License

GPL-3.0-only. See [LICENSE](LICENSE).
