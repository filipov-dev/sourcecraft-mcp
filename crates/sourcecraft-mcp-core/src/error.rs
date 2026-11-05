//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. Токен в тексты ошибок не попадает: [`ApiError`]
//! его не содержит, а аргументы инструмента токена не принимают.

use std::fmt;

use sourcecraft_api::executor::ApiError;

/// Ошибка вызова инструмента с текстом для агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    fn from(err: ApiError) -> Self {
        Self(describe_api_error(&err))
    }
}

/// Текст ошибки исполнителя запросов для агента.
///
/// Тексты `ApiError` написаны для логов на русском, поэтому здесь своя формулировка;
/// подробности (тело ответа, причина сетевой ошибки) добавляются как есть.
pub fn describe_api_error(err: &ApiError) -> String {
    match err {
        ApiError::Http { status, body } => {
            let hint = match status {
                300..=399 => {
                    " (a redirect outside the SourceCraft API was not followed; the resource is \
                     probably served from another host)"
                }
                400 | 422 => {
                    " (the request was rejected: check the arguments against the tool schema)"
                }
                401 => {
                    // Как заменить токен, зависит от режима сервера (хранилище или заголовок
                    // HTTP-запроса), поэтому подробности — в инструкциях сервера.
                    " (the token was rejected: it may be expired or revoked; ask the user to \
                     replace it as described in the server instructions)"
                }
                403 => " (the token has no permission for this operation or resource)",
                404 => {
                    " (not found: check the slugs or IDs, and that the token can access the \
                     resource)"
                }
                409 => " (conflict with the current state of the resource)",
                429 => " (rate limited: retry later)",
                500..=599 => " (SourceCraft server error: retry later)",
                _ => "",
            };
            let body = body.trim();
            if body.is_empty() {
                format!("SourceCraft API returned HTTP {status}{hint}.")
            } else {
                format!("SourceCraft API returned HTTP {status}{hint}. Response body: {body}")
            }
        }
        ApiError::MissingPathParam(name) => format!("missing path argument `{name}`"),
        ApiError::InvalidPath(_) => "an identifier argument does not form a valid API path: \
             empty values and values like `.` or `..` are rejected"
            .to_owned(),
        ApiError::InvalidRequest(detail) => format!("invalid request: {detail}"),
        ApiError::InvalidConfig(detail) => format!("server configuration error: {detail}"),
        ApiError::MissingToken => "no SourceCraft token was supplied for this request".to_owned(),
        ApiError::Transport(detail) => format!("could not reach the SourceCraft API: {detail}"),
        ApiError::Io(detail) => format!("local file error: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_errors_have_status_hint_and_body() {
        let text = describe_api_error(&ApiError::Http {
            status: 404,
            body: r#"{"message":"repo not found"}"#.into(),
        });
        assert!(text.contains("HTTP 404"), "{text}");
        assert!(text.contains("not found: check the slugs"), "{text}");
        assert!(text.contains("repo not found"), "{text}");

        let text = describe_api_error(&ApiError::Http {
            status: 401,
            body: " ".into(),
        });
        assert!(text.contains("server instructions"), "{text}");
        assert!(!text.contains("Response body"), "{text}");
    }

    #[test]
    fn other_errors_are_english() {
        assert_eq!(
            describe_api_error(&ApiError::MissingPathParam("repo_slug".into())),
            "missing path argument `repo_slug`"
        );
        assert!(
            describe_api_error(&ApiError::InvalidPath("детали".into())).contains("valid API path")
        );
        assert!(
            describe_api_error(&ApiError::Transport("timeout".into()))
                .contains("could not reach the SourceCraft API: timeout")
        );
    }
}
