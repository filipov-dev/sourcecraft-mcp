//! Сквозные тесты HTTP-режима: настоящий HTTP-сервер на loopback, SourceCraft API подменён
//! `wiremock`, запросы MCP отправляются вручную через `reqwest`.

use std::time::Duration;

use serde_json::{Value, json};
use sourcecraft_api::AccessLevel;
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_mcp_core::{FilePolicy, ServerOptions, SourceCraftServer};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

const PROTOCOL_VERSION: &str = "2025-06-18";

struct TestServer {
    base: String,
    http: reqwest::Client,
    cancel: CancellationToken,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn start(api: &MockServer, max_access: AccessLevel, origins: &[&str]) -> TestServer {
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
            max_access,
            local_machine: false,
            ..ServerOptions::default()
        },
        FilePolicy::disabled(),
        token_source(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let options = HttpOptions {
        allowed_hosts: allowed_hosts(addr, &[]),
        allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
    };
    let cancel = CancellationToken::new();
    let app = router(server, &options, cancel.clone());
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    TestServer {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        cancel,
    }
}

impl TestServer {
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{MCP_PATH}", self.base))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string())
    }

    /// JSON-RPC запрос с токеном; возвращает статус и тело ответа как текст.
    async fn rpc(&self, token: &str, body: &Value) -> (u16, String) {
        let response = self.post(body).bearer_auth(token).send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn call_tool(&self, token: &str, name: &str, args: Value) -> Value {
        let (status, body) = self
            .rpc(
                token,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {"name": name, "arguments": args}
                }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let message: Value = serde_json::from_str(&body).unwrap();
        message["result"].clone()
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
}

fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn health_needs_no_token() {
    let api = MockServer::start().await;
    let server = start(&api, AccessLevel::Read, &[]).await;
    let response = server
        .http
        .get(format!("{}{HEALTH_PATH}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["mcp_endpoint"], MCP_PATH);
}

#[tokio::test]
async fn missing_or_malformed_token_is_401_before_mcp() {
    let api = MockServer::start().await;
    let server = start(&api, AccessLevel::Read, &[]).await;

    let response = server.post(&initialize()).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let challenge = response.headers()["www-authenticate"].to_str().unwrap();
    assert!(challenge.starts_with("Bearer "), "{challenge}");
    assert!(!challenge.contains("error="), "{challenge}");
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Authorization: Bearer")
    );

    for value in [
        "Basic dXNlcjpwYXNz",
        "Bearer",
        "Bearer   ",
        "bearer a b",
        "Token abc",
    ] {
        let response = server
            .post(&initialize())
            .header("authorization", value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401, "{value}");
        let challenge = response.headers()["www-authenticate"].to_str().unwrap();
        assert!(challenge.contains("error=\"invalid_request\""), "{value}");
        let body = response.text().await.unwrap();
        assert!(
            !body.contains("dXNlcjpwYXNz") && !body.contains("abc"),
            "{body}"
        );
    }

    // Два заголовка Authorization — неоднозначно, отклоняется.
    let response = server
        .post(&initialize())
        .header("authorization", "Bearer one")
        .header("authorization", "Bearer two")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // Схема без учёта регистра: `bearer` тоже подходит.
    let response = server
        .post(&initialize())
        .header("authorization", "bearer fake-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    // До API ничего не дошло.
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn browser_origin_and_foreign_host_are_rejected() {
    let api = MockServer::start().await;
    let server = start(&api, AccessLevel::Read, &["http://localhost:6274"]).await;

    let (status, _) = server.rpc("fake-token", &initialize()).await;
    assert_eq!(status, 200, "без Origin (не браузер) запрос проходит");

    let response = server
        .post(&initialize())
        .bearer_auth("fake-token")
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);

    let response = server
        .post(&initialize())
        .bearer_auth("fake-token")
        .header("origin", "http://localhost:6274")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    // DNS rebinding: имя злоумышленника в Host.
    let response = server
        .post(&initialize())
        .bearer_auth("fake-token")
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
}

#[tokio::test]
async fn origin_is_rejected_by_default() {
    let api = MockServer::start().await;
    let server = start(&api, AccessLevel::Read, &[]).await;
    let response = server
        .post(&initialize())
        .bearer_auth("fake-token")
        .header("origin", "http://localhost:3000")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
}

#[tokio::test]
async fn initialize_and_tools_have_no_local_machine_features() {
    let api = MockServer::start().await;
    let server = start(&api, AccessLevel::Destructive, &[]).await;

    let (status, body) = server.rpc("fake-token", &initialize()).await;
    assert_eq!(status, 200, "{body}");
    let init: Value = serde_json::from_str(&body).unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    for absent in ["secret exec", "secret write", "secret get", "--allow-path"] {
        assert!(!instructions.contains(absent), "{absent}");
    }
    assert!(instructions.contains("HTTP mode"));

    let (status, body) = server
        .rpc(
            "fake-token",
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let list: Value = serde_json::from_str(&body).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"api_request") && names.contains(&"secret_set"));
    assert!(!names.contains(&"upload_issue_attachment"), "{names:?}");
    for arg in ["body_file", "save_to", "value_file"] {
        assert!(!body.contains(arg), "{arg}");
    }

    // Файловые аргументы, переданные вопреки схеме, отклоняются, к API запрос не идёт.
    let result = server
        .call_tool(
            "fake-token",
            "api_request",
            json!({"method": "GET", "path": "/user", "save_to": "/tmp/out.json"}),
        )
        .await;
    assert_eq!(result["isError"], true);
    assert!(result_text(&result).contains("HTTP mode"));
    let result = server
        .call_tool(
            "fake-token",
            "upload_issue_attachment",
            json!({"org_slug": "a", "repo_slug": "b", "issue_slug": "c", "file": "/etc/hosts"}),
        )
        .await;
    assert_eq!(result["isError"], true);
    assert!(result_text(&result).contains("HTTP mode"));
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn each_request_forwards_its_own_token() {
    let api = MockServer::start().await;
    for token in ["agent-alpha-token", "agent-beta-token"] {
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": token})))
            .expect(1)
            .mount(&api)
            .await;
    }
    let server = start(&api, AccessLevel::Read, &[]).await;

    for token in ["agent-alpha-token", "agent-beta-token"] {
        let result = server.call_tool(token, "get_my_profile", json!({})).await;
        assert_ne!(result["isError"], true, "{result}");
    }
    // Ожидания wiremock (`expect(1)` на каждый токен) проверяются при drop сервера API.
    api.verify().await;
}

#[tokio::test]
async fn rejected_token_never_appears_in_results() {
    const TOKEN: &str = "leaky-token-7d1e";
    let api = MockServer::start().await;
    Mock::given(path("/user"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(401)
                .set_body_string(format!(r#"{{"message":"invalid token: {auth}"}}"#))
        })
        .mount(&api)
        .await;
    Mock::given(path("/repos/acme/web"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"note": format!("you are {auth}")}))
        })
        .mount(&api)
        .await;
    let server = start(&api, AccessLevel::Read, &[]).await;

    let (status, body) = server
        .rpc(
            TOKEN,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "get_my_profile", "arguments": {}}
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("HTTP 401"), "{body}");
    assert!(body.contains("\"isError\":true"), "{body}");
    assert!(!body.contains(TOKEN), "{body}");

    let (_, body) = server
        .rpc(
            TOKEN,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "api_request",
                    "arguments": {"method": "GET", "path": "/repos/acme/web"}
                }
            }),
        )
        .await;
    assert!(body.contains("you are"), "{body}");
    assert!(!body.contains(TOKEN), "{body}");
}

