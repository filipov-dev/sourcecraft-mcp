//! Чтение и запись значений секретов репозитория через API — общая часть CLI
//! (`sourcecraft-mcp secret …`) и инструмента `secret_copy`.
//!
//! Запросы собираются сгенерированными операциями (`list_secrets`, `set_secret`,
//! `get_secret_operation`) через [`build_request`], поэтому адресация и кодирование пути — те
//! же, что у MCP-инструментов. Тексты ошибок API проходят через [`redact_api_error`], а ошибки
//! записи ещё и вычищаются от самого значения и его base64 ([`sensitive_forms`]). Тела ответов
//! со значениями в ошибки не попадают.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde_json::{Map, Value, json};
use sourcecraft_api::executor::{ApiClient, ApiResponse, ResponseBody};
use sourcecraft_api::spec::find;

use crate::error::{ToolError, describe_api_error};
use crate::files::FilePolicy;
use crate::redact::redact_api_error;
use crate::request::build_request;
use crate::secrets::{SET_SECRET_OPERATION, encode_secret_value, scrub, sensitive_forms};

/// Операция чтения секретов.
pub const LIST_SECRETS_OPERATION: &str = "list_secrets";
/// Операция статуса асинхронной операции над секретами.
pub const SECRET_OPERATION_OPERATION: &str = "get_secret_operation";

/// base64 значения секрета: стандартный алфавит, паддинг необязателен.
const LENIENT_STANDARD: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Репозиторий, с секретами которого работаем.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoTarget {
    Slug { org: String, repo: String },
    Id(String),
}

impl RepoTarget {
    fn insert_args(&self, args: &mut Map<String, Value>) {
        match self {
            Self::Slug { org, repo } => {
                args.insert("org_slug".to_owned(), Value::String(org.clone()));
                args.insert("repo_slug".to_owned(), Value::String(repo.clone()));
            }
            Self::Id(id) => {
                args.insert("repo_id".to_owned(), Value::String(id.clone()));
            }
        }
    }
}

impl std::fmt::Display for RepoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Slug { org, repo } => write!(f, "{org}/{repo}"),
            Self::Id(id) => write!(f, "id:{id}"),
        }
    }
}

/// Раскодированные значения секретов по ключам. Не `Debug`: внутри значения.
pub struct Secrets(BTreeMap<String, Vec<u8>>);

impl Secrets {
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.0.get(key).map(Vec::as_slice)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Набор из пар ключ–значение (для тестов потребителей).
    pub fn from_pairs(pairs: &[(&str, &[u8])]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.to_vec()))
                .collect(),
        )
    }
}

fn base_args(target: &RepoTarget, group: Option<&str>) -> Map<String, Value> {
    let mut args = Map::new();
    target.insert_args(&mut args);
    if let Some(group) = group {
        args.insert("secret_group".to_owned(), Value::String(group.to_owned()));
    }
    args
}

/// Политика файлов не нужна этим операциям: пустая.
fn no_files() -> Result<FilePolicy, ToolError> {
    FilePolicy::new(&[]).map_err(|err| ToolError::new(err.to_string()))
}

/// Читает секреты репозитория. `keys` — фильтр по ключам (пусто — все секреты).
pub async fn fetch_secrets(
    client: &ApiClient,
    target: &RepoTarget,
    group: Option<&str>,
    keys: &[String],
) -> Result<Secrets, ToolError> {
    let op = find(LIST_SECRETS_OPERATION).ok_or_else(|| {
        ToolError::new("the SourceCraft API specification has no operation to list secrets")
    })?;
    let mut args = base_args(target, group);
    if !keys.is_empty() {
        args.insert(
            "keys".to_owned(),
            Value::Array(keys.iter().cloned().map(Value::String).collect()),
        );
    }
    let request = build_request(op, &args, &no_files()?)?;
    let path = request.path.clone();

    let response = client
        .execute(request)
        .await
        .map_err(|err| ToolError::new(describe_api_error(&redact_api_error(&path, err))))?;
    if response.truncated {
        return Err(ToolError::new("the list of secrets is too large to read"));
    }
    // Тело ответа не печатается ни в одной ошибке: в нём значения.
    let body = match response.body {
        ResponseBody::Json(body) => body,
        ResponseBody::Empty => Value::Object(Map::new()),
        _ => {
            return Err(ToolError::new(
                "unexpected response from the SourceCraft API: not JSON",
            ));
        }
    };
    parse_secrets(&body)
}

