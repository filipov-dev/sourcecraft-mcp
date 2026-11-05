//! Скрытая подкоманда `sourcecraft-mcp jq-worker`: выполняет один фильтр `jq` для `serve`.
//!
//! Сервер запускает её дочерним процессом с пустым окружением (см.
//! `sourcecraft_mcp_core::jq`): бесконечная рекурсия или зацикливание фильтра завершают
//! только этот процесс. Запрос — JSON в stdin, ответ — JSON в stdout. Токен, keychain и
//! сеть здесь не используются.

use anyhow::Context;

/// Имя подкоманды; `serve` передаёт его в `JqRunner::subprocess`.
pub const SUBCOMMAND: &str = "jq-worker";

pub fn run() -> anyhow::Result<()> {
    sourcecraft_mcp_core::jq::run_worker(std::io::stdin().lock(), std::io::stdout().lock())
        .context("jq worker failed")
}
