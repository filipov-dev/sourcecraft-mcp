//! MCP-инструменты SourceCraft API без привязки к транспорту.
//!
//! - [`api_request`], [`api_search`] — инструменты произвольного запроса и поиска операций;
//! - [`routes`] — сопоставление произвольного запроса с операциями и уровень доступа;
//! - [`server`] — обработчик `rmcp` ([`SourceCraftServer`]) и инструкции для агента;
//! - [`registry`] — какие инструменты регистрируются при заданных [`ServerOptions`];
//! - [`request`] — сборка запроса к API из аргументов инструмента;
//! - [`response`] — представление ответа API в результате инструмента;
//! - [`redact`] — вырезание значений секретов и учётных данных из ответов;
//! - [`secrets`] — `secret_set` и раскрытие значений секретов (`reveal_values`);
//! - [`files`] — политика доступа к локальным файлам;
//! - [`toolsets`] — наборы инструментов по тегам спецификации.
//!
//! Транспорт (stdio, позже HTTP) подключается в бинарнике `sourcecraft-mcp`.

pub mod api_request;
pub mod api_search;
pub mod error;
pub mod files;
pub mod redact;
pub mod registry;
pub mod request;
pub mod response;
pub mod routes;
pub mod secrets;
pub mod server;
pub mod toolsets;

pub use error::ToolError;
pub use files::{FilePolicy, FilePolicyError};
pub use registry::{RegisteredTool, ToolKind, ToolRegistry};
pub use server::SourceCraftServer;
pub use toolsets::{Toolset, available_toolsets, normalize_toolsets, toolset_slug};

use sourcecraft_api::AccessLevel;

/// Настройки MCP-сервера.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Максимальный разрешённый уровень доступа. Инструменты выше него не регистрируются.
    pub max_access: AccessLevel,
    /// Наборы инструментов (slug тегов спецификации, см. [`toolsets`]).
    /// `None` или пустой список — все наборы.
    pub toolsets: Option<Vec<String>>,
    /// Разрешить раскрывать значения секретов по явному запросу (`reveal_values: true`).
    /// По умолчанию выключено: значения вырезаются из всех ответов (см. [`redact`]).
    pub allow_secret_values: bool,
    /// Профиль токена, с которым запущен сервер. Подставляется в примеры команд
    /// `sourcecraft-mcp secret …` в инструкциях, чтобы агент использовал тот же профиль.
    pub profile: Option<String>,
    /// Путь к бинарнику `sourcecraft-mcp` для примеров команд в инструкциях; `None` —
    /// просто `sourcecraft-mcp` (из `PATH`).
    pub executable: Option<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_access: AccessLevel::Read,
            toolsets: None,
            allow_secret_values: false,
            profile: None,
            executable: None,
        }
    }
}
