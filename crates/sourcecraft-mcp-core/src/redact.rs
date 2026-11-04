//! Вырезание значений секретов и учётных данных из ответов API.
//!
//! Значения секретов не должны попадать в контекст агента без явного запроса. Правила:
//!
//! 1. **Эндпоинты секретов** ([`is_secret_path`]): `/repos/{org}/{repo}/secrets…`,
//!    `/repos/id:{repo_id}/secrets…` и `/operations/secrets/…`. В JSON-ответе заменяется
//!    значение каждого поля `value` на любой глубине — не только в `SecretEntry`: схема
//!    результата операции над секретами в спецификации непрозрачна (`object`), поэтому
//!    на форму объекта не полагаемся. Текстовое (не JSON или обрезанное) тело заменяется
//!    заглушкой целиком. Правило отключает только `reveal_values: true`, а его принимает
//!    лишь сервер с `allow_secret_values`.
//! 2. **Учётные данные на всех эндпоинтах** ([`is_credential_key`]): строковые значения полей,
//!    имя которых похоже на пароль, токен или ключ ([`CREDENTIAL_KEYS`]). Действует всегда,
//!    в том числе при `reveal_values: true`: раскрываются значения секретов, а не случайные
//!    учётные данные в соседних полях. Для текстовых тел то же правило применяется к парам
//!    `"ключ": "строка"` в тексте (обрезанный JSON).
//!
//! Тела ошибок обрабатываются так же ([`redact_api_error`]), без раскрытия.
//! Функции не зависят от `OperationDef`: им нужен только относительный путь запроса,
//! поэтому они работают и для сгенерированных инструментов, и для произвольных запросов.

use std::collections::BTreeSet;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::percent_decode_str;
use serde_json::Value;
use sourcecraft_api::executor::{ApiError, ApiResponse, ResponseBody};

/// Чем заменяются скрытые значения.
pub const REDACTED: &str = "[redacted]";

/// Поле с раскодированным значением секрета, которое добавляется при раскрытии.
pub const DECODED_VALUE_FIELD: &str = "value_text";

/// Имена полей с учётными данными. Совпадение — имя целиком или суффикс после `_`
/// (`db_password`, `runner_token`) без учёта регистра; camelCase и `-` приводятся к `_`.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "secret_value",
    "token",
    "access_token",
    "refresh_token",
    "private_key",
    "api_key",
    "client_secret",
    "webhook_secret",
];

/// Суффиксы имён, которые совпадают с [`CREDENTIAL_KEYS`], но не являются учётными данными:
/// токены пагинации (`next_page_token`) нужны агенту для следующей страницы.
const NOT_CREDENTIAL_SUFFIXES: &[&str] = &["page_token"];

/// Префиксы флагов (`has_password`, `is_secret`): это признаки, а не значения.
const FLAG_PREFIXES: &[&str] = &["has_", "is_"];

/// Сколько раз раскодировать сегмент пути (как в проверке путей исполнителя).
const MAX_DECODE_ROUNDS: usize = 3;

