//! Тесты исполнителя запросов на `wiremock`, без реальной сети.

use std::time::Duration;

use serde_json::json;
use sourcecraft_api::ApiToken;
use sourcecraft_api::HttpMethod;
use sourcecraft_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, MultipartPart, RequestBody, ResponseBody,
};
use wiremock::matchers::{body_json, body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Узнаваемое фиктивное значение, чтобы искать утечки в текстах ошибок.
const TOKEN: &str = "test-token-LEAKCHECK-7f3a9";

fn config(base: &str) -> ClientConfig {
    ClientConfig {
        base_url: url::Url::parse(base).unwrap(),
        token: Some(ApiToken::new(TOKEN)),
        timeout: Duration::from_secs(5),
        max_response_bytes: 64 * 1024,
    }
}

fn client(server: &MockServer) -> ApiClient {
    ApiClient::new(config(&server.uri())).unwrap()
}

fn get(path: &str) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Get,
        path: path.into(),
        query: Vec::new(),
        body: RequestBody::Empty,
        save_to: None,
    }
}

#[tokio::test]
async fn sends_bearer_auth_and_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header(
            "user-agent",
            concat!("sourcecraft-mcp/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "me"})))
        .expect(1)
        .mount(&server)
        .await;

    let response = client(&server).execute(get("/me")).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, ResponseBody::Json(json!({"login": "me"})));
    assert!(!response.truncated);
}

#[tokio::test]
async fn encodes_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/org/repo/pulls"))
        .and(query_param("filter", "a b&c=d"))
        .and(query_param("page_size", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = get("/repos/org/repo/pulls");
    request.query = vec![
        ("filter".into(), "a b&c=d".into()),
        ("page_size".into(), "10".into()),
    ];
    let response = client(&server).execute(request).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!([])));
}

#[tokio::test]
async fn keeps_base_url_path_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(2)
        .mount(&server)
        .await;

    for base in [
        format!("{}/api/v1", server.uri()),
        format!("{}/api/v1/", server.uri()),
    ] {
        let client = ApiClient::new(config(&base)).unwrap();
        let response = client.execute(get("/me")).await.unwrap();
        assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    }
}

