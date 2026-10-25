# Mod portal text

The text below is pasted into the Factorio mod portal: the summary into the summary field (500 characters at most), the description into the description field. It uses only headings, paragraphs, lists, bold, links and inline code, which the portal renders.

## Summary

Push-to-talk voice radio for multiplayer, with channels, a speaker list and admin controls. Every player who talks or listens needs the free ptt-radio companion app running and the game launched with --enable-lua-udp 34556. Required on the server and on every client.

## Description (English)

**PTT Radio** is push-to-talk voice chat for Factorio multiplayer. Hold a key and talk; the others hear you, and a list at the left edge of the screen shows who is speaking, in their player colour.

**Every player needs two things for voice to work:**

- the **ptt-radio companion** running on their computer. It records the microphone and plays the voices. Factorio mods cannot use audio devices, so the companion is a separate free program for Windows x86_64, macOS (Apple silicon and Intel) and Linux x86_64: [download it from GitHub](https://github.com/vshuraeff/ptt-radio/releases);
- the game launched with **`--enable-lua-udp 34556`**. In Steam, the launch option that the companion's installer prints starts the companion with the game and adds this flag itself. If you start the companion on your own, put the flag into the launch options of Factorio.

A player without the companion sees who is speaking but hears nothing and cannot talk. The mod is required on the server and on every client, in the same version everywhere. A headless server needs no companion, and needs `--enable-lua-udp` only when the optional relay hub runs beside it.

### Features

- Push-to-talk on `F8` in the companion, or an open microphone toggled with `ALT + V` or the shortcut button.
- Speaker list with the transmit channel icon and three link quality bars.
- Radio channels: pick any signal as your transmit channel, listen to several at once, keep up to 8 presets. Everyone starts on the shared `ANY` channel; the Here channel (`HERE` in commands) reaches only the players on the surface you are on.
- Settings window on `ALT + SHIFT + V`, with a switch that turns the radio off completely.
- A map setting, "Who hears the radio": everyone hears everyone by default, or players hear only their own force.
- Admin tools in the same window and as the `/radio-admin` command: an admin channel everyone hears, channel bans, mutes, locks. The policy is kept in the save.
- Voice goes through the game, or directly between players over UDP when their networks allow it. An optional relay hub beside the server helps players who cannot connect directly.
- Half-duplex by default ("Mute the radio while I talk"): while you hold the push-to-talk key, other voices are muted, so your speakers cannot feed your microphone. It does not apply to the open microphone. Separately, the companion warns you when it hears your speakers in your microphone.

### Limitations

- Channels separate listeners only among honest clients. The server and a modified client can hear every channel.
- A paused game, and an autosave, pause the voice that goes through the game.
- On Linux the push-to-talk key needs an X11 session.

### Help

Installation, server setup and troubleshooting: [wiki](https://github.com/vshuraeff/ptt-radio/wiki). Source code and bug reports: [GitHub](https://github.com/vshuraeff/ptt-radio).

## Описание (русский)

**PTT Radio** — голосовая рация для мультиплеера Factorio: держите клавишу и говорите, остальные вас услышат. Слева на экране видно, кто сейчас говорит, ник — в цвете игрока.

**Чтобы голос работал, каждому игроку нужны две вещи:**

- запущенный на его компьютере **компаньон ptt-radio**. Он записывает микрофон и воспроизводит голоса. Моды Factorio не могут работать со звуковыми устройствами, поэтому компаньон — отдельная бесплатная программа для Windows x86_64, macOS (Apple silicon и Intel) и Linux x86_64: [скачать на GitHub](https://github.com/vshuraeff/ptt-radio/releases);
- игра, запущенная с параметром **`--enable-lua-udp 34556`**. В Steam строка параметров запуска, которую печатает установщик компаньона, запускает компаньон вместе с игрой и сама добавляет этот флаг. Если вы запускаете компаньон отдельно, впишите флаг в параметры запуска Factorio.

Игрок без компаньона видит, кто говорит, но ничего не слышит и сам говорить не может. Мод нужен на сервере и у каждого клиента, причём везде одной и той же версии. Выделенному серверу компаньон не нужен, а флаг `--enable-lua-udp` — только если рядом с ним работает необязательный хаб.

### Возможности

- Передача, пока держите `F8` (клавишу перехватывает компаньон), или открытый микрофон: `ALT + V` или кнопка «Открытый микрофон» на панели ярлыков.
- Список говорящих со значком канала передачи и тремя делениями качества связи.
- Каналы: любой сигнал можно сделать своим каналом передачи, слушать сразу несколько каналов, хранить до 8 пресетов. Сначала все говорят на общем канале `ANY`; канал «Здесь» (`HERE` в командах) слышат только игроки на той же поверхности, где сейчас вы.
- Окно настроек на `ALT + SHIFT + V` с переключателем, который полностью выключает рацию.
- Настройка карты «Кто слышит рацию»: по умолчанию все слышат всех, но можно сделать так, чтобы каждый слышал только свою команду.
- Инструменты администратора в том же окне и командой `/radio-admin`: админ-канал, который слышат все, запрет каналов, заглушение игроков, блокировки. Правила рации хранятся в сохранении карты.
- Голос идёт через игру или напрямую между игроками по UDP, если их сети это позволяют. Необязательный хаб рядом с сервером помогает тем, кто не может соединиться напрямую.
- По умолчанию включена настройка «Глушить эфир, пока говорю»: пока вы держите клавишу рации, голоса других игроков не звучат, поэтому звук из динамиков не попадает в микрофон. На открытый микрофон это не действует. Кроме того, компаньон предупреждает, если слышит ваши динамики в микрофоне.

### Ограничения

- Каналы разделяют слушателей только среди честных клиентов. Сервер и модифицированный клиент слышат все каналы.
- На паузе и во время автосохранения голос, который идёт через игру, замолкает.
- В Linux клавиша рации работает только в сеансе X11.

### Помощь

Установка, настройка сервера и решение проблем: [вики](https://github.com/vshuraeff/ptt-radio/wiki). Исходный код и сообщения об ошибках: [GitHub](https://github.com/vshuraeff/ptt-radio).
