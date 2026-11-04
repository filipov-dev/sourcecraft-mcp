//! Инструменты для секретов репозитория: `secret_set` и аргумент `reveal_values`.
//!
//! - `secret_set` заменяет сгенерированный `set_secret` (тот ждёт значение уже в base64, и
//!   агенту легко записать неверное значение). Он принимает открытый текст (`value`) или путь
//!   к локальному файлу (`value_file`, проверяется [`FilePolicy::check_read`]) и сам кодирует
//!   значение в канонический base64 (стандартный алфавит с `=`). С `value_file` значение не
//!   попадает в контекст агента. Запрос собирается по операции `set_secret`, поэтому адресация
//!   (slug или ID) проверяется так же, как в сгенерированных инструментах.
//! - `reveal_values: true` у инструментов чтения секретов отключает вырезание значений
//!   ([`crate::redact`]) для одного вызова. В схему аргумент попадает, только если сервер
//!   запущен с `allow_secret_values`; без флага вызов с `reveal_values: true` отклоняется.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use sourcecraft_api::executor::ApiRequest;
use sourcecraft_api::spec::{HttpMethod, OperationDef, find};

use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::{DECODED_VALUE_FIELD, REDACTED, is_secret_path};
use crate::request::build_request;

/// Имя инструмента записи секрета.
pub const SECRET_SET_TOOL: &str = "secret_set";

/// Сгенерированная операция, по которой `secret_set` собирает запрос.
pub const SET_SECRET_OPERATION: &str = "set_secret";

/// Аргумент раскрытия значений секретов.
pub const REVEAL_ARG: &str = "reveal_values";

/// Максимальный размер значения секрета из `value_file`.
pub const MAX_SECRET_BYTES: u64 = 64 * 1024;

/// Аргументы `secret_set`.
const SECRET_SET_ARGS: &[&str] = &[
    "org_slug",
    "repo_slug",
    "repo_id",
    "key",
    "secret_group",
    "value",
    "value_file",
    "trim_trailing_newline",
];

/// Аргументы, которые `secret_set` передаёт операции `set_secret` как есть.
const PASSTHROUGH_ARGS: &[&str] = &["org_slug", "repo_slug", "repo_id", "key", "secret_group"];

/// Короче этого значение не вычищается из текстов ошибок: короткая строка совпадёт
/// с чем угодно и испортит сообщение, а секретом почти наверняка не является.
const MIN_SCRUB_LEN: usize = 4;

/// Кодирует значение секрета в канонический base64 (стандартный алфавит с `=`), как ждёт API.
pub fn encode_secret_value(value: &[u8]) -> String {
    STANDARD.encode(value)
}

/// Поддерживает ли операция `reveal_values`: чтение с эндпоинта секретов.
pub fn supports_reveal(op: &OperationDef) -> bool {
    op.method == HttpMethod::Get && is_secret_path(op.path)
}

/// Дополняет описание инструмента операции с секретами: значения скрыты, а при
/// `allow_secret_values` добавляет в схему аргумент `reveal_values`.
pub fn annotate_operation_tool(op: &OperationDef, tool: &mut Tool, allow_secret_values: bool) {
    if !supports_reveal(op) {
        return;
    }
    let mut description = tool.description.as_deref().unwrap_or_default().to_owned();
    if allow_secret_values {
        let mut schema = (*tool.input_schema).clone();
        if let Some(Value::Object(properties)) = schema.get_mut("properties") {
            properties.insert(
                REVEAL_ARG.to_owned(),
                json!({
                    "type": "boolean",
                    "description": format!(
                        "Show secret values instead of \"{REDACTED}\". Use only when the user \
                         explicitly needs to see the values. Values are base64 as returned by \
                         the API; `{DECODED_VALUE_FIELD}` holds the decoded text when it is \
                         UTF-8. Default: false."
                    ),
                }),
            );
        }
        tool.input_schema = Arc::new(schema);
        description.push_str(&format!(
            "\n\nSecret values are replaced with \"{REDACTED}\" unless `{REVEAL_ARG}: true` is \
             passed. Do not reveal values just to check that a secret exists: the keys are \
             always shown."
        ));
    } else {
        description.push_str(&format!(
            "\n\nSecret values are replaced with \"{REDACTED}\"; only keys and metadata are \
             shown. This server does not reveal secret values."
        ));
    }
    tool.description = Some(description.into());
}