#[tokio::test]
async fn json_body_is_sent_as_object() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/issues"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"title": "t", "labels": ["x"]})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "1"})))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server);
    let mut request = get("/repos/org/repo/issues");
    request.method = HttpMethod::Post;
    request.body = RequestBody::Json(json!({"title": "t", "labels": ["x"]}));
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(response.status, 201);

    // Регрессия: тело, переданное строкой с JSON, уходит объектом, а не строкой.
    request.body = RequestBody::Json(json!(r#"{"title": "t", "labels": ["x"]}"#));
    client.execute(request).await.unwrap();
}

#[tokio::test]
async fn file_body_is_streamed_with_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .and(header("content-type", "image/png"))
        .and(header("content-length", "5"))
        .and(body_string("hello"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload-explicit"))
        .and(header("content-type", "text/markdown"))
        .and(body_string("hello"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload-unknown"))
        .and(header("content-type", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("pic.png");
    std::fs::write(&png, "hello").unwrap();
    let unknown = dir.path().join("blob.zzzunknown");
    std::fs::write(&unknown, [0u8, 1, 2]).unwrap();

    let client = client(&server);
    let put = |p: &str, body: RequestBody| ApiRequest {
        method: HttpMethod::Put,
        body,
        ..get(p)
    };

    let response = client
        .execute(put(
            "/upload",
            RequestBody::File {
                path: png.clone(),
                content_type: None,
            },
        ))
        .await
        .unwrap();
    assert_eq!(response.status, 204);
    assert_eq!(response.body, ResponseBody::Empty);

    client
        .execute(put(
            "/upload-explicit",
            RequestBody::File {
                path: png.clone(),
                content_type: Some("text/markdown".into()),
            },
        ))
        .await
        .unwrap();
    client
        .execute(put(
            "/upload-unknown",
            RequestBody::File {
                path: unknown,
                content_type: None,
            },
        ))
        .await
        .unwrap();

    let missing = client
        .execute(put(
            "/upload",
            RequestBody::File {
                path: dir.path().join("missing.bin"),
                content_type: None,
            },
        ))
        .await
        .unwrap_err();
    assert!(matches!(missing, ApiError::Io(_)), "{missing:?}");

    let bad_type = client
        .execute(put(
            "/upload",
            RequestBody::File {
                path: png,
                content_type: Some("not a mime".into()),
            },
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(bad_type, ApiError::InvalidRequest(_)),
        "{bad_type:?}"
    );
}

#[tokio::test]
async fn multipart_sends_text_and_file_parts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/issues/id:1/attachments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.txt");
    std::fs::write(&file, "file-content-123").unwrap();

    let request = ApiRequest {
        method: HttpMethod::Post,
        body: RequestBody::Multipart(vec![
            MultipartPart::Text {
                name: "comment".into(),
                value: "hello-text".into(),
            },
            MultipartPart::File {
                name: "file".into(),
                path: file,
                file_name: Some("renamed.log".into()),
                content_type: Some("text/x-log".into()),
            },
        ]),
        ..get("/issues/id:1/attachments")
    };
    client(&server).execute(request).await.unwrap();

    let received = server.received_requests().await.unwrap();
    let req: &Request = &received[0];
    let ct = req.headers.get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains("name=\"comment\""));
    assert!(body.contains("hello-text"));
    assert!(body.contains("name=\"file\"; filename=\"renamed.log\""));
    assert!(body.contains("Content-Type: text/x-log"));
    assert!(body.contains("file-content-123"));
}

#[tokio::test]
async fn non_success_maps_to_http_error() {
    let server = MockServer::start().await;
    Mock::given(path("/missing"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"message": "repo not found"})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/huge-error"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;

    let client = client(&server);
    match client.execute(get("/missing")).await.unwrap_err() {
        ApiError::Http { status, body } => {
            assert_eq!(status, 404);
            assert!(body.contains("repo not found"), "{body}");
        }
        other => panic!("ожидалась Http, получено {other:?}"),
    }
    match client.execute(get("/huge-error")).await.unwrap_err() {
        ApiError::Http { status, body } => {
            assert_eq!(status, 500);
            assert!(
                body.len() < 5000,
                "тело ошибки должно обрезаться: {}",
                body.len()
            );
            assert!(body.ends_with("(обрезано)"));
        }
        other => panic!("ожидалась Http, получено {other:?}"),
    }
}

#[tokio::test]
async fn large_response_is_truncated() {
    let server = MockServer::start().await;
    let big = format!("{{\"data\": \"{}\"}}", "a".repeat(10_000));
    Mock::given(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(big, "application/json"))
        .mount(&server)
        .await;
    Mock::given(path("/big-text"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("ж".repeat(1000), "text/plain; charset=utf-8"),
        )
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.max_response_bytes = 1001;
    let client = ApiClient::new(cfg).unwrap();

    let response = client.execute(get("/big")).await.unwrap();
    assert!(response.truncated);
    match response.body {
        ResponseBody::Text(text) => {
            assert_eq!(text.len(), 1001);
            assert!(text.starts_with("{\"data\": \"aaa"));
        }
        other => panic!("ожидался Text, получено {other:?}"),
    }

    // Обрезка посреди многобайтового символа не даёт битого текста.
    let response = client.execute(get("/big-text")).await.unwrap();
    assert!(response.truncated);
    match response.body {
        ResponseBody::Text(text) => assert_eq!(text, "ж".repeat(500)),
        other => panic!("ожидался Text, получено {other:?}"),
    }

    // Лимит отдельного запроса заменяет лимит конфигурации.
    let response = client
        .execute_with_limit(get("/big"), 64 * 1024)
        .await
        .unwrap();
    assert!(!response.truncated);
    assert!(matches!(response.body, ResponseBody::Json(_)));
}

#[tokio::test]
async fn text_and_untyped_json_responses() {
    let server = MockServer::start().await;
    Mock::given(path("/text"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("hello", "text/plain"))
        .mount(&server)
        .await;
    Mock::given(path("/html"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{\"a\":1}", "text/html"))
        .mount(&server)
        .await;
    Mock::given(path("/untyped"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{\"a\":1}".to_vec()))
        .mount(&server)
        .await;

    let client = client(&server);
    assert_eq!(
        client.execute(get("/text")).await.unwrap().body,
        ResponseBody::Text("hello".into())
    );
    assert_eq!(
        client.execute(get("/html")).await.unwrap().body,
        ResponseBody::Text("{\"a\":1}".into())
    );
    assert_eq!(
        client.execute(get("/untyped")).await.unwrap().body,
        ResponseBody::Json(json!({"a": 1}))
    );
}

#[tokio::test]
async fn binary_response_returns_note() {
    let server = MockServer::start().await;
    Mock::given(path("/blob"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(vec![0u8, 159, 146, 150], "application/zip"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/sniffed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8, 159, 146, 150]))
        .mount(&server)
        .await;

    let client = client(&server);
    for p in ["/blob", "/sniffed"] {
        let response = client.execute(get(p)).await.unwrap();
        match response.body {
            ResponseBody::Text(note) => {
                assert!(note.contains("двоичный"), "{note}");
                assert!(note.contains("save_to"), "{note}");
                assert!(note.contains("4 байт"), "{note}");
            }
            other => panic!("ожидалось пояснение, получено {other:?}"),
        }
        assert!(!response.truncated);
    }
}

#[tokio::test]
async fn save_to_writes_file_and_refuses_overwrite() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    Mock::given(path("/download"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(payload.clone(), "application/octet-stream"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/fail"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.bin");
    let mut cfg = config(&server.uri());
    // Лимит ответа не распространяется на сохранение в файл.
    cfg.max_response_bytes = 10;
    let client = ApiClient::new(cfg).unwrap();

    let request = ApiRequest {
        save_to: Some(target.clone()),
        ..get("/download")
    };
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::SavedToFile {
            path: target.clone(),
            bytes: payload.len() as u64,
        }
    );
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Повторная запись в существующий файл запрещена.
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Каталог должен существовать, путь — быть абсолютным.
    for bad in [
        dir.path().join("no-such-dir").join("x.bin"),
        std::path::PathBuf::from("relative.bin"),
    ] {
        let err = client
            .execute(ApiRequest {
                save_to: Some(bad),
                ..get("/download")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }

    // При ошибке API файл не создаётся.
    let failed = dir.path().join("failed.bin");
    let err = client
        .execute(ApiRequest {
            save_to: Some(failed.clone()),
            ..get("/fail")
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Http { status: 403, .. }), "{err:?}");
    assert!(!failed.exists());
}

#[tokio::test]
async fn invalid_paths_never_reach_server() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = client(&server);
    for p in [
        "https://evil.example/x",
        "//evil.example/x",
        "/repos/../admin",
        "/repos/%2e%2e/admin",
        "/repos?x=1",
        "repos",
    ] {
        let err = client.execute(get(p)).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{p}: {err:?}");
    }
}

#[tokio::test]
async fn cross_origin_redirect_is_not_followed() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&other)
        .await;
    Mock::given(path("/external"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/steal?sig=secret", other.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/internal"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/me"))
        .mount(&server)
        .await;
    Mock::given(path("/me"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "me"})))
        .mount(&server)
        .await;

    let client = client(&server);
    match client.execute(get("/external")).await.unwrap_err() {
        ApiError::Http { status, body } => {
            assert_eq!(status, 302);
            assert!(!body.contains("sig=secret"), "{body}");
        }
        other => panic!("ожидалась Http, получено {other:?}"),
    }
    let response = client.execute(get("/internal")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"login": "me"})));
}

#[tokio::test]
async fn client_without_token_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(path("/me"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut cfg = config(&server.uri());
    cfg.token = None;
    let client = ApiClient::new(cfg).unwrap();
    let err = client.execute(get("/me")).await.unwrap_err();
    assert!(matches!(err, ApiError::MissingToken), "{err:?}");
}

#[tokio::test]
async fn with_token_uses_its_own_token_per_copy() {
    let server = MockServer::start().await;
    for token in ["token-agent-a", "token-agent-b"] {
        Mock::given(path("/me"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": token})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(path("/echo"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(403).set_body_string(format!("denied: {auth}"))
        })
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.token = None;
    let base = ApiClient::new(cfg).unwrap();
    let a = base.with_token(ApiToken::new("token-agent-a")).unwrap();
    let b = base.with_token(ApiToken::new("token-agent-b")).unwrap();
    a.execute(get("/me")).await.unwrap();
    b.execute(get("/me")).await.unwrap();
    // Исходный клиент токен не получил.
    assert!(matches!(
        base.execute(get("/me")).await.unwrap_err(),
        ApiError::MissingToken
    ));

    // Токен копии вырезается из ошибок и из текстов через scrub_token.
    let err = a.execute(get("/echo")).await.unwrap_err();
    assert!(!format!("{err}").contains("token-agent-a"), "{err}");
    assert_eq!(a.scrub_token("x token-agent-a y".into()), "x *** y");

    // Недопустимые значения отклоняются без отражения в ошибке.
    for bad in ["", "  ", "bad\ntoken"] {
        let err = base.with_token(ApiToken::new(bad)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{err:?}");
        assert!(!format!("{err:?}").contains("bad\ntoken"));
    }
}

#[tokio::test]
async fn token_never_leaks() {
    // Конфигурация и клиент в Debug не показывают токен.
    let cfg = config("https://api.example");
    assert!(!format!("{cfg:?}").contains(TOKEN));
    let client = ApiClient::new(cfg).unwrap();
    assert!(!format!("{client:?}").contains(TOKEN));

    // Сервер отражает заголовок Authorization в теле ошибки и успешного ответа.
    let server = MockServer::start().await;
    Mock::given(path("/echo-error"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(401).set_body_string(format!("bad credentials: {auth}"))
        })
        .mount(&server)
        .await;
    Mock::given(path("/echo-text"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(200).set_body_raw(format!("you sent {auth}"), "text/plain")
        })
        .mount(&server)
        .await;

    let client = ApiClient::new(config(&server.uri())).unwrap();
    let mut outputs = Vec::new();

    let err = client.execute(get("/echo-error")).await.unwrap_err();
    outputs.push(format!("{err}"));
    outputs.push(format!("{err:?}"));
    let ok = client.execute(get("/echo-text")).await.unwrap();
    outputs.push(format!("{ok:?}"));

    // Ошибки пути, параметров, ввода-вывода и соединения.
    for e in [
        client.execute(get("/../x")).await.unwrap_err(),
        client
            .execute(ApiRequest {
                body: RequestBody::File {
                    path: "/nonexistent/file".into(),
                    content_type: None,
                },
                ..get("/x")
            })
            .await
            .unwrap_err(),
    ] {
        outputs.push(format!("{e}"));
        outputs.push(format!("{e:?}"));
    }

    // Порт, на котором никто не слушает: соединение отклоняется.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let unreachable = ApiClient::new(config(&format!("http://127.0.0.1:{closed}"))).unwrap();
    let transport = unreachable.execute(get("/me")).await.unwrap_err();
    assert!(matches!(transport, ApiError::Transport(_)), "{transport:?}");
    outputs.push(format!("{transport}"));
    outputs.push(format!("{transport:?}"));

    for out in &outputs {
        assert!(!out.contains(TOKEN), "токен утёк: {out}");
    }
    assert!(outputs[0].contains("***"), "{}", outputs[0]);
}

#[tokio::test]
async fn timeout_maps_to_transport_error() {
    let server = MockServer::start().await;
    Mock::given(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(200);
    let client = ApiClient::new(cfg).unwrap();
    match client.execute(get("/slow")).await.unwrap_err() {
        ApiError::Transport(text) => assert!(text.contains("таймаут"), "{text}"),
        other => panic!("ожидалась Transport, получено {other:?}"),
    }
}

#[test]
fn rejects_insecure_or_invalid_config() {
    for base in [
        "http://api.sourcecraft.tech",
        "https://user:pw@api.example",
        "ftp://api.example",
    ] {
        let err = ApiClient::new(config(base)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{base}: {err:?}");
    }
    let mut cfg = config("https://api.example");
    cfg.token = Some(ApiToken::new("bad\ntoken"));
    let err = ApiClient::new(cfg).unwrap_err();
    assert!(matches!(err, ApiError::InvalidConfig(_)));
    assert!(!format!("{err:?}").contains("bad\ntoken"));
}
