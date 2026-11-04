//! Инструмент `api_request`: запрос к произвольному пути SourceCraft API.
//!
//! Это запасной выход для операций, которых нет среди сгенерированных инструментов или
//! которые отключены наборами (`--toolsets` его не ограничивают). Границы безопасности
//! те же, что у остальных инструментов:
//!
//! - путь только относительный и проходит `validate_relative_path`; query передаётся отдельно,
//!   заголовки задать нельзя, поэтому `Authorization` не переопределяется;
//! - требуемый уровень доступа определяется сопоставлением с операциями ([`crate::routes`]),
//!   а без совпадения — по методу; выше `max_access` запрос отклоняется;
//! - запись секрета сырым `PUT` отклоняется (API ждёт base64), проверка видимости
//!   `update_repository` применяется так же, как у сгенерированного инструмента;
//! - локальные файлы (`body_file`, `save_to`) проходят [`FilePolicy`].

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use sourcecraft_api::executor::{ApiRequest, RequestBody, validate_relative_path};
use sourcecraft_api::spec::{AccessLevel, HttpMethod, OperationDef};

use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::is_secret_path;
use crate::registry::{access_flag, access_name};
use crate::request::{UPDATE_REPOSITORY_TOOL, check_guards};
use crate::routes::{AccessDecision, required_access};

/// Имя инструмента.
pub const API_REQUEST_TOOL: &str = "api_request";

/// Сгенерированная операция записи секрета: сырое значение должно быть в base64.
const SET_SECRET_OPERATION: &str = "set_secret";

const ARGUMENTS: &[&str] = &[
    "method",
    "path",
    "query",
    "body",
    "body_file",
    "content_type",
    "save_to",
];

/// Подготовленный запрос и самая точная совпавшая операция (для постобработки ответа).
#[derive(Debug)]
pub struct PreparedRequest {
    pub request: ApiRequest,
    pub operation: Option<&'static OperationDef>,
}

/// Описание инструмента; аннотации соответствуют максимальному уровню доступа сервера.
pub fn api_request_tool(max_access: AccessLevel) -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "method": {
                "type": "string",
                "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                "description": "HTTP method."
            },
            "path": {
                "type": "string",
                "description": "API path relative to the SourceCraft API base, starting with `/`, \
                    e.g. `/repos/my-org/my-repo/pulls` or `/repos/id:<repo_id>/pulls`. Full URLs, \
                    `?` and `#` are rejected: pass query parameters in `query`. Percent-encode \
                    special characters inside a segment."
            },
            "query": {
                "type": "object",
                "description": "Query parameters. Values may be strings, numbers, booleans or \
                    arrays of them; an array repeats the key (`keys=a&keys=b`).",
                "additionalProperties": {
                    "anyOf": [
                        {"type": ["string", "number", "boolean"]},
                        {"type": "array", "items": {"type": ["string", "number", "boolean"]}}
                    ]
                }
            },
            "body": {
                "description": "JSON request body (any JSON value), sent as application/json. \
                    Mutually exclusive with `body_file`."
            },
            "body_file": {
                "type": "string",
                "description": "Path to a local file whose raw content is sent as the request \
                    body. Must be inside the directories allowed by the server. Mutually \
                    exclusive with `body`."
            },
            "content_type": {
                "type": "string",
                "description": "Content type for `body_file`; guessed from the file extension \
                    when omitted."
            },
            "save_to": {
                "type": "string",
                "description": "Save the successful response body to this new local file \
                    (absolute path inside the allowed directories) instead of returning it. \
                    Existing files are never overwritten."
            }
        },
        "required": ["method", "path"],
        "additionalProperties": false
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };

    let allowed = match max_access {
        AccessLevel::Read => {
            "This server is read-only, so only requests that read data are allowed \
             (GET and read-only operations)."
        }
        AccessLevel::Write => {
            "This server allows reading and changing data; deleting (and other destructive \
             operations) needs --allow-destructive."
        }
        AccessLevel::Destructive => {
            "This server allows reading, changing and deleting data; confirm destructive \
             requests with the user."
        }
    };
    let description = format!(
        "Send a request to any SourceCraft REST API path. Use it only when no dedicated tool \
         covers the operation: dedicated tools validate arguments and document the schema. Use \
         `api_search` to find the operation (and its dedicated tool) first.\n\n\
         The path is relative to the API base (`/repos/...`, `/orgs/...`, `/user`). Headers \
         cannot be set; authentication is added by the server.\n\n\
         Access: a request that matches a known API operation needs that operation's access \
         level; otherwise GET is read, POST/PUT/PATCH are write and DELETE is destructive. {allowed} \
         Toolsets do not restrict this tool.\n\n\
         Do not set secrets with it: the raw API expects base64-encoded values; use the \
         `secret_set` tool."
    );

    let annotations = match max_access {
        AccessLevel::Read => ToolAnnotations::new().read_only(true),
        AccessLevel::Write => ToolAnnotations::new().read_only(false).destructive(false),
        AccessLevel::Destructive => ToolAnnotations::new().read_only(false).destructive(true),
    };
    let mut tool = Tool::new(API_REQUEST_TOOL, description, Arc::new(schema));
    tool.title = Some("Custom SourceCraft API request".to_owned());
    tool.annotations = Some(annotations);
    tool
}

