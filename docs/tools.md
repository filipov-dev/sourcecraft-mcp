# MCP-инструменты

Сервер предоставляет три вида инструментов:

- **инструменты операций** — по одному на каждую операцию SourceCraft REST API, генерируются из спецификации;
- **служебные**: `api_search` (поиск операций) и `api_request` (запрос к любому пути API);
- **`secret_set`** — запись секрета из открытого текста или файла вместо сырой операции API.

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

## Уровни доступа

У каждого инструмента есть уровень доступа. По умолчанию он определяется HTTP-методом: `GET` — чтение, `POST`, `PUT`, `PATCH` — запись, `DELETE` — удаление. Исключения, где метод не отражает последствия:

| Инструменты | Уровень | Почему |
| --- | --- | --- |
| `secret_set` (операция `set_secret`) | удаление | Перезаписывает секрет без возможности вернуть прежнее значение; секреты использует CI. |
| `add_organization_roles`, `remove_organization_roles`, `add_repo_roles`, `remove_repo_roles` | удаление | Меняют права доступа людей. |
| `create_organization_invites` | удаление | Дают внешним пользователям доступ в организацию и отправляют им письма. |
| `set_code_assist_workplaces`, `occupy_code_assist_workplaces_bulk`, `release_code_assist_workplaces_bulk`, `transfer_code_assist_workplace` | удаление | Платные лицензии: меняют счёт и отбирают доступ у пользователей. |
| `remove_issue_labels`, `remove_pull_request_labels`, `remove_linked_prs`, `remove_issue_comment_reaction` | запись | Удаляют только связь, которую можно сразу вернуть. |
| `generate_release_notes` | чтение | Только формирует предпросмотр, ничего не создаёт. |

Кроме того, `update_repository` с `visibility: public` в теле требует уровня удаления: публикация репозитория необратима по последствиям.

Сервер регистрирует только инструменты в пределах разрешённого уровня (`--allow-write`, `--allow-destructive`). Остальные не видны в `tools/list`; если агент всё же вызовет такой инструмент, он получит ошибку с объяснением, какой флаг его включает. Аннотации MCP `readOnlyHint` и `destructiveHint` соответствуют уровню.

## Наборы инструментов

Набор (toolset) — тег операции в спецификации, записанный в виде slug: нижний регистр, всё кроме `a-z0-9` заменено на `-`. Например, `Repository | PullRequest` → `repository-pullrequest`, `CI/CD` → `ci-cd`. Набор выбирается точным совпадением: `repository` не включает `repository-pullrequest`.

```bash
sourcecraft-mcp toolsets
sourcecraft-mcp serve --toolsets repository,repository-pullrequest,issues
```

Наборы сокращают список инструментов, который видит агент, но не повышают уровень доступа: инструменты выше разрешённого уровня скрыты в любом наборе. `api_search` и `api_request` регистрируются всегда и наборами не ограничиваются; `secret_set` входит в набор `secrets`.

Инструменты по наборам (версия спецификации из этого релиза; актуальный список — `sourcecraft-mcp toolsets` и `api_search`):

| Набор | Чтение | Запись (`--allow-write`) | Удаление (`--allow-destructive`) |
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
| `secrets` | `list_secrets` | — | `delete_secret`, `secret_set` |
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

Каждый результат содержит имя инструмента, метод и путь (и путь по ID, если он есть), уровень доступа, набор, обязательные аргументы и признак, зарегистрирован ли инструмент на сервере. Для незарегистрированного указано, почему: какой флаг (`--allow-write`, `--allow-destructive`) или набор его включает. Так агент не ищет обходных путей, а просит пользователя перезапустить сервер с нужным флагом.

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

Ограничения:

- **Только относительные пути API.** Полные URL, `//`, `..`, `.`, пустые сегменты, `?` и `#` в `path` отклоняются; query передаётся отдельно. Запрос уходит только на адрес API, с которым запущен сервер, — это не HTTP-прокси.
- **Заголовки задать нельзя.** Авторизацию добавляет сервер; заголовок `Authorization` не переопределяется.
- **Уровень доступа.** Если путь и метод совпадают с известной операцией, нужен её уровень (с учётом исключений выше); если совпадает несколько — максимальный из них; если ни одна — уровень по методу. Запрос выше уровня сервера отклоняется с подсказкой о нужном флаге. Наборы `api_request` не ограничивают.
- **Секреты.** Записать секрет через `api_request` нельзя (API ждёт значение в base64, используйте `secret_set`). На эндпоинтах секретов не принимаются `save_to` и `body_file`. Значения секретов в ответах вырезаются так же, как у других инструментов, и раскрыть их через `api_request` нельзя.
- `update_repository` с `visibility: public` требует уровня удаления и через `api_request`; при уровне записи изменения настроек репозитория принимаются только в `body`, не в `body_file`.
- Локальные файлы `body_file` и `save_to` проверяются [правилами доступа к файлам](security.md#локальные-файлы).

## secret_set

Создаёт или обновляет секрет репозитория. Заменяет сгенерированный `set_secret`, который ждёт значение уже в base64: агенту легко ошибиться и молча записать неверное значение. Регистрируется при `--allow-destructive` и включённом наборе `secrets`.

| Аргумент | Назначение |
| --- | --- |
| `org_slug` + `repo_slug` или `repo_id` | Репозиторий. |
| `key` | Ключ секрета (обязателен). |
| `secret_group` | Группа секретов. |
| `value` | Значение открытым текстом; сервер сам кодирует его в base64. |
| `value_file` | Путь к локальному файлу со значением (любые байты, до 64 KiB). Значение читает сервер, в контекст агента оно не попадает. |
| `trim_trailing_newline` | Убрать один завершающий перевод строки (`\n` или `\r\n`). По умолчанию включён для `value_file` и выключен для `value`. |

Нужен ровно один из `value` и `value_file`; пустое значение отклоняется (для удаления есть `delete_secret`). Секрет записывается асинхронно: ответ — операция, её статус проверяет `get_secret_operation`. Значение и его base64 вычищаются из ответа и текста ошибок.

## Ответы и ошибки

- Успешный ответ — JSON тела ответа API (компактный), текст как есть или `{"status": …}` для пустого тела; при `save_to` — `{"saved_to": …, "bytes": …}`.
- Ответ больше `--max-response-bytes` (256 KiB по умолчанию) обрезается, и к нему добавляется пояснение: запросите меньше данных (`page_size`, фильтры).
- Если из ответа вырезаны значения секретов или учётных данных, к нему добавляется пояснение (см. [Безопасность](security.md#вырезание-значений-из-ответов)).
- Ошибка API, неверные аргументы или запрещённый вызов возвращаются как результат инструмента с `isError: true` и понятным текстом; сервер при этом продолжает работать. Ошибкой протокола MCP отвечает только вызов неизвестного инструмента.
