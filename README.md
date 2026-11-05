# sourcecraft-mcp

MCP-сервер и CLI для [SourceCraft](https://sourcecraft.dev) REST API. Даёт Claude Code, Codex и другим MCP-клиентам работать с репозиториями, задачами, пул-реквестами, релизами, CI и секретами SourceCraft.

- **Все операции API — MCP-инструменты.** Они генерируются из официальной спецификации SourceCraft при сборке; `api_search` ищет нужную операцию, `api_request` вызывает те, для которых нет отдельного инструмента.
- **Токен — только в системном хранилище** (macOS Keychain, Secret Service в Linux, Credential Manager в Windows), отдельный профиль и токен для каждого агента. Ни переменные окружения, ни `.env`, ни конфигурация клиента токен не содержат. Исключение — HTTP-режим (ниже): там токен агента приходит в заголовке каждого запроса и сервер его не хранит.
- **По умолчанию только чтение.** Запись и удаление включаются явно; инструменты выше разрешённого уровня агенту не видны.
- **Значения секретов не попадают к агенту.** Они вырезаются из всех ответов, а `sourcecraft-mcp secret exec` позволяет использовать секрет в команде, не показывая его.

Сервер работает через stdio: MCP-клиент сам запускает `sourcecraft-mcp serve`. Для Linux-машин без системного хранилища и клиентов, которые подключаются по адресу, есть [HTTP-режим](docs/http.md) (`serve --http`, в том числе в Docker): там профилей нет, и каждый агент передаёт свой токен в заголовке `Authorization`.

## Установка

### Установочный скрипт (Linux и macOS)

```bash
curl -fsSL https://codeload.sourcecraft.tech/filipov-dev/sourcecraft-mcp/tarball/refs/heads/install | tar -xzO | sh
```

Скрипт ставит последнюю версию в `~/.local/bin` без `sudo` и сверяет архив с `SHA256SUMS` релиза; повторный запуск обновляет бинарник. Параметры (`--version`, `--prefix`) и подробности — в [docs/install.md](docs/install.md#установочный-скрипт).

### Homebrew (macOS и Linux)

```bash
brew tap filipov-dev/tap https://git.sourcecraft.dev/filipov-dev/homebrew-tap.git
brew install filipov-dev/tap/sourcecraft-mcp
```

Tap размещён на SourceCraft, поэтому `brew tap` нужен полный адрес. Устанавливайте формулу по полному имени `filipov-dev/tap/sourcecraft-mcp`: Homebrew не ставит формулы из сторонних tap по короткому имени.

### Готовый архив

Скачайте архив для своей платформы со [страницы релизов](https://sourcecraft.dev/filipov-dev/sourcecraft-mcp/releases), проверьте контрольную сумму по `SHA256SUMS` и положите бинарник `sourcecraft-mcp` (в Windows — `sourcecraft-mcp.exe`) в каталог из `PATH`.

| Платформа | Архив |
| --- | --- |
| macOS, Apple silicon и Intel (подписан и нотаризован) | `sourcecraft-mcp-<версия>-macos-universal.tar.gz` |
| Linux x86_64 (статический, без зависимостей) | `sourcecraft-mcp-<версия>-linux-x86_64.tar.gz` |
| Linux aarch64 (статический, без зависимостей) | `sourcecraft-mcp-<версия>-linux-aarch64.tar.gz` |
| Windows x86_64 (без зависимостей, не подписан) | `sourcecraft-mcp-<версия>-windows-x86_64.zip` |

Бинарник для Windows не подписан сертификатом подписи кода: при запуске из Проводника SmartScreen может предупредить о неизвестном издателе. Установка в PowerShell — в [docs/install.md](docs/install.md#windows).

### Из исходников

```bash
cargo build --release
```

Бинарник: `target/release/sourcecraft-mcp`. Нужен Rust 1.88 или новее и C-компилятор.

Подробнее, включая проверку контрольных сумм и обновление: [docs/install.md](docs/install.md).

## Быстрый старт: Claude Code и Codex

```bash
sourcecraft-mcp connect claude-code
# или
sourcecraft-mcp connect codex
```

`connect` откроет в браузере форму создания персонального токена SourceCraft и подскажет, как её заполнить (по SSH — только напечатает ссылку), примет токен скрытым вводом, проверит запросом к API и сохранит в системное хранилище под профилем с именем клиента. Затем покажет, что именно изменится в конфигурации клиента, и после подтверждения зарегистрирует сервер `sourcecraft`. Перезапустите клиент.

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

## HTTP-режим

```bash
docker compose up --build -d   # или: sourcecraft-mcp serve --http
claude mcp add --transport http sourcecraft http://127.0.0.1:8765/mcp \
  --header "Authorization: Bearer <токен>"
```

Сервер ничего не хранит: токен приходит в каждом запросе и уходит только в SourceCraft API. Токен виден агенту в конфигурации клиента, поэтому граница безопасности здесь — роли токена (свой токен с минимальными ролями для каждого агента), а флаги доступа сервера — страховка от ошибок. Локальные файлы в HTTP-режиме выключены. Подробнее: [docs/http.md](docs/http.md).

## Уровни доступа

| Уровень | Как включить | Что доступно |
| --- | --- | --- |
| Чтение | по умолчанию | Только инструменты, которые читают данные. |
| Запись | `--allow-write` | Создание и изменение: задачи, комментарии, пул-реквесты, релизы, запуск CI. |
| Удаление | `--allow-destructive` (включает запись) | Удаление, запись секретов, роли и приглашения, публичная видимость репозитория. |

Инструменты выше разрешённого уровня не регистрируются: агент их не видит и не может вызвать. `--toolsets` дополнительно сужает список инструментов до нужных разделов API (`sourcecraft-mcp toolsets`), а `--dynamic-toolsets` позволяет агенту включать наборы по запросу в клиентах, которые поддерживают `notifications/tools/list_changed` ([подробнее](docs/tools.md#динамические-наборы)).

Частые запросы можно описать собственными инструментами в `~/.config/sourcecraft-mcp/tools.toml` — с теми же ограничениями, что у встроенных: [docs/tools.md](docs/tools.md#пользовательские-инструменты).

Большие ответы можно сократить аргументом `jq` любого инструмента API: например, `.pull_requests[] | {id, title}` оставит от списка пул-реквестов только идентификаторы и заголовки. Подробнее: [docs/tools.md](docs/tools.md#фильтр-ответа-jq).

## Секреты

- Значения секретов репозитория и поля, похожие на пароли и токены, заменяются на `"[redacted]"` во всех ответах, включая `api_request` и ошибки.
- Показать значения можно только на сервере с `--allow-secret-values` и только по явному запросу инструмента.
- Секрет записывается инструментом `secret_set`: он принимает открытый текст или файл и сам кодирует значение в base64.
- `secret_copy` (и `sourcecraft-mcp secret copy`) копирует секрет в другой репозиторий, не показывая значение агенту.
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
- [HTTP-режим](docs/http.md)
- [Справочник CLI](docs/cli.md)
- [MCP-инструменты](docs/tools.md)
- [Безопасность и секреты](docs/security.md)
- [Решение проблем](docs/troubleshooting.md)
- [Разработка](docs/development.md)

## Лицензия

`MIT OR Apache-2.0` — на ваш выбор: [LICENSE-MIT](LICENSE-MIT) или [LICENSE-APACHE](LICENSE-APACHE).