/// Что было скрыто в одном ответе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// Число скрытых значений секретов (поля `value` на эндпоинтах секретов).
    pub secret_values: usize,
    /// Текстовое тело ответа эндпоинта секретов заменено заглушкой.
    pub body_hidden: bool,
    /// Имена полей, похожих на учётные данные, значения которых скрыты.
    pub credential_fields: BTreeSet<String>,
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.secret_values == 0 && !self.body_hidden && self.credential_fields.is_empty()
    }

    /// Пояснение для агента: что скрыто и как с этим работать. `None`, если ничего не скрыто.
    ///
    /// `reveal_allowed` — включён ли на сервере `allow_secret_values`.
    pub fn note(&self, reveal_allowed: bool) -> Option<String> {
        let mut lines = Vec::new();
        if self.secret_values > 0 || self.body_hidden {
            let what = if self.body_hidden {
                "The response of this secrets endpoint is not shown because it may contain \
                 secret values."
                    .to_owned()
            } else {
                format!(
                    "Secret values are hidden: {} value(s) were replaced with \"{REDACTED}\".",
                    self.secret_values
                )
            };
            let reveal = if reveal_allowed {
                "If the user explicitly needs to see the values, call `list_secrets` with \
                 `reveal_values: true`; never copy revealed values into files, commands or \
                 messages."
            } else {
                "This server does not reveal secret values (the user can allow it by starting \
                 the server with --allow-secret-values)."
            };
            lines.push(format!(
                "[{what} {reveal} To use a secret without seeing it, refer to it by its key \
                 (for example in CI configuration) or run a shell command with it through \
                 `sourcecraft-mcp secret exec` (see the server instructions); to change it, use \
                 `secret_set`.]"
            ));
        }
        if !self.credential_fields.is_empty() {
            let fields: Vec<&str> = self.credential_fields.iter().map(String::as_str).collect();
            lines.push(format!(
                "[Values of fields that look like credentials were replaced with \"{REDACTED}\": \
                 {}.]",
                fields.join(", ")
            ));
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// Относится ли путь запроса к секретам репозитория.
///
/// Путь — относительный путь API (`/repos/acme/web/secrets/KEY`), допускаются query, шаблоны
/// (`{org_slug}`) и percent-кодирование: сегменты раскодируются и сравниваются без учёта
/// регистра, поэтому `%73ecrets` или `SECRETS` не обходят проверку.
pub fn is_secret_path(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let segments: Vec<String> = path
        .split('/')
        .map(decode_segment)
        .filter(|segment| !segment.is_empty() && segment != ".")
        .collect();
    let seg = |index: usize| segments.get(index).map(String::as_str);
    (0..segments.len()).any(|k| match seg(k) {
        Some("repos") => {
            let by_id = seg(k + 1).is_some_and(|repo| repo.starts_with("id:"))
                && seg(k + 2) == Some("secrets");
            by_id || seg(k + 3) == Some("secrets")
        }
        Some("operations") => seg(k + 1) == Some("secrets"),
        _ => false,
    })
}

fn decode_segment(segment: &str) -> String {
    let mut current = segment.to_owned();
    for _ in 0..MAX_DECODE_ROUNDS {
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            break;
        }
        current = decoded;
    }
    current.trim().to_lowercase()
}

/// Похоже ли имя поля на поле с учётными данными (см. [`CREDENTIAL_KEYS`]).
pub fn is_credential_key(key: &str) -> bool {
    let key = normalize_key(key);
    if FLAG_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || NOT_CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
    {
        return false;
    }
    CREDENTIAL_KEYS.iter().any(|candidate| {
        key == *candidate
            || key
                .strip_suffix(candidate)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

/// `accessToken`, `Access-Token` → `access_token`.
fn normalize_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for c in key.trim().chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            normalized.push('_');
        }
        if matches!(c, '-' | ' ' | '.') {
            normalized.push('_');
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized
}

/// Вырезает значения из JSON-ответа запроса по пути `path` (правила 1 и 2 модуля).
///
/// При `reveal_secret_values` правило 1 не применяется, а к записям секретов с base64-значением
/// добавляется поле [`DECODED_VALUE_FIELD`] с текстом (если это UTF-8).
pub fn redact_json(path: &str, json: &mut Value, reveal_secret_values: bool) -> Redactions {
    let secret_endpoint = is_secret_path(path);
    let mut redactions = Redactions::default();
    walk(
        json,
        secret_endpoint && !reveal_secret_values,
        &mut redactions,
    );
    if secret_endpoint && reveal_secret_values {
        add_decoded_values(json);
    }
    redactions
}

fn walk(value: &mut Value, hide_secret_values: bool, redactions: &mut Redactions) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if hide_secret_values && key.eq_ignore_ascii_case("value") && !item.is_null() {
                    *item = Value::String(REDACTED.to_owned());
                    redactions.secret_values += 1;
                    continue;
                }
                if let Value::String(text) = item {
                    if !text.is_empty() && is_credential_key(key) {
                        *text = REDACTED.to_owned();
                        redactions.credential_fields.insert(key.clone());
                    }
                    continue;
                }
                walk(item, hide_secret_values, redactions);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, hide_secret_values, redactions);
            }
        }
        _ => {}
    }
}

/// Значения секретов в API — base64 (`format: byte`). Агенту, которому раскрыли значения,
/// удобнее текст: добавляем его рядом, исходное поле не трогаем.
fn add_decoded_values(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let decoded = match (map.get("key"), map.get("value")) {
                (Some(Value::String(_)), Some(Value::String(encoded))) => STANDARD
                    .decode(encoded)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok()),
                _ => None,
            };
            if let Some(text) = decoded {
                map.insert(DECODED_VALUE_FIELD.to_owned(), Value::String(text));
            }
            for item in map.values_mut() {
                add_decoded_values(item);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(add_decoded_values),
        _ => {}
    }
}

