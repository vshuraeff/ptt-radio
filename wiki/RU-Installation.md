# Установка

В итоге мод будет стоять в вашей игре, компаньон — распакован на компьютере, а игра — запускаться с флагом, который нужен рации.

Для работы нужны три вещи:

- мод на сервере **и** у каждого игрока. Только на клиенте он работать не может, да и Factorio не пустит на сервер с другим набором модов;
- компаньон на компьютере каждого игрока, на той же машине, где запущена игра;
- игра, запущенная с флагом `--enable-lua-udp <порт>`: через него мод общается с компаньоном.

Моду нужна Factorio 2.1. Как поставить его на сервер, описано в разделе [Настройка сервера](RU-Server-Setup).

## 1. Поставьте мод

**С портала модов.** В игре откройте «Моды», найдите *PTT Radio*, установите и перезапустите игру. Если мод уже стоит на сервере, игра сама предложит его скачать при подключении.

**Из zip-архива.**

1. Скачайте `ptt-radio_<версия>.zip` со [страницы релизов](https://github.com/vshuraeff/ptt-radio/releases).
2. Положите архив как есть, не распаковывая, в папку `mods` игры:
   - Windows: `%APPDATA%\Factorio\mods`
   - macOS: `~/Library/Application Support/factorio/mods`
   - Linux: `~/.factorio/mods`
3. Запустите игру и проверьте в «Модах», что *PTT Radio* включён.

## 2. Установите компаньон

На macOS или Linux выполните в Терминале:

```sh
curl -fsSL https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh | sh
```

На Windows x86_64 выполните в PowerShell 5.1 или новее:

```powershell
irm https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 | iex
```

Установщик скачивает последний релиз, сверяет SHA-256 и помещает программу в `ptt-radio-companion` внутри каталога данных Factorio (родительской папки `mods`, указанной выше). Он печатает команду запуска и параметр Steam, не требует прав администратора и не меняет настройки Steam. Повторный запуск обновляет установку. При ошибке загрузки, контрольной суммы или структуры архива предыдущая установка сохраняется.

Для своих папок или конкретной версии сначала скачайте установщик и передайте параметры:

```sh
curl -fsSLo install.sh https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.sh
sh install.sh --factorio-dir "/path/to/Factorio data" --mods-dir "/path/to/custom mods" --version v0.4.0
```

```powershell
Invoke-WebRequest -UseBasicParsing https://github.com/vshuraeff/ptt-radio/releases/latest/download/install.ps1 -OutFile install.ps1
& .\install.ps1 -FactorioDir 'D:\Games\Factorio data' -ModsDir 'D:\Games\custom mods' -Version v0.4.0
```

`--mods-dir` / `-ModsDir` записывает абсолютный путь к модам в `install.json`; папку модов и `mod-list.json` установщик не меняет. Без этого параметра используется `<каталог данных>/mods`. Версия компаньона должна совпадать с модом. Архив Linux пока есть только для x86_64 с glibc 2.35 или новее; macOS поддерживает Apple silicon и Intel. Для остальных платформ установщик сообщает об отсутствии подходящего архива.

### Ручная загрузка

1. На [странице релизов](https://github.com/vshuraeff/ptt-radio/releases) скачайте файл для своей системы:

   | Система | Файл |
   | --- | --- |
   | Windows (64 бит) | `ptt-radio-companion-<версия>-windows-x86_64.zip` |
   | macOS (Apple silicon и Intel) | `ptt-radio-companion-<версия>-macos-universal.zip` |
   | Linux (x86_64, glibc 2.35 и новее: Ubuntu 22.04+, Debian 12+) | `ptt-radio-companion-<версия>-linux-x86_64.tar.gz` |

2. По желанию сверьте контрольную сумму с файлом `SHA256SUMS` из того же релиза: на Linux — `sha256sum --check --ignore-missing SHA256SUMS`, на macOS — `shasum -a 256 <файл>`.
3. Распакуйте в папку, куда у вас есть право записи: тогда компаньон сможет сам обновляться. Внутри одна программа `ptt-radio-companion` (на Windows — `ptt-radio-companion.exe`), а также `LICENSE` и `README.md`.

Компаньон берите из того же релиза, что и мод. Первый запуск, клавиша рации и запросы разрешений описаны в разделе [Компаньон](RU-Companion).

## 3. Добавьте флаг запуска

Игра должна запускаться с `--enable-lua-udp 34556`. Компаньон по умолчанию ждёт игру именно на порту `34556` (его параметр `--game-port`). Если выберете другой порт, запускайте компаньон с `--game-port <тот же порт>`.

**Steam (Windows, macOS, Linux).**

1. В библиотеке Steam щёлкните по Factorio правой кнопкой и выберите «Свойства».
2. На вкладке «Общие» вставьте напечатанный установщиком параметр Steam в поле «Параметры запуска». Он имеет вид `"<каталог данных>/ptt-radio-companion/ptt-radio-companion" --launch %command%` (на Windows — с `.exe`). Компаньон запустится вместе с игрой и добавит флаг Lua UDP. Если запускаете компаньон отдельно, укажите `--enable-lua-udp 34556`.
3. Закройте окно и запускайте игру как обычно.

**Версия с factorio.com.** Запускайте игру с флагом или допишите его в ярлык:

- Windows: `bin\x64\factorio.exe --enable-lua-udp 34556` из папки Factorio. В ярлыке на рабочем столе допишите флаг в конец поля «Объект».
- Linux: `./bin/x64/factorio --enable-lua-udp 34556` из папки Factorio.

**Приложение для macOS (не Steam).** В Терминале:

```sh
open -a factorio --args --enable-lua-udp 34556
```

Если Factorio уже запущена, сначала закройте её: `open` передаёт флаг только при новом запуске.

## 4. Проверьте

1. Выполните напечатанную установщиком команду запуска или запустите Factorio через указанный параметр Steam (см. [Компаньон](RU-Companion)).
2. Зайдите в сетевую игру.
3. Через несколько секунд в чате появится `Рация: компаньон подключён.`, а компаньон выведет список игроков после строки `roster:`.

Если сообщения нет, загляните в раздел [Если не работает](RU-Troubleshooting).
