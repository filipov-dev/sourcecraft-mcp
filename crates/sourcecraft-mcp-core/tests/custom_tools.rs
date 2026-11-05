//! Пользовательские инструменты: регистрация по уровню доступа и вызов с теми же
//! ограничениями, что у `api_request`. SourceCraft API подменён `wiremock`.

use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_api::{AccessLevel, ApiToken};
use sourcecraft_mcp_core::{CustomTool, FilePolicy, ServerOptions, SourceCraftServer};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool(value: Value) -> CustomTool {
    CustomTool::compile(serde_json::from_value(value).unwrap(), "tools.toml").unwrap()
}

fn tools() -> Vec<CustomTool> {
    vec![
        tool(json!({
            "name": "open_prs",
            "description": "Open pull requests of a repository",
            "method": "GET",
            "path": "/repos/{org}/{repo}/pulls",
            "query": {"state": "open", "page_size": "{limit}"},
            "jq": ".pull_requests[] | {id, title}",
            "arguments": [
                {"name": "org", "required": true},
                {"name": "repo", "required": true},
                {"name": "limit", "type": "integer"}
            ]
        })),
        tool(json!({
            "name": "repo_section",
            "description": "Any repository section",
            "method": "GET",
            "path": "/repos/{org}/{repo}/{section}",
            "arguments": [
                {"name": "org", "required": true},
                {"name": "repo", "required": true},
                {"name": "section", "required": true}
            ]
        })),
        tool(json!({
            "name": "post_section",
            "description": "POST to a repository section",
            "method": "POST",
            "path": "/repos/{org}/{repo}/{section}",
            "body": {"title": "{title}"},
            "arguments": [
                {"name": "org", "required": true},
                {"name": "repo", "required": true},
                {"name": "section", "required": true},
                {"name": "title", "required": true}
            ]
        })),
        tool(json!({
            "name": "drop_repo",
            "description": "Delete a repository",
            "method": "DELETE",
            "path": "/repos/{org}/{repo}",
            "arguments": [{"name": "org", "required": true}, {"name": "repo", "required": true}]
        })),
    ]
}