/// Вырезает значения из текстового тела ответа.
///
/// Тело эндпоинта секретов без раскрытия заменяется заглушкой целиком: обрезанный или не-JSON
/// текст нельзя надёжно разобрать. В остальных случаях скрываются строковые значения пар
/// `"ключ": "строка"` с ключами учётных данных, в том числе незакрытая строка в конце.
pub fn redact_text(path: &str, text: &str, reveal_secret_values: bool) -> (String, Redactions) {
    let mut redactions = Redactions::default();
    if is_secret_path(path) && !reveal_secret_values && !text.trim().is_empty() {
        redactions.body_hidden = true;
        return (
            format!("[hidden: {} bytes from a secrets endpoint]", text.len()),
            redactions,
        );
    }
    let text = redact_credential_pairs(text, &mut redactions);
    (text, redactions)
}

fn redact_credential_pairs(text: &str, redactions: &mut Redactions) -> String {
    let bytes = text.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(offset) = text.get(i..).and_then(|rest| rest.find('"')) {
        let start = i + offset;
        let (key_end, after_key) = string_end(bytes, start);
        i = after_key;
        let colon = skip_ws(after_key);
        if bytes.get(colon) != Some(&b':') {
            continue;
        }
        let value_start = skip_ws(colon + 1);
        if bytes.get(value_start) != Some(&b'"') {
            continue;
        }
        let (value_end, after_value) = string_end(bytes, value_start);
        i = after_value;
        let key = text.get(start + 1..key_end).unwrap_or_default();
        if value_end > value_start + 1 && is_credential_key(key) {
            result.push_str(text.get(copied..value_start).unwrap_or_default());
            result.push('"');
            result.push_str(REDACTED);
            result.push('"');
            copied = after_value;
            redactions.credential_fields.insert(key.to_owned());
        }
    }
    result.push_str(text.get(copied..).unwrap_or_default());
    result
}

/// Для строки, открытой кавычкой в `start`: позиция закрывающей кавычки (или конец текста)
/// и позиция сразу после строки. Обе позиции — границы символов UTF-8: это `"` или конец.
fn string_end(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut j = start + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return (j, j + 1),
            _ => j += 1,
        }
    }
    (bytes.len(), bytes.len())
}

/// Вырезает значения из успешного ответа API и возвращает, что скрыто.
pub fn redact_response(
    path: &str,
    response: &mut ApiResponse,
    reveal_secret_values: bool,
) -> Redactions {
    match &mut response.body {
        ResponseBody::Json(json) => redact_json(path, json, reveal_secret_values),
        ResponseBody::Text(text) => {
            let (redacted, redactions) = redact_text(path, text, reveal_secret_values);
            *text = redacted;
            redactions
        }
        // Файл агент не видит, пустое тело скрывать нечего.
        ResponseBody::SavedToFile { .. } | ResponseBody::Empty => Redactions::default(),
    }
}

/// Тело ошибки API с вырезанными значениями (без раскрытия).
///
/// JSON обрабатывается как успешный ответ; не-JSON тело эндпоинта секретов скрывается целиком,
/// в остальных случаях скрываются пары с ключами учётных данных.
pub fn redact_error_body(path: &str, body: &str) -> String {
    if let Ok(mut json) = serde_json::from_str::<Value>(body) {
        let redactions = redact_json(path, &mut json, false);
        return if redactions.is_empty() {
            body.to_owned()
        } else {
            json.to_string()
        };
    }
    redact_text(path, body, false).0
}

