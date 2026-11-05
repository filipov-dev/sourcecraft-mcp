# Установка и обновление

`sourcecraft-mcp` — один бинарник: и MCP-сервер (`serve`), и CLI для настройки (`connect`, `auth`, `secret`, `toolsets`). Готовые сборки есть для macOS, Linux и Windows; на других платформах соберите бинарник из исходников.

## Платформы

| Платформа | Сборка | Хранилище токенов |
| --- | --- | --- |
| macOS, Apple silicon и Intel | универсальный бинарник, подписан и нотаризован | Keychain |
| Linux x86_64 | статический бинарник (musl), без зависимостей | Secret Service |
| Linux aarch64 | статический бинарник (musl), без зависимостей | Secret Service |
| Windows x86_64 | `sourcecraft-mcp.exe` без зависимостей (CRT встроен), не подписан | Credential Manager |

Linux-бинарник собран статически: ему не нужны glibc определённой версии, `libdbus-1` и другие системные библиотеки. Он запускается в любом дистрибутиве, включая Alpine, distroless- и slim-образы и старые CentOS/RHEL.

Для HTTPS-запросов к SourceCraft нужны корневые сертификаты системы (пакет `ca-certificates`; в настольных дистрибутивах и Alpine он есть, в Debian/Ubuntu slim-образах его нужно поставить). Другой файл сертификатов можно указать в переменной `SSL_CERT_FILE`.

