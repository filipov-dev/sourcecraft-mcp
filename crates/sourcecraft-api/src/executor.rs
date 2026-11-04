//! Выполнение запросов к SourceCraft API.
//!
//! Все сгенерированные инструменты и `api_request` работают через [`ApiClient::execute`].
//! Это граница безопасности:
//! - путь проверяется [`validate_relative_path`] и приклеивается к `base_url`, поэтому
//!   запрос не уходит на другой хост;
//! - заголовок `Authorization` формируется только здесь и помечен как чувствительный;
//! - редиректы выполняются только в пределах того же origin;
//! - размер тела ответа ограничен `max_response_bytes`, ошибки — [`ERROR_BODY_LIMIT`].

use std::fmt;
use std::path::{Path, PathBuf};

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Body, Method, StatusCode, multipart, redirect};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::{spec::HttpMethod, token::ApiToken};

pub use crate::path::{render_path, validate_relative_path};

/// Максимальный размер тела ошибки (не-2xx), который попадает в [`ApiError::Http`].
pub const ERROR_BODY_LIMIT: usize = 4 * 1024;

/// Максимальное число редиректов в пределах того же origin.
const MAX_REDIRECTS: usize = 5;

/// Настройки клиента.
///
/// `Debug` безопасен: [`ApiToken`] не раскрывает значение.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Базовый адрес API. Может содержать префикс пути (`https://host/api/v1`), он сохраняется.
    /// Допустимы `https://` и `http://` только для loopback-адресов (локальные тесты).
    pub base_url: url::Url,
    pub token: ApiToken,
    /// Таймаут всего запроса, включая чтение тела (и запись в `save_to`).
    pub timeout: std::time::Duration,
    /// Максимальный размер тела ответа, возвращаемого в MCP, в байтах. Больший ответ обрезается.
    /// На `save_to` не распространяется.
    pub max_response_bytes: usize,
}

/// Тело запроса.
#[derive(Debug, Clone, Default)]
pub enum RequestBody {
    #[default]
    Empty,
    /// JSON-тело, отправляется как `application/json`.
    ///
    /// Если значение — строка с JSON-объектом или массивом, она разбирается и отправляется
    /// как объект: MCP-клиенты иногда передают тело строкой, а SourceCraft ждёт объект.
    Json(Value),
    /// Содержимое локального файла как тело запроса (потоково).
    /// `content_type` по умолчанию угадывается по расширению, иначе `application/octet-stream`.
    File {
        path: PathBuf,
        content_type: Option<String>,
    },
    Multipart(Vec<MultipartPart>),
}

/// Часть multipart-запроса.
#[derive(Debug, Clone)]
pub enum MultipartPart {
    Text {
        name: String,
        value: String,
    },
    /// Файл; `file_name` по умолчанию — имя локального файла, `content_type` — по расширению.
    File {
        name: String,
        path: PathBuf,
        file_name: Option<String>,
        content_type: Option<String>,
    },
}

/// Запрос к API. `path` — относительный путь с уже подставленными параметрами
/// (см. [`render_path`]); query передаётся отдельно в `query`.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: RequestBody,
    /// Если задан, тело успешного ответа сохраняется в файл, а не возвращается.
    ///
    /// Путь должен быть абсолютным, каталог — существовать, а файла с таким именем быть
    /// не должно: существующие файлы не перезаписываются. При ошибке частично записанный
    /// файл удаляется. Лимит `max_response_bytes` к файлу не применяется.
    pub save_to: Option<PathBuf>,
}

/// Тело ответа.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Empty,
    Json(Value),
    /// Текст ответа. Для обрезанного или двоичного ответа — обрезанный текст или пояснение.
    Text(String),
    SavedToFile {
        path: PathBuf,
        bytes: u64,
    },
}

/// Успешный ответ API (статус 2xx).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: ResponseBody,
    /// Тело было обрезано по `max_response_bytes`; тогда `body` — [`ResponseBody::Text`].
    pub truncated: bool,
}

