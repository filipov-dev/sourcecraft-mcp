# HTTP-режим

`sourcecraft-mcp serve --http` запускает тот же MCP-сервер не через stdio, а как HTTP-сервис (Streamable HTTP). Он нужен, когда:

- агент работает на Linux-машине без графической сессии, где нет Keychain и Secret Service, и хранить токен в системном хранилище негде;
- MCP-клиент не запускает локальный процесс, а подключается к серверу по адресу.

Инструменты, уровни доступа, наборы и вырезание секретов — те же, что у stdio-сервера ([MCP-инструменты](tools.md)). Отличается то, откуда берётся токен, и то, что сервер не работает с локальными файлами.

## Как устроен токен

В HTTP-режиме **нет профилей и хранилища токенов**. Каждый HTTP-запрос агента несёт его токен SourceCraft в заголовке:

```text
Authorization: Bearer <токен>
```

- Без заголовка или с некорректным заголовком сервер отвечает `401 Unauthorized` с `WWW-Authenticate: Bearer` и не передаёт запрос в MCP.
- Токен читается заново из каждого запроса и используется только для вызовов SourceCraft API, сделанных в ответ на этот запрос. Сервер не хранит его ни в памяти между запросами, ни на диске, не пишет в логи и не возвращает в ответах и ошибках (если API отразил токен в ответе, он заменяется на `***`).
- Токен уходит только на адрес SourceCraft API (`--base-url`). Редиректы на другие хосты не выполняются, произвольные URL `api_request` не принимает.
- Сервер работает без сессий: каждый запрос обрабатывается независимо, поэтому разные агенты с разными токенами могут работать с одним сервером одновременно.

### Модель угроз

Токен лежит в конфигурации MCP-клиента, и агент может его прочитать. Поэтому в HTTP-режиме:

- **настоящая граница — роли токена в SourceCraft.** Выдавайте каждому агенту свой токен с минимальными ролями, которых хватает для его задач. Такой токен можно отозвать, не затрагивая остальных;
- флаги доступа сервера (`--allow-write`, `--allow-destructive`, `--toolsets`) и вырезание секретов — **страховка от ошибок агента, а не граница безопасности**: агент, который знает токен, может обратиться к SourceCraft API и в обход сервера;
- сервер на loopback (по умолчанию) доступен только процессам этой машины. За её пределами токены идут открытым текстом, поэтому снаружи сервер должен быть доступен только через TLS (reverse proxy) или в доверенной сети.