fn server(api: &MockServer, max_access: AccessLevel, custom: Vec<CustomTool>) -> SourceCraftServer {
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
            custom_tools: custom,
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

async fn call(server: &SourceCraftServer, name: &str, value: Value) -> CallToolResult {
    server.call(name, args(value)).await.unwrap()
}

#[tokio::test]
async fn custom_tools_follow_access_level() {
    let api = MockServer::start().await;
    let read = server(&api, AccessLevel::Read, tools());
    assert!(read.registry().get("open_prs").is_some());
    assert!(read.registry().get("repo_section").is_some());
    assert!(read.registry().get("post_section").is_none());
    assert!(read.registry().get("drop_repo").is_none());
    let result = call(&read, "drop_repo", json!({"org": "a", "repo": "b"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("--allow-destructive"),
        "{}",
        text(&result)
    );
    assert!(api.received_requests().await.unwrap().is_empty());

    let write = server(&api, AccessLevel::Write, tools());
    let registry = write.registry();
    let tool = &registry.get("post_section").unwrap().tool;
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(false)
    );
    assert!(write.registry().get("drop_repo").is_none());

    let all = server(&api, AccessLevel::Destructive, tools());
    let registry = all.registry();
    let drop = &registry.get("drop_repo").unwrap().tool;
    assert_eq!(
        drop.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
}

#[tokio::test]
async fn custom_tool_calls_the_api() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("page_size", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pull_requests": [
                {"id": "pr-1", "title": "Fix", "description": "long"},
                {"id": "pr-2", "title": "Docs", "description": "long"}
            ]
        })))
        .expect(2)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/secrets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "secrets": [{"key": "DEPLOY", "value": "ZGVwbG95LXZhbHVl"}]
        })))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/web/issues"))
        .and(body_json(json!({"title": "\"}, \"x\": {"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "i-1"})))
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Destructive, tools());

    let result = call(
        &server,
        "open_prs",
        json!({"org": "acme", "repo": "web", "limit": 2}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(
        text(&result),
        r#"[{"id":"pr-1","title":"Fix"},{"id":"pr-2","title":"Docs"}]"#
    );
    // Фильтр из вызова применяется после фильтра инструмента.
    let result = call(
        &server,
        "open_prs",
        json!({"org": "acme", "repo": "web", "limit": "2", "jq": ".id"}),
    )
    .await;
    assert_eq!(text(&result), r#"["pr-1","pr-2"]"#);

    // Секреты вырезаются, как у встроенных инструментов.
    let result = call(
        &server,
        "repo_section",
        json!({"org": "acme", "repo": "web", "section": "secrets"}),
    )
    .await;
    let listed = text(&result);
    assert!(!listed.contains("ZGVwbG95LXZhbHVl"), "{listed}");
    assert!(listed.contains("[redacted]"), "{listed}");

    // Тело — JSON-значение, а не склейка строк.
    let result = call(
        &server,
        "post_section",
        json!({"org": "acme", "repo": "web", "section": "issues", "title": "\"}, \"x\": {"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));

    // Аргумент не может выйти из сегмента пути.
    let before = api.received_requests().await.unwrap().len();
    for section in ["..", "a/../../user", "pulls?x=1"] {
        let result = call(
            &server,
            "repo_section",
            json!({"org": "acme", "repo": "web", "section": section}),
        )
        .await;
        if section.contains('?') {
            // `?` кодируется в сегмент: запрос уходит на `/pulls%3Fx%3D1`, а не с query.
            continue;
        }
        assert_eq!(result.is_error, Some(true), "{section}");
    }
    let requests = api.received_requests().await.unwrap();
    assert_eq!(requests.len(), before + 1);
    let last = requests.last().unwrap();
    assert_eq!(last.url.path(), "/repos/acme/web/pulls%3Fx%3D1");
    assert_eq!(last.url.query(), None);

    // Вызов не может требовать уровня выше объявленного у инструмента: `roles` — это
    // `add_repo_roles` (удаление), а инструмент зарегистрирован с уровнем записи.
    let result = call(
        &server,
        "post_section",
        json!({"org": "acme", "repo": "web", "section": "roles", "title": "x"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("registered with write access"),
        "{}",
        text(&result)
    );

    // Запись секрета сырым запросом запрещена и для пользовательских инструментов.
    let secret = tool(json!({
        "name": "raw_secret",
        "description": "Set a secret",
        "method": "PUT",
        "path": "/repos/{org}/{repo}/secrets/{key}",
        "body": {"value": "{value}"},
        "arguments": [
            {"name": "org", "required": true},
            {"name": "repo", "required": true},
            {"name": "key", "required": true},
            {"name": "value", "required": true}
        ]
    }));
    let raw = self::server(&api, AccessLevel::Destructive, vec![secret]);
    let result = call(
        &raw,
        "raw_secret",
        json!({"org": "acme", "repo": "web", "key": "K", "value": "v"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("secret_set"), "{}", text(&result));
}

#[test]
fn name_clash_with_builtin_stops_the_server() {
    let clash = tool(json!({
        "name": "get_repository",
        "description": "Shadow a built-in tool",
        "method": "GET",
        "path": "/user"
    }));
    let client = ApiClient::new(ClientConfig {
        base_url: "https://api.sourcecraft.tech".parse().unwrap(),
        token: Some(ApiToken::new("test-token-value")),
        timeout: Duration::from_secs(10),
        max_response_bytes: 64 * 1024,
    })
    .unwrap();
    let err = SourceCraftServer::new(
        client,
        ServerOptions {
            custom_tools: vec![clash],
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap_err();
    assert!(err.contains("built-in tool"), "{err}");
}

/// Имена служебных инструментов заняты всегда, в том числе инструментов динамических наборов
/// без `--dynamic-toolsets` и `secret_copy` без `--allow-write`: файл не должен ломаться
/// от смены флагов.
#[test]
fn name_clash_with_service_tools_stops_the_server() {
    for name in [
        "list_toolsets",
        "enable_toolset",
        "disable_toolset",
        "secret_copy",
    ] {
        let clash = tool(json!({
            "name": name,
            "description": "Shadow a toolset tool",
            "method": "GET",
            "path": "/user"
        }));
        let client = ApiClient::new(ClientConfig {
            base_url: "https://api.sourcecraft.tech".parse().unwrap(),
            token: Some(ApiToken::new("test-token-value")),
            timeout: Duration::from_secs(10),
            max_response_bytes: 64 * 1024,
        })
        .unwrap();
        let err = SourceCraftServer::new(
            client,
            ServerOptions {
                custom_tools: vec![clash],
                ..ServerOptions::default()
            },
            FilePolicy::new(&[]).unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("built-in tool"), "{name}: {err}");
    }
}

#[derive(Clone)]
struct TestRequestToken(ApiToken);

/// HTTP-режим: пользовательский инструмент идёт в API с токеном вызова, а не с токеном
/// сервера (его нет), и без токена завершается управляемой ошибкой.
#[tokio::test]
async fn custom_tools_in_http_mode_use_request_token() {
    use std::sync::Arc;

    use rmcp::model::Extensions;
    use sourcecraft_mcp_core::TokenSource;

    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/pulls"))
        .and(header("authorization", "Bearer agent-token-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pull_requests": [{"id": "1", "title": "Fix", "body": "agent-token-value"}]
        })))
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
            custom_tools: tools(),
            local_machine: false,
            ..ServerOptions::default()
        },
        FilePolicy::disabled(),
        TokenSource::PerRequest(Arc::new(|ext: &Extensions| {
            ext.get::<TestRequestToken>().map(|t| t.0.clone())
        })),
    )
    .unwrap();
    assert!(server.registry().get("open_prs").is_some());

    let mut ext = Extensions::new();
    ext.insert(TestRequestToken(ApiToken::new("agent-token-value")));
    let call_args = args(json!({
        "org": "acme",
        "repo": "app",
        "jq": "{title, body}"
    }));
    let result = server
        .call_with_extensions("open_prs", call_args.clone(), &ext)
        .await
        .unwrap();
    let output = text(&result);
    assert_ne!(result.is_error, Some(true), "{output}");
    assert!(output.contains("Fix"), "{output}");
    assert!(!output.contains("agent-token-value"), "{output}");

    let missing = server
        .call_with_extensions("open_prs", call_args, &Extensions::new())
        .await
        .unwrap();
    assert_eq!(missing.is_error, Some(true), "{}", text(&missing));
}