#[test]
fn bearer_token_parsing() {
    let headers = |values: &[&str]| {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        }
        map
    };
    assert_eq!(
        bearer_token(&headers(&["Bearer abc.DEF-123_~+/="]))
            .unwrap()
            .expose(),
        "abc.DEF-123_~+/="
    );
    assert_eq!(
        bearer_token(&headers(&["  BEARER   tok  "]))
            .unwrap()
            .expose(),
        "tok"
    );
    assert_eq!(
        bearer_token(&headers(&[])).unwrap_err(),
        AuthProblem::Missing
    );
    let long = format!("Bearer {}", "a".repeat(MAX_TOKEN_LEN + 1));
    for bad in ["Bearer", "Basic abc", "Bearer a b", long.as_str()] {
        assert_eq!(
            bearer_token(&headers(&[bad])).unwrap_err(),
            AuthProblem::Malformed,
            "{bad}"
        );
    }
    assert_eq!(
        bearer_token(&headers(&["Bearer a", "Bearer b"])).unwrap_err(),
        AuthProblem::Malformed
    );
}

#[test]
fn allowed_hosts_include_specific_listen_address() {
    let loopback = allowed_hosts("127.0.0.1:8765".parse().unwrap(), &[]);
    assert_eq!(loopback, LOOPBACK_HOSTS);
    let any = allowed_hosts("0.0.0.0:8765".parse().unwrap(), &["mcp.example.com".into()]);
    assert_eq!(any, ["localhost", "127.0.0.1", "::1", "mcp.example.com"]);
    let specific = allowed_hosts("10.0.0.5:8765".parse().unwrap(), &[" ".into()]);
    assert_eq!(specific, ["localhost", "127.0.0.1", "::1", "10.0.0.5"]);
}