Если важно, чтобы агент не видел токен, используйте stdio-сервер: там токен хранится в системном хранилище и агенту недоступен ([Безопасность](security.md#токен)).

## Отличия от stdio

| | stdio (`serve`) | HTTP (`serve --http`) |
| --- | --- | --- |
| Токен | Системное хранилище, профиль `--profile` | Заголовок `Authorization` каждого запроса |
| Настройки доступа | Флаги или настройки профиля | Только флаги командной строки |
| Локальные файлы | В разрешённых каталогах (`--allow-path`) | Выключены |
| Инструкции о CLI `secret` | Есть | Нет |
| [Пользовательские инструменты](tools.md#пользовательские-инструменты) | `tools.toml` и `--tools-file` | То же: файлы читает сервер при запуске, вызовы идут с токеном агента |

Локальные файлы выключены, потому что сервер работает не на машине агента: путь из аргумента указывал бы на файл сервера. Поэтому в HTTP-режиме:

- инструменты загрузки вложений (`upload_issue_attachment`, `upload_issue_comment_attachment`, `add_release_asset_attachment`) не регистрируются;
- у `api_request` нет аргументов `body_file`, `content_type` и `save_to`;
- у `secret_set` нет `value_file`: значение передаётся только в `value`, то есть через диалог с агентом;
- `secret_copy` копирует только между репозиториями (`sc://…`): системное хранилище машины сервера агенту не принадлежит, поэтому источники `keychain://` отклоняются и в описании инструмента не упоминаются.

`--profile`, `--allow-path`, `--allow-keychain-source` и `--dynamic-toolsets` вместе с `--http` — ошибка запуска. Динамическим наборам нужна сессия, в которой хранится список включённых наборов, а HTTP-сервер работает без сессий; набор инструментов в HTTP-режиме задаётся `--toolsets` при запуске.

## Запуск

### Бинарник

```bash
sourcecraft-mcp serve --http
# sourcecraft-mcp: MCP endpoint http://127.0.0.1:8765/mcp
```

| Флаг | По умолчанию | Назначение |
| --- | --- | --- |
| `--http` | выкл. | Режим HTTP вместо stdio. |
| `--listen <IP:порт>` | `127.0.0.1:8765` | Адрес и порт. Адрес не loopback — предупреждение в stderr: нужен TLS. |
| `--allowed-host <имя>` | — | Дополнительное допустимое значение заголовка `Host` (`имя` или `имя:порт`). Можно повторять. |
| `--allowed-origin <origin>` | — | Допустимый `Origin` браузерного клиента, например `http://localhost:6274`. Можно повторять. |

Остальные флаги `serve` (`--allow-write`, `--allow-destructive`, `--toolsets`, `--allow-secret-values`, `--base-url`, `--timeout`, `--max-response-bytes`) работают так же, как в stdio ([Справочник CLI](cli.md#serve)).

Эндпоинты:

- `POST /mcp` — MCP (Streamable HTTP без сессий, ответы `application/json`); нужен токен. `GET` и `DELETE` отвечают `405`.
- `GET /health` — проверка готовности без токена: `{"status":"ok",…}`.

Сервер завершается по SIGINT и SIGTERM. Логи пишутся в stderr, уровень задаёт `SOURCECRAFT_MCP_LOG`.

### Защита от DNS rebinding

- **`Host`.** Принимаются только `localhost`, `127.0.0.1`, `::1`, конкретный адрес из `--listen` и имена из `--allowed-host`; остальные запросы получают `403`. За reverse proxy добавьте имя, которое proxy передаёт в `Host`.
- **`Origin`.** Запросы без `Origin` (обычные MCP-клиенты, не браузер) принимаются. Запрос из браузера с `Origin` принимается, только если origin указан в `--allowed-origin`; по умолчанию такие запросы отклоняются (`403`).

### Docker

В репозитории есть `Dockerfile`: бинарник собирается в Alpine статически (musl), как бинарники релизов, без системных библиотек. Итоговый образ — минимальный `alpine`, из пакетов в нём только корневые сертификаты (`ca-certificates`) для проверки TLS SourceCraft API; непривилегированный пользователь, порт `8765`, `HEALTHCHECK` по `/health` через `wget` из BusyBox.

```bash
docker build -t sourcecraft-mcp .
docker run -d --name sourcecraft-mcp --restart unless-stopped \
  -p 127.0.0.1:8765:8765 sourcecraft-mcp
curl http://127.0.0.1:8765/health
```

Внутри контейнера сервер слушает `0.0.0.0:8765` (команда по умолчанию `serve --http --listen 0.0.0.0:8765`), поэтому при запуске печатает предупреждение о TLS. Публикуйте порт на loopback хоста (`127.0.0.1:8765:8765`), а не на всех интерфейсах. Флаги добавляются после имени образа, команда при этом задаётся целиком:

```bash
docker run -d -p 127.0.0.1:8765:8765 sourcecraft-mcp \
  serve --http --listen 0.0.0.0:8765 --allow-write --toolsets issues,repository-pullrequest
```

Если к серверу обращаются другие контейнеры по имени сервиса (`http://sourcecraft-mcp:8765/mcp`), добавьте это имя: `--allowed-host sourcecraft-mcp`.

### Docker Compose

`compose.yaml` в репозитории:

```yaml
services:
  sourcecraft-mcp:
    build: .
    image: sourcecraft-mcp:local
    command: ["serve", "--http", "--listen", "0.0.0.0:8765"]
    ports:
      - "127.0.0.1:8765:8765"
    environment:
      SOURCECRAFT_MCP_LOG: warn
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    restart: unless-stopped
```

```bash
docker compose up --build -d
docker compose logs -f sourcecraft-mcp
```

### systemd

Бинарник из [готового архива](install.md#готовый-архив), например `/usr/local/bin/sourcecraft-mcp`. Файл `/etc/systemd/system/sourcecraft-mcp.service`:

```ini
[Unit]
Description=SourceCraft MCP server (HTTP)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/sourcecraft-mcp serve --http --listen 127.0.0.1:8765
Environment=SOURCECRAFT_MCP_LOG=warn
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now sourcecraft-mcp
curl http://127.0.0.1:8765/health
```

Токенов в unit-файле нет: их передают агенты.

### За reverse proxy с TLS

Чтобы агенты с других машин подключались к серверу, оставьте его на loopback и поставьте перед ним reverse proxy с TLS. Пример для nginx:

```nginx
server {
    listen 443 ssl;
    server_name mcp.example.com;
    # ssl_certificate и ssl_certificate_key — ваши сертификаты

    location /mcp {
        proxy_pass http://127.0.0.1:8765;
        proxy_set_header Host $host;
        proxy_http_version 1.1;
        proxy_buffering off;
        proxy_read_timeout 120s;
    }
}
```

Сервер запускается с `--allowed-host mcp.example.com`. Proxy не должен логировать заголовок `Authorization`.

## Подключение агентов

Каждому агенту — свой токен SourceCraft (Home → Access → Personal access tokens) с минимальными ролями. URL — эндпоинт `/mcp`, а не `/health`.

### Claude Code

```bash
claude mcp add --transport http sourcecraft http://127.0.0.1:8765/mcp \
  --header "Authorization: Bearer <токен>"
```

Токен сохраняется в конфигурации Claude Code (для `--scope project` — в `.mcp.json` проекта, не коммитьте его). Проверка: `claude mcp list` или `/mcp` в сессии.

### Codex

Codex берёт токен из переменной окружения. В `~/.codex/config.toml`:

```toml
[mcp_servers.sourcecraft]
url = "http://127.0.0.1:8765/mcp"
bearer_token_env_var = "SOURCECRAFT_TOKEN"
```

Переменная `SOURCECRAFT_TOKEN` должна быть задана в окружении, из которого запускается Codex. Этот вариант только для HTTP-режима: сам `sourcecraft-mcp` токен из окружения не читает, его отправляет Codex в заголовке.

### Другие клиенты

Укажите адрес `http://<хост>:8765/mcp` (транспорт Streamable HTTP) и заголовок `Authorization: Bearer <токен>`. Большинство клиентов принимают такую запись:

```json
{
  "mcpServers": {
    "sourcecraft": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp",
      "headers": { "Authorization": "Bearer <токен>" }
    }
  }
}
```

Браузерным клиентам (например, MCP Inspector) нужен ещё `--allowed-origin` с их origin, например `--allowed-origin http://localhost:6274`.

## Проблемы

| Симптом | Причина |
| --- | --- |
| `401` и `WWW-Authenticate: Bearer` | Клиент не отправил заголовок `Authorization: Bearer <токен>` или отправил его в другом формате. |
| Инструмент вернул `SourceCraft API returned HTTP 401` | Токен отклонил SourceCraft: он истёк или отозван. Замените его в конфигурации клиента. |
| `403 Forbidden: Host header is not allowed` | Клиент или proxy обращается по имени, которого нет в списке: добавьте `--allowed-host`. |
| `403 Forbidden: Origin header is not allowed` | Запрос из браузера: добавьте его origin в `--allowed-origin`. |
| `405` на `/mcp` | Запрос `GET` или `DELETE`: сервер без сессий принимает только `POST`. |
| Клиент пишет `Unexpected content type` | В конфигурации указан `/health` вместо `/mcp`. |