/// Проверяет аргументы, уровень доступа и файлы и собирает запрос.
pub fn prepare(
    args: &JsonObject,
    max_access: AccessLevel,
    files: &FilePolicy,
) -> Result<PreparedRequest, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());
    reject_unknown(args)?;

    let method = parse_method(args.get("method"))?;
    let path = parse_path(args.get("path"))?;

    let decision = required_access(method, &path);
    check_access(method, &path, &decision, max_access)?;
    check_secret_files(&path, &present)?;

    if present("body") && present("body_file") {
        return Err(ToolError::new(
            "`body` and `body_file` are mutually exclusive; pass only one of them",
        ));
    }
    if present("content_type") && !present("body_file") {
        return Err(ToolError::new(
            "`content_type` applies only to `body_file`; a JSON `body` is always sent as \
             application/json",
        ));
    }
    if method == HttpMethod::Get && (present("body") || present("body_file")) {
        return Err(ToolError::new(
            "GET requests cannot have a body; pass parameters in `query`",
        ));
    }

    // Проверка видимости — как у сгенерированного update_repository: тело в `body`.
    if let Some(op) = decision
        .matches
        .iter()
        .map(|m| m.op)
        .find(|op| op.tool_name == UPDATE_REPOSITORY_TOOL)
    {
        check_guards(op, args, max_access)?;
        if present("body_file") && !max_access.allows(AccessLevel::Destructive) {
            return Err(ToolError::new(
                "this request updates repository settings; pass the changes as a JSON `body` \
                 instead of `body_file` so the server can check them",
            ));
        }
    }

    let query = parse_query(args.get("query"))?;

    let body = if let Some(value) = args.get("body").filter(|value| !value.is_null()) {
        RequestBody::Json(value.clone())
    } else if let Some(value) = args.get("body_file").filter(|value| !value.is_null()) {
        let path = string_arg("body_file", value)?;
        let path = files
            .check_read(path)
            .map_err(|err| ToolError::new(format!("argument `body_file`: {err}")))?;
        let content_type = match args.get("content_type").filter(|value| !value.is_null()) {
            Some(value) => Some(string_arg("content_type", value)?.trim().to_owned()),
            None => None,
        };
        RequestBody::File { path, content_type }
    } else {
        RequestBody::Empty
    };

    let save_to = match args.get("save_to").filter(|value| !value.is_null()) {
        Some(value) => {
            let path = string_arg("save_to", value)?;
            if !std::path::Path::new(path).is_absolute() {
                return Err(ToolError::new(format!(
                    "argument `save_to`: `{path}` must be an absolute path"
                )));
            }
            Some(
                files
                    .check_write(path)
                    .map_err(|err| ToolError::new(format!("argument `save_to`: {err}")))?,
            )
        }
        None => None,
    };

    Ok(PreparedRequest {
        operation: decision.operation(),
        request: ApiRequest {
            method,
            path,
            query,
            body,
            save_to,
        },
    })
}

