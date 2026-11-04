# sourcecraft-mcp

MCP-сервер и CLI для [SourceCraft](https://sourcecraft.dev) REST API. Даёт Claude Code, Codex и другим MCP-клиентам работать с репозиториями, задачами, пул-реквестами, релизами, CI и секретами SourceCraft.

- **Все операции API — MCP-инструменты.** Они генерируются из официальной спецификации SourceCraft при сборке; `api_search` ищет нужную операцию, `api_request` вызывает те, для которых нет отдельного инструмента.
- **Токен — только в системном хранилище** (macOS Keychain, Secret Service в Linux), отдельный профиль и токен для каждого агента. Ни переменные окружения, ни `.env`, ни конфигурация клиента токен не содержат.
- **По умолчанию только чтение.** Запись и удаление включаются явно; инструменты выше разрешённого уровня агенту не видны.
- **Значения секретов не попадают к агенту.** Они вырезаются из всех ответов, а `sourcecraft-mcp secret exec` позволяет использовать секрет в команде, не показывая его.

Сервер работает через stdio: MCP-клиент сам запускает `sourcecraft-mcp serve`.

## Установка

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/sourcecraft-mcp
```

Tap размещён на SourceCraft, поэтому `brew tap` нужен полный адрес. Устанавливайте формулу по полному имени `filipov-dev/tap/sourcecraft-mcp`: Homebrew не ставит формулы из сторонних tap по короткому имени.

### Готовый архив

Скачайте архив для своей платформы со [страницы релизов](https://sourcecraft.dev/filipov-dev/sourcecraft-mcp/releases), проверьте контрольную сумму по `SHA256SUMS` и положите бинарник `sourcecraft-mcp` в каталог из `PATH`.

| Платформа | Архив |
| --- | --- |
| macOS, Apple silicon и Intel (подписан и нотаризован) | `sourcecraft-mcp-<версия>-macos-universal.tar.gz` |
| Linux x86_64 (glibc 2.34+, нужен `libdbus-1`) | `sourcecraft-mcp-<версия>-linux-x86_64.tar.gz` |
| Linux aarch64 (glibc 2.34+, нужен `libdbus-1`) | `sourcecraft-mcp-<версия>-linux-aarch64.tar.gz` |

### Из исходников

```bash
cargo build --release
```

Бинарник: `target/release/sourcecraft-mcp`. Нужен Rust 1.88 или новее; в Linux — `pkg-config` и `libdbus-1-dev`.

Подробнее, включая проверку контрольных сумм и обновление: [docs/install.md](docs/install.md).

## Быстрый старт: Claude Code и Codex

```bash
sourcecraft-mcp connect claude-code
# или
sourcecraft-mcp connect codex
```

`connect` подскажет, как создать персональный токен в SourceCraft (Home → Access → Personal access tokens), примет его скрытым вводом, проверит запросом к API и сохранит в системное хранилище под профилем с именем клиента. Затем покажет, что именно изменится в конфигурации клиента, и после подтверждения зарегистрирует сервер `sourcecraft`. Перезапустите клиент.

Права задаются при подключении и попадают в команду запуска сервера:

```bash
sourcecraft-mcp connect codex --allow-write --toolsets repository,repository-pullrequest,issues
```

Повторный `connect` с другими флагами меняет настройки; сохранённый токен используется заново.

## Другие MCP-клиенты

`connect` умеет настраивать только Claude Code и Codex. Любой другой клиент со stdio-серверами подключается вручную в два шага.

1. Сохраните токен в отдельный профиль:

   ```bash
   sourcecraft-mcp auth login --profile cursor
   ```

2. Зарегистрируйте в клиенте stdio-сервер с командой `sourcecraft-mcp` и аргументами `serve --profile <профиль>`. Большинство клиентов принимают такую запись:

   ```json
   {
     "mcpServers": {
       "sourcecraft": {
         "command": "/opt/homebrew/bin/sourcecraft-mcp",
         "args": ["serve", "--profile", "cursor"]
       }
     }
   }
   ```

   Укажите в `command` абсолютный путь из `command -v sourcecraft-mcp`: клиенты, запущенные не из терминала, часто не видят `PATH` оболочки. Чтобы разрешить запись или удаление, добавьте в `args` `"--allow-write"` или `"--allow-destructive"`.

Токен в конфигурацию клиента не пишется. Подробнее: [docs/clients.md](docs/clients.md).

## Уровни доступа

| Уровень | Как включить | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Только инструменты, которые читают данные. |
| Запись | `--allow-write` | Создание и изменение: задачи, комментарии, пул-реквесты, релизы, запуск CI. |
| Удаление | `--allow-destructive` (включает запись) | Удаление, запись секретов, роли и приглашения, публичная видимость репозитория. |

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` дополнительно сужает список инструментов до нужных разделов API (`sourcecraft-mcp toolsets`).

## Секреты

- Значения секретов репозитория и поля, похожие на пароли и токены, заменяются на `"[redacted]"` во всех ответах, включая `api_request` и ошибки.
- Показать значения можно только на сервере с `--allow-secret-values` и только по явному запросу инструмента.
- Секрет записывается инструментом `secret_set`: он принимает открытый текст или файл и сам кодирует значение в base64.
- Чтобы использовать секрет в команде, не показывая его агенту:

  ```bash
  sourcecraft-mcp secret exec --profile codex --repo acme/web --env CONF=APP_CONFIG -- \
    sh -c 'printf %s "$CONF" | ssh deploy@host "cat > /etc/app/config"'
  ```

  Значение передаётся команде в переменной окружения и маскируется в её выводе.

Подробнее и об ограничениях: [docs/security.md](docs/security.md).

## Документация

- [Установка и обновление](docs/install.md)
- [Подключение к MCP-клиентам](docs/clients.md)
- [Справочник CLI](docs/cli.md)
- [MCP-инструменты](docs/tools.md)
- [Безопасность и секреты](docs/security.md)
- [Решение проблем](docs/troubleshooting.md)
- [Разработка](docs/development.md)

## Лицензия

`MIT OR Apache-2.0` — на ваш выбор: [LICENSE-MIT](LICENSE-MIT) или [LICENSE-APACHE](LICENSE-APACHE).
