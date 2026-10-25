# PTT Radio

**English** · [Русский](#русский)

PTT Radio is push-to-talk voice chat for Factorio 2.1 multiplayer. Hold a key, speak, and the players on your radio channel hear you. A stack on the left edge of the screen shows who is talking, with each name in that player's colour.

No voice server is required. Voice goes from companion to companion by one of four routes, and the companion picks among them on its own: through the multiplayer game itself, which always works but is slower; directly between two players; through a third player's companion; or through the optional hub. Players need two parts:

- **the mod** `ptt-radio`, installed on the server and by every player;
- **the companion** `ptt-radio-companion`, a small program each player runs beside the game. It records the microphone, plays the voices and watches the push-to-talk key.

A third program, the **hub** `ptt-radio-hub`, is optional. It runs on the server machine, relays voice and plays no audio itself. It is released as a separate Linux build (x86_64 and aarch64, plus a container image); see [Hub](Hub).

A player without the companion still sees who is talking and hears the radio's start and stop clicks, but hears no voice.

## Quick start

1. Install the mod on the server, and in your own game (see [Installation](Installation)).
2. Download the companion for your system from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases) and unpack it.
3. Add `--enable-lua-udp 34556` to the game's launch options.
4. Start the companion from the folder you unpacked it to: `.\ptt-radio-companion.exe` on Windows, `./ptt-radio-companion` on macOS and Linux (details in [Companion](Companion)). Leave it running.
5. Join the multiplayer game. When the chat says `Radio: companion connected.`, hold `F8` and talk. Or press `ALT + V` to switch on the open microphone. On Linux, `F8` works only in an X11 session: under pure Wayland it does nothing, although the companion still prints `push-to-talk key: F8`, so use `ALT + V` or log into an X11 session.

## Pages

- [Installation](Installation): the mod, the companion and the launch flag
- [Companion](Companion): options, devices, the key, console messages, OS permissions
- [Server Setup](Server-Setup): the mod on a dedicated server, map settings, administrators
- [Hub](Hub): the optional relay beside the server
- [Channels and Admin](Channels-and-Admin): radio channels, the speaker stack, the settings window, `/radio-admin`
- [Troubleshooting](Troubleshooting): the messages you may see and what to do
- [Building from Source](Building-from-Source)

---

## Русский

PTT Radio — рация для мультиплеера Factorio 2.1: зажмите клавишу, говорите, и вас услышат игроки на вашем канале. У левого края экрана видно, кто сейчас говорит; ники окрашены в цвета игроков.

Голосовой сервер не нужен. Голос идёт от компаньона к компаньону одним из четырёх маршрутов, и компаньон сам выбирает, каким: через саму сетевую игру (этот путь работает всегда, но медленнее), напрямую между двумя игроками, через компаньон третьего игрока или через необязательный хаб. Рация состоит из двух частей, и игрокам нужны обе:

- **мод** `ptt-radio` — он нужен и на сервере, и у каждого игрока;
- **компаньон** `ptt-radio-companion` — небольшая программа, которую каждый игрок запускает рядом с игрой. Она снимает звук с микрофона, воспроизводит голоса и следит за клавишей рации.

Третья программа, **хаб** `ptt-radio-hub`, необязательна. Она работает на машине сервера и только пересылает голос, сама ничего не воспроизводит. Хаб выходит отдельной сборкой для Linux (x86_64 и aarch64, а также образ контейнера); подробнее — в разделе [Хаб](RU-Hub).

Игрок без компаньона видит, кто говорит, и слышит щелчки рации в начале и в конце передачи, но голосов не слышит.

## Быстрый старт

1. Поставьте мод на сервер и в свою игру (см. [Установка](RU-Installation)).
2. Скачайте компаньон для своей системы со [страницы релизов](https://github.com/vshuraeff/ptt-radio/releases) и распакуйте.
3. Добавьте `--enable-lua-udp 34556` в параметры запуска игры.
4. Запустите компаньон из папки, куда его распаковали: `.\ptt-radio-companion.exe` на Windows, `./ptt-radio-companion` на macOS и Linux (подробнее в разделе [Компаньон](RU-Companion)), и не закрывайте его.
5. Зайдите в сетевую игру. Когда в чате появится `Рация: компаньон подключён.`, зажмите `F8` и говорите. Или включите открытый микрофон клавишей `ALT + V`. В Linux `F8` работает только в сеансе X11: в чистом Wayland клавиша ничего не делает, хотя компаньон всё равно пишет `push-to-talk key: F8`. В таком случае пользуйтесь `ALT + V` или войдите в сеанс X11.

## Страницы

- [Установка](RU-Installation): мод, компаньон и флаг запуска
- [Компаньон](RU-Companion): параметры, устройства, клавиша, сообщения в консоли, разрешения ОС
- [Настройка сервера](RU-Server-Setup): мод на выделенном сервере, настройки карты, администраторы
- [Хаб](RU-Hub): необязательный ретранслятор рядом с сервером
- [Каналы и администрирование](RU-Channels-and-Admin): каналы рации, список говорящих, окно настроек, `/radio-admin`
- [Если не работает](RU-Troubleshooting): сообщения, которые вы можете увидеть, и что с ними делать
- [Сборка из исходников](RU-Building-from-Source)