/// Файлы на эндпоинтах секретов запрещены: `save_to` записал бы ответ со значениями
/// секретов на диск в обход вырезания в `render`, а `body_file` передал бы значение мимо
/// `secret_set`.
fn check_secret_files(path: &str, present: &impl Fn(&str) -> bool) -> Result<(), ToolError> {
    if !is_secret_path(path) {
        return Ok(());
    }
    for arg in ["save_to", "body_file"] {
        if present(arg) {
            return Err(ToolError::new(format!(
                "`{arg}` cannot be used with secrets endpoints: secret values are hidden from \
                 the agent, and a file would bypass that. Call `api_request` without `{arg}` to \
                 see the keys; to see values, use `list_secrets` with `reveal_values: true` (only \
                 if the user started the server with --allow-secret-values), or ask the user to \
                 look them up in SourceCraft or with the `sourcecraft-mcp` CLI. To set a secret, \
                 use `secret_set` (it reads the value from a local file with `value_file`)"
            )));
        }
    }
    Ok(())
}

/// Уровень доступа и запрещённые операции.
fn check_access(
    method: HttpMethod,
    path: &str,
    decision: &AccessDecision,
    max_access: AccessLevel,
) -> Result<(), ToolError> {
    if decision.matches_tool(SET_SECRET_OPERATION) {
        return Err(ToolError::new(
            "setting a secret with a raw API request is not allowed: the API expects a \
             base64-encoded value and a mistake silently stores a wrong secret. Use the \
             `secret_set` tool, which takes the plain value and encodes it",
        ));
    }
    if max_access.allows(decision.required) {
        return Ok(());
    }
    let reason = match decision.operation() {
        Some(op) => format!("it matches the `{}` operation", op.tool_name),
        None => format!(
            "{} requests to paths that match no known operation default to {} access",
            method.as_str(),
            access_name(method.default_access())
        ),
    };
    Err(ToolError::new(format!(
        "{} {path} needs {} access ({reason}), but the server runs with {} access. Ask the user \
         to restart the server with {} if this change is intended",
        method.as_str(),
        access_name(decision.required),
        access_name(max_access),
        access_flag(decision.required),
    )))
}

fn reject_unknown(args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGUMENTS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let allowed: Vec<String> = ARGUMENTS.iter().map(|name| format!("`{name}`")).collect();
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {} (headers cannot be set)",
        unknown.join(", "),
        allowed.join(", ")
    )))
}

fn parse_method(value: Option<&Value>) -> Result<HttpMethod, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Err(ToolError::new("missing required argument(s): `method`"));
    };
    let method = string_arg("method", value)?.trim();
    let parsed = [
        HttpMethod::Get,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Patch,
        HttpMethod::Delete,
    ]
    .into_iter()
    .find(|candidate| candidate.as_str().eq_ignore_ascii_case(method));
    parsed.ok_or_else(|| {
        ToolError::new(format!(
            "argument `method` must be one of GET, POST, PUT, PATCH, DELETE; got `{method}`"
        ))
    })
}

fn parse_path(value: Option<&Value>) -> Result<String, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Err(ToolError::new("missing required argument(s): `path`"));
    };
    let path = string_arg("path", value)?.trim();
    if path.contains("://") || path.starts_with("//") {
        return Err(ToolError::new(
            "argument `path` must be relative to the SourceCraft API base, e.g. \
             `/repos/my-org/my-repo/pulls`; full URLs are not accepted",
        ));
    }
    if path.contains(['?', '#']) {
        return Err(ToolError::new(
            "argument `path` must not contain `?` or `#`; pass query parameters in `query`",
        ));
    }
    if !path.starts_with('/') {
        return Err(ToolError::new(
            "argument `path` must start with `/`, e.g. `/repos/my-org/my-repo/pulls`",
        ));
    }
    validate_relative_path(path).map_err(|_| {
        ToolError::new(
            "argument `path` is not a valid API path: use only URL path characters \
             (percent-encode others as %XX), and no empty, `.` or `..` segments",
        )
    })?;
    Ok(path.to_owned())
}

