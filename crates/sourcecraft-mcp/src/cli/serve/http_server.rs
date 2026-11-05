//! `serve --http`: MCP-сервер через Streamable HTTP (`POST /mcp`).
//!
//! Профилей и хранилища токенов здесь нет. Каждый HTTP-запрос агента несёт его токен
//! SourceCraft в `Authorization: Bearer <token>`:
//!
//! - слой [`require_bearer`] проверяет заголовок до MCP (без него — 401 с `WWW-Authenticate`),
//!   убирает его из запроса и кладёт токен в расширения запроса ([`RequestToken`]);
//! - `rmcp` передаёт части HTTP-запроса (`http::request::Parts`) в расширения запроса MCP,
//!   откуда ядро берёт токен на каждый вызов инструмента ([`token_source`]);
//! - ядро делает копию клиента API с этим токеном только на время вызова: токен уходит
//!   только на `--base-url`, нигде не сохраняется и не пишется в логи.
//!
//! Сервер без состояния (stateless Streamable HTTP, ответы `application/json`): сессий нет,
//! каждый POST обрабатывается независимо, поэтому токен одного запроса не переживает его.
//! `Host` и `Origin` проверяет `rmcp` (защита от DNS rebinding): по умолчанию разрешены только
//! loopback-имена и запросы без `Origin` (не из браузера).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::Request;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::model::Extensions;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use sourcecraft_api::ApiToken;
use sourcecraft_mcp_core::{SourceCraftServer, TokenSource};
use tokio_util::sync::CancellationToken;

/// Адрес по умолчанию: только локальные подключения.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8765";
/// Путь MCP-эндпоинта.
pub const MCP_PATH: &str = "/mcp";
/// Путь проверки готовности (без токена).
pub const HEALTH_PATH: &str = "/health";
/// Имена хостов, разрешённые в `Host` всегда.
pub const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];
/// Максимальная длина токена в заголовке: больше — явно не токен SourceCraft.
const MAX_TOKEN_LEN: usize = 4096;
const REALM: &str = "Bearer realm=\"sourcecraft-mcp\"";

/// Настройки HTTP-транспорта.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Допустимые значения `Host` (имя или `имя:порт`).
    pub allowed_hosts: Vec<String>,
    /// Допустимые `Origin` браузерных клиентов; пусто — любой присланный `Origin` отклоняется.
    pub allowed_origins: Vec<String>,
}

/// Токен SourceCraft текущего HTTP-запроса. `Debug` не раскрывает значение ([`ApiToken`]).
#[derive(Debug, Clone)]
pub struct RequestToken(ApiToken);

/// Источник токена для ядра: токен из расширений HTTP-запроса, которые `rmcp` кладёт
/// в расширения запроса MCP как `http::request::Parts`.
pub fn token_source() -> TokenSource {
    TokenSource::PerRequest(Arc::new(|extensions: &Extensions| {
        extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<RequestToken>())
            .map(|token| token.0.clone())
    }))
}

