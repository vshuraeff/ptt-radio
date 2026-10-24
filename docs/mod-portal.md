# Mod portal text

The text below is pasted into the Factorio mod portal: the summary into the summary field (500 characters at most), the description into the description field. It uses only headings, paragraphs, lists, bold, links and inline code, which the portal renders.

## Summary

Push-to-talk voice radio for multiplayer, with channels, a speaker list and admin controls. Every player who talks or listens needs the free ptt-radio companion app running and the game launched with --enable-lua-udp 34556. Required on the server and on every client.

## Description (English)

**PTT Radio** is push-to-talk voice chat for Factorio multiplayer. Hold a key and talk; the others hear you, and a list at the left edge of the screen shows who is speaking, in their player colour.

**Every player needs two things for voice to work:**

- the **ptt-radio companion** running on their computer. It records the microphone and plays the voices. Factorio mods cannot use audio devices, so the companion is a separate free program for Windows, macOS and Linux: [download it from GitHub](https://github.com/vshuraeff/ptt-radio/releases);
- the game launched with **`--enable-lua-udp 34556`**. In Steam, put it into the launch options of Factorio.

A player without the companion sees who is speaking but hears nothing and cannot talk. The mod is required on the server and on every client, and the headless server needs `--enable-lua-udp` too.

### Features

- Push-to-talk on `F8` in the companion, or an open microphone toggled with `ALT + V` or the shortcut button.
- Speaker list with the transmit channel icon and three link quality bars.
- Radio channels: pick any signal as your transmit channel, listen to several at once, keep up to 8 presets. Everyone starts on the shared `ANY` channel; `HERE` follows the surface you are on.
- Settings window on `ALT + SHIFT + V`, with a switch that turns the radio off completely.
- Admin tools in the same window and as the `/radio-admin` command: an admin channel everyone hears, channel bans, mutes, locks. The policy is kept in the save.
- Voice goes through the game, or directly between players over UDP when their networks allow it. An optional relay hub beside the server helps players who cannot connect directly.
- Half-duplex by default, so your speakers do not echo into your microphone, and an echo warning when they do.

### Limitations

- Channels separate listeners only among honest clients. The server and a modified client can hear every channel.
- A paused game, and an autosave, pause the voice that goes through the game.
- On Linux the push-to-talk key needs an X11 session.

### Help

Installation, server setup and troubleshooting: [wiki](https://github.com/vshuraeff/ptt-radio/wiki). Source code and bug reports: [GitHub](https://github.com/vshuraeff/ptt-radio).

## Описание (русский)

**PTT Radio** — голосовая рация с кнопкой «нажми и говори» для мультиплеера Factorio. Держите клавишу и говорите: остальные вас слышат, а слева на экране видно, кто сейчас говорит, с ником в цвете игрока.

**Чтобы голос работал, каждому игроку нужны две вещи:**

- запущенный **компаньон ptt-radio**. Он записывает микрофон и проигрывает голоса. Моды Factorio не имеют доступа к звуковым устройствам, поэтому компаньон — отдельная бесплатная программа для Windows, macOS и Linux: [скачать на GitHub](https://github.com/vshuraeff/ptt-radio/releases);
- игра, запущенная с параметром **`--enable-lua-udp 34556`**. В Steam его добавляют в параметры запуска Factorio.

Игрок без компаньона видит, кто говорит, но ничего не слышит и сам говорить не может. Мод нужен на сервере и у каждого клиента, а headless-серверу тоже нужен `--enable-lua-udp`.

### Возможности

- Передача по клавише `F8` в компаньоне или открытый микрофон: `ALT + V` или кнопка на панели быстрого доступа.
- Список говорящих со значком канала передачи и тремя делениями качества связи.
- Каналы: любой сигнал можно сделать своим каналом передачи, слушать сразу несколько каналов, хранить до 8 пресетов. По умолчанию все на общем канале `ANY`; `HERE` следует за поверхностью, на которой вы находитесь.
- Окно настроек на `ALT + SHIFT + V` с переключателем, который полностью выключает рацию.
- Инструменты администратора в том же окне и командой `/radio-admin`: админ-канал, который слышат все, запрет каналов, заглушение игроков, блокировки. Политика сохраняется в сейве.
- Голос идёт через игру или напрямую между игроками по UDP, если сеть это позволяет. Необязательный хаб рядом с сервером помогает тем, кто не может соединиться напрямую.
- Полудуплекс по умолчанию, чтобы динамики не давали эхо в микрофон, и предупреждение, если эхо всё же появилось.

### Ограничения

- Каналы разделяют слушателей только среди честных клиентов. Сервер и изменённый клиент слышат все каналы.
- Пауза и автосохранение останавливают голос, который идёт через игру.
- В Linux клавиша PTT работает только в сеансе X11.

### Помощь

Установка, настройка сервера и решение проблем: [вики](https://github.com/vshuraeff/ptt-radio/wiki/RU-Installation). Исходный код и сообщения об ошибках: [GitHub](https://github.com/vshuraeff/ptt-radio).