/// Разбирает ответ `list_secrets`. Ошибки называют ключ, но не значение.
pub fn parse_secrets(body: &Value) -> Result<Secrets, ToolError> {
    let bad = |message: String| Err(ToolError::new(message));
    let entries = match body.get("secrets") {
        None | Some(Value::Null) => return Ok(Secrets(BTreeMap::new())),
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return bad(
                "unexpected response from the SourceCraft API: `secrets` is not a list".into(),
            );
        }
    };
    let mut secrets = BTreeMap::new();
    for entry in entries {
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            return bad(
                "unexpected response from the SourceCraft API: a secret without a key".into(),
            );
        };
        let value = match entry.get("value") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(encoded)) => match LENIENT_STANDARD.decode(encoded.trim()) {
                Ok(value) => value,
                Err(_) => return bad(format!("the value of secret `{key}` is not valid base64")),
            },
            Some(_) => return bad(format!("the value of secret `{key}` is not a string")),
        };
        secrets.insert(key.to_owned(), value);
    }
    Ok(Secrets(secrets))
}

/// Состояние асинхронной операции над секретами (без тела результата).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretOperation {
    pub id: Option<String>,
    /// `scheduled`, `in_progress`, `success`, `failed`, `cancel`; пусто, если API не сообщил.
    pub status: String,
    /// Сообщение об ошибке операции (уже вычищенное от значения).
    pub error: Option<String>,
}

impl SecretOperation {
    pub fn is_finished(&self) -> bool {
        matches!(self.status.as_str(), "success" | "failed" | "cancel")
    }

    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        if let Some(id) = &self.id {
            out.insert("operation_id".into(), json!(id));
        }
        out.insert("status".into(), json!(self.status));
        if let Some(error) = &self.error {
            out.insert("error".into(), json!(error));
        }
        Value::Object(out)
    }

    fn from_response(response: &ApiResponse, sensitive: &[String]) -> Self {
        let body = match &response.body {
            ResponseBody::Json(body) => body,
            _ => &Value::Null,
        };
        let text = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_owned);
        let error = body.get("error").filter(|e| !e.is_null()).map(|error| {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.get("error_code").and_then(Value::as_str))
                .unwrap_or("the operation failed");
            scrub(message, sensitive)
        });
        Self {
            id: text("operation_id"),
            status: text("status").unwrap_or_default(),
            error,
        }
    }
}

/// Записывает значение секрета (`set_secret`): кодирует в base64 сама. Возвращает состояние
/// операции; ошибки вычищены от значения.
pub async fn set_secret(
    client: &ApiClient,
    target: &RepoTarget,
    group: Option<&str>,
    key: &str,
    value: &[u8],
) -> Result<SecretOperation, ToolError> {
    let op = find(SET_SECRET_OPERATION).ok_or_else(|| {
        ToolError::new("the SourceCraft API specification has no operation to set secrets")
    })?;
    let sensitive = sensitive_forms(value);
    let mut args = base_args(target, group);
    args.insert("key".to_owned(), Value::String(key.to_owned()));
    args.insert(
        "body".to_owned(),
        json!({ "value": encode_secret_value(value) }),
    );
    let request = build_request(op, &args, &no_files()?)?;
    let path = request.path.clone();
    match client.execute(request).await {
        Ok(response) => Ok(SecretOperation::from_response(&response, &sensitive)),
        Err(err) => Err(ToolError::new(scrub(
            &describe_api_error(&redact_api_error(&path, err)),
            &sensitive,
        ))),
    }
}