/// Разрешённые `Host`: loopback-имена, конкретный адрес из `--listen` и `--allowed-host`.
pub fn allowed_hosts(listen: SocketAddr, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| (*h).to_owned()).collect();
    let ip = listen.ip();
    if !ip.is_unspecified() && !ip.is_loopback() {
        hosts.push(ip.to_string());
    }
    for host in extra {
        let host = host.trim().to_owned();
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

/// Маршруты сервера: `/mcp` (с токеном) и `/health` (без токена).
pub fn router(
    server: SourceCraftServer,
    options: &HttpOptions,
    cancel: CancellationToken,
) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(options.allowed_hosts.clone())
        .with_allowed_origins(options.allowed_origins.clone())
        // Пустой список `Origin` — отклонять любой присланный `Origin`, а не пропускать все.
        .enforce_origin_validation()
        // Без сессий: каждый запрос самостоятельный, сервер ничего не хранит между запросами.
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancel);
    let mcp = StreamableHttpService::new(
        // Своя сессия ядра на каждый запрос: состояние сессии (наборы инструментов)
        // не делится между запросами и агентами.
        move || Ok(server.new_session()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let mcp_routes = Router::new()
        .route_service(MCP_PATH, mcp)
        .route_layer(middleware::from_fn(require_bearer));
    Router::new()
        .route(HEALTH_PATH, get(health))
        .merge(mcp_routes)
}

/// Запускает сервер и ждёт SIGINT/SIGTERM.
pub async fn run(
    server: SourceCraftServer,
    listen: SocketAddr,
    options: HttpOptions,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener
        .local_addr()
        .context("cannot read the listen address")?;
    if !addr.ip().is_loopback() {
        // Предупреждение для человека, поэтому в stderr напрямую, независимо от уровня логов:
        // токены идут открытым текстом, снаружи сервер должен быть доступен только через TLS.
        eprintln!(
            "warning: listening on {addr}, not only on loopback. Tokens travel in plain HTTP: \
             expose the server only through a TLS reverse proxy or a private network."
        );
    }
    tracing::info!(
        %addr,
        allowed_hosts = ?options.allowed_hosts,
        allowed_origins = ?options.allowed_origins,
        "запуск MCP-сервера SourceCraft по HTTP"
    );
    eprintln!("sourcecraft-mcp: MCP endpoint http://{addr}{MCP_PATH}");

    let cancel = CancellationToken::new();
    let app = router(server, &options, cancel.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("получен сигнал завершения, останавливаю сервер");
            cancel.cancel();
        })
        .await
        .context("HTTP server failed")?;
    Ok(())
}

/// Ждёт SIGINT (Ctrl+C) или SIGTERM (`docker stop`, systemd).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "не удалось подписаться на SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "не удалось подписаться на SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn health() -> Response {
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "status": "ok",
            "service": "sourcecraft-mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "mcp_endpoint": MCP_PATH,
        }),
    )
}

/// Слой авторизации `/mcp`: без корректного `Authorization: Bearer` запрос не доходит до MCP.
///
/// Заголовок убирается из запроса, а токен кладётся в расширения как [`RequestToken`]: так
/// исходное значение заголовка не попадёт ни в какой отладочный вывод частей запроса.
async fn require_bearer(mut request: Request, next: Next) -> Response {
    match bearer_token(request.headers()) {
        Ok(token) => {
            request.headers_mut().remove(AUTHORIZATION);
            request.extensions_mut().insert(RequestToken(token));
            next.run(request).await
        }
        Err(problem) => unauthorized(problem),
    }
}

/// Что не так с заголовком `Authorization`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthProblem {
    Missing,
    Malformed,
}

/// Токен из единственного заголовка `Authorization: Bearer <token>`.
/// Значение заголовка в ошибки не попадает.
fn bearer_token(headers: &HeaderMap) -> Result<ApiToken, AuthProblem> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().ok_or(AuthProblem::Missing)?;
    if values.next().is_some() {
        return Err(AuthProblem::Malformed);
    }
    let value = value.to_str().map_err(|_| AuthProblem::Malformed)?;
    let (scheme, token) = value.trim().split_once(' ').ok_or(AuthProblem::Malformed)?;
    let token = token.trim();
    let valid = scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && token.bytes().all(|b| b.is_ascii_graphic());
    if !valid {
        return Err(AuthProblem::Malformed);
    }
    Ok(ApiToken::new(token))
}

fn unauthorized(problem: AuthProblem) -> Response {
    let (challenge, message) = match problem {
        AuthProblem::Missing => (
            REALM.to_owned(),
            "missing SourceCraft token: send it in the `Authorization: Bearer <token>` header",
        ),
        AuthProblem::Malformed => (
            format!("{REALM}, error=\"invalid_request\""),
            "malformed Authorization header: expected exactly one `Authorization: Bearer <token>`",
        ),
    };
    let mut response = json_response(
        StatusCode::UNAUTHORIZED,
        serde_json::json!({ "error": "unauthorized", "message": message }),
    );
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response.headers_mut().insert(WWW_AUTHENTICATE, value);
    }
    response
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
