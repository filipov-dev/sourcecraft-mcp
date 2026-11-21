# MCP-инструменты

Сервер предоставляет такие виды инструментов:

- **инструменты операций** — по одному на каждую операцию SourceCraft REST API, генерируются из спецификации;
- **служебные**: `api_search` (поиск операций) и `api_request` (запрос к любому пути API), а с `--dynamic-toolsets` — `list_toolsets`, `enable_toolset` и `disable_toolset` ([динамические наборы](#динамические-наборы));
- **`secret_set`** — запись секрета из открытого текста или файла вместо сырой операции API;
- **`secret_copy`** — копирование секрета в другой репозиторий без раскрытия значения;
- **пользовательские** — именованные шаблоны запросов из `tools.toml` (см. [Пользовательские инструменты](#пользовательские-инструменты)).

Кроме списка инструментов, сервер передаёт клиенту инструкции для агента (поле `instructions` при инициализации MCP): текущий уровень доступа, правила адресации, работа с секретами и примеры `sourcecraft-mcp secret exec` с профилем сервера.

## Как генерируются инструменты

Таблица операций строится при сборке из официальной спецификации SourceCraft API (`crates/sourcecraft-api/spec/sourcecraft.swagger.json`, источник — <https://api.sourcecraft.tech/docs/sourcecraft.swagger.json>). Новая версия `sourcecraft-mcp` с обновлённой спецификацией автоматически получает новые операции.

- **Имя** инструмента — `operationId` операции в `snake_case`: `GetPullRequest` → `get_pull_request`. Слишком общие имена уточнены: операции релизов называются `create_release`, `list_releases` и т. п., метки задач — `add_issue_labels`.
- **Описание** — summary и description операции из спецификации, правила адресации и уровень доступа.
- **Аргументы** — параметры пути и query, а также тело запроса в аргументе `body` (JSON-объект). Схема аргументов — JSON Schema из спецификации с раскрытыми ссылками.
- **Адресация по slug или ID.** Если у операции есть пара вариантов (по slug — `/repos/{org_slug}/{repo_slug}/…` и по ID — `/repos/id:{repo_id}/…`), они объединены в один инструмент. Передайте ровно один набор: например, `org_slug` + `repo_slug` или `repo_id`.
- **Проверка аргументов.** Неизвестные аргументы, пропущенные обязательные, смешение slug и ID и неверные типы отклоняются до запроса к API с понятным агенту сообщением. ID и целые числа можно передавать строками.
- **Пагинация.** Списки принимают `page_size` и `page_token`; токен следующей страницы приходит в ответе.
- **Файлы.** Инструменты загрузки вложений (`upload_issue_attachment` и др.) принимают путь к локальному файлу в аргументе `file`; путь проверяется [правилами доступа к файлам](security.md#локальные-файлы).
- **Фильтр ответа.** У каждого инструмента операции есть необязательный аргумент `jq` — см. [Фильтр ответа jq](#фильтр-ответа-jq).

## Уровни доступа

У каждого инструмента есть уровень доступа. По умолчанию он определяется HTTP-методом: `GET` — чтение, `POST`, `PUT`, `PATCH` — запись, `DELETE` — удаление. Исключения, где метод не отражает последствия:

| Инструменты | Уровень | Почему |
| --- | --- | --- |
| `secret_set` (операция `set_secret`) | удаление | Перезаписывает секрет без возможности вернуть прежнее значение; секреты использует CI. |
| `secret_copy` | запись; перезапись существующего секрета — удаление | Новый ключ ничего не затирает; перезапись необратима, как у `secret_set`. Проверяется при вызове. |
| `add_organization_roles`, `remove_organization_roles`, `add_repo_roles`, `remove_repo_roles` | удаление | Меняют права доступа людей. |
| `create_organization_invites` | удаление | Дают внешним пользователям доступ в организацию и отправляют им письма. |
| `set_code_assist_workplaces`, `occupy_code_assist_workplaces_bulk`, `release_code_assist_workplaces_bulk`, `transfer_code_assist_workplace` | удаление | Платные лицензии: меняют счёт и отбирают доступ у пользователей. |
| `remove_issue_labels`, `remove_pull_request_labels`, `remove_linked_prs`, `remove_issue_comment_reaction` | запись | Удаляют только связь, которую можно сразу вернуть. |
| `generate_release_notes` | чтение | Только формирует предпросмотр, ничего не создаёт. |

Кроме того, `update_repository` с `visibility: public` в теле требует уровня удаления: публикация репозитория необратима по последствиям.

Сервер регистрирует только инструменты в пределах разрешённого уровня. Уровень stdio-сервера — настройка профиля (`allow_write`, `allow_destructive`): его меняют `sourcecraft-mcp ui` или `connect --allow-write`/`--allow-destructive`, на macOS — с одобрением через Touch ID или пароль ([Безопасность](security.md#уровни-доступа)). Уровень HTTP-сервера задают флаги `serve --http --allow-write`/`--allow-destructive`. Остальные инструменты не видны в `tools/list`; если агент всё же вызовет такой инструмент, он получит ошибку с подсказкой, как поднять уровень: для stdio — права профиля в `sourcecraft-mcp ui` и перезапуск клиента, для HTTP — флаг. Аннотации MCP `readOnlyHint` и `destructiveHint` соответствуют уровню.

## Наборы инструментов

Набор (toolset) — тег операции в спецификации, записанный в виде slug: нижний регистр, всё кроме `a-z0-9` заменено на `-`. Например, `Repository | PullRequest` → `repository-pullrequest`, `CI/CD` → `ci-cd`. Набор выбирается точным совпадением: `repository` не включает `repository-pullrequest`.

```bash
sourcecraft-mcp toolsets
sourcecraft-mcp serve --toolsets repository,repository-pullrequest,issues
```

Наборы сокращают список инструментов, который видит агент, но не повышают уровень доступа: инструменты выше разрешённого уровня скрыты в любом наборе. `api_search` и `api_request` регистрируются всегда и наборами не ограничиваются; `secret_set` и `secret_copy` входят в набор `secrets`.

### Динамические наборы

Клиенты без отложенной загрузки инструментов кладут описания всех ~110 инструментов в контекст модели сразу. С флагом `serve --dynamic-toolsets` (или `connect --dynamic-toolsets`) сервер при запуске регистрирует только:

- `api_search` и `api_request` — как обычно;
- [пользовательские инструменты](#пользовательские-инструменты) из `tools.toml` — их оператор настроил явно, поэтому они регистрируются сразу и не входят ни в один набор;
- `list_toolsets` — наборы с тегом API, числом инструментов, доступных на текущем уровне, примерами имён, числом операций выше уровня доступа (`above_access_level`) и признаком `enabled`;
- `enable_toolset` — включить набор (аргумент `toolset`, можно несколько через запятую). Ответ перечисляет добавленные инструменты;
- `disable_toolset` — выключить набор, чтобы снова сократить список.

`--toolsets` вместе с `--dynamic-toolsets` задаёт наборы, включённые при запуске; остальные агент включает сам. После `enable_toolset` и `disable_toolset` сервер отправляет клиенту `notifications/tools/list_changed` (в инициализации объявлена возможность `tools.listChanged`): в сессии с `initialize` — сразу после вызова, в протоколе MCP 2026-07-28 — по подписке `subscriptions/listen`. Клиент перечитывает `tools/list` и видит новые инструменты.

Уровень доступа действует как обычно: `enable_toolset` не регистрирует инструменты выше разрешённого уровня, `secret_set` появляется только на уровне удаления и с набором `secrets`, `secret_copy` — на уровне записи и с набором `secrets`. Включённые инструменты операций — те же, что без `--dynamic-toolsets`, и тоже принимают [фильтр `jq`](#фильтр-ответа-jq). Сами инструменты управления наборами — уровня чтения: они меняют только список инструментов сервера, а не данные SourceCraft. Если клиент не перечитывает список инструментов, агент может вызвать ту же операцию через `api_request` (метод и путь подскажет `api_search`). Какие клиенты поддерживают обновление списка — в [Подключении к MCP-клиентам](clients.md#динамические-наборы-инструментов).

Динамические наборы работают только в stdio: HTTP-сервер ([HTTP-режим](http.md)) не хранит сессий, поэтому `--dynamic-toolsets` вместе с `--http` — ошибка запуска.

Состояние наборов — у сессии: при перезапуске сервера включены снова только наборы из `--toolsets`.

Инструменты по наборам (версия спецификации из этого релиза; актуальный список — `sourcecraft-mcp toolsets` и `api_search`):

| Набор | Чтение | Запись | Удаление |
| --- | --- | --- | --- |
| `ci-cd` | `get_cube_artifacts`, `get_cube_logs`, `get_run`, `get_workflow`, `list_runs` | `run_workflows` | — |
| `codeassistworkplaces` | `get_code_assist_workplaces`, `list_occupied_code_assist_workplaces` | — | `occupy_code_assist_workplaces_bulk`, `release_code_assist_workplaces_bulk`, `set_code_assist_workplaces`, `transfer_code_assist_workplace` |
| `issues` | `get_issue`, `list_issues_assigned_to_authenticated_user`, `list_repository_issues` | `create_issue`, `update_issue` | `delete_issue` |
| `issues-attachments` | `get_issue_attachment`, `list_issue_attachments` | `upload_issue_attachment` | `delete_issue_attachment` |
| `issues-comments` | `get_issue_comment`, `list_issue_comments` | `create_issue_comment`, `update_issue_comment` | `delete_issue_comment` |
| `issues-comments-attachments` | `get_issue_comment_attachment`, `list_issue_comment_attachments` | `upload_issue_comment_attachment` | `delete_issue_comment_attachment` |
| `issues-comments-reactions` | — | `add_issue_comment_reaction`, `remove_issue_comment_reaction` | — |
| `issues-labels` | `get_issue_labels` | `add_issue_labels`, `remove_issue_labels`, `replace_issue_labels` | — |
| `issues-linked-issues` | `list_issue_links` | `create_issue_link` | `delete_issue_link` |
| `issues-linked-prs` | `get_linked_prs` | `add_linked_prs`, `remove_linked_prs` | — |
| `issues-statuses` | `list_issue_status` | — | — |
| `organization` | `get_organization`, `get_organization_neurocredits_usage`, `list_organization_repositories` | — | — |
| `organization-invites` | `get_organization_invite`, `list_organization_invites` | — | `create_organization_invites` |
| `organization-operation` | `get_organization_invites_operation` | — | — |
| `organization-roles` | `list_organization_roles` | — | `add_organization_roles`, `remove_organization_roles` |
| `pullrequest-labels` | `get_pull_request_labels` | `add_pull_request_labels`, `remove_pull_request_labels`, `replace_pull_request_labels` | — |
| `quota` | `list_my_personal_quotas`, `list_quotas` | — | — |
| `release-attachments` | — | `add_release_asset_attachment` | — |
| `repository` | `get_repository`, `list_branches`, `list_tags`, `list_tree` | `create_repository`, `fork_repository`, `update_repository` | `delete_repository` |
| `repository-labels` | `get_label`, `list_labels_for_repository` | `create_label`, `update_label` | `delete_label` |
| `repository-milestones` | `get_milestone`, `list_milestones_for_repository` | `create_milestone`, `update_milestone` | `delete_milestone` |
| `repository-pullrequest` | `get_merge_checks`, `get_pull_request`, `list_pull_request_files`, `list_repository_pull_requests` | `create_pull_request`, `discard_pull_request`, `draft_pull_request`, `merge_pull_request`, `publish_pull_request`, `reopen_pull_request`, `update_pull_request` | — |
| `repository-pullrequest-comments` | `get_pull_request_comment`, `list_pull_request_comments` | `create_pull_request_comment`, `publish_pull_request_comments_drafts`, `update_pull_request_comment` | `delete_pull_request_comment` |
| `repository-pullrequest-operation` | `get_pull_request_merge_operation` | — | — |
| `repository-pullrequest-reviewers` | `list_reviewers` | `auto_assign_reviewers`, `set_review_decision`, `update_reviewers` | — |
| `repository-releases` | `generate_release_notes`, `get_latest_release`, `get_release`, `list_releases` | `create_release`, `discard_release`, `publish_release`, `update_release` | `delete_release` |
| `repository-roles` | `list_repo_roles` | — | `add_repo_roles`, `remove_repo_roles` |
| `secrets` | `list_secrets` | `secret_copy` | `delete_secret`, `secret_set` |
| `secrets-operation` | `get_secret_operation` | — | — |
| `user` | `get_my_code_assist_billing_org`, `get_my_profile`, `get_profile`, `get_profile_by_cloud_id` | — | — |
| `user-pullrequest` | `list_my_pull_requests`, `list_user_pull_requests` | — | — |

## api_search

Ищет по всем операциям API — в том числе по тем, чьи инструменты на этом сервере не зарегистрированы. Работает локально по таблице операций и в API не обращается; уровень — чтение.

| Аргумент | Назначение |
| --- | --- |
| `query` | Слова для поиска в имени, `operationId`, summary, description, пути и теге, например `pull request merge`. |
| `tool_name` | Точное имя инструмента: вернуть полную схему аргументов одной операции. Нельзя вместе с `query`. |
| `toolset` | Искать только в одном наборе; без `query` — перечислить все операции набора. |
| `limit` | Число результатов, по умолчанию 20, не больше 100. |

Каждый результат содержит имя инструмента, метод и путь (и путь по ID, если он есть), уровень доступа, набор, обязательные аргументы и признак, зарегистрирован ли инструмент на сервере. Для незарегистрированного указано, почему, в поле `enable_with`: какой уровень доступа (для stdio — права профиля в `sourcecraft-mcp ui`, для HTTP — флаг `--allow-write` или `--allow-destructive`) или набор его включает; с `--dynamic-toolsets` вместо `--toolsets` подсказывается вызов `enable_toolset`. Так агент не ищет обходных путей, а просит пользователя поднять права профиля (или перезапустить HTTP-сервер с нужным флагом).

## api_request

Запрос к произвольному пути SourceCraft API — для операций, у которых нет отдельного инструмента или чей набор не включён. Если отдельный инструмент есть, агенту лучше вызывать его: он проверяет аргументы и описывает схему.

| Аргумент | Назначение |
| --- | --- |
| `method` | `GET`, `POST`, `PUT`, `PATCH` или `DELETE`. |
| `path` | Путь относительно адреса API, начинается с `/`: `/repos/acme/web/pulls`, `/repos/id:<repo_id>/pulls`, `/user`. |
| `query` | Параметры query объектом; значения — строки, числа, логические значения или массивы (массив повторяет ключ). |
| `body` | JSON-тело запроса. Нельзя вместе с `body_file`; у `GET` тела быть не может. |
| `body_file` | Путь к локальному файлу, содержимое которого отправляется телом. |
| `content_type` | Тип содержимого для `body_file`; по умолчанию определяется по расширению. |
| `save_to` | Сохранить успешный ответ в новый локальный файл (абсолютный путь) вместо возврата агенту. Существующие файлы не перезаписываются. |
| `jq` | Фильтр ответа, см. [Фильтр ответа jq](#фильтр-ответа-jq). |

Ограничения:

- **Только относительные пути API.** Полные URL, `//`, `..`, `.`, пустые сегменты, `?` и `#` в `path` отклоняются; query передаётся отдельно. Запрос уходит только на адрес API, с которым запущен сервер, — это не HTTP-прокси.
- **Заголовки задать нельзя.** Авторизацию добавляет сервер; заголовок `Authorization` не переопределяется.
- **Уровень доступа.** Если путь и метод совпадают с известной операцией, нужен её уровень (с учётом исключений выше); если совпадает несколько — максимальный из них; если ни одна — уровень по методу. Запрос выше уровня сервера отклоняется с подсказкой, как поднять уровень. Наборы `api_request` не ограничивают.
- **Секреты.** Записать секрет через `api_request` нельзя (API ждёт значение в base64, используйте `secret_set`). На эндпоинтах секретов не принимаются `save_to` и `body_file`. Значения секретов в ответах вырезаются так же, как у других инструментов, и раскрыть их через `api_request` нельзя.
- `update_repository` с `visibility: public` требует уровня удаления и через `api_request`; при уровне записи изменения настроек репозитория принимаются только в `body`, не в `body_file`.
- Локальные файлы `body_file` и `save_to` проверяются [правилами доступа к файлам](security.md#локальные-файлы). В [HTTP-режиме](http.md#отличия-от-stdio) аргументов `body_file`, `content_type` и `save_to` нет.

## secret_set

Создаёт или обновляет секрет репозитория. Заменяет сгенерированный `set_secret`, который ждёт значение уже в base64: агенту легко ошибиться и молча записать неверное значение. Регистрируется на уровне удаления и при включённом наборе `secrets`.

| Аргумент | Назначение |
| --- | --- |
| `org_slug` + `repo_slug` или `repo_id` | Репозиторий. |
| `key` | Ключ секрета (обязателен). |
| `secret_group` | Группа секретов. |
| `value` | Значение открытым текстом; сервер сам кодирует его в base64. |
| `value_file` | Путь к локальному файлу со значением (любые байты, до 64 KiB). Значение читает сервер, в контекст агента оно не попадает. |
| `trim_trailing_newline` | Убрать один завершающий перевод строки (`\n` или `\r\n`). По умолчанию включён для `value_file` и выключен для `value`. |

В [HTTP-режиме](http.md#отличия-от-stdio) `value_file` нет, и значение передаётся только в `value`. Нужен ровно один из `value` и `value_file`; пустое значение отклоняется (для удаления есть `delete_secret`). Секрет записывается асинхронно: ответ — операция, её статус проверяет `get_secret_operation`. Значение и его base64 вычищаются из ответа и текста ошибок.

## secret_copy

Копирует секрет так, что значение не проходит через контекст агента: сервер сам читает его из источника и записывает в назначение. Регистрируется на уровне записи и при включённом наборе `secrets` (с `--dynamic-toolsets` — после `enable_toolset` с набором `secrets`).

| Аргумент | Назначение |
| --- | --- |
| `source` | Что копировать: `sc://<org>/<repo>/<KEY>`, для группы секретов — `sc://<org>/<repo>/<KEY>?group=<имя>`. С `serve --allow-keychain-source` — ещё и запись системного хранилища этой машины `keychain://<service>/<account>`. |
| `destination` | Куда: секрет репозитория `sc://<org>/<repo>/<KEY>[?group=<имя>]`. Ключ может отличаться от исходного. |

- **Уровень доступа.** Новый ключ создаётся на уровне записи. Если секрет назначения уже есть, нужна перезапись — уровень удаления; без него вызов завершается ошибкой, и ничего не записывается. Наличие секрета проверяется чтением списка секретов назначения перед записью; проверка и запись не атомарны.
- **Результат.** Только ссылки, `result` (`created` или `overwritten`) и состояние операции записи (`operation_id`, `status`). Сервер ждёт завершения записи до 10 секунд; если она ещё идёт, в ответе есть подсказка проверить `get_secret_operation`.
- **Значение** не попадает ни в результат, ни в ошибки (из них вычищаются значение и его base64), ни в логи: в лог (`info`) пишутся только ссылки и результат. Поэтому `--allow-secret-values` для копирования не нужен — подробнее в [Безопасности](security.md#копирование-секретов).
- **`keychain://`** в MCP-инструменте выключен по умолчанию и как назначение недоступен вовсе. Записи с токенами `sourcecraft-mcp` не копируются никогда. В [HTTP-режиме](http.md) `keychain://` недоступен совсем: `--allow-keychain-source` с `--http` — ошибка запуска.
- Пустой секрет и копирование секрета в самого себя отклоняются.
- Аргумента [`jq`](#фильтр-ответа-jq) нет: результат — короткий отчёт, фильтровать в нём нечего.

Тот же сценарий из оболочки — [`sourcecraft-mcp secret copy`](cli.md#secret-copy).

## Фильтр ответа jq

Ответы API бывают большими: список пул-реквестов с описаниями быстро занимает контекст агента. Инструменты операций, `api_request` и [пользовательские инструменты](#пользовательские-инструменты) принимают необязательный аргумент `jq` — фильтр на языке [jq](https://jqlang.org/manual/) (диалект [jaq](https://github.com/01mf02/jaq)), который применяется к JSON-ответу до возврата агенту:

```json
{
  "org_slug": "acme",
  "repo_slug": "web",
  "jq": ".pull_requests[] | {id, title}"
}
```

- **Порядок обработки:** вырезание секретов → фильтр → лимит размера. Фильтр получает уже очищенный ответ, поэтому никакой фильтр (например, `.. | strings`) не раскрывает значения секретов и учётных данных. Лимит `--max-response-bytes` применяется к результату фильтра; исходный ответ для фильтра читается целиком, до 16 MiB.
- **Результат:** ровно один результат фильтра возвращается как есть; несколько — JSON-массивом (`.pull_requests[] | {id, title}` → `[{…}, {…}]`); ни одного — `[]`.
- **Ошибки.** Синтаксис и имена функций проверяются до запроса к API: неверный фильтр — ошибка `isError: true`, а запрос не отправляется. Если фильтр упал при выполнении, ошибка сообщает, что сам запрос к API выполнен: запрос, изменяющий данные, повторять не нужно.
- **Ответ не в JSON** (текст, пустое тело, `save_to`) возвращается без фильтра, с пояснением.
- **Ограничения фильтра.** Длина — до 4096 байт. Недоступны окружение процесса (`env`, `$ENV`), входные потоки (`input`, `inputs`) и модули (`import`, `include`); `debug` и `stderr` ничего не выводят, `halt` — ошибка. Фильтр выполняется в отдельном процессе с пустым окружением и прерывается через 10 секунд: зацикленный или бесконечно рекурсивный фильтр возвращает ошибку и не влияет на сервер.

## Пользовательские инструменты

Частые запросы можно оформить именованными инструментами, чтобы агент не собирал их каждый раз заново: например, «мои открытые пул-реквесты в репозитории». Инструменты описываются в TOML:

- `~/.config/sourcecraft-mcp/tools.toml` (каталог можно переопределить переменной `SOURCECRAFT_MCP_CONFIG_DIR`) — читается всегда, если файл есть;
- другие файлы — только явно, флагом `serve --tools-file <путь>` (можно повторять). Файл из репозитория проекта — недоверенный ввод, поэтому клонирование репозитория само по себе инструментов не добавляет.

В [HTTP-режиме](http.md) файлы читаются так же — на машине сервера при запуске; инструмент вызывается с токеном агента из запроса.

```toml
[[tools]]
name = "open_prs"
description = "Open pull requests of a repository: id and title only"
method = "GET"
path = "/repos/{org}/{repo}/pulls"
query = { state = "open", page_size = "{limit}" }
jq = ".pull_requests[] | {id, title}"

[[tools.arguments]]
name = "org"
description = "Organization slug"
required = true

[[tools.arguments]]
name = "repo"
description = "Repository slug"
required = true

[[tools.arguments]]
name = "limit"
type = "integer"
description = "How many pull requests to return"

[[tools]]
name = "comment_issue"
description = "Add a comment to an issue"
method = "POST"
path = "/repos/{org}/{repo}/issues/{issue}/comments"
body = { text = "{text}" }
arguments = [
  { name = "org", required = true },
  { name = "repo", required = true },
  { name = "issue", required = true },
  { name = "text", required = true, description = "Comment text" },
]
```

| Поле | Назначение |
| --- | --- |
| `name` | Имя инструмента: строчные латинские буквы, цифры и `_`, начинается с буквы, до 64 символов. |
| `description` | Описание для агента (обязательно). |
| `method` | `GET`, `POST`, `PUT`, `PATCH` или `DELETE`. |
| `path` | Шаблон пути относительно адреса API; `{имя}` — подстановка аргумента. |
| `query` | Параметры query: литералы или строки `"{имя}"`; значения — скаляры или массивы скаляров. |
| `body` | Шаблон JSON-тела (TOML-таблица); у `GET` тела нет. |
| `jq` | [Фильтр ответа](#фильтр-ответа-jq), который применяется всегда. |
| `arguments` | Аргументы: `name`, `type` (`string` по умолчанию, `integer`, `number`, `boolean`), `description`, `required` (по умолчанию `false`). |

Подстановка аргументов:

- **В путь** значение подставляется как один сегмент с percent-кодированием: `/`, `?`, `#` и `%` внутри значения не меняют путь, а значения `.`, `..` и содержащие `/../` отклоняются. Аргументы пути должны быть обязательными.
- **В `query` и `body`** подставляется строка, целиком равная `{имя}`, — JSON-значением аргумента (число остаётся числом). Склейки строк нет, поэтому тело всегда остаётся валидным JSON, а кавычки в значении не меняют его структуру. Если необязательный аргумент не передан, ключ (или элемент массива) убирается.
- Каждый объявленный аргумент должен где-то использоваться, а каждая подстановка — ссылаться на объявленный аргумент.
- Числа и логические значения можно передавать строками (`"5"`, `"true"`).

Кроме своих аргументов, пользовательский инструмент принимает `jq`: фильтр из вызова применяется после фильтра из файла.

Ограничения — те же, что у [`api_request`](#api_request): только относительные пути API, заголовки задать нельзя, правила секретов и проверка видимости `update_repository` действуют так же. Уровень доступа инструмента вычисляется по методу и шаблону пути (по совпавшей операции API, иначе по методу); инструменты выше уровня сервера не регистрируются, а вызов такого инструмента объясняет, как поднять уровень. При вызове уровень проверяется ещё раз по фактическому пути: если аргументы приводят к операции с более высоким уровнем (например, `{section}` = `roles`), вызов отклоняется. Наборы инструментов (`--toolsets`) пользовательские инструменты не ограничивают; с `--dynamic-toolsets` они регистрируются сразу при запуске, без `enable_toolset`.

Ошибки в файле останавливают запуск сервера с понятным сообщением (файл и инструмент): неверный TOML, неизвестные поля, неверный путь или фильтр, необъявленный аргумент. Имя, совпадающее со встроенным инструментом (с любой операцией API, `api_request`, `api_search`, `secret_set`, `secret_copy`, `list_toolsets`, `enable_toolset`, `disable_toolset`) или с другим пользовательским инструментом, — тоже ошибка запуска, а не тихий пропуск. Проверить файлы заранее можно командой [`sourcecraft-mcp tools check`](cli.md#tools-check).

## Ответы и ошибки

- Успешный ответ — JSON тела ответа API (компактный), текст как есть или `{"status": …}` для пустого тела; при `save_to` — `{"saved_to": …, "bytes": …}`.
- Ответ больше `--max-response-bytes` (256 KiB по умолчанию) обрезается, и к нему добавляется пояснение: запросите меньше данных (`page_size`, фильтры API, [фильтр `jq`](#фильтр-ответа-jq)).
- Если из ответа вырезаны значения секретов или учётных данных, к нему добавляется пояснение (см. [Безопасность](security.md#вырезание-значений-из-ответов)).
- Ошибка API, неверные аргументы или запрещённый вызов возвращаются как результат инструмента с `isError: true` и понятным текстом; сервер при этом продолжает работать. Ошибкой протокола MCP отвечает только вызов неизвестного инструмента.
