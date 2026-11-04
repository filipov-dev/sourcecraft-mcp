# Установка и обновление

`sourcecraft-mcp` — один бинарник: и MCP-сервер (`serve`), и CLI для настройки (`connect`, `auth`, `secret`, `toolsets`). Готовые сборки есть для macOS и Linux; на других платформах соберите бинарник из исходников.

## Платформы

| Платформа | Сборка | Хранилище токенов |
| --- | --- | --- |
| macOS, Apple silicon и Intel | универсальный бинарник, подписан и нотаризован | Keychain |
| Linux x86_64 | glibc 2.34 или новее, динамически связан с `libdbus-1` | Secret Service |
| Linux aarch64 | glibc 2.34 или новее, динамически связан с `libdbus-1` | Secret Service |

В Linux токен хранится через Secret Service (GNOME Keyring, KeePassXC, KWallet и т. п.): провайдер должен быть запущен в сессии D-Bus пользователя. Без него сохранить и прочитать токен нельзя — см. [Решение проблем](troubleshooting.md#linux-secret-service-недоступен).

## Homebrew

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/sourcecraft-mcp
sourcecraft-mcp --version
```

- Tap размещён на SourceCraft, а не на GitHub, поэтому `brew tap` нужен полный адрес репозитория.
- Формулу устанавливайте по полному имени `filipov-dev/tap/sourcecraft-mcp`. Так Homebrew фиксирует, что вы доверяете формуле из стороннего tap; по короткому имени `sourcecraft-mcp` новые версии Homebrew её не ставят.
- В Linux формула подтягивает зависимость `dbus`.

Обновление и удаление:

```bash
brew update && brew upgrade filipov-dev/tap/sourcecraft-mcp
brew uninstall filipov-dev/tap/sourcecraft-mcp
brew untap filipov-dev/tap
```

`connect` записывает в конфигурацию клиента путь `…/bin/sourcecraft-mcp`, а не путь внутри `Cellar`, поэтому после `brew upgrade` клиенты запускают новую версию без повторного подключения.

## Готовый архив

Архивы и файл `SHA256SUMS` публикуются на [странице релизов](https://sourcecraft.dev/filipov-dev/sourcecraft-mcp/releases). Имя архива: `sourcecraft-mcp-<тег>-<платформа>.tar.gz`, например `sourcecraft-mcp-v0.1.0-linux-x86_64.tar.gz`. Внутри — каталог с бинарником `sourcecraft-mcp`, `README.md` и лицензиями.

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

В Linux нужна системная библиотека `libdbus-1` (пакет `libdbus-1-3` в Debian и Ubuntu, `dbus-libs` в Fedora); обычно она уже установлена.

Чтобы обновиться, замените бинарник новым по тому же пути. Если путь не меняется, конфигурацию клиентов трогать не нужно.

## Из исходников

Нужен Rust 1.88 или новее (`rustup` подхватит версию из `rust-toolchain.toml`). В Linux дополнительно нужны `pkg-config` и заголовки D-Bus (`libdbus-1-dev` в Debian и Ubuntu, `dbus-devel` в Fedora).

```bash
cargo build --release
```

Бинарник: `target/release/sourcecraft-mcp`. Скопируйте его в каталог из `PATH` или установите сразу:

```bash
cargo install --path crates/sourcecraft-mcp
```

На macOS собранный локально бинарник не подписан Developer ID: при каждой пересборке Keychain снова спросит разрешение на доступ к токену (см. [Решение проблем](troubleshooting.md#macos-keychain-спрашивает-разрешение)).

## Удаление

1. Отключите сервер в клиентах: `claude mcp remove --scope user sourcecraft`, запись `[mcp_servers.sourcecraft]` в `~/.codex/config.toml` или запись `sourcecraft` в конфигурации другого клиента.
2. Удалите токены и настройки профилей: `sourcecraft-mcp auth logout --profile <профиль> --forget` для каждого профиля.
3. Отзовите токены в SourceCraft: Home → Access → Personal access tokens.
4. Удалите бинарник (`brew uninstall …` или файл из `PATH`) и, если осталась, папку `~/.config/sourcecraft-mcp`.