fn parse_query(value: Option<&Value>) -> Result<Vec<(String, String)>, ToolError> {
    let map = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(ToolError::new(
                "argument `query` must be an object of parameter names to values",
            ));
        }
    };
    let mut query = Vec::new();
    for (key, value) in map {
        if key.is_empty() {
            return Err(ToolError::new(
                "argument `query` has an empty parameter name",
            ));
        }
        match value {
            Value::Null => {}
            Value::Array(items) => {
                for item in items {
                    query.push((key.clone(), query_scalar(key, item)?));
                }
            }
            other => query.push((key.clone(), query_scalar(key, other)?)),
        }
    }
    Ok(query)
}

fn query_scalar(key: &str, value: &Value) -> Result<String, ToolError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(ToolError::new(format!(
            "query parameter `{key}` must be a string, number, boolean or an array of them"
        ))),
    }
}

fn string_arg<'a>(name: &str, value: &'a Value) -> Result<&'a str, ToolError> {
    value
        .as_str()
        .ok_or_else(|| ToolError::new(format!("argument `{name}` must be a string")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("не объект: {other}"),
        }
    }

    fn policy() -> FilePolicy {
        FilePolicy::new(&[]).unwrap()
    }

    fn prep(value: Value, max_access: AccessLevel) -> Result<PreparedRequest, ToolError> {
        prepare(&args(value), max_access, &policy())
    }

    fn err(value: Value, max_access: AccessLevel) -> String {
        prep(value, max_access).unwrap_err().message().to_owned()
    }

    #[test]
    fn get_with_query() {
        let prepared = prep(
            json!({
                "method": "get",
                "path": "/repos/acme/web/issues",
                "query": {"page_size": 5, "keys": ["A", "B"], "flag": true, "skip": null}
            }),
            AccessLevel::Read,
        )
        .unwrap();
        let request = prepared.request;
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/repos/acme/web/issues");
        let mut query = request.query.clone();
        query.sort();
        assert_eq!(
            query,
            vec![
                ("flag".into(), "true".into()),
                ("keys".into(), "A".into()),
                ("keys".into(), "B".into()),
                ("page_size".into(), "5".into()),
            ]
        );
        assert!(matches!(request.body, RequestBody::Empty));
        assert!(request.save_to.is_none());
        assert_eq!(
            prepared.operation.map(|op| op.tool_name),
            Some("list_repository_issues")
        );
    }

    #[test]
    fn unknown_path_has_no_operation() {
        let prepared = prep(
            json!({"method": "GET", "path": "/some/new/endpoint"}),
            AccessLevel::Read,
        )
        .unwrap();
        assert!(prepared.operation.is_none());
    }

    #[test]
    fn validation_errors() {
        let read = AccessLevel::Read;
        assert!(err(json!({"path": "/user"}), read).contains("`method`"));
        assert!(err(json!({"method": "GET"}), read).contains("`path`"));
        assert!(err(json!({"method": "HEAD", "path": "/user"}), read).contains("must be one of"));
        assert!(err(json!({"method": 1, "path": "/user"}), read).contains("must be a string"));
        assert!(
            err(
                json!({"method": "GET", "path": "https://evil.example/user"}),
                read
            )
            .contains("full URLs")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "//evil.example/user"}),
                read
            )
            .contains("full URLs")
        );
        assert!(err(json!({"method": "GET", "path": "/user?x=1"}), read).contains("`query`"));
        assert!(err(json!({"method": "GET", "path": "/user#x"}), read).contains("`query`"));
        assert!(err(json!({"method": "GET", "path": "user"}), read).contains("start with `/`"));
        assert!(
            err(json!({"method": "GET", "path": "/repos/../user"}), read)
                .contains("valid API path")
        );
        assert!(
            err(json!({"method": "GET", "path": "/repos/%2e%2e/user"}), read)
                .contains("valid API path")
        );
        assert!(err(json!({"method": "GET", "path": "/a b"}), read).contains("valid API path"));

        let unknown = err(
            json!({"method": "GET", "path": "/user", "headers": {"Authorization": "x"}}),
            read,
        );
        assert!(
            unknown.contains("unknown argument(s) `headers`"),
            "{unknown}"
        );
        assert!(unknown.contains("headers cannot be set"), "{unknown}");

        assert!(
            err(
                json!({"method": "GET", "path": "/user", "query": "a=1"}),
                read
            )
            .contains("must be an object")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/user", "query": {"a": {"b": 1}}}),
                read
            )
            .contains("query parameter `a`")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/user", "query": {"a": [[1]]}}),
                read
            )
            .contains("query parameter `a`")
        );
        assert!(
            err(json!({"method": "GET", "path": "/user", "body": {}}), read)
                .contains("GET requests cannot have a body")
        );

        let write = AccessLevel::Write;
        assert!(
            err(
                json!({"method": "POST", "path": "/x", "body": {}, "body_file": "/tmp/x"}),
                write
            )
            .contains("mutually exclusive")
        );
        assert!(
            err(
                json!({"method": "POST", "path": "/x", "content_type": "text/plain"}),
                write
            )
            .contains("only to `body_file`")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/user", "save_to": "relative.json"}),
                read
            )
            .contains("absolute path")
        );
    }

    #[test]
    fn access_is_checked_before_files() {
        let message = err(
            json!({"method": "POST", "path": "/repos/a/b/pulls", "body": {"title": "t"}}),
            AccessLevel::Read,
        );
        assert!(message.contains("needs write access"), "{message}");
        assert!(message.contains("`create_pull_request`"), "{message}");
        assert!(message.contains("--allow-write"), "{message}");

        let message = err(
            json!({"method": "DELETE", "path": "/some/thing"}),
            AccessLevel::Write,
        );
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("match no known operation"), "{message}");
        assert!(message.contains("--allow-destructive"), "{message}");

        // Отказ по доступу раньше проверки файлов: путь вне корней не раскрывается.
        let message = err(
            json!({"method": "POST", "path": "/x", "body_file": "/etc/hosts"}),
            AccessLevel::Read,
        );
        assert!(message.contains("--allow-write"), "{message}");

        // POST, который только читает, доступен на сервере только для чтения.
        let prepared = prep(
            json!({"method": "POST", "path": "/repos/a/b/releases/generate-notes", "body": {}}),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(
            prepared.operation.map(|op| op.tool_name),
            Some("generate_release_notes")
        );
        // DELETE метки — запись.
        assert!(
            prep(
                json!({"method": "DELETE", "path": "/issues/id:1/labels", "body": {}}),
                AccessLevel::Write
            )
            .is_ok()
        );
    }

    #[test]
    fn set_secret_is_refused_at_any_level() {
        for path in [
            "/repos/a/b/secrets/TOKEN",
            "/repos/id:r/secrets/TOKEN",
            "/repos/a/b/secr%65ts/TOKEN/",
        ] {
            let message = err(
                json!({"method": "PUT", "path": path, "body": {"value": "x"}}),
                AccessLevel::Destructive,
            );
            assert!(message.contains("`secret_set`"), "{path}: {message}");
        }
        // Чтение и удаление секретов — обычные операции со своим уровнем.
        assert!(
            prep(
                json!({"method": "GET", "path": "/repos/a/b/secrets"}),
                AccessLevel::Read
            )
            .is_ok()
        );
        assert!(
            prep(
                json!({"method": "DELETE", "path": "/repos/a/b/secrets/TOKEN"}),
                AccessLevel::Destructive
            )
            .is_ok()
        );
    }

    #[test]
    fn files_are_refused_on_secret_paths() {
        for path in [
            "/repos/a/b/secrets",
            "/repos/id:r/secrets",
            "/repos/a/b/SECR%65TS",
            "/operations/secrets/id:op",
        ] {
            let message = err(
                json!({"method": "GET", "path": path, "save_to": "/tmp/secrets.json"}),
                AccessLevel::Destructive,
            );
            assert!(
                message.contains("`save_to` cannot be used"),
                "{path}: {message}"
            );
            assert!(message.contains("reveal_values"), "{message}");
        }
        let message = err(
            json!({"method": "DELETE", "path": "/repos/a/b/secrets/K", "body_file": "/tmp/x"}),
            AccessLevel::Destructive,
        );
        assert!(message.contains("`body_file` cannot be used"), "{message}");
        // Без файлов чтение секретов разрешено (ответ очищается в `render`).
        assert!(
            prep(
                json!({"method": "GET", "path": "/repos/a/b/secrets"}),
                AccessLevel::Read
            )
            .is_ok()
        );
    }

    #[test]
    fn visibility_guard_applies() {
        for path in ["/repos/a/b", "/repos/id:r", "/repos/a/b/"] {
            let message = err(
                json!({"method": "PATCH", "path": path, "body": {"visibility": "public"}}),
                AccessLevel::Write,
            );
            assert!(message.contains("--allow-destructive"), "{path}: {message}");
        }
        assert!(
            prep(
                json!({"method": "PATCH", "path": "/repos/a/b", "body": {"visibility": "public"}}),
                AccessLevel::Destructive
            )
            .is_ok()
        );
        assert!(
            prep(
                json!({"method": "PATCH", "path": "/repos/a/b", "body": {"description": "d"}}),
                AccessLevel::Write
            )
            .is_ok()
        );
        let message = err(
            json!({"method": "PATCH", "path": "/repos/a/b", "body_file": "/tmp/x.json"}),
            AccessLevel::Write,
        );
        assert!(message.contains("instead of `body_file`"), "{message}");
    }

    #[test]
    fn files_go_through_policy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let input = root.join("body.bin");
        std::fs::write(&input, b"raw").unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();

        let prepared = prepare(
            &args(json!({
                "method": "POST",
                "path": "/x/upload",
                "body_file": input.display().to_string(),
                "content_type": "application/zip",
                "save_to": root.join("out.json").display().to_string()
            })),
            AccessLevel::Write,
            &files,
        )
        .unwrap();
        assert!(matches!(
            prepared.request.body,
            RequestBody::File { ref path, content_type: Some(ref ct) } if path == &input && ct == "application/zip"
        ));
        assert_eq!(prepared.request.save_to, Some(root.join("out.json")));

        let message = prepare(
            &args(json!({"method": "POST", "path": "/x", "body_file": "/etc/hosts"})),
            AccessLevel::Write,
            &files,
        )
        .unwrap_err();
        assert!(
            message
                .message()
                .contains("argument `body_file`: `/etc/hosts` is outside"),
            "{message}"
        );
        let message = prepare(
            &args(json!({"method": "GET", "path": "/user", "save_to": "/etc/out.json"})),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(
            message.message().contains("argument `save_to`"),
            "{message}"
        );
    }

    #[test]
    fn tool_annotations_follow_max_access() {
        let read = api_request_tool(AccessLevel::Read);
        assert_eq!(
            read.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        let write = api_request_tool(AccessLevel::Write);
        let a = write.annotations.as_ref().unwrap();
        assert_eq!(
            (a.read_only_hint, a.destructive_hint),
            (Some(false), Some(false))
        );
        let all = api_request_tool(AccessLevel::Destructive);
        assert_eq!(
            all.annotations.as_ref().unwrap().destructive_hint,
            Some(true)
        );
        let description = read.description.unwrap_or_default();
        assert!(description.contains("api_search"));
        assert!(description.contains("Toolsets do not restrict"));
    }
}