/// Извлекает `reveal_values` из аргументов. Для операций без раскрытия аргумент не трогается
/// (его отклонит проверка неизвестных аргументов).
pub fn take_reveal(
    op: &OperationDef,
    args: &mut JsonObject,
    allow_secret_values: bool,
) -> Result<bool, ToolError> {
    if !supports_reveal(op) {
        return Ok(false);
    }
    let reveal = match args.remove(REVEAL_ARG) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => value,
        Some(_) => {
            return Err(ToolError::new(format!(
                "argument `{REVEAL_ARG}` must be a boolean"
            )));
        }
    };
    if reveal && !allow_secret_values {
        return Err(ToolError::new(format!(
            "revealing secret values is disabled on this server: call `{}` without \
             `{REVEAL_ARG}` to see the keys. Only the user can allow revealing values, by \
             restarting the server with --allow-secret-values",
            op.tool_name
        )));
    }
    Ok(reveal)
}

/// Описание инструмента `secret_set`. Схема адресации и описания аргументов берутся из
/// операции `set_secret`, тело (`body`) заменяется аргументами значения.
pub fn secret_set_tool(op: &OperationDef) -> Result<Tool, String> {
    let mut schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;
    let addressing = schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        properties.remove("body");
        properties.insert(
            "value".to_owned(),
            json!({
                "type": "string",
                "description": "The secret value as plain text; the server base64-encodes it. \
                    Do not encode it yourself. Prefer `value_file` so the value does not appear \
                    in the conversation. Pass exactly one of `value` or `value_file`.",
            }),
        );
        properties.insert(
            "value_file".to_owned(),
            json!({
                "type": "string",
                "description": "Path to a local file whose content is the secret value \
                    (any bytes, up to 64 KiB). The path must be inside the directories allowed \
                    by the server. The value is read by the server and never shown.",
            }),
        );
        properties.insert(
            "trim_trailing_newline".to_owned(),
            json!({
                "type": "boolean",
                "description": "Remove one trailing line ending (`\\n` or `\\r\\n`) from the \
                    value. Default: true for `value_file` (files usually end with a newline), \
                    false for `value`.",
            }),
        );
    }
    schema.insert(
        "required".to_owned(),
        Value::Array(vec![Value::String("key".to_owned())]),
    );

    let description = format!(
        "Create or update a repository secret (secrets are used by SourceCraft CI).\n\n\
         Takes the value as plain text and base64-encodes it itself. Prefer `value_file`: the \
         server reads the value from a local file, so it never enters the conversation (for \
         example, ask the user to put it into a file). Empty values are rejected; to remove a \
         secret use `delete_secret`. The secret is set asynchronously: the response is an \
         operation, poll it with `get_secret_operation`. Never echo, log or commit the value.\n\n\
         {addressing}\n\n\
         Access: destructive (overwrites the secret; the old value cannot be restored)."
    );
    let mut tool = Tool::new(SECRET_SET_TOOL, description, Arc::new(schema));
    tool.title = Some("Create or update a secret from plain text or a file".to_owned());
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(true),
    );
    Ok(tool)
}

/// Готовый запрос `secret_set` и строки, которые нужно вычистить из ответа и ошибок.
#[derive(Debug)]
pub struct SecretSetRequest {
    pub request: ApiRequest,
    /// Значение и его base64: API может вернуть их в тексте ошибки.
    pub sensitive: Vec<String>,
}

