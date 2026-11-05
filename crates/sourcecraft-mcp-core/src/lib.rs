//! MCP-инструменты SourceCraft API без привязки к транспорту.
//!
//! - [`api_request`], [`api_search`] — инструменты произвольного запроса и поиска операций;
//! - [`custom_tools`] — пользовательские инструменты из `tools.toml`;
//! - [`routes`] — сопоставление произвольного запроса с операциями и уровень доступа;
//! - [`server`] — обработчик `rmcp` ([`SourceCraftServer`]) и инструкции для агента;
//! - [`registry`] — какие инструменты регистрируются при заданных [`ServerOptions`];
//! - [`request`] — сборка запроса к API из аргументов инструмента;
//! - [`response`] — представление ответа API в результате инструмента;
//! - [`redact`] — вырезание значений секретов и учётных данных из ответов;
//! - [`secrets`] — `secret_set` и раскрытие значений секретов (`reveal_values`);
//! - [`repo_secrets`] — чтение и запись значений секретов через API (общее с CLI);
//! - [`secret_copy`] — `secret_copy`: копирование секрета без раскрытия значения;
//! - [`files`] — политика доступа к локальным файлам;
//! - [`jq`] — фильтр ответа в стиле jq (аргумент `jq`);
//! - [`toolsets`] — наборы инструментов по тегам спецификации;
//! - [`dynamic_toolsets`] — `list_toolsets`, `enable_toolset`, `disable_toolset` (режим
//!   `--dynamic-toolsets`).
//!
//! Транспорт (stdio, HTTP) подключается в бинарнике `sourcecraft-mcp`. Ядро о транспорте
//! знает две вещи, обе в настройках: работает ли сервер на машине агента
//! ([`ServerOptions::local_machine`]) и откуда брать токен ([`TokenSource`]).

pub mod api_request;
pub mod api_search;
pub mod custom_tools;
pub mod dynamic_toolsets;
pub mod error;
pub mod files;
pub mod jq;
pub mod redact;
pub mod registry;
pub mod repo_secrets;
pub mod request;
pub mod response;
pub mod routes;
pub mod secret_copy;
pub mod secrets;
pub mod server;
pub mod toolsets;

pub use custom_tools::{CustomTool, CustomToolDef, CustomToolsFile};
pub use error::ToolError;
pub use files::{FilePolicy, FilePolicyError, LOCAL_FILES_DISABLED};
pub use jq::JqRunner;
pub use registry::{RegisteredTool, ToolKind, ToolRegistry};
pub use server::{RequestTokenFn, SourceCraftServer, TokenSource};
pub use toolsets::{Toolset, available_toolsets, normalize_toolsets, toolset_slug};

use std::sync::Arc;

use sourcecraft_api::AccessLevel;
use sourcecraft_api::keychain::Keychain;

/// Настройки MCP-сервера.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Максимальный разрешённый уровень доступа. Инструменты выше него не регистрируются.
    pub max_access: AccessLevel,
    /// Наборы инструментов (slug тегов спецификации, см. [`toolsets`]).
    /// `None` или пустой список — все наборы. При `dynamic_toolsets` — наборы, включённые
    /// при запуске (`None` — ни одного).
    pub toolsets: Option<Vec<String>>,
    /// Динамические наборы: при запуске регистрируются только `api_search`, `api_request` и
    /// инструменты управления наборами, остальные агент включает `enable_toolset`
    /// (см. [`dynamic_toolsets`]). Уровень доступа действует как обычно.
    pub dynamic_toolsets: bool,
    /// Разрешить раскрывать значения секретов по явному запросу (`reveal_values: true`).
    /// По умолчанию выключено: значения вырезаются из всех ответов (см. [`redact`]).
    pub allow_secret_values: bool,
    /// Профиль токена, с которым запущен сервер. Подставляется в примеры команд
    /// `sourcecraft-mcp secret …` в инструкциях, чтобы агент использовал тот же профиль.
    pub profile: Option<String>,
    /// Путь к бинарнику `sourcecraft-mcp` для примеров команд в инструкциях; `None` —
    /// просто `sourcecraft-mcp` (из `PATH`).
    pub executable: Option<String>,
    /// Как выполняются фильтры `jq` (см. [`jq`]). По умолчанию — в потоке этого процесса;
    /// `serve` выполняет их в отдельном процессе.
    pub jq: JqRunner,
    /// Пользовательские инструменты (см. [`custom_tools`]). Регистрируются те, чей уровень
    /// доступа не выше `max_access`; наборы инструментов их не ограничивают, и при
    /// `dynamic_toolsets` они регистрируются сразу, без `enable_toolset`.
    pub custom_tools: Vec<CustomTool>,
    /// Сервер работает на машине агента (stdio): инструменты читают и пишут локальные файлы
    /// (вложения, `save_to`, `value_file`), инструкции описывают CLI `sourcecraft-mcp secret`.
    ///
    /// `false` — сервер удалённый (HTTP-режим): файлы сервера не имеют отношения к агенту,
    /// поэтому инструменты загрузки файлов не регистрируются, аргументы с путями убираются
    /// из схем и отклоняются ([`FilePolicy::disabled`]), а инструкции не упоминают CLI.
    pub local_machine: bool,
    /// Доступ к системному хранилищу для источника `keychain://` в `secret_copy`. `None` —
    /// такие источники отклоняются (по умолчанию). Заполнять только для локального сервера
    /// по явному флагу пользователя (`serve --allow-keychain-source`): агент читает записи
    /// хранилища этой машины и отправляет их в удалённый репозиторий. Без `local_machine`
    /// (HTTP) поле не действует: см. [`ServerOptions::keychain_source`].
    pub keychain_source: Option<Arc<dyn Keychain>>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_access: AccessLevel::Read,
            toolsets: None,
            dynamic_toolsets: false,
            allow_secret_values: false,
            profile: None,
            executable: None,
            jq: JqRunner::default(),
            custom_tools: Vec::new(),
            local_machine: true,
            keychain_source: None,
        }
    }
}

impl ServerOptions {
    /// Хранилище для источников `keychain://`, если они разрешены. Удалённому серверу
    /// (без [`ServerOptions::local_machine`]) — всегда `None`: хранилище его машины не имеет
    /// отношения к агенту, и читать его по запросу агента нельзя.
    pub fn keychain_source(&self) -> Option<&Arc<dyn Keychain>> {
        self.keychain_source.as_ref().filter(|_| self.local_machine)
    }

    /// Как `secret_copy` относится к источникам `keychain://` на этом сервере.
    pub fn keychain_mode(&self) -> secret_copy::KeychainMode {
        if !self.local_machine {
            secret_copy::KeychainMode::Unavailable
        } else if self.keychain_source().is_some() {
            secret_copy::KeychainMode::Allowed
        } else {
            secret_copy::KeychainMode::Disabled
        }
    }
}