/// Ждёт завершения операции, опрашивая `get_secret_operation` с интервалом `interval`, не
/// дольше `timeout`. Возвращает последнее известное состояние (возможно, незавершённое).
pub async fn wait_operation(
    client: &ApiClient,
    operation: SecretOperation,
    sensitive: &[String],
    interval: Duration,
    timeout: Duration,
) -> Result<SecretOperation, ToolError> {
    let Some(id) = operation.id.clone() else {
        return Ok(operation);
    };
    let Some(op) = find(SECRET_OPERATION_OPERATION) else {
        return Ok(operation);
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut current = operation;
    while !current.is_finished() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        let mut args = Map::new();
        args.insert("operation_id".to_owned(), Value::String(id.clone()));
        let request = build_request(op, &args, &no_files()?)?;
        let path = request.path.clone();
        let response = client.execute(request).await.map_err(|err| {
            ToolError::new(scrub(
                &describe_api_error(&redact_api_error(&path, err)),
                sensitive,
            ))
        })?;
        current = SecretOperation::from_response(&response, sensitive);
        if current.id.is_none() {
            current.id = Some(id.clone());
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sourcecraft_api::ApiToken;
    use sourcecraft_api::executor::ClientConfig;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer) -> ApiClient {
        ApiClient::new(ClientConfig {
            base_url: server.uri().parse().unwrap(),
            token: Some(ApiToken::new("test-token")),
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
        })
        .unwrap()
    }

    fn acme() -> RepoTarget {
        RepoTarget::Slug {
            org: "acme".into(),
            repo: "web".into(),
        }
    }

    #[tokio::test]
    async fn fetches_and_decodes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/secrets"))
            .and(query_param("secret_group", "ci"))
            .and(query_param("keys", "API_TOKEN"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "secrets": [
                    {"key": "API_TOKEN", "value": "czNjcmV0"},
                    {"key": "UNPADDED", "value": "aGk"},
                    {"key": "EMPTY"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let secrets = fetch_secrets(
            &client(&server),
            &acme(),
            Some("ci"),
            &["API_TOKEN".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(secrets.get("API_TOKEN"), Some(&b"s3cret"[..]));
        assert_eq!(secrets.get("UNPADDED"), Some(&b"hi"[..]));
        assert_eq!(secrets.get("EMPTY"), Some(&b""[..]));
        assert!(secrets.contains("EMPTY"));
        assert_eq!(
            secrets.keys().collect::<Vec<_>>(),
            ["API_TOKEN", "EMPTY", "UNPADDED"]
        );
    }

    #[tokio::test]
    async fn fetches_by_repo_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/id:42/secrets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let target = RepoTarget::Id("42".into());
        let secrets = fetch_secrets(&client(&server), &target, None, &[])
            .await
            .unwrap();
        assert!(secrets.is_empty());
    }

    #[tokio::test]
    async fn api_errors_hide_values() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/secrets"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"message": "denied", "value": "czNjcmV0"})),
            )
            .mount(&server)
            .await;
        let err = fetch_secrets(&client(&server), &acme(), None, &[])
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(!err.contains("czNjcmV0"), "{err}");
        assert!(!err.contains("test-token"), "{err}");
    }

    #[test]
    fn rejects_bad_responses() {
        for (body, expected) in [
            (json!({"secrets": "x"}), "not a list"),
            (json!({"secrets": [{"value": "eA=="}]}), "without a key"),
            (
                json!({"secrets": [{"key": "K", "value": "%%%"}]}),
                "not valid base64",
            ),
            (
                json!({"secrets": [{"key": "K", "value": 1}]}),
                "not a string",
            ),
        ] {
            let err = parse_secrets(&body).err().unwrap().to_string();
            assert!(err.contains(expected), "{body}: {err}");
            assert!(!err.contains("%%%"), "{err}");
        }
    }

    #[tokio::test]
    async fn sets_and_waits() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/repos/acme/web/secrets/K"))
            .and(body_json(json!({"value": "czNjcmV0LXZhbHVl"})))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "operation_id": "op-1", "status": "scheduled"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/operations/secrets/id:op-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "operation_id": "op-1", "status": "success", "response": {"value": "x"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server);
        let operation = set_secret(&client, &acme(), None, "K", b"s3cret-value")
            .await
            .unwrap();
        assert_eq!(operation.status, "scheduled");
        let done = wait_operation(
            &client,
            operation,
            &[],
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(done.status, "success");
        assert_eq!(
            done.to_json(),
            json!({"operation_id": "op-1", "status": "success"})
        );
    }

    #[tokio::test]
    async fn set_errors_hide_the_value() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/repos/acme/web/secrets/K"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "message": "bad value s3cret-value (czNjcmV0LXZhbHVl)"
            })))
            .mount(&server)
            .await;
        let err = set_secret(&client(&server), &acme(), None, "K", b"s3cret-value")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("400"), "{err}");
        assert!(!err.contains("s3cret-value"), "{err}");
        assert!(!err.contains("czNjcmV0LXZhbHVl"), "{err}");
    }
}
