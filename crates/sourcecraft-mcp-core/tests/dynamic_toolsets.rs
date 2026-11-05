//! Динамические наборы инструментов: начальный список, `enable_toolset` с уведомлением
//! `notifications/tools/list_changed` (через сессию с `initialize` и через подписку протокола
//! 2026-07-28) и уровень доступа. SourceCraft API подменён `wiremock`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, JsonObject, ProtocolVersion, ServerNotification,
    SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, ClientServiceExt, RoleClient, ServiceExt};
use serde_json::{Value, json};
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_api::{AccessLevel, ApiToken};
use sourcecraft_mcp_core::{FilePolicy, ServerOptions, SourceCraftServer};
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CORE_TOOLS: [&str; 5] = [
    "api_request",
    "api_search",
    "disable_toolset",
    "enable_toolset",
    "list_toolsets",
];

/// Клиент, который считает уведомления о смене списка инструментов.
#[derive(Clone, Default)]
struct CountingClient {
    changes: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ClientHandler for CountingClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn server(api: &MockServer, max_access: AccessLevel, dynamic: bool) -> SourceCraftServer {
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
            dynamic_toolsets: dynamic,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap()
}

fn spawn(server: SourceCraftServer) -> tokio::io::DuplexStream {
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    client_io
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

async fn call<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
    name: &str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(value)))
        .await
        .unwrap()
}

