//! Сквозная проверка: сервер в процессе, клиент `rmcp` через in-memory транспорт,
//! SourceCraft API подменён `wiremock`.

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
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    start_with(
        api,
        ServerOptions {
            max_access,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .await
}

async fn start_with(
    api: &MockServer,
    options: ServerOptions,
    files: FilePolicy,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    let client = ApiClient::new(ClientConfig {
        base_url: api.uri().parse().unwrap(),
        token: Some(ApiToken::new(TEST_TOKEN)),
        timeout: Duration::from_secs(10),
        max_response_bytes: 64 * 1024,
    })
    .unwrap();
    let server = SourceCraftServer::new(client, options, files).unwrap();

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

#[tokio::test]
async fn read_only_server_end_to_end() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/issues"))
        .and(query_param("page_size", "2"))
        .and(header(
            "authorization",
            format!("Bearer {TEST_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [{"id": "i-1", "title": "Bug"}],
            "next_page_token": "p2"
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/id:missing"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"message": "repository not found"})),
        )
        .mount(&api)
        .await;

    let (client, server) = start(&api, AccessLevel::Read).await;

    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "sourcecraft-mcp");
    assert!(info.instructions.as_deref().unwrap().contains("read-only"));

    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "list_repository_issues"));
    assert!(tools.iter().all(|t| {
        t.annotations
            .as_ref()
            .is_some_and(|a| a.read_only_hint == Some(true))
    }));
    assert!(!tools.iter().any(|t| t.name == "create_pull_request"));
    assert!(!tools.iter().any(|t| t.name == "set_secret"));

    // Успешный вызов: путь, query и Authorization проверяет mock.
    let result = client
        .call_tool(
            CallToolRequestParams::new("list_repository_issues").with_arguments(args(json!({
                "org_slug": "acme",
                "repo_slug": "web",
                "page_size": 2
            }))),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        text(&result),
        r#"{"issues":[{"id":"i-1","title":"Bug"}],"next_page_token":"p2"}"#
    );

    // 404 API — управляемая ошибка, не ошибка протокола.
    let result = client
        .call_tool(
            CallToolRequestParams::new("get_repository")
                .with_arguments(args(json!({"repo_id": "missing"}))),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 404"), "{message}");
    assert!(message.contains("repository not found"), "{message}");
    assert!(!message.contains(TEST_TOKEN));

    // Инструмент записи не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = client
        .call_tool(
            CallToolRequestParams::new("create_pull_request").with_arguments(args(json!({
                "org_slug": "acme", "repo_slug": "web", "body": {"title": "x"}
            }))),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`write` access"));

    // Ошибка аргументов не доходит до API.
    let result = client
        .call_tool(
            CallToolRequestParams::new("list_repository_issues")
                .with_arguments(args(json!({"org_slug": "acme"}))),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("missing required argument(s): `repo_slug`"));

    // Неизвестный инструмент — ошибка протокола, сервер продолжает работать.
    assert!(
        client
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await
            .is_err()
    );
    assert!(!client.list_all_tools().await.unwrap().is_empty());

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn write_server_lists_write_tools_and_guards_visibility() {
    let api = MockServer::start().await;
    let (client, server) = start(&api, AccessLevel::Write).await;

    let tools = client.list_all_tools().await.unwrap();
    let create = tools
        .iter()
        .find(|t| t.name == "create_pull_request")
        .unwrap();
    let annotations = create.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert!(!tools.iter().any(|t| t.name == "delete_repository"));

    let result = client
        .call_tool(
            CallToolRequestParams::new("update_repository").with_arguments(args(json!({
                "org_slug": "acme", "repo_slug": "web", "body": {"visibility": "public"}
            }))),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`destructive` access"));
    // Запрос в API не отправлялся.
    assert!(api.received_requests().await.unwrap().is_empty());

    client.cancel().await.unwrap();
    server.await.unwrap();
}

/// Ответ `list_secrets` в форме из спецификации; значения — base64.
fn secrets_body() -> Value {
    json!({
        "secrets": [
            {"key": "DEPLOY_TOKEN", "value": "ZGVwbG95LXZhbHVl"},
            {"key": "REGION", "value": "ZXUtd2VzdA=="}
        ]
    })
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(arguments)))
        .await
        .unwrap()
}

fn error_text(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "{}", text(result));
    text(result)
}

#[tokio::test]
async fn secret_values_are_redacted_by_default() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/secrets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(secrets_body()))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/id:42/secrets"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"message": "bad group", "value": "ZGVwbG95LXZhbHVl"})),
        )
        .mount(&api)
        .await;
    let (client, server) = start(&api, AccessLevel::Read).await;

    let info = client.peer_info().unwrap();
    assert!(info.instructions.as_deref().unwrap().contains("[redacted]"));
    let tools = client.list_all_tools().await.unwrap();
    let list = tools.iter().find(|t| t.name == "list_secrets").unwrap();
    assert!(
        list.input_schema["properties"]
            .get("reveal_values")
            .is_none()
    );
    assert!(!tools.iter().any(|t| t.name == "secret_set"));

    let result = call(
        &client,
        "list_secrets",
        json!({"org_slug": "acme", "repo_slug": "web"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let listed = text(&result);
    assert!(
        listed.contains(r#"{"key":"DEPLOY_TOKEN","value":"[redacted]"}"#),
        "{listed}"
    );
    assert!(listed.contains(r#""key":"REGION""#), "{listed}");
    assert!(
        !listed.contains("ZGVwbG95LXZhbHVl") && !listed.contains("ZXUtd2VzdA=="),
        "{listed}"
    );
    assert!(listed.contains("2 value(s) were replaced"), "{listed}");
    assert!(listed.contains("--allow-secret-values"), "{listed}");

    // Без флага сервера раскрытие отклоняется, запрос в API не уходит.
    let before = api.received_requests().await.unwrap().len();
    let result = call(
        &client,
        "list_secrets",
        json!({"org_slug": "acme", "repo_slug": "web", "reveal_values": true}),
    )
    .await;
    assert!(error_text(&result).contains("--allow-secret-values"));
    assert_eq!(api.received_requests().await.unwrap().len(), before);

    // Тело ошибки эндпоинта секретов тоже очищается.
    let result = call(&client, "list_secrets", json!({"repo_id": "42"})).await;
    let message = error_text(&result);
    assert!(
        message.contains("HTTP 400") && message.contains("bad group"),
        "{message}"
    );
    assert!(!message.contains("ZGVwbG95LXZhbHVl"), "{message}");

    // `api_request` проходит то же вырезание: и ответ, и тело ошибки.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/repos/acme/web/secrets"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let raw = text(&result);
    assert!(raw.contains(r#""value":"[redacted]""#), "{raw}");
    assert!(!raw.contains("ZGVwbG95LXZhbHVl"), "{raw}");
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/repos/id:42/secrets"}),
    )
    .await;
    let message = error_text(&result);
    assert!(!message.contains("ZGVwbG95LXZhbHVl"), "{message}");

    // Сохранение ответа эндпоинта секретов в файл обошло бы вырезание — отказ без запроса.
    let before = api.received_requests().await.unwrap().len();
    let target = std::env::temp_dir().join("sourcecraft-mcp-test-secrets.json");
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "GET",
            "path": "/repos/acme/web/secrets",
            "save_to": target.display().to_string()
        }),
    )
    .await;
    let message = error_text(&result);
    assert!(message.contains("`save_to` cannot be used"), "{message}");
    assert!(!target.exists());
    assert_eq!(api.received_requests().await.unwrap().len(), before);

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn secret_values_are_revealed_only_on_request() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/secrets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(secrets_body()))
        .mount(&api)
        .await;
    let (client, server) = start_with(
        &api,
        ServerOptions {
            allow_secret_values: true,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .await;

    let tools = client.list_all_tools().await.unwrap();
    let list = tools.iter().find(|t| t.name == "list_secrets").unwrap();
    assert_eq!(
        list.input_schema["properties"]["reveal_values"]["type"],
        "boolean"
    );

    // Флаг сервера сам по себе значения не раскрывает.
    let result = call(
        &client,
        "list_secrets",
        json!({"org_slug": "acme", "repo_slug": "web"}),
    )
    .await;
    let hidden = text(&result);
    assert!(!hidden.contains("ZGVwbG95LXZhbHVl"), "{hidden}");
    assert!(hidden.contains("reveal_values"), "{hidden}");

    let result = call(
        &client,
        "list_secrets",
        json!({"org_slug": "acme", "repo_slug": "web", "reveal_values": true}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["secrets"][0]["value"], "ZGVwbG95LXZhbHVl");
    assert_eq!(value["secrets"][0]["value_text"], "deploy-value");
    assert_eq!(value["secrets"][1]["value_text"], "eu-west");

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn secret_set_encodes_value() {
    let api = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/repos/acme/web/secrets/DEPLOY_TOKEN"))
        .and(body_json(json!({"value": "ZGVwbG95LXZhbHVl"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "operation_id": "op-1",
            "status_url": "/operations/secrets/id:op-1"
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/id:42/secrets/BAD"))
        .respond_with(ResponseTemplate::new(400).set_body_string("cannot store 'plain-echo'"))
        .mount(&api)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/acme/web/secrets/ECHO"))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(json!({"message": "plain-echo is already used"})),
        )
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("token.txt"), "deploy-value\n").unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root))
        .unwrap()
        .with_base_dir(&root);
    let (client, server) = start_with(
        &api,
        ServerOptions {
            max_access: AccessLevel::Destructive,
            ..ServerOptions::default()
        },
        files,
    )
    .await;

    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "secret_set").unwrap();
    assert_eq!(
        tool.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    assert!(!tools.iter().any(|t| t.name == "set_secret"));

    // Значение из файла: перевод строки убран, тело — base64, метод PUT.
    let result = call(
        &client,
        "secret_set",
        json!({
            "org_slug": "acme", "repo_slug": "web", "key": "DEPLOY_TOKEN",
            "value_file": "token.txt"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert!(text(&result).contains("op-1"));

    // Тексты ошибок не повторяют значение.
    let result = call(
        &client,
        "secret_set",
        json!({"repo_id": "42", "key": "BAD", "value": "plain-echo"}),
    )
    .await;
    let message = error_text(&result);
    assert!(message.contains("HTTP 400"), "{message}");
    assert!(!message.contains("plain-echo"), "{message}");
    let result = call(
        &client,
        "secret_set",
        json!({"org_slug": "acme", "repo_slug": "web", "key": "ECHO", "value": "plain-echo"}),
    )
    .await;
    let message = error_text(&result);
    assert!(message.contains("already used"), "{message}");
    assert!(!message.contains("plain-echo"), "{message}");

    // Сгенерированный `set_secret` недоступен и подсказывает `secret_set`.
    let result = call(
        &client,
        "set_secret",
        json!({"org_slug": "acme", "repo_slug": "web", "key": "K", "body": {"value": "eA=="}}),
    )
    .await;
    assert!(error_text(&result).contains("secret_set"));

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn jq_filter_runs_after_redaction_and_before_size_limit() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pull_requests": [
                {"id": "pr-1", "title": "Fix", "description": "long text", "author": {"slug": "a"}},
                {"id": "pr-2", "title": "Docs", "description": "more text", "author": {"slug": "b"}}
            ],
            "next_page_token": ""
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/secrets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(secrets_body()))
        .mount(&api)
        .await;
    // Ответ больше лимита сервера (64 KiB): без фильтра обрезается, фильтр видит его целиком.
    let items: Vec<Value> = (0..2000)
        .map(|i| json!({"id": i, "payload": "x".repeat(100)}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": items})))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/text"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("plain", "text/plain"))
        .mount(&api)
        .await;
    let (client, server) = start(&api, AccessLevel::Read).await;

    let tools = client.list_all_tools().await.unwrap();
    for name in ["list_repository_pull_requests", "api_request"] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        assert!(
            tool.input_schema["properties"].get("jq").is_some(),
            "{name}"
        );
    }

    // Только выбранные поля.
    let result = call(
        &client,
        "list_repository_pull_requests",
        json!({"org_slug": "acme", "repo_slug": "web", "jq": ".pull_requests[] | {id, title}"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(
        text(&result),
        r#"[{"id":"pr-1","title":"Fix"},{"id":"pr-2","title":"Docs"}]"#
    );

    // Фильтр получает уже очищенный ответ: значения секретов ему недоступны.
    for filter in [".. | strings", "[.secrets[].value | @base64d?]", "tojson"] {
        let result = call(
            &client,
            "list_secrets",
            json!({"org_slug": "acme", "repo_slug": "web", "jq": filter}),
        )
        .await;
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        let filtered = text(&result);
        assert!(
            !filtered.contains("ZGVwbG95LXZhbHVl") && !filtered.contains("ZXUtd2VzdA=="),
            "{filtered}"
        );
        assert!(!filtered.contains("deploy-value"), "{filtered}");
        assert!(filtered.contains("were replaced"), "{filtered}");
    }

    // Ошибка разбора — до запроса в API.
    let before = api.received_requests().await.unwrap().len();
    for bad in [json!(".pull_requests["), json!("env"), json!(42)] {
        let result = call(
            &client,
            "list_repository_pull_requests",
            json!({"org_slug": "acme", "repo_slug": "web", "jq": bad}),
        )
        .await;
        assert!(error_text(&result).contains("argument `jq`"));
    }
    assert_eq!(api.received_requests().await.unwrap().len(), before);

    // Ошибка выполнения — управляемая, с пояснением, что запрос выполнен.
    let result = call(
        &client,
        "list_repository_pull_requests",
        json!({"org_slug": "acme", "repo_slug": "web", "jq": ".pull_requests[] | .title.x"}),
    )
    .await;
    let message = error_text(&result);
    assert!(
        message.contains("the API request succeeded (HTTP 200)"),
        "{message}"
    );
    assert!(message.contains("cannot index"), "{message}");

    // Лимит размера — после фильтра.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/big"}),
    )
    .await;
    assert!(text(&result).contains("truncated"));
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/big", "jq": "[.items[].id] | length"}),
    )
    .await;
    assert_eq!(text(&result), "2000");
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/big", "jq": ".items[]"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let filtered = text(&result);
    assert!(filtered.starts_with(r#"[{"id":0,"#), "{}", &filtered[..50]);
    assert!(filtered.contains("truncated"));
    assert!(filtered.len() < 64 * 1024 + 500, "{}", filtered.len());

    // Ответ не в JSON возвращается без фильтра, с пояснением.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/text", "jq": ".a"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false));
    let plain = text(&result);
    assert!(plain.starts_with("plain"), "{plain}");
    assert!(plain.contains("not JSON"), "{plain}");

    client.cancel().await.unwrap();
    server.await.unwrap();
}

/// Токен в расширениях запроса, как его кладёт HTTP-транспорт бинарника.
#[derive(Clone)]
struct TestRequestToken(ApiToken);

#[tokio::test]
async fn per_request_token_is_taken_from_each_call() {
    use std::sync::Arc;

    use rmcp::model::Extensions;
    use sourcecraft_mcp_core::TokenSource;

    let api = MockServer::start().await;
    for token in ["agent-one-token", "agent-two-token"] {
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": token})))
            .expect(1)
            .mount(&api)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer echo-me-token"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"message":"bad token echo-me-token"}"#),
        )
        .expect(1)
        .mount(&api)
        .await;

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
            local_machine: false,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
        TokenSource::PerRequest(Arc::new(|ext: &Extensions| {
            ext.get::<TestRequestToken>().map(|t| t.0.clone())
        })),
    )
    .unwrap();

    let call = |token: Option<&str>| {
        let mut ext = Extensions::new();
        if let Some(token) = token {
            ext.insert(TestRequestToken(ApiToken::new(token)));
        }
        let server = server.clone();
        async move {
            server
                .call_with_extensions("get_my_profile", JsonObject::new(), &ext)
                .await
                .unwrap()
        }
    };

    for token in ["agent-one-token", "agent-two-token"] {
        let result = call(Some(token)).await;
        assert_ne!(result.is_error, Some(true), "{}", text(&result));
    }

    let missing = call(None).await;
    assert_eq!(missing.is_error, Some(true));
    assert!(
        text(&missing).contains("Authorization: Bearer"),
        "{}",
        text(&missing)
    );

    let echoed = call(Some("echo-me-token")).await;
    assert_eq!(echoed.is_error, Some(true));
    assert!(text(&echoed).contains("HTTP 401"), "{}", text(&echoed));
    assert!(
        !text(&echoed).contains("echo-me-token"),
        "{}",
        text(&echoed)
    );

    // Поиск по операциям не требует токена.
    let search = server
        .call_with_extensions(
            "api_search",
            args(json!({"query": "profile"})),
            &Extensions::new(),
        )
        .await
        .unwrap();
    assert_ne!(search.is_error, Some(true), "{}", text(&search));
}

/// В HTTP-режиме фильтр `jq` получает ответ, из которого уже вырезан токен запроса:
/// преобразование (`@base64`, ключи объекта) не выводит токен мимо итоговой очистки.
#[tokio::test]
async fn jq_filter_in_http_mode_sees_scrubbed_request_token() {
    use std::sync::Arc;

    use rmcp::model::Extensions;
    use sourcecraft_mcp_core::TokenSource;

    let token = "jq-agent-token";
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"login": "agent", "echo": token, token: 1})),
        )
        .expect(1)
        .mount(&api)
        .await;

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
            local_machine: false,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
        TokenSource::PerRequest(Arc::new(|ext: &Extensions| {
            ext.get::<TestRequestToken>().map(|t| t.0.clone())
        })),
    )
    .unwrap();

    let mut ext = Extensions::new();
    ext.insert(TestRequestToken(ApiToken::new(token)));
    let result = server
        .call_with_extensions(
            "get_my_profile",
            args(json!({"jq": "{login, encoded: (.echo | @base64), keys: (keys | map(@base64))}"})),
            &ext,
        )
        .await
        .unwrap();
    let output = text(&result);
    assert_ne!(result.is_error, Some(true), "{output}");
    assert!(output.contains("\"agent\""), "{output}");
    assert!(!output.contains(token), "{output}");
    // base64 от токена.
    assert!(!output.contains("anEtYWdlbnQtdG9rZW4"), "{output}");
}