/// Ошибки запроса. Текст ошибок не содержит токен.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("недопустимый путь API: {0}")]
    InvalidPath(String),
    #[error("не хватает параметра пути `{0}`")]
    MissingPathParam(String),
    /// Некорректные аргументы запроса, не связанные с путём (тип содержимого, `save_to`).
    #[error("некорректный запрос: {0}")]
    InvalidRequest(String),
    /// Некорректная конфигурация клиента (`base_url`, токен).
    #[error("некорректная конфигурация клиента SourceCraft API: {0}")]
    InvalidConfig(String),
    #[error("SourceCraft API вернул {status}: {body}")]
    Http { status: u16, body: String },
    #[error("ошибка соединения с SourceCraft API: {0}")]
    Transport(String),
    #[error("ошибка ввода-вывода: {0}")]
    Io(String),
}

/// Клиент SourceCraft API.
#[derive(Clone)]
pub struct ApiClient {
    config: ClientConfig,
    http: reqwest::Client,
    /// Готовый заголовок `Authorization: Bearer <token>`, помечен как sensitive.
    auth: HeaderValue,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Вручную, чтобы заголовок авторизации не попал в Debug ни при каких условиях.
        f.debug_struct("ApiClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        check_base_url(&config.base_url)?;

        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.token.expose()))
            .map_err(|_| {
                ApiError::InvalidConfig("токен содержит недопустимые для заголовка символы".into())
            })?;
        auth.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, */*;q=0.8"),
        );

        // Редиректы разрешены только в пределах origin из base_url. Внешние редиректы
        // (например, на хранилище файлов) не выполняются: так токен и тело запроса не уходят
        // на чужой хост, а клиент не превращается в прокси. reqwest и сам убирает
        // `Authorization` при смене хоста, но мы не полагаемся только на это.
        // Остановленный редирект возвращается как ответ 3xx и превращается в `ApiError::Http`.
        let origin = config.base_url.origin();
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("слишком много редиректов")
            } else if attempt.url().origin() == origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });

        let http = reqwest::Client::builder()
            .user_agent(concat!("sourcecraft-mcp/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .timeout(config.timeout)
            .redirect(policy)
            .build()
            .map_err(|e| ApiError::InvalidConfig(describe_reqwest_error(&e)))?;

        Ok(Self { config, http, auth })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Выполняет запрос. Не-2xx ответы возвращаются как [`ApiError::Http`].
    pub async fn execute(&self, request: ApiRequest) -> Result<ApiResponse, ApiError> {
        let url = self.build_url(&request.path, &request.query)?;
        if let Some(save_to) = &request.save_to {
            check_save_to(save_to).await?;
        }

        tracing::debug!(method = request.method.as_str(), path = %request.path, "запрос к SourceCraft API");

        let mut builder = self
            .http
            .request(to_method(request.method), url)
            .header(header::AUTHORIZATION, self.auth.clone());
        builder = attach_body(builder, request.body).await?;

        let response = builder
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?;
        let status = response.status();

        if !status.is_success() {
            return Err(self.http_error(status, response).await);
        }

        match &request.save_to {
            Some(path) => save_to_file(response, path).await.map(|bytes| ApiResponse {
                status: status.as_u16(),
                body: ResponseBody::SavedToFile {
                    path: path.clone(),
                    bytes,
                },
                truncated: false,
            }),
            None => self.read_body(status, response).await,
        }
    }

    /// Собирает URL: префикс пути из `base_url` + проверенный относительный путь + query.
    fn build_url(&self, path: &str, query: &[(String, String)]) -> Result<url::Url, ApiError> {
        validate_relative_path(path)?;

        let mut url = self.config.base_url.clone();
        let prefix = url.path().trim_end_matches('/').to_string();
        // Путь уже состоит только из pchar и `%XX`, поэтому set_path его не меняет.
        url.set_path(&format!("{prefix}{path}"));
        url.set_query(None);
        url.set_fragment(None);
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }

        // Повторная проверка: итоговый адрес обязан остаться в origin базового URL.
        if url.origin() != self.config.base_url.origin() {
            return Err(ApiError::InvalidPath(
                "путь выводит за пределы SourceCraft API".into(),
            ));
        }
        Ok(url)
    }

    async fn http_error(&self, status: StatusCode, mut response: reqwest::Response) -> ApiError {
        let body = if status.is_redirection() {
            // Location не возвращаем: там может быть подписанная ссылка на внешнее хранилище.
            "редирект на другой адрес не выполняется".to_string()
        } else {
            match read_limited(&mut response, ERROR_BODY_LIMIT).await {
                Ok((bytes, truncated)) => {
                    let mut text = lossy_prefix(&bytes).trim().to_string();
                    if truncated {
                        text.push_str(" …(обрезано)");
                    }
                    if text.is_empty() {
                        text = status
                            .canonical_reason()
                            .unwrap_or("пустой ответ")
                            .to_string();
                    }
                    text
                }
                Err(e) => format!("не удалось прочитать тело ошибки: {e}"),
            }
        };
        ApiError::Http {
            status: status.as_u16(),
            body: self.redact(body),
        }
    }

    async fn read_body(
        &self,
        status: StatusCode,
        mut response: reqwest::Response,
    ) -> Result<ApiResponse, ApiError> {
        let status_code = status.as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase());
        let content_length = response.content_length();

        // Заведомо двоичный ответ не читаем вовсе — сразу подсказываем `save_to`.
        let declared_binary = content_type
            .as_deref()
            .filter(|ct| is_binary_content_type(ct));
        if let Some(ct) = declared_binary
            && content_length != Some(0)
        {
            return Ok(ApiResponse {
                status: status_code,
                body: ResponseBody::Text(binary_note(Some(ct), content_length, None)),
                truncated: false,
            });
        }

        let limit = self.config.max_response_bytes;
        let (bytes, truncated) = read_limited(&mut response, limit).await?;

        if bytes.is_empty() {
            return Ok(ApiResponse {
                status: status_code,
                body: ResponseBody::Empty,
                truncated: false,
            });
        }

        if looks_binary(&bytes, truncated) {
            return Ok(ApiResponse {
                status: status_code,
                body: ResponseBody::Text(binary_note(
                    content_type.as_deref(),
                    content_length,
                    truncated.then_some(bytes.len()),
                )),
                truncated: false,
            });
        }

        if truncated {
            return Ok(ApiResponse {
                status: status_code,
                body: ResponseBody::Text(self.redact(lossy_prefix(&bytes))),
                truncated: true,
            });
        }

        let json_declared = content_type.as_deref().is_some_and(is_json_content_type);
        let json_allowed = json_declared
            || content_type
                .as_deref()
                .is_none_or(|ct| ct.starts_with("text/plain"));
        if json_allowed && let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Ok(ApiResponse {
                status: status_code,
                body: ResponseBody::Json(value),
                truncated: false,
            });
        }

        Ok(ApiResponse {
            status: status_code,
            body: ResponseBody::Text(self.redact(lossy_prefix(&bytes))),
            truncated: false,
        })
    }

    /// Защита в глубину: если сервер вдруг отразил токен в тексте, вырезаем его.
    fn redact(&self, text: String) -> String {
        let token = self.config.token.expose();
        if token.is_empty() || !text.contains(token) {
            return text;
        }
        text.replace(token, "***")
    }
}

fn to_method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
    }
}

fn check_base_url(base: &url::Url) -> Result<(), ApiError> {
    if base.cannot_be_a_base() || base.host().is_none() {
        return Err(ApiError::InvalidConfig(format!(
            "base_url `{base}` не является адресом HTTP API"
        )));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(ApiError::InvalidConfig(
            "base_url не должен содержать логин или пароль".into(),
        ));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(ApiError::InvalidConfig(
            "base_url не должен содержать query или fragment".into(),
        ));
    }
    match base.scheme() {
        "https" => Ok(()),
        // Без TLS токен ушёл бы открытым текстом, поэтому http — только для локальных адресов.
        "http" if is_loopback(base) => Ok(()),
        "http" => Err(ApiError::InvalidConfig(
            "http допускается только для localhost; используйте https".into(),
        )),
        other => Err(ApiError::InvalidConfig(format!(
            "схема `{other}` не поддерживается"
        ))),
    }
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

async fn attach_body(
    builder: reqwest::RequestBuilder,
    body: RequestBody,
) -> Result<reqwest::RequestBuilder, ApiError> {
    Ok(match body {
        RequestBody::Empty => builder,
        RequestBody::Json(value) => builder.json(&normalize_json_body(value)),
        RequestBody::File { path, content_type } => {
            let mime = match content_type {
                Some(ct) => parse_content_type(&ct)?,
                None => guess_mime(&path),
            };
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| io_error("не удалось открыть файл", &path, &e))?;
            let len = file
                .metadata()
                .await
                .map_err(|e| io_error("не удалось прочитать атрибуты файла", &path, &e))?
                .len();
            builder
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, len)
                .body(Body::from(file))
        }
        RequestBody::Multipart(parts) => {
            let mut form = multipart::Form::new();
            for part in parts {
                form = match part {
                    MultipartPart::Text { name, value } => form.text(name, value),
                    MultipartPart::File {
                        name,
                        path,
                        file_name,
                        content_type,
                    } => {
                        // Part::file потоково читает файл, задаёт имя и угадывает тип по расширению.
                        let mut part = multipart::Part::file(&path)
                            .await
                            .map_err(|e| io_error("не удалось открыть файл", &path, &e))?;
                        if let Some(file_name) = file_name {
                            part = part.file_name(file_name);
                        }
                        if let Some(ct) = content_type {
                            part = part.mime_str(&ct).map_err(|_| {
                                ApiError::InvalidRequest(format!(
                                    "некорректный тип содержимого `{ct}`"
                                ))
                            })?;
                        }
                        form.part(name, part)
                    }
                };
            }
            builder.multipart(form)
        }
    })
}

/// Регрессия: строка с JSON-объектом или массивом отправляется как объект,
/// а не как JSON-строка с экранированным JSON внутри.
fn normalize_json_body(value: Value) -> Value {
    if let Value::String(text) = &value {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() || parsed.is_array() => return parsed,
                _ => {}
            }
        }
    }
    value
}

fn parse_content_type(ct: &str) -> Result<HeaderValue, ApiError> {
    let invalid = || ApiError::InvalidRequest(format!("некорректный тип содержимого `{ct}`"));
    let mime: mime_guess::mime::Mime = ct.parse().map_err(|_| invalid())?;
    HeaderValue::from_str(mime.as_ref()).map_err(|_| invalid())
}

fn guess_mime(path: &Path) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    HeaderValue::from_str(mime.as_ref())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// Проверяет `save_to` до отправки запроса, чтобы не выполнять запрос впустую.
async fn check_save_to(path: &Path) -> Result<(), ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::InvalidRequest(format!(
            "save_to должен быть абсолютным путём: `{}`",
            path.display()
        )));
    }
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(ApiError::InvalidRequest(format!(
            "save_to должен указывать на файл: `{}`",
            path.display()
        )));
    };
    match tokio::fs::metadata(parent).await {
        Ok(meta) if meta.is_dir() => {}
        _ => {
            return Err(ApiError::InvalidRequest(format!(
                "каталог для save_to не существует: `{}`",
                parent.display()
            )));
        }
    }
    if tokio::fs::symlink_metadata(path).await.is_ok() {
        return Err(ApiError::InvalidRequest(format!(
            "файл уже существует, перезапись запрещена: `{}`",
            path.display()
        )));
    }
    Ok(())
}

async fn save_to_file(mut response: reqwest::Response, path: &Path) -> Result<u64, ApiError> {
    // create_new: не перезаписываем существующий файл и не идём по симлинку,
    // даже если он появился после check_save_to.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| io_error("не удалось создать файл", path, &e))?;

    let result = async {
        let mut written: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| io_error("ошибка записи в файл", path, &e))?;
            written += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| io_error("ошибка записи в файл", path, &e))?;
        Ok(written)
    }
    .await;

    if result.is_err() {
        drop(file);
        // Частично записанный файл бесполезен и вводит в заблуждение — удаляем.
        let _ = tokio::fs::remove_file(path).await;
    }
    result
}

/// Читает тело не больше `limit` байт. Возвращает прочитанное и признак обрезки.
async fn read_limited(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), ApiError> {
    let mut buf = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?
    {
        let room = limit.saturating_sub(buf.len());
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            // Остаток не дочитываем: соединение закроется при drop ответа.
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, false))
}

/// UTF-8 текст из байтов; незавершённый символ в конце (после обрезки) отбрасывается.
fn lossy_prefix(bytes: &[u8]) -> String {
    let end = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => bytes.len(),
    };
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn is_json_content_type(ct: &str) -> bool {
    let essence = ct.split(';').next().unwrap_or("").trim();
    essence == "application/json" || essence.ends_with("+json")
}

fn is_text_content_type(ct: &str) -> bool {
    let essence = ct.split(';').next().unwrap_or("").trim();
    essence.starts_with("text/")
        || is_json_content_type(essence)
        || essence.ends_with("+xml")
        || matches!(
            essence,
            "application/xml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-ndjson"
                | "application/problem+json"
        )
}

fn is_binary_content_type(ct: &str) -> bool {
    !ct.is_empty() && !is_text_content_type(ct)
}

/// Эвристика для ответов без явного текстового типа: NUL или невалидный UTF-8.
fn looks_binary(bytes: &[u8], truncated: bool) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        // Обрезка могла разрезать многобайтовый символ — это не признак двоичных данных.
        Err(e) => !(truncated && e.error_len().is_none()),
    }
}

fn binary_note(content_type: Option<&str>, length: Option<u64>, read: Option<usize>) -> String {
    let ct = content_type.unwrap_or("тип не указан");
    let size = match (length, read) {
        (Some(len), _) => format!("{len} байт"),
        (None, Some(read)) => format!("больше {read} байт"),
        (None, None) => "размер неизвестен".to_string(),
    };
    format!(
        "Ответ двоичный ({ct}, {size}) и не возвращается текстом. \
         Повторите запрос с save_to, чтобы сохранить его в файл."
    )
}

fn io_error(action: &str, path: &Path, err: &std::io::Error) -> ApiError {
    ApiError::Io(format!("{action} `{}`: {err}", path.display()))
}

/// Безопасное описание ошибки reqwest: без URL (там может быть query) и с причинами.
fn describe_reqwest_error(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "превышен таймаут"
    } else if err.is_connect() {
        "не удалось подключиться"
    } else if err.is_redirect() {
        "ошибка редиректа"
    } else if err.is_body() || err.is_decode() {
        "ошибка чтения тела ответа"
    } else if err.is_builder() {
        "некорректный запрос"
    } else {
        "ошибка запроса"
    };
    let mut text = kind.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_parses_json_strings() {
        assert_eq!(
            normalize_json_body(Value::String(r#"{"a":1}"#.into())),
            serde_json::json!({"a": 1})
        );
        assert_eq!(
            normalize_json_body(Value::String(" [1,2]".into())),
            serde_json::json!([1, 2])
        );
        assert_eq!(
            normalize_json_body(Value::String("plain".into())),
            Value::String("plain".into())
        );
        assert_eq!(
            normalize_json_body(Value::String("{broken".into())),
            Value::String("{broken".into())
        );
    }

    #[test]
    fn lossy_prefix_drops_cut_char() {
        let text = "ab\u{0436}";
        let bytes = text.as_bytes();
        assert_eq!(lossy_prefix(&bytes[..bytes.len() - 1]), "ab");
        assert_eq!(lossy_prefix(bytes), text);
    }

    #[test]
    fn content_type_classification() {
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("application/vnd.api+json"));
        assert!(is_text_content_type("text/plain"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(is_binary_content_type("image/png"));
        assert!(!is_binary_content_type("application/xml"));
    }

    #[test]
    fn base_url_rules() {
        let ok = |s: &str| check_base_url(&url::Url::parse(s).unwrap()).is_ok();
        assert!(ok("https://api.sourcecraft.tech"));
        assert!(ok("https://api.example/prefix/"));
        assert!(ok("http://127.0.0.1:8080"));
        assert!(ok("http://localhost:8080/api"));
        assert!(ok("http://[::1]:8080"));
        assert!(!ok("http://api.sourcecraft.tech"));
        assert!(!ok("https://user:pass@api.example"));
        assert!(!ok("https://api.example/?x=1"));
        assert!(!ok("ftp://api.example"));
        assert!(!ok("mailto:a@b"));
    }
}