async fn tool_names<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn enable_toolset_adds_tools_and_notifies() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "i-1"})))
        .expect(1)
        .mount(&api)
        .await;

    let handler = CountingClient::default();
    let client = handler
        .clone()
        .serve(spawn(server(&api, AccessLevel::Read, true)))
        .await
        .unwrap();

    let info = client.peer_info().unwrap();
    let tools_capability = info.capabilities.tools.as_ref().unwrap();
    assert_eq!(tools_capability.list_changed, Some(true));
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .contains("`enable_toolset`")
    );

    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    // До включения набора инструмент недоступен, ошибка подсказывает `enable_toolset`.
    let result = call(
        &client,
        "get_issue",
        json!({"org_slug": "acme", "repo_slug": "web", "issue_slug": "1"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`enable_toolset`"),
        "{}",
        text(&result)
    );

    let listed: Value =
        serde_json::from_str(&text(&call(&client, "list_toolsets", json!({})).await)).unwrap();
    let issues = listed["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["toolset"] == "issues")
        .unwrap();
    assert_eq!(issues["enabled"], false);

    let result = call(&client, "enable_toolset", json!({"toolset": "issues"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 1);

    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_issue"), "{names:?}");
    // Уровень доступа действует и для включённого набора.
    assert!(!names.iter().any(|n| n == "create_issue"), "{names:?}");
    assert!(!names.iter().any(|n| n == "update_issue"), "{names:?}");

    let result = call(
        &client,
        "get_issue",
        json!({"org_slug": "acme", "repo_slug": "web", "issue_slug": "1"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));

    // Повторное включение не меняет список и не шлёт уведомление.
    call(&client, "enable_toolset", json!({"toolset": "issues"})).await;
    let result = call(&client, "disable_toolset", json!({"toolset": "issues"})).await;
    assert_eq!(result.is_error, Some(false));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 2);
    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tools_appear_only_with_write_access() {
    let api = MockServer::start().await;
    let client = ().serve(spawn(server(&api, AccessLevel::Write, true))).await.unwrap();
    call(
        &client,
        "enable_toolset",
        json!({"toolset": "issues,secrets"}),
    )
    .await;
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "create_issue"), "{names:?}");
    // Удаление и `secret_set` требуют `Destructive`.
    assert!(!names.iter().any(|n| n == "delete_issue"), "{names:?}");
    assert!(!names.iter().any(|n| n == "secret_set"), "{names:?}");
    assert!(names.iter().any(|n| n == "list_secrets"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn subscription_receives_list_changed() {
    let api = MockServer::start().await;
    let client = ()
        .serve_with_lifecycle(
            spawn(server(&api, AccessLevel::Read, true)),
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let mut subscription = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .unwrap();
    assert_eq!(
        subscription.acknowledged(),
        &SubscriptionFilter::builder().tools_list_changed().build()
    );

    let result = call(&client, "enable_toolset", json!({"toolset": "repository"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let notification = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .expect("tools/list_changed");
    assert!(matches!(
        notification,
        ServerNotification::ToolListChangedNotification(_)
    ));
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_repository"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn static_mode_is_unchanged() {
    let api = MockServer::start().await;
    let client = ().serve(spawn(server(&api, AccessLevel::Read, false))).await.unwrap();
    let info = client.peer_info().unwrap();
    assert_ne!(
        info.capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_issue"));
    assert!(!names.iter().any(|n| n == "enable_toolset"));
    let result = client
        .call_tool(CallToolRequestParams::new("enable_toolset").with_arguments(JsonObject::new()))
        .await;
    assert!(result.is_err(), "неизвестный инструмент — ошибка протокола");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn sessions_are_independent() {
    let api = MockServer::start().await;
    let first = server(&api, AccessLevel::Read, true);
    let second = first.new_session();
    first
        .call("enable_toolset", args(json!({"toolset": "issues"})))
        .await
        .unwrap();
    assert!(first.registry().get("get_issue").is_some());
    assert!(second.registry().get("get_issue").is_none());
    // Клон делит сессию.
    assert!(first.clone().registry().get("get_issue").is_some());
}

/// Токен запроса в расширениях (как его кладёт HTTP-транспорт).
#[derive(Clone)]
struct TestRequestToken(ApiToken);

#[tokio::test]
async fn toolsets_work_with_per_request_tokens() {
    use rmcp::model::Extensions;
    use sourcecraft_mcp_core::TokenSource;
    use wiremock::matchers::header;

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
            dynamic_toolsets: true,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
        TokenSource::PerRequest(Arc::new(|ext: &Extensions| {
            ext.get::<TestRequestToken>().map(|t| t.0.clone())
        })),
    )
    .unwrap();
    let with_token = |token: Option<&str>| {
        let mut ext = Extensions::new();
        if let Some(token) = token {
            ext.insert(TestRequestToken(ApiToken::new(token)));
        }
        ext
    };

    // Управление наборами к API не обращается: токен ему не нужен и не запоминается.
    for (tool, arguments) in [
        ("list_toolsets", json!({})),
        ("enable_toolset", json!({"toolset": "user"})),
    ] {
        let result = server
            .call_with_extensions(tool, args(arguments), &with_token(None))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{tool}: {}", text(&result));
    }
    assert!(server.registry().get("get_my_profile").is_some());

    // Включённый инструмент берёт токен каждого вызова, а не токен времени включения.
    for token in ["agent-one-token", "agent-two-token"] {
        let result = server
            .call_with_extensions(
                "get_my_profile",
                JsonObject::new(),
                &with_token(Some(token)),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        // Мок отвечает только на свой токен (`expect(1)`); отражённый токен вычищается.
        assert!(!text(&result).contains(token), "{}", text(&result));
    }
    let missing = server
        .call_with_extensions("get_my_profile", JsonObject::new(), &with_token(None))
        .await
        .unwrap();
    assert_eq!(missing.is_error, Some(true));
}

/// Инструменты, включённые `enable_toolset`, — те же инструменты операций: принимают `jq`.
#[tokio::test]
async fn enabled_tools_accept_jq() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "i-1",
            "title": "Bug",
            "description": "long text"
        })))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, true);
    server
        .call("enable_toolset", args(json!({"toolset": "issues"})))
        .await
        .unwrap();
    let registry = server.registry();
    let tool = &registry.get("get_issue").unwrap().tool;
    assert!(tool.input_schema["properties"].get("jq").is_some());

    let result = server
        .call(
            "get_issue",
            args(json!({
                "org_slug": "acme",
                "repo_slug": "web",
                "issue_slug": "1",
                "jq": "{id, title}"
            })),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"id":"i-1","title":"Bug"}"#);
}

/// Пользовательские инструменты настроил оператор: в динамическом режиме они есть сразу,
/// не входят ни в один набор и переживают `enable_toolset`/`disable_toolset`. Уровень
/// доступа действует как обычно.
#[tokio::test]
async fn custom_tools_are_registered_from_start() {
    use sourcecraft_mcp_core::CustomTool;

    let custom = |name: &str, method: &str| {
        CustomTool::compile(
            serde_json::from_value(json!({
                "name": name,
                "description": "Custom tool",
                "method": method,
                "path": "/repos/{org}/{repo}/issues",
                "arguments": [
                    {"name": "org", "required": true},
                    {"name": "repo", "required": true}
                ]
            }))
            .unwrap(),
            "tools.toml",
        )
        .unwrap()
    };
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/web/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issues": []})))
        .expect(1)
        .mount(&api)
        .await;
    let client = ApiClient::new(ClientConfig {
        base_url: api.uri().parse().unwrap(),
        token: Some(ApiToken::new("test-token-value")),
        timeout: Duration::from_secs(10),
        max_response_bytes: 64 * 1024,
    })
    .unwrap();
    let server = SourceCraftServer::new(
        client,
        ServerOptions {
            dynamic_toolsets: true,
            custom_tools: vec![custom("my_issues", "GET"), custom("new_issue", "POST")],
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap();
    let names = |server: &SourceCraftServer| {
        let mut names: Vec<String> = server
            .registry()
            .iter()
            .map(|t| t.tool.name.to_string())
            .collect();
        names.sort();
        names
    };
    let mut expected: Vec<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
    expected.push("my_issues".to_owned());
    expected.sort();
    assert_eq!(names(&server), expected);
    // Выше уровня доступа — не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = server
        .call("new_issue", args(json!({"org": "a", "repo": "b"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));

    for tool in ["enable_toolset", "disable_toolset"] {
        let result = server
            .call(tool, args(json!({"toolset": "issues"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        assert!(server.registry().get("my_issues").is_some(), "{tool}");
        // Пользовательские инструменты не попадают в списки добавленных и убранных.
        assert!(!text(&result).contains("my_issues"), "{}", text(&result));
    }
    // Новая сессия тоже начинается с пользовательских инструментов.
    assert!(server.new_session().registry().get("my_issues").is_some());

    let result = server
        .call("my_issues", args(json!({"org": "acme", "repo": "web"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
}
