//! Сквозная проверка `api_request` и `api_search`: сервер в процессе, клиент `rmcp` через
//! in-memory транспорт, SourceCraft API подменён `wiremock`.

use std::path::Path;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_api::{AccessLevel, ApiToken};
use sourcecraft_mcp_core::{FilePolicy, ServerOptions, SourceCraftServer};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "test-token-value";

async fn start(
    api: &MockServer,
    max_access: AccessLevel,
    root: &Path,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    let client = ApiClient::new(ClientConfig {
        base_url: api.uri().parse().unwrap(),
        token: Some(ApiToken::new(TEST_TOKEN)),
        timeout: Duration::from_secs(10),
        max_response_bytes: 64 * 1024,
    })
    .unwrap();
    let server = SourceCraftServer::new(
        client,
        ServerOptions {
            max_access,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[root.to_path_buf()]).unwrap(),
    )
    .unwrap();

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();
    (client, handle)
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

async fn call(client: &RunningService<RoleClient, ()>, name: &str, value: Value) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(value)))
        .await
        .unwrap()
}

#[tokio::test]
async fn api_request_on_read_only_server() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/custom"))
        .and(query_param("page_size", "2"))
        .and(query_param("state", "open"))
        .and(header(
            "authorization",
            format!("Bearer {TEST_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [1, 2]})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/archive"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"archive-bytes".to_vec()))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "no repo"})))
        .mount(&api)
        .await;

    let (client, server) = start(&api, AccessLevel::Read, &root).await;

    let tools = client.list_all_tools().await.unwrap();
    let request_tool = tools.iter().find(|t| t.name == "api_request").unwrap();
    assert_eq!(
        request_tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tools.iter().any(|t| t.name == "api_search"));
    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(instructions.contains("`api_request`"), "{instructions}");

    // GET с query: путь, параметры и Authorization проверяет mock.
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "GET",
            "path": "/repos/acme/web/custom",
            "query": {"page_size": 2, "state": "open"}
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"items":[1,2]}"#);

    // save_to внутри разрешённого каталога.
    let target = root.join("archive.bin");
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "GET",
            "path": "/repos/acme/web/archive",
            "save_to": target.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert!(text(&result).contains("saved_to"), "{}", text(&result));
    assert_eq!(std::fs::read(&target).unwrap(), b"archive-bytes");

    // save_to вне разрешённых каталогов — отказ без запроса в API.
    let before = api.received_requests().await.unwrap().len();
    let outside = tempfile::tempdir().unwrap();
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "GET",
            "path": "/repos/acme/web/archive",
            "save_to": outside.path().join("x.bin").display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("outside the allowed directories"),
        "{}",
        text(&result)
    );
    assert_eq!(api.received_requests().await.unwrap().len(), before);

    // 404 — управляемая ошибка.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/repos/acme/missing"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 404"), "{message}");
    assert!(message.contains("no repo"), "{message}");
    assert!(!message.contains(TEST_TOKEN));

    // Запись на сервере только для чтения — отказ с названием флага, без запроса в API.
    let result = call(
        &client,
        "api_request",
        json!({"method": "POST", "path": "/repos/acme/web/pulls", "body": {"title": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));
    // Полный URL отклоняется.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "https://example.com/user"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(api.received_requests().await.unwrap().len(), before + 1);

    // api_search видит и незарегистрированные операции.
    let result = call(
        &client,
        "api_search",
        json!({"query": "create pull request", "limit": 1}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let found: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(found["results"][0]["tool_name"], "create_pull_request");
    assert_eq!(found["results"][0]["registered"], false);
    assert_eq!(found["results"][0]["enable_with"], "--allow-write");

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn api_request_on_write_server() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/web/pulls"))
        .and(body_json(json!({"title": "Fix", "source_branch": "fix"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "pr-1"})))
        .expect(1)
        .mount(&api)
        .await;

    let (client, server) = start(&api, AccessLevel::Write, &root).await;

    let tools = client.list_all_tools().await.unwrap();
    let request_tool = tools.iter().find(|t| t.name == "api_request").unwrap();
    let annotations = request_tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));

    let result = call(
        &client,
        "api_request",
        json!({
            "method": "POST",
            "path": "/repos/acme/web/pulls",
            "body": {"title": "Fix", "source_branch": "fix"}
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"id":"pr-1"}"#);

    // Секрет сырым PUT не записывается, публичность репозитория требует --allow-destructive.
    let result = call(
        &client,
        "api_request",
        json!({"method": "PUT", "path": "/repos/acme/web/secrets/TOKEN", "body": {"value": "eA=="}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`secret_set`"), "{}", text(&result));
    let result = call(
        &client,
        "api_request",
        json!({"method": "PATCH", "path": "/repos/acme/web", "body": {"visibility": "public"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("--allow-destructive"),
        "{}",
        text(&result)
    );
    // DELETE неизвестного пути — удаление.
    let result = call(
        &client,
        "api_request",
        json!({"method": "DELETE", "path": "/repos/acme/web/something"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("--allow-destructive"),
        "{}",
        text(&result)
    );
    assert_eq!(api.received_requests().await.unwrap().len(), 1);

    client.cancel().await.unwrap();
    server.await.unwrap();
}
