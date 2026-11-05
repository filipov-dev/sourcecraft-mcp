//! `secret_copy`: создание и перезапись секрета, уровень доступа, источник `keychain://`
//! и отсутствие значения в результате, ошибках и логах. SourceCraft API подменён `wiremock`.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_api::keychain::{Keychain, KeychainItem, MemoryKeychain};
use sourcecraft_api::{AccessLevel, ApiToken};
use sourcecraft_mcp_core::{FilePolicy, ServerOptions, SourceCraftServer};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Значение секрета и его base64 (`czNjcmV0LXZhbHVlLTQy`).
const VALUE: &str = "s3cret-value-42";
const VALUE_B64: &str = "czNjcmV0LXZhbHVlLTQy";

fn server(
    api: &MockServer,
    max_access: AccessLevel,
    keychain: Option<Arc<dyn Keychain>>,
) -> SourceCraftServer {
    let client = ApiClient::new(ClientConfig {
        base_url: api.uri().parse().unwrap(),
        token: Some(ApiToken::new("test-token-value")),
        timeout: Duration::from_secs(10),
        max_response_bytes: 64 * 1024,
    })
    .unwrap();
    SourceCraftServer::new(
        client,
        ServerOptions {
            max_access,
            keychain_source: keychain,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap()
}

fn args(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => panic!("не объект"),
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn copy(server: &SourceCraftServer, source: &str, destination: &str) -> CallToolResult {
    server
        .call(
            "secret_copy",
            args(json!({"source": source, "destination": destination})),
        )
        .await
        .unwrap()
}

/// Источник `acme/web` с секретом `API_TOKEN`.
async fn mount_source(api: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/secrets"))
        .and(query_param("keys", "API_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "secrets": [{"key": "API_TOKEN", "value": VALUE_B64}]
        })))
        .mount(api)
        .await;
}

/// Назначение `acme/api`: секрет `API_TOKEN` есть (`exists`) или нет.
async fn mount_destination(api: &MockServer, exists: bool, expected_puts: u64) {
    let secrets = if exists {
        json!({"secrets": [{"key": "API_TOKEN", "value": "b2xkLXZhbHVl"}]})
    } else {
        json!({"secrets": []})
    };
    Mock::given(method("GET"))
        .and(path("/repos/acme/api/secrets"))
        .and(query_param("keys", "API_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(secrets))
        .mount(api)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/acme/api/secrets/API_TOKEN"))
        .and(body_json(json!({"value": VALUE_B64})))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "operation_id": "op-7", "status": "success", "response": {"value": VALUE_B64}
        })))
        .expect(expected_puts)
        .mount(api)
        .await;
}

/// Буфер, в который пишет подписчик `tracing` в тесте.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Общий для всех тестов файла подписчик `tracing` (уровень TRACE): локальный подписчик
/// (`set_default`) ненадёжен при параллельных тестах из-за кэша интереса callsite. Логи всех
/// тестов попадают в один буфер — проверка «значения нет в логах» покрывает их все.
fn logs() -> &'static LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = LogBuffer::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        logs
    })
}

#[tokio::test]
async fn copy_creates_a_new_secret_without_exposing_the_value() {
    let logs = logs();
    let api = MockServer::start().await;
    mount_source(&api).await;
    mount_destination(&api, false, 1).await;
    let server = server(&api, AccessLevel::Write, None);

    let result = copy(
        &server,
        "sc://acme/web/API_TOKEN",
        "sc://acme/api/API_TOKEN",
    )
    .await;

    let output = text(&result);
    assert_eq!(result.is_error, Some(false), "{output}");
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        parsed,
        json!({
            "source": "sc://acme/web/API_TOKEN",
            "destination": "sc://acme/api/API_TOKEN",
            "result": "created",
            "operation": {"operation_id": "op-7", "status": "success"}
        })
    );

    let log = logs.text();
    assert!(log.contains("sc://acme/web/API_TOKEN"), "{log}");
    assert!(log.contains("created"), "{log}");
    for leaked in [VALUE, VALUE_B64] {
        assert!(!output.contains(leaked), "{output}");
        assert!(!log.contains(leaked), "{log}");
    }
}

#[tokio::test]
async fn overwrite_requires_destructive_access() {
    logs();
    let api = MockServer::start().await;
    mount_source(&api).await;
    mount_destination(&api, true, 0).await;
    let result = copy(
        &server(&api, AccessLevel::Write, None),
        "sc://acme/web/API_TOKEN",
        "sc://acme/api/API_TOKEN",
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("already exists"), "{message}");
    assert!(message.contains("--allow-destructive"), "{message}");
    assert!(!message.contains(VALUE), "{message}");
}

#[tokio::test]
async fn destructive_server_overwrites() {
    logs();
    let api = MockServer::start().await;
    mount_source(&api).await;
    mount_destination(&api, true, 1).await;
    let result = copy(
        &server(&api, AccessLevel::Destructive, None),
        "sc://acme/web/API_TOKEN",
        "sc://acme/api/API_TOKEN",
    )
    .await;
    let output = text(&result);
    assert_eq!(result.is_error, Some(false), "{output}");
    assert!(output.contains(r#""result":"overwritten""#), "{output}");
    assert!(!output.contains(VALUE_B64), "{output}");
}

#[tokio::test]
async fn write_errors_hide_the_value() {
    logs();
    let api = MockServer::start().await;
    mount_source(&api).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/api/secrets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&api)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/acme/api/secrets/API_TOKEN"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": format!("invalid value {VALUE} ({VALUE_B64})")
        })))
        .mount(&api)
        .await;
    let result = copy(
        &server(&api, AccessLevel::Write, None),
        "sc://acme/web/API_TOKEN",
        "sc://acme/api/API_TOKEN",
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("400"), "{message}");
    assert!(!message.contains(VALUE), "{message}");
    assert!(!message.contains(VALUE_B64), "{message}");
}