/// Собирает запрос `secret_set` из аргументов инструмента.
pub async fn build_secret_set(
    args: &JsonObject,
    files: &FilePolicy,
) -> Result<SecretSetRequest, ToolError> {
    let op = find(SET_SECRET_OPERATION).ok_or_else(|| {
        ToolError::new("the SourceCraft API specification has no operation to set secrets")
    })?;
    let present = |name: &str| args.get(name).filter(|value| !value.is_null());

    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !SECRET_SET_ARGS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if !unknown.is_empty() {
        let allowed: Vec<String> = SECRET_SET_ARGS.iter().map(|a| format!("`{a}`")).collect();
        return Err(ToolError::new(format!(
            "unknown argument(s) {}; allowed arguments: {}",
            unknown.join(", "),
            allowed.join(", ")
        )));
    }

    let string_arg = |name: &str| match present(name) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    };
    let trim = match present("trim_trailing_newline") {
        None => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Err(ToolError::new(
                "argument `trim_trailing_newline` must be a boolean",
            ));
        }
    };

    let (mut value, trim) = match (string_arg("value")?, string_arg("value_file")?) {
        (Some(_), Some(_)) => {
            return Err(ToolError::new(
                "pass exactly one of `value` or `value_file`, not both",
            ));
        }
        (None, None) => {
            return Err(ToolError::new(
                "missing the secret value: pass `value_file` (a local file with the value, \
                 preferred: the value never enters the conversation) or `value` (plain text)",
            ));
        }
        (Some(value), None) => (value.as_bytes().to_vec(), trim.unwrap_or(false)),
        (None, Some(path)) => (read_value_file(path, files).await?, trim.unwrap_or(true)),
    };
    if trim {
        trim_trailing_newline(&mut value);
    }
    if value.is_empty() {
        return Err(ToolError::new(
            "the secret value is empty; SourceCraft does not store empty secrets. To remove a \
             secret use `delete_secret`",
        ));
    }

    let encoded = encode_secret_value(&value);
    let mut op_args = JsonObject::new();
    for name in PASSTHROUGH_ARGS {
        if let Some(arg) = present(name) {
            op_args.insert((*name).to_owned(), arg.clone());
        }
    }
    op_args.insert("body".to_owned(), json!({ "value": encoded }));
    let request = build_request(op, &op_args, files)?;

    let mut sensitive = vec![encoded];
    if let Ok(text) = String::from_utf8(value) {
        sensitive.push(text);
    }
    sensitive.retain(|s| s.len() >= MIN_SCRUB_LEN);
    Ok(SecretSetRequest { request, sensitive })
}

async fn read_value_file(path: &str, files: &FilePolicy) -> Result<Vec<u8>, ToolError> {
    let canonical = files
        .check_read(path)
        .map_err(|err| ToolError::new(format!("`value_file`: {err}")))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|err| ToolError::new(format!("`value_file`: cannot read `{path}`: {err}")))?;
    if metadata.len() > MAX_SECRET_BYTES {
        return Err(ToolError::new(format!(
            "`value_file`: `{path}` is larger than {MAX_SECRET_BYTES} bytes; a secret value \
             cannot be that large"
        )));
    }
    tokio::fs::read(&canonical)
        .await
        .map_err(|err| ToolError::new(format!("`value_file`: cannot read `{path}`: {err}")))
}

/// Убирает одно завершающее `\n` или `\r\n`.
fn trim_trailing_newline(value: &mut Vec<u8>) {
    if value.last() == Some(&b'\n') {
        value.pop();
        if value.last() == Some(&b'\r') {
            value.pop();
        }
    }
}

