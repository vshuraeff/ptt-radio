# Сборка из исходников

В итоге вы сами соберёте компаньон и хаб и прогоните тесты мода.

## Что понадобится

- **Rust** 1.88 или новее (крейт использует edition 2024), ставится через [rustup](https://rustup.rs).
- **cmake** и **компилятор C**: кодек Opus (libopus 1.6.1) собирается из исходников в комплекте и линкуется статически, системный libopus не нужен.
- **Только на Linux:** пакет разработки ALSA и `pkg-config`, например на Debian или Ubuntu:

  ```sh
  sudo apt-get install build-essential cmake pkg-config libasound2-dev
  ```

- **macOS:** инструменты командной строки Xcode (`xcode-select --install`) и cmake, например из Homebrew.
- **Windows:** инструменты сборки Visual Studio для C++ и cmake.
- Для тестов мода: `lua` и `luac` в `PATH`.

## Сборка компаньона

1. Получите исходники:

   ```sh
   git clone https://github.com/vshuraeff/ptt-radio.git
   cd ptt-radio/companion
   ```

2. Соберите:

   ```sh
   cargo build --release
   ```

3. Программы лежат в `target/release/`: `ptt-radio-companion`, `ptt-radio-hub` и `relay` — тестовый эмулятор игры. Проверьте: `./target/release/ptt-radio-companion --help`. Собранный вами компаньон никогда не заменяет себя релизом: сами обновляются только сборки со страницы релизов.

## Только хаб

Хабу звук не нужен. Всё, что связано со звуком и клавиатурой (звуковая библиотека, глобальная клавиша, фреймворки macOS), подключает фича `audio`, она включена по умолчанию. Если её выключить, хаб соберётся без этого, например на сервере без заголовков ALSA:

```sh
cargo build --release --no-default-features --bin ptt-radio-hub
```

Результат — `target/release/ptt-radio-hub`. Сам компаньон без фичи `audio` не собирается.

## Тесты

Из `companion/`:

```sh
cargo test --lib --bins
```

Тесты мода, из корня репозитория:

```sh
luac -p mod/ptt-radio/*.lua mod/tests/*.lua
lua mod/tests/control_test.lua mod/ptt-radio/control.lua
lua mod/tests/channels_test.lua
lua mod/tests/admin_test.lua
lua mod/tests/settings_gui_test.lua
```

Они гоняют обработчики событий мода без Factorio, но проверку в настоящей игре не заменяют.

### Сквозные сценарии

Здесь несколько компаньонов с виртуальным звуком работают против эмулятора `relay`, без игры, и всё меряется в реальном времени. Из `companion/`:

```sh
cargo test --release --test scenarios -- --nocapture --test-threads=1   # все транспорты, около 8 минут
cargo test --release --test scenarios direct:: -- --nocapture           # только прямой UDP
PTT_E2E_VOICE=talk.wav cargo test --test scenarios -- --nocapture       # свой голос вместо синтетического у игрока A
```

Сценарии проверяют задержку «рот–ухо», эхо от динамиков, двойной разговор, полудуплекс и остановку на 500 мс посреди фразы. Модули `game`, `direct`, `hub`, `player_relay` и `fallback` гоняют одни и те же проверки по каждому маршруту. Файлы последнего прогона лежат в `target/tmp/scenarios/<транспорт>/<сценарий>/aec-on/` и `aec-off/`: что каждый игрок сказал и услышал (WAV), трассы и логи компаньонов.

### Лаборатории эха

Офлайн-лаборатории эхо-гейта нужны два независимых WAV-файла, моно, 48 кГц, 16 бит:

```sh
PTT_LAB_A=a.wav PTT_LAB_B=b.wav cargo test --release --test aec_gate -- --ignored --nocapture
```

По умолчанию на строку уходит 60 секунд; `PTT_LAB_SECONDS` меняет длину основной таблицы (не меньше 20). WAV без нормализации сохраняются в `target/tmp/aec_gate/`.

Лаборатория эха на нескольких игроков — `cargo test --release --test multiparty_lab -- --ignored --nocapture`, голоса задаются в `PTT_LAB_A`, `PTT_LAB_B` и `PTT_LAB_C`. `PTT_LAB_PATH` выбирает модель задержки: `game` (по умолчанию), `direct` (односторонние задержки A–B, A–C, B–C — 15, 35 и 25 мс) или `hub` (два плеча до сервера без игровых тиков). Полный прогон в release идёт около 5 минут.

### Два компаньона без игры

Эмулятор `relay` заменяет игру и мод. В трёх терминалах из `companion/`: сначала эмулятор на двух игроков, у второго открытый микрофон; затем два компаньона с виртуальным звуком, второй — «с динамиками»:

```sh
./target/release/relay --player 34556:34555 --player 34566:34565 --open-mic 2
./target/release/ptt-radio-companion --ptt-key none --mic-wav talk.wav --speaker-wav a-heard.wav --auto-ptt 4 --exit-after 8
./target/release/ptt-radio-companion --game-port 34566 --port 34565 --ptt-key none \
  --speaker-wav b-heard.wav --couple-gain-db -6 --couple-delay-ms 2 --exit-after 8
```

Эмулятор запускайте первым и дождитесь строк `playerN: ...`: HELLO, отправленный раньше, теряется, и список игроков (а с ним и `--auto-ptt`) придёт только со следующим heartbeat, через 5 секунд. `a-heard.wav` — голос A, вернувшийся через динамики B. У эмулятора есть ещё `--echo N`, `--half-duplex N`, `--delay-ms`, `--jitter-ms` и `--stall-every S`. Без `--mic-wav` и `--speaker-wav` компаньоны работают с настоящими устройствами, так что так можно проверить и два живых компаньона на одной машине.

### Полная проверка

```sh
tools/gate.sh [--twice] [каталог логов]   # из корня репозитория
```

Скрипт запускает `cargo fmt --check`, clippy по всем целям, отладочные тесты `--lib --bins` и все интеграционные наборы, release-тесты `--lib`, сценарии, `aec_gate`, замеры политики во времени, `luac -p` по моду и четыре Lua-теста. Один прогон на прогретом кеше занимает около 8,5 минуты. `--twice` добавляет второй полный прогон сценариев, чтобы поймать нестабильные проверки.

- Логи по умолчанию пишутся в `<target>/gate/<время>/`; в `summary.txt` там код выхода и время каждого шага. Если хоть один шаг упал, скрипт завершается с ненулевым кодом.
- Упавший тест реального времени перезапускается один раз по точному имени; если перезапуск прошёл, шаг помечается как `flaky` и проверку не валит.
- Шаги реального времени держат `flock` на общем файле блокировки, чтобы две задачи на одной машине не мерили одновременно. Файл задаёт `PTT_REALTIME_LOCK` (по умолчанию `ptt-realtime.lock` в общем каталоге git, одном на все worktree), а `PTT_REALTIME_WAIT` ограничивает ожидание (по умолчанию 3600 секунд).
- Нужны `flock`, `lua` и `luac` в `PATH`; на macOS `flock` ставится из Homebrew.

## Мод из исходников

Скопируйте папку `mod/ptt-radio` в папку `mods` игры под именем `ptt-radio` или сделайте на неё ссылку. В сетевой игре у сервера и у всех игроков должна быть одна и та же версия мода.
