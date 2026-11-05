# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/sourcecraft-api` | Спецификация SourceCraft, генератор таблицы операций (`build.rs`), исполнитель HTTP-запросов, хранилище токенов. |
| `crates/sourcecraft-mcp-core` | MCP-инструменты: реестр, уровни доступа, наборы, `api_request`, `api_search`, `secret_set`, вырезание секретов, политика файлов. Не зависит от транспорта. |
| `crates/sourcecraft-mcp` | Бинарник: CLI (`serve`, `connect`, `auth`, `secret`, `toolsets`, `tools`) и транспорты stdio и HTTP (`src/cli/serve/http_server.rs`). |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`. Для крейта `aws-lc-sys` (криптография TLS) нужен C-компилятор; системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Тесты не обращаются к реальному SourceCraft API (HTTP подменяется `wiremock`) и не используют системное хранилище токенов.

### Windows

Сборка — нативная, целевая платформа `x86_64-pc-windows-msvc`: нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» и `rustup`. Релизный бинарник собирается со статическим CRT, чтобы на машине пользователя не требовался Visual C++ Redistributable:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p sourcecraft-mcp
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

Ветки кода для Windows (хранилище токенов в Credential Manager, запуск `claude.cmd`, права файлов в `secret write`) компилируются или выполняются только там, поэтому проверяйте изменения в них этими командами. С macOS и Linux `cargo check --target x86_64-pc-windows-msvc` без Windows SDK не проходит: C-код `aws-lc-sys` требует заголовков Windows.

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
