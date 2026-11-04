# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/sourcecraft-api` | Спецификация SourceCraft, генератор таблицы операций (`build.rs`), исполнитель HTTP-запросов, хранилище токенов. |
| `crates/sourcecraft-mcp-core` | MCP-инструменты: реестр, уровни доступа, наборы, `api_request`, `api_search`, `secret_set`, вырезание секретов, политика файлов. Не зависит от транспорта. |
| `crates/sourcecraft-mcp` | Бинарник: CLI (`serve`, `connect`, `auth`, `secret`, `toolsets`) и транспорт stdio. |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`. В Linux нужны `pkg-config` и `libdbus-1-dev`.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Тесты не обращаются к реальному SourceCraft API (HTTP подменяется `wiremock`) и не используют системное хранилище токенов.

Запустить CLI из исходников:

```bash
cargo run -p sourcecraft-mcp -- --help
cargo run -p sourcecraft-mcp -- toolsets
```

## Обновление спецификации API

Инструменты операций генерируются из `crates/sourcecraft-api/spec/sourcecraft.swagger.json`. Чтобы обновить их:

1. Замените файл актуальной спецификацией: <https://api.sourcecraft.tech/docs/sourcecraft.swagger.json>.
2. Соберите проект. Генератор падает, если в спецификации пропала операция из списка исключений уровня доступа или переименований — обновите эти списки в `crates/sourcecraft-api/build/`.
3. Обновите снапшоты тестов операций, если они изменились, и проверьте, что новые операции получили правильный уровень доступа.
4. Обновите таблицу инструментов в [tools.md](tools.md#наборы-инструментов).

Сгенерированный код не правится вручную: единственный источник операций — спецификация.
