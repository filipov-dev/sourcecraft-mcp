//! Проверка токена запросом текущего пользователя (`GET /user`, операция `GetMyProfile`).

use std::time::Duration;

use anyhow::anyhow;
use sourcecraft_api::executor::{
    ApiClient, ApiError, ApiRequest, ApiResponse, ClientConfig, RequestBody, ResponseBody,
};
use sourcecraft_api::{ApiToken, HttpMethod};

use crate::base_url::default_base_url;

/// Путь операции `GetMyProfile` (тег `User`) в спецификации SourceCraft.
pub const CURRENT_USER_PATH: &str = "/user";

/// Проверка токена. В тестах подменяется, чтобы не ходить в сеть.
pub trait TokenVerifier {
    /// Возвращает имя пользователя, которому принадлежит токен.
    async fn verify(&self, token: &ApiToken) -> anyhow::Result<String>;
}

/// Проверка через SourceCraft API.
#[derive(Debug, Clone)]
pub struct ApiVerifier {
    base_url: url::Url,
}

impl ApiVerifier {
    /// Проверка через API по адресу `SOURCECRAFT_BASE_URL` или адресу по умолчанию —
    /// тому же, который использует `serve` без `--base-url`.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            base_url: default_base_url()?,
        })
    }
}

impl TokenVerifier for ApiVerifier {
    async fn verify(&self, token: &ApiToken) -> anyhow::Result<String> {
        let client = ApiClient::new(ClientConfig {
            base_url: self.base_url.clone(),
            token: Some(token.clone()),
            timeout: Duration::from_secs(30),
            max_response_bytes: 1024 * 1024,
        })
        .map_err(check_error)?;
        let response = client
            .execute(ApiRequest {
                method: HttpMethod::Get,
                path: CURRENT_USER_PATH.to_owned(),
                query: Vec::new(),
                body: RequestBody::Empty,
                save_to: None,
            })
            .await
            .map_err(check_error)?;
        Ok(user_name(&response))
    }
}

/// Понятная ошибка проверки токена. Тело ответа не включается: оно не нужно пользователю.
pub fn check_error(err: ApiError) -> anyhow::Error {
    match err {
        ApiError::Http {
            status: status @ (401 | 403),
            ..
        } => anyhow!(
            "SourceCraft rejected the token (HTTP {status}). Make sure it was copied completely \
             and has not expired or been revoked"
        ),
        other => anyhow!(
            "could not verify the token: {other}. Re-run with --skip-check to store it without \
             verification"
        ),
    }
}

/// Имя пользователя из ответа `GET /user`.
fn user_name(response: &ApiResponse) -> String {
    if let ResponseBody::Json(body) = &response.body {
        for key in ["username", "display_name", "id"] {
            if let Some(name) = body
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
            {
                return name.to_owned();
            }
        }
    }
    "unknown user".to_owned()
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Проверка без сети: принимает только заданное значение токена.
    #[derive(Debug)]
    pub struct FakeVerifier {
        pub valid_token: String,
    }

    impl TokenVerifier for FakeVerifier {
        async fn verify(&self, token: &ApiToken) -> anyhow::Result<String> {
            if token.expose() == self.valid_token {
                Ok("test-user".to_owned())
            } else {
                Err(check_error(ApiError::Http {
                    status: 401,
                    body: String::new(),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn current_user_operation_exists_in_spec() {
        let op = sourcecraft_api::spec::operations()
            .iter()
            .find(|op| op.path == CURRENT_USER_PATH && op.method == HttpMethod::Get);
        assert!(
            op.is_some(),
            "GET {CURRENT_USER_PATH} not found in operations"
        );
    }

    #[test]
    fn maps_errors() {
        let err = check_error(ApiError::Http {
            status: 401,
            body: "details".into(),
        });
        assert!(err.to_string().contains("rejected the token"));
        assert!(!err.to_string().contains("details"));

        let err = check_error(ApiError::Transport("offline".into()));
        assert!(err.to_string().contains("--skip-check"));
        assert!(err.to_string().contains("offline"));
    }

    #[test]
    fn extracts_user_name() {
        let response = |body| ApiResponse {
            status: 200,
            body: ResponseBody::Json(body),
            truncated: false,
        };
        assert_eq!(
            user_name(&response(json!({"username": "andrey", "id": "1"}))),
            "andrey"
        );
        assert_eq!(user_name(&response(json!({"display_name": "A"}))), "A");
        assert_eq!(user_name(&response(json!({}))), "unknown user");
    }
}
