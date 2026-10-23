# ptt-radio

Push-to-talk рация для мультиплеера Factorio 2.1.23: голос идёт через сам мультиплеер игры (Lua UDP API), плашка «кто говорит» с именем в цвете игрока видна всем. Дизайн и обоснования: [DESIGN.md](DESIGN.md), замеры транспорта: [../udp-probe/README.md](../udp-probe/README.md).

Состояние: этап 1 из плана (мод v0.1 и эмулятор фиктивного голоса) готов и проверен. Компаньона с настоящим звуком ещё нет.

## Состав

- `mod/ptt-radio/`: мод. Пакеты HELLO/START/VOICE/STOP, пересылка компаньонам слушателей, плашка в `gui.left`, щелчки `utility/gui_click` и `utility/list_box_click`, таймаут говорящего по тикам с синтетическим STOP, клавиша `ALT + T` (TOGGLE своему компаньону). Настройки: `ptt-radio-companion-port` и `ptt-radio-echo` (игрок), `ptt-radio-scope` (карта: `all` или `force`).
- `tools/radio.py`: эмулятор компаньона без звука, `uv run -q tools/radio.py {hello|talk|listen}`; `talk --no-stop` изображает упавший компаньон.
- `stand/`: headless-сервер 2.1.23 с модом, образ берётся из `../udp-probe/server`.
- `client-mods/`: каталог модов для клиента (`--mod-directory`), чтобы не трогать основную установку.

## Запуск стенда

```sh
cd stand && docker compose up -d --build
open -n -a /Applications/factorio.app --args \
  --mod-directory "$PWD/../client-mods" --enable-lua-udp 34556 --mp-connect 127.0.0.1:34197
uv run -q ../tools/radio.py hello
uv run -q ../tools/radio.py talk --seconds 3                                    # плашка с вашим именем
docker exec ptt-radio-factorio uv run -q /tools/radio.py talk --seconds 3 --expect-silence   # плашка «Сервер»
```

## Проверено 2026-10-24

Один клиент macOS и headless-сервер в OrbStack на одной машине.

| Проверка | Результат |
|---|---|
| HELLO → ROSTER | список игроков с индексами и цветами, включая псевдоигрока 0 «Сервер» |
| Плашка и щелчки | видно глазами: рамка «Рация» с ником в цвете игрока, затем «Сервер»; щелчки на начале и конце |
| Пересылка от «Сервера» | 150/150 кадров, START и STOP |
| Без эхо | свой голос не возвращается |
| Эхо | 250/250 кадров подряд; RTT кадра p50 75–77 мс, p95 ≤87 мс; STOP через 76–84 мс после отпускания |
| Упавший компаньон (`--no-stop`) | мод рассылает STOP через 587 мс после последнего кадра |
| `ALT + T` в игре | TOGGLE приходит своему компаньону |

Разовый выброс: первый кадр одного прогона вернулся через 317 мс, в повторах этого не было.