#[tokio::test]
async fn keychain_source_is_rejected_by_default() {
    logs();
    let api = MockServer::start().await;
    let result = copy(
        &server(&api, AccessLevel::Destructive, None),
        "keychain://deploy/prod",
        "sc://acme/api/API_TOKEN",
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("--allow-keychain-source"), "{message}");
    assert!(api.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn keychain_source_when_allowed() {
    logs();
    let api = MockServer::start().await;
    mount_destination(&api, false, 1).await;
    let keychain = Arc::new(MemoryKeychain::new());
    keychain
        .set(&KeychainItem::new("deploy", "prod"), VALUE.as_bytes())
        .unwrap();
    let server = server(&api, AccessLevel::Write, Some(keychain));

    let result = copy(&server, "keychain://deploy/prod", "sc://acme/api/API_TOKEN").await;
    let output = text(&result);
    assert_eq!(result.is_error, Some(false), "{output}");
    assert!(output.contains(r#""result":"created""#), "{output}");
    assert!(!output.contains(VALUE), "{output}");

    // Записи с токенами сервера недоступны и с флагом; назначением keychain быть не может.
    let result = copy(
        &server,
        "keychain://dev.filipov.sourcecraft-mcp/codex",
        "sc://acme/api/API_TOKEN",
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("API token"), "{}", text(&result));

    let result = copy(
        &server,
        "sc://acme/web/API_TOKEN",
        "keychain://deploy/other",
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("local keychain"),
        "{}",
        text(&result)
    );
}

/// Токен запроса в расширениях (как его кладёт HTTP-транспорт).
#[derive(Clone)]
struct TestRequestToken(ApiToken);

/// HTTP-режим: хранилище недоступно, даже если его передали; копирование между
/// репозиториями работает с токеном каждого запроса.
#[tokio::test]
async fn remote_server_never_reads_the_keychain() {
    use rmcp::model::Extensions;
    use sourcecraft_mcp_core::TokenSource;

    logs();
    let api = MockServer::start().await;
    mount_source(&api).await;
    mount_destination(&api, false, 1).await;
    let keychain = Arc::new(MemoryKeychain::new());
    keychain
        .set(&KeychainItem::new("deploy", "prod"), VALUE.as_bytes())
        .unwrap();
    let client = ApiClient::new(ClientConfig {
        base_url: api.uri().parse().unwrap(),
        token: None,
        timeout: Duration::from_secs(10),
        max_response_bytes: 64 * 1024,
    })
    .unwrap();
    let server = SourceCraftServer::with_token_source(
        client,
        ServerOptions {
            max_access: AccessLevel::Write,
            local_machine: false,
            keychain_source: Some(keychain),
            ..ServerOptions::default()
        },
        FilePolicy::disabled(),
        TokenSource::PerRequest(Arc::new(|ext: &Extensions| {
            ext.get::<TestRequestToken>().map(|t| t.0.clone())
        })),
    )
    .unwrap();
    let description = server
        .registry()
        .get("secret_copy")
        .unwrap()
        .tool
        .description
        .clone()
        .unwrap_or_default();
    assert!(!description.contains("keychain"), "{description}");
    assert!(!server.instructions().contains("keychain"));

    let mut ext = Extensions::new();
    ext.insert(TestRequestToken(ApiToken::new("agent-token")));
    let call = |source: &str| {
        server.call_with_extensions(
            "secret_copy",
            args(json!({"source": source, "destination": "sc://acme/api/API_TOKEN"})),
            &ext,
        )
    };

    let result = call("keychain://deploy/prod").await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP mode"), "{message}");
    assert!(!message.contains("--allow-keychain-source"), "{message}");
    assert!(api.received_requests().await.unwrap().is_empty());

    let result = call("sc://acme/web/API_TOKEN").await.unwrap();
    let output = text(&result);
    assert_eq!(result.is_error, Some(false), "{output}");
    assert!(output.contains(r#""result":"created""#), "{output}");
    assert!(
        !output.contains(VALUE) && !output.contains(VALUE_B64),
        "{output}"
    );
    for request in api.received_requests().await.unwrap() {
        let auth = request.headers.get("authorization").unwrap();
        assert_eq!(auth.to_str().unwrap(), "Bearer agent-token");
    }
}

#[tokio::test]
async fn registration_follows_access_level() {
    let api = MockServer::start().await;
    let read = server(&api, AccessLevel::Read, None);
    assert!(read.registry().get("secret_copy").is_none());
    let result = copy(&read, "sc://a/b/K", "sc://c/d/K").await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"));
    assert!(api.received_requests().await.unwrap().is_empty());

    let write = server(&api, AccessLevel::Write, None);
    let registry = write.registry();
    let tool = &registry.get("secret_copy").unwrap().tool;
    assert_eq!(
        tool.annotations.as_ref().unwrap().destructive_hint,
        Some(false)
    );
    assert!(write.instructions().contains("`secret_copy`"));

    let result = copy(&write, "sc://a/b/K", "sc://a/b/K").await;
    assert!(text(&result).contains("the same secret"));
}