/// Ошибка API с вырезанными значениями в теле ответа.
pub fn redact_api_error(path: &str, err: ApiError) -> ApiError {
    match err {
        ApiError::Http { status, body } => ApiError::Http {
            status,
            body: redact_error_body(path, &body),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LIST: &str = "/repos/acme/web/secrets";

    #[test]
    fn secret_paths() {
        for path in [
            "/repos/acme/web/secrets",
            "/repos/acme/web/secrets/",
            "/repos/acme/web/secrets/DEPLOY_KEY",
            "/repos/acme/web/secrets?secret_group=default",
            "/repos/id:42/secrets",
            "/repos/id:42/secrets/KEY",
            "/repos/{org_slug}/{repo_slug}/secrets/{key}",
            "/repos/id:{repo_id}/secrets",
            "/operations/secrets/id:op-1",
            "/operations/secrets/id:{operation_id}",
            "/repos/acme/web/%73ecrets",
            "/repos/acme/web/%2573ecrets",
            "/repos/acme/web/SECRETS",
            "repos/acme/web/secrets",
            "/repos/acme/web/./secrets",
        ] {
            assert!(is_secret_path(path), "{path}");
        }
        for path in [
            "/repos/acme/web",
            "/repos/acme/secrets",
            "/repos/acme/web/issues",
            "/repos/acme/web/contents/secrets/file.txt",
            "/repos/acme/web/secretsx",
            "/operations/id:op-1",
            "/user",
            "",
        ] {
            assert!(!is_secret_path(path), "{path}");
        }
    }

    #[test]
    fn credential_keys() {
        for key in [
            "password",
            "PASSWORD",
            "db_password",
            "passwd",
            "secret",
            "secret_value",
            "client_secret",
            "webhook_secret",
            "shared_secret",
            "token",
            "access_token",
            "accessToken",
            "Refresh-Token",
            "runner_token",
            "private_key",
            "ssh_private_key",
            "privateKey",
            "api_key",
            "apiKey",
            "passphrase",
        ] {
            assert!(is_credential_key(key), "{key}");
        }
        for key in [
            "token_count",
            "secrets_count",
            "secrets",
            "has_password",
            "is_secret",
            "next_page_token",
            "page_token",
            "nextPageToken",
            "token_type",
            "secret_id",
            "key",
            "value",
            "keys",
            "author",
            "tokenizer",
            "mytoken",
        ] {
            assert!(!is_credential_key(key), "{key}");
        }
    }

    #[test]
    fn list_secrets_shape_is_redacted() {
        let mut body = json!({
            "secrets": [
                {"key": "DEPLOY_KEY", "value": "c2VjcmV0"},
                {"key": "EMPTY"},
                {"key": "NULL", "value": null}
            ],
            "next_page_token": "p2"
        });
        let redactions = redact_json(LIST, &mut body, false);
        assert_eq!(
            body,
            json!({
                "secrets": [
                    {"key": "DEPLOY_KEY", "value": REDACTED},
                    {"key": "EMPTY"},
                    {"key": "NULL", "value": null}
                ],
                "next_page_token": "p2"
            })
        );
        assert_eq!(redactions.secret_values, 1);
        assert!(redactions.credential_fields.is_empty());
        let note = redactions.note(false).unwrap();
        assert!(note.contains("1 value(s)"), "{note}");
        assert!(note.contains("--allow-secret-values"), "{note}");
        assert!(redactions.note(true).unwrap().contains("reveal_values"));
    }

    #[test]
    fn secret_values_are_redacted_at_any_depth() {
        let mut body = json!({"result": {"items": [[{"value": {"nested": 1}}]], "value": 5}});
        let redactions = redact_json("/operations/secrets/id:op", &mut body, false);
        assert_eq!(
            body,
            json!({"result": {"items": [[{"value": REDACTED}]], "value": REDACTED}})
        );
        assert_eq!(redactions.secret_values, 2);
    }

    #[test]
    fn value_fields_outside_secret_endpoints_are_kept() {
        let mut body = json!({"labels": [{"key": "env", "value": "prod"}]});
        let redactions = redact_json("/repos/acme/web/labels", &mut body, false);
        assert!(redactions.is_empty());
        assert_eq!(body["labels"][0]["value"], "prod");
        assert_eq!(redactions.note(false), None);
    }

    #[test]
    fn credentials_are_redacted_everywhere() {
        let mut body = json!({
            "hooks": [{"url": "https://x", "webhook_secret": "s1", "has_password": true}],
            "nested": {"auth": {"accessToken": "t1", "token_count": 3, "token": ""}},
            "next_page_token": "p2"
        });
        let redactions = redact_json("/repos/acme/web/hooks", &mut body, false);
        assert_eq!(body["hooks"][0]["webhook_secret"], REDACTED);
        assert_eq!(body["hooks"][0]["has_password"], true);
        assert_eq!(body["nested"]["auth"]["accessToken"], REDACTED);
        assert_eq!(body["nested"]["auth"]["token_count"], 3);
        assert_eq!(body["nested"]["auth"]["token"], "");
        assert_eq!(body["next_page_token"], "p2");
        assert_eq!(
            redactions.credential_fields.iter().collect::<Vec<_>>(),
            ["accessToken", "webhook_secret"]
        );
        let note = redactions.note(false).unwrap();
        assert!(note.contains("accessToken, webhook_secret"), "{note}");
        assert!(!note.contains("Secret values"), "{note}");
    }

    #[test]
    fn reveal_skips_secret_values_but_not_credentials() {
        let mut body = json!({
            "secrets": [
                {"key": "PLAIN", "value": "aGVsbG8g0LzQuNGA"},
                {"key": "BINARY", "value": "/w=="},
                {"key": "BROKEN", "value": "not base64!"}
            ],
            "password": "p"
        });
        let redactions = redact_json(LIST, &mut body, true);
        assert_eq!(redactions.secret_values, 0);
        assert_eq!(body["secrets"][0]["value"], "aGVsbG8g0LzQuNGA");
        assert_eq!(body["secrets"][0][DECODED_VALUE_FIELD], "hello мир");
        assert!(body["secrets"][1].get(DECODED_VALUE_FIELD).is_none());
        assert!(body["secrets"][2].get(DECODED_VALUE_FIELD).is_none());
        assert_eq!(body["password"], REDACTED);
    }

    #[test]
    fn text_bodies() {
        // Обрезанный ответ эндпоинта секретов скрывается целиком.
        let (text, redactions) =
            redact_text(LIST, r#"{"secrets":[{"key":"A","value":"c2Vj"#, false);
        assert!(text.starts_with("[hidden: "), "{text}");
        assert!(!text.contains("c2Vj"));
        assert!(redactions.body_hidden);
        assert!(redactions.note(false).unwrap().contains("not shown"));

        // С раскрытием — текст как есть, но учётные данные скрыты.
        let (text, _) = redact_text(LIST, r#"{"value":"c2Vj","token":"t"}"#, true);
        assert_eq!(text, r#"{"value":"c2Vj","token":"[redacted]"}"#);

        // Обычный эндпоинт: пары с ключами учётных данных, включая обрезанную строку.
        let raw = "{\"name\": \"bot\", \"password\" : \"p\\\"w\", \"next_page_token\":\"n\", \
                   \"items\":[\"token\"], \"мир\":\"ok\", \"api_key\":\"abc";
        let (text, redactions) = redact_text("/user", raw, false);
        assert_eq!(
            text,
            "{\"name\": \"bot\", \"password\" : \"[redacted]\", \"next_page_token\":\"n\", \
             \"items\":[\"token\"], \"мир\":\"ok\", \"api_key\":\"[redacted]\""
        );
        assert_eq!(
            redactions.credential_fields.iter().collect::<Vec<_>>(),
            ["api_key", "password"]
        );

        let (text, redactions) = redact_text("/user", "plain text", false);
        assert_eq!(text, "plain text");
        assert!(redactions.is_empty());
        let (text, redactions) = redact_text(LIST, "  ", false);
        assert_eq!(text, "  ");
        assert!(redactions.is_empty());
    }

    #[test]
    fn responses() {
        let mut response = ApiResponse {
            status: 200,
            body: ResponseBody::Json(json!({"secrets": [{"key": "A", "value": "c2Vj"}]})),
            truncated: false,
        };
        let redactions = redact_response("/repos/id:1/secrets", &mut response, false);
        assert_eq!(redactions.secret_values, 1);
        assert_eq!(
            response.body,
            ResponseBody::Json(json!({"secrets": [{"key": "A", "value": REDACTED}]}))
        );

        let mut response = ApiResponse {
            status: 204,
            body: ResponseBody::Empty,
            truncated: false,
        };
        assert!(redact_response(LIST, &mut response, false).is_empty());
    }

    #[test]
    fn error_bodies() {
        let body = r#"{"message":"bad","details":{"value":"c2Vj","token":"t"}}"#;
        let redacted = redact_error_body("/repos/acme/web/secrets/A", body);
        assert!(
            !redacted.contains("c2Vj") && !redacted.contains("\"t\""),
            "{redacted}"
        );
        assert!(redacted.contains("bad"), "{redacted}");

        // Без изменений тело возвращается как есть (без переформатирования).
        let body = r#"{ "message": "not found" }"#;
        assert_eq!(redact_error_body("/repos/acme/web/secrets/A", body), body);

        let hidden = redact_error_body("/repos/acme/web/secrets/A", "invalid value c2Vj");
        assert!(!hidden.contains("c2Vj"), "{hidden}");

        // Обычный эндпоинт: значения `value` остаются, учётные данные скрываются.
        let redacted = redact_error_body("/user", r#"{"value":"v","password":"p"}"#);
        assert_eq!(redacted, r#"{"password":"[redacted]","value":"v"}"#);
        assert_eq!(redact_error_body("/user", "oops"), "oops");

        let err = redact_api_error(
            "/repos/id:1/secrets",
            ApiError::Http {
                status: 400,
                body: r#"{"value":"c2Vj"}"#.into(),
            },
        );
        match err {
            ApiError::Http { status, body } => {
                assert_eq!(status, 400);
                assert_eq!(body, r#"{"value":"[redacted]"}"#);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            redact_api_error("/x", ApiError::Transport("t".into())),
            ApiError::Transport(_)
        ));
    }
}