/// Заменяет вхождения чувствительных строк на [`REDACTED`].
pub fn scrub(text: &str, sensitive: &[String]) -> String {
    sensitive
        .iter()
        .filter(|needle| !needle.is_empty())
        .fold(text.to_owned(), |text, needle| {
            text.replace(needle.as_str(), REDACTED)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use base64::engine::general_purpose::STANDARD;
    use sourcecraft_api::executor::RequestBody;

    use super::*;

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("не объект"),
        }
    }

    fn body_value(request: &ApiRequest) -> String {
        match &request.body {
            RequestBody::Json(body) => body["value"].as_str().unwrap().to_owned(),
            other => panic!("{other:?}"),
        }
    }

    struct Files {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        policy: FilePolicy,
    }

    fn files() -> Files {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root))
            .unwrap()
            .with_base_dir(&root);
        Files {
            _dir: dir,
            root,
            outside,
            policy,
        }
    }

    #[test]
    fn encoding_is_canonical_base64() {
        assert_eq!(encode_secret_value(b"hello"), "aGVsbG8=");
        assert_eq!(encode_secret_value(b"hi"), "aGk=");
        assert_eq!(encode_secret_value(b"abc"), "YWJj");
        // Стандартный алфавит (`+` и `/`), не URL-safe.
        assert_eq!(encode_secret_value(&[0xfb, 0xff]), "+/8=");
        assert_eq!(encode_secret_value("пароль".as_bytes()), "0L/QsNGA0L7Qu9GM");
        assert_eq!(
            STANDARD
                .decode(encode_secret_value("ключ 🔑".as_bytes()))
                .unwrap(),
            "ключ 🔑".as_bytes()
        );
    }

    #[test]
    fn trims_one_line_ending() {
        for (input, expected) in [
            (&b"a\n"[..], &b"a"[..]),
            (b"a\r\n", b"a"),
            (b"a\n\n", b"a\n"),
            (b"a", b"a"),
            (b"a\r", b"a\r"),
            (b"", b""),
        ] {
            let mut value = input.to_vec();
            trim_trailing_newline(&mut value);
            assert_eq!(value, expected, "{input:?}");
        }
    }

    #[tokio::test]
    async fn builds_request_from_plain_value() {
        let f = files();
        let built = build_secret_set(
            &args(json!({
                "org_slug": "acme", "repo_slug": "web", "key": "API_TOKEN",
                "value": "s3cret\n", "secret_group": "ci"
            })),
            &f.policy,
        )
        .await
        .unwrap();
        assert_eq!(built.request.method, HttpMethod::Put);
        assert_eq!(built.request.path, "/repos/acme/web/secrets/API_TOKEN");
        assert_eq!(
            built.request.query,
            [("secret_group".to_owned(), "ci".to_owned())]
        );
        // Для `value` перевод строки по умолчанию сохраняется.
        assert_eq!(body_value(&built.request), encode_secret_value(b"s3cret\n"));
        assert!(built.sensitive.contains(&"s3cret\n".to_owned()));

        let built = build_secret_set(
            &args(json!({
                "repo_id": "42", "key": "K", "value": "пароль\n", "trim_trailing_newline": true
            })),
            &f.policy,
        )
        .await
        .unwrap();
        assert_eq!(built.request.path, "/repos/id:42/secrets/K");
        assert_eq!(body_value(&built.request), "0L/QsNGA0L7Qu9GM");
    }

    #[tokio::test]
    async fn reads_value_file() {
        let f = files();
        fs::write(f.root.join("token.txt"), "from-file\n").unwrap();
        fs::write(f.root.join("binary.bin"), [0xff, 0x00, 0x0a]).unwrap();

        let built = build_secret_set(
            &args(
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value_file": "token.txt"}),
            ),
            &f.policy,
        )
        .await
        .unwrap();
        assert_eq!(
            body_value(&built.request),
            encode_secret_value(b"from-file")
        );

        let built = build_secret_set(
            &args(json!({
                "org_slug": "a", "repo_slug": "b", "key": "K",
                "value_file": f.root.join("token.txt").display().to_string(),
                "trim_trailing_newline": false
            })),
            &f.policy,
        )
        .await
        .unwrap();
        assert_eq!(
            body_value(&built.request),
            encode_secret_value(b"from-file\n")
        );

        // Произвольные байты кодируются как есть.
        let built = build_secret_set(
            &args(json!({"repo_id": "1", "key": "K", "value_file": "binary.bin"})),
            &f.policy,
        )
        .await
        .unwrap();
        assert_eq!(
            body_value(&built.request),
            encode_secret_value(&[0xff, 0x00])
        );
    }

    #[tokio::test]
    async fn rejects_bad_arguments() {
        let f = files();
        fs::write(f.outside.join("secret.txt"), "x").unwrap();
        fs::write(f.root.join("empty.txt"), "\n").unwrap();
        fs::write(
            f.root.join("big.txt"),
            vec![b'a'; MAX_SECRET_BYTES as usize + 1],
        )
        .unwrap();
        let outside = f.outside.join("secret.txt").display().to_string();
        let cases = [
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value": "x", "value_file": "f"}),
                "exactly one of",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K"}),
                "missing the secret value",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value": ""}),
                "empty",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value_file": "empty.txt"}),
                "empty",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value_file": outside}),
                "outside the allowed directories",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value_file": "missing.txt"}),
                "does not exist",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value_file": "big.txt"}),
                "larger than",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value": 5}),
                "`value` must be a string",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value": "x",
                       "trim_trailing_newline": "yes"}),
                "must be a boolean",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "key": "K", "value": "x",
                       "body": {"value": "eA=="}}),
                "unknown argument(s) `body`",
            ),
            (
                json!({"org_slug": "a", "repo_slug": "b", "value": "x"}),
                "missing required argument(s): `key`",
            ),
            (
                json!({"org_slug": "a", "repo_id": "1", "key": "K", "value": "x"}),
                "not both",
            ),
        ];
        for (input, expected) in cases {
            let err = build_secret_set(&args(input.clone()), &f.policy)
                .await
                .unwrap_err();
            assert!(err.message().contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn tool_schema() {
        let op = find(SET_SECRET_OPERATION).unwrap();
        let tool = secret_set_tool(op).unwrap();
        assert_eq!(tool.name, SECRET_SET_TOOL);
        let properties = tool.input_schema["properties"].as_object().unwrap();
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        let mut expected = SECRET_SET_ARGS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(tool.input_schema["required"], json!(["key"]));
        assert_eq!(tool.input_schema["additionalProperties"], json!(false));
        let annotations = tool.annotations.unwrap();
        assert_eq!(annotations.destructive_hint, Some(true));
        assert!(tool.description.unwrap().contains("Access: destructive"));
    }

    #[test]
    fn reveal_argument() {
        let list = find("list_secrets").unwrap();
        let operation = find("get_secret_operation").unwrap();
        assert!(supports_reveal(list));
        assert!(supports_reveal(operation));
        assert!(!supports_reveal(find("set_secret").unwrap()));
        assert!(!supports_reveal(find("delete_secret").unwrap()));
        assert!(!supports_reveal(find("get_repository").unwrap()));

        let mut a = args(json!({"org_slug": "a", "reveal_values": true}));
        assert!(take_reveal(list, &mut a, true).unwrap());
        assert!(!a.contains_key(REVEAL_ARG));

        let mut a = args(json!({"reveal_values": true}));
        let err = take_reveal(list, &mut a, false).unwrap_err();
        assert!(err.message().contains("--allow-secret-values"), "{err}");

        let mut a = args(json!({"reveal_values": false}));
        assert!(!take_reveal(list, &mut a, false).unwrap());
        let mut a = args(json!({"reveal_values": "yes"}));
        assert!(take_reveal(list, &mut a, true).is_err());

        // Для других операций аргумент не извлекается.
        let repo = find("get_repository").unwrap();
        let mut a = args(json!({"reveal_values": true}));
        assert!(!take_reveal(repo, &mut a, true).unwrap());
        assert!(a.contains_key(REVEAL_ARG));
    }

    #[test]
    fn annotates_secret_tools() {
        let list = find("list_secrets").unwrap();
        let mut tool = crate::registry::operation_tool(list).unwrap();
        annotate_operation_tool(list, &mut tool, false);
        assert!(tool.input_schema["properties"].get(REVEAL_ARG).is_none());
        assert!(tool.description.as_deref().unwrap().contains(REDACTED));

        let mut tool = crate::registry::operation_tool(list).unwrap();
        annotate_operation_tool(list, &mut tool, true);
        assert_eq!(
            tool.input_schema["properties"][REVEAL_ARG]["type"],
            "boolean"
        );

        let repo = find("get_repository").unwrap();
        let mut tool = crate::registry::operation_tool(repo).unwrap();
        let before = tool.clone();
        annotate_operation_tool(repo, &mut tool, true);
        assert_eq!(tool, before);
    }

    #[test]
    fn scrubs_values() {
        let text = "bad value s3cret (czNjcmV0)";
        assert_eq!(
            scrub(text, &["s3cret".into(), "czNjcmV0".into()]),
            "bad value [redacted] ([redacted])"
        );
        assert_eq!(scrub(text, &[]), text);
    }
}