В Linux токен хранится через Secret Service (GNOME Keyring, KeePassXC, KWallet и т. п.): провайдер должен быть запущен в сессии D-Bus пользователя. Без него сохранить и прочитать токен нельзя — см. [Решение проблем](troubleshooting.md#linux-secret-service-недоступен).

## Установочный скрипт

Одна команда для Linux (x86_64, aarch64) и macOS, Homebrew не нужен:

```bash
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/sourcecraft-mcp/tarball/refs/heads/install | tar -xzO | sh
```

Адрес — архив ветки `install` публичного репозитория: в ней лежит только `install.sh` со списком релизов (адреса архивов и их SHA-256), его обновляет релизный конвейер. `tar -xzO` печатает этот скрипт, `sh` выполняет. Прямой постоянной ссылки на отдельный файл ветки SourceCraft не даёт, поэтому команда распаковывает архив ветки.

Что делает скрипт:

- определяет ОС и архитектуру и выбирает архив релиза (`linux-x86_64`, `linux-aarch64` или `macos-universal`);
- скачивает архив и `SHA256SUMS` релиза (через `curl` или `wget`, только HTTPS) и сверяет сумму архива с `SHA256SUMS` и со списком в скрипте; при расхождении ничего не устанавливает;
- кладёт бинарник в `~/.local/bin/sourcecraft-mcp` без `sudo`; замена атомарная, уже запущенный сервер продолжает работать со старой копией;
- если каталога нет в `PATH`, подсказывает, как его добавить.

Повторный запуск обновляет бинарник до последней версии (если она уже стоит — ничего не скачивает). Путь не меняется, поэтому `connect` повторять не нужно.

Параметры передаются через `sh -s --`:

```bash
# Конкретная версия
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/sourcecraft-mcp/tarball/refs/heads/install | tar -xzO | sh -s -- --version v0.2.0

# Другой каталог: бинарник попадёт в /opt/sourcecraft/bin
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/sourcecraft-mcp/tarball/refs/heads/install | tar -xzO | sh -s -- --prefix /opt/sourcecraft
```

| Параметр | Что делает |
| --- | --- |
| `--version vX.Y.Z` | Версия вместо последней (с `v` или без). Список — `--list`. |
| `--prefix DIR` | Установить в `DIR/bin` (по умолчанию `~/.local`). |
| `--bin-dir DIR` | Установить ровно в `DIR` (важнее `--prefix`). |
| `--force` | Переустановить, даже если эта версия уже стоит. |
| `--list` | Показать доступные версии и выйти. |

Нужны `sh`, `tar`, `curl` или `wget`, `sha256sum` или `shasum`. Через скрипт доступны релизы со статическими Linux-бинарниками (начиная с v0.2.0). Чтобы прочитать скрипт перед запуском, сохраните его в файл: `curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/sourcecraft-mcp/tarball/refs/heads/install | tar -xzO > install.sh`.

## Homebrew

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/sourcecraft-mcp
sourcecraft-mcp --version
```

- Tap размещён на SourceCraft, а не на GitHub, поэтому `brew tap` нужен полный адрес репозитория.
- Формулу устанавливайте по полному имени `filipov-dev/tap/sourcecraft-mcp`. Так Homebrew фиксирует, что вы доверяете формуле из стороннего tap; по короткому имени `sourcecraft-mcp` новые версии Homebrew её не ставят.
- В Linux формула ставит статический бинарник, зависимостей у неё нет.

Обновление и удаление:

```bash
brew update && brew upgrade filipov-dev/tap/sourcecraft-mcp
brew uninstall filipov-dev/tap/sourcecraft-mcp
brew untap filipov-dev/tap
```

`connect` записывает в конфигурацию клиента путь `…/bin/sourcecraft-mcp`, а не путь внутри `Cellar`, поэтому после `brew upgrade` клиенты запускают новую версию без повторного подключения.

## Готовый архив

Архивы и файл `SHA256SUMS` публикуются на [странице релизов](https://sourcecraft.dev/filipov-dev/sourcecraft-mcp/releases). Имя архива: `sourcecraft-mcp-<тег>-<платформа>.tar.gz`, например `sourcecraft-mcp-v0.1.0-linux-x86_64.tar.gz`. Внутри — каталог с бинарником `sourcecraft-mcp`, `README.md` и лицензиями. Архив для Windows — `.zip`, его установка описана в разделе [Windows](#windows).

Проверьте контрольную сумму (из каталога, куда скачаны архив и `SHA256SUMS`):

```bash
# Linux
grep linux-x86_64 SHA256SUMS | sha256sum -c -

# macOS
grep macos-universal SHA256SUMS | shasum -a 256 -c -
```

Распакуйте и установите бинарник в каталог из `PATH`:

```bash
tar -xzf sourcecraft-mcp-v0.1.0-linux-x86_64.tar.gz
sudo install -m 0755 sourcecraft-mcp-v0.1.0-linux-x86_64/sourcecraft-mcp /usr/local/bin/
sourcecraft-mcp --version
```

Чтобы обновиться, замените бинарник новым по тому же пути. Если путь не меняется, конфигурацию клиентов трогать не нужно.

## Windows

Для Windows есть только архив `sourcecraft-mcp-<тег>-windows-x86_64.zip` на [странице релизов](https://sourcecraft.dev/filipov-dev/sourcecraft-mcp/releases) (установочного скрипта и пакета для менеджеров пакетов нет). Внутри — каталог с `sourcecraft-mcp.exe`, `README.md` и лицензиями. Бинарнику не нужен Visual C++ Redistributable и другие библиотеки. Токены хранятся в Windows Credential Manager (общие учётные данные `<профиль>.dev.filipov.sourcecraft-mcp`).

Скачайте архив и `SHA256SUMS` в один каталог и выполните в PowerShell из него (тег замените на нужный):

```powershell
$tag = 'v0.3.0'
$zip = "sourcecraft-mcp-$tag-windows-x86_64.zip"

# Контрольная сумма: значение должно совпасть со строкой windows-x86_64 в SHA256SUMS
(Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
Select-String -SimpleMatch $zip SHA256SUMS

# Распаковка и установка в профиль пользователя (права администратора не нужны)
Unblock-File $zip
Expand-Archive $zip -DestinationPath "$env:TEMP\sourcecraft-mcp" -Force
$dir = "$env:LOCALAPPDATA\Programs\sourcecraft-mcp"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item "$env:TEMP\sourcecraft-mcp\sourcecraft-mcp-$tag-windows-x86_64\sourcecraft-mcp.exe" $dir -Force
```

Добавьте каталог в `PATH` пользователя (один раз) и откройте новое окно терминала:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\sourcecraft-mcp"
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', "$path;$dir", 'User')
}
```

```powershell
sourcecraft-mcp --version
```

Бинарник не подписан сертификатом подписи кода. Если открыть его из Проводника или из браузера, SmartScreen может показать «Система Windows защитила ваш компьютер»: проверьте контрольную сумму и нажмите «Подробнее» → «Выполнить в любом случае». `Unblock-File` снимает отметку «загружено из интернета» с архива, и распакованный бинарник её не получает. В Windows 11 со включённым Smart App Control неподписанные программы блокируются без возможности запуска — используйте другую машину или соберите бинарник из исходников.

Чтобы обновиться, замените `sourcecraft-mcp.exe` новым по тому же пути: конфигурацию клиентов трогать не нужно. Пока MCP-клиент держит сервер запущенным, файл занят — закройте клиент или сначала переименуйте старый файл (`Rename-Item sourcecraft-mcp.exe sourcecraft-mcp.old.exe`; переименовать запущенный файл Windows разрешает) и удалите его после перезапуска клиента.

Настройки профилей лежат в `%APPDATA%\sourcecraft-mcp\profiles.toml`.

## Из исходников

Нужен Rust 1.88 или новее (`rustup` подхватит версию из `rust-toolchain.toml`) и C-компилятор (`cc`) для криптографии TLS. Системные библиотеки D-Bus и OpenSSL не нужны: Secret Service используется через D-Bus-клиент на чистом Rust.

```bash
cargo build --release
```

Бинарник: `target/release/sourcecraft-mcp` (в Windows — `target\release\sourcecraft-mcp.exe`). Скопируйте его в каталог из `PATH` или установите сразу:

```bash
cargo install --path crates/sourcecraft-mcp
```

В Windows нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» (компилятор и компоновщик MSVC). Релизный архив собирается так: `cargo build --release --target x86_64-pc-windows-msvc` с `RUSTFLAGS=-C target-feature=+crt-static` — тогда бинарнику не нужен Visual C++ Redistributable.

На macOS собранный локально бинарник не подписан Developer ID: при каждой пересборке Keychain снова спросит разрешение на доступ к токену (см. [Решение проблем](troubleshooting.md#macos-keychain-спрашивает-разрешение)).

## Первое подключение

После установки подключите сервер к клиенту: `sourcecraft-mcp connect claude-code` или `sourcecraft-mcp connect codex`. `connect` откроет в браузере форму создания токена SourceCraft и подскажет, как её заполнить; останется вставить созданный токен. По SSH и без графической сессии браузер не открывается — `connect` печатает ссылку, откройте её на своём компьютере. Подробнее — [Подключение к MCP-клиентам](clients.md).

## Удаление

1. Отключите сервер в клиентах: `claude mcp remove --scope user sourcecraft`, запись `[mcp_servers.sourcecraft]` в `~/.codex/config.toml` или запись `sourcecraft` в конфигурации другого клиента.
2. Удалите токены и настройки профилей: `sourcecraft-mcp auth logout --profile <профиль> --forget` для каждого профиля.
3. Отзовите токены в SourceCraft: Home → Access → Personal access tokens.
4. Удалите бинарник (`brew uninstall …` или файл из `PATH`) и, если осталась, папку `~/.config/sourcecraft-mcp` (в Windows — `%APPDATA%\sourcecraft-mcp`).
