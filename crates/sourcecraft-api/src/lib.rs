//! Клиент SourceCraft REST API.
//!
//! - [`spec`] — операции API, сгенерированные из Swagger-спецификации при сборке.
//! - [`executor`] — выполнение HTTP-запросов к API.
//! - [`path`] — проверка и сборка относительных путей API.
//! - [`auth`] — хранение токенов в системном хранилище (Keychain и аналоги).
//! - [`token`] — обёртка токена, которая не попадает в логи и `Debug`.

pub mod auth;
pub mod executor;
pub mod path;
pub mod spec;
pub mod token;

pub use spec::{
    AccessLevel, HttpMethod, OperationDef, ParamDef, ParamLocation, ParamType, ParamVariant,
    PathVariant,
};
pub use token::ApiToken;

/// Базовый адрес публичного API SourceCraft.
pub const DEFAULT_BASE_URL: &str = "https://api.sourcecraft.tech";
