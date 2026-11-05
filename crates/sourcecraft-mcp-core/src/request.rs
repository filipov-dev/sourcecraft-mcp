//! Сборка запроса к API из аргументов сгенерированного инструмента.
//!
//! Порядок описан в документации `sourcecraft_api::spec`:
//! неизвестные аргументы отклоняются, вариант адресации (slug/id) выбирается по переданным
//! path-параметрам, аргументы другого варианта и пропущенные обязательные — ошибка.
//! Значение `null` считается непереданным аргументом: некоторые клиенты так передают
//! необязательные параметры.

use std::collections::HashMap;

use rmcp::model::JsonObject;
use serde_json::Value;
use sourcecraft_api::executor::{ApiRequest, MultipartPart, RequestBody, render_path};
use sourcecraft_api::spec::{
    AccessLevel, OperationDef, ParamDef, ParamLocation, ParamType, ParamVariant, PathVariant,
};

use crate::error::ToolError;
use crate::files::FilePolicy;

/// Инструмент, для которого действует проверка видимости репозитория.
pub const UPDATE_REPOSITORY_TOOL: &str = "update_repository";

/// Собирает [`ApiRequest`] для операции `op` из аргументов инструмента.
pub fn build_request(
    op: &OperationDef,
    args: &JsonObject,
    files: &FilePolicy,
) -> Result<ApiRequest, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());

    reject_unknown(op, args)?;
    let variant = op.select_variant(present);
    reject_other_variant(op, variant, &present)?;

    let missing: Vec<String> = op
        .params_for(variant)
        .filter(|param| param.required && !present(param.name))
        .map(|param| format!("`{}`", param.name))
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::new(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        )));
    }

    let mut path_values: HashMap<&str, String> = HashMap::new();
    let mut query = Vec::new();
    let mut body = RequestBody::Empty;
    let mut parts = Vec::new();

    for param in op.params_for(variant) {
        let Some(value) = args.get(param.name).filter(|value| !value.is_null()) else {
            continue;
        };
        match param.location {
            ParamLocation::Path => {
                path_values.insert(param.name, scalar(param, value)?);
            }
            ParamLocation::Query => {
                for item in query_values(param, value)? {
                    query.push((param.name.to_owned(), item));
                }
            }
            ParamLocation::Body => body = RequestBody::Json(value.clone()),
            ParamLocation::FormData => parts.push(form_part(param, value, files)?),
        }
    }
    if !parts.is_empty() {
        body = RequestBody::Multipart(parts);
    }

    let template = op.path_for(variant).ok_or_else(|| {
        ToolError::new(format!(
            "`{}` has no ID addressing; use the slug arguments",
            op.tool_name
        ))
    })?;
    let path = render_path(template, |name| path_values.get(name).cloned())?;

    Ok(ApiRequest {
        method: op.method,
        path,
        query,
        body,
        save_to: None,
    })
}

/// Дополнительные проверки отдельных операций сверх уровня доступа инструмента.
///
/// `update_repository` с `visibility: public` открывает репозиторий всему интернету, поэтому
/// требует уровень `Destructive`, хотя сама операция — `Write`.
pub fn check_guards(
    op: &OperationDef,
    args: &JsonObject,
    max_access: AccessLevel,
) -> Result<(), ToolError> {
    if op.tool_name == UPDATE_REPOSITORY_TOOL
        && sets_public_visibility(args.get("body"))
        && !max_access.allows(AccessLevel::Destructive)
    {
        return Err(ToolError::new(
            "making a repository public is treated as a destructive change and requires the \
             server to run with --allow-destructive; ask the user to make the repository public \
             in SourceCraft or to restart the server with that flag",
        ));
    }
    Ok(())
}

/// Задаёт ли тело `visibility: public`. Тело может прийти строкой с JSON (см. `RequestBody::Json`).
fn sets_public_visibility(body: Option<&Value>) -> bool {
    let parsed;
    let body = match body {
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(value) => {
                parsed = value;
                &parsed
            }
            Err(_) => return false,
        },
        Some(value) => value,
        None => return false,
    };
    body.get("visibility")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("public"))
}

fn reject_unknown(op: &OperationDef, args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !op.params.iter().any(|param| param.name == key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let allowed: Vec<String> = op
        .params
        .iter()
        .map(|param| format!("`{}`", param.name))
        .collect();
    let allowed = if allowed.is_empty() {
        "none".to_owned()
    } else {
        allowed.join(", ")
    };
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {allowed}",
        unknown.join(", ")
    )))
}

fn reject_other_variant(
    op: &OperationDef,
    variant: PathVariant,
    present: &impl Fn(&str) -> bool,
) -> Result<(), ToolError> {
    let foreign: Vec<String> = op
        .params
        .iter()
        .filter(|param| !param.variant.applies_to(variant) && present(param.name))
        .map(|param| format!("`{}`", param.name))
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    let set = |v: ParamVariant| {
        op.params
            .iter()
            .filter(|param| param.variant == v && param.location == ParamLocation::Path)
            .map(|param| format!("`{}`", param.name))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let chosen = match variant {
        PathVariant::Slug => "slug",
        PathVariant::Id => "ID",
    };
    Err(ToolError::new(format!(
        "argument(s) {} cannot be used with {chosen} addressing; identify the target either by \
         slug ({}) or by ID ({}), not both",
        foreign.join(", "),
        set(ParamVariant::Slug),
        set(ParamVariant::Id),
    )))
}

/// Значение path- или query-параметра строкой.
fn scalar(param: &ParamDef, value: &Value) -> Result<String, ToolError> {
    let invalid = |expected: &str| {
        ToolError::new(format!(
            "argument `{}` must be {expected}, got {}",
            param.name,
            kind(value)
        ))
    };
    match param.value_type {
        ParamType::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
            Value::String(s) if is_integer(s) => Ok(s.clone()),
            _ => Err(invalid("an integer (a number or a string of digits)")),
        },
        ParamType::Number => match value {
            Value::Number(n) => Ok(n.to_string()),
            Value::String(s) if s.trim().parse::<f64>().is_ok() => Ok(s.trim().to_owned()),
            _ => Err(invalid("a number")),
        },
        ParamType::Boolean => match value {
            Value::Bool(b) => Ok(b.to_string()),
            Value::String(s) if s.eq_ignore_ascii_case("true") => Ok("true".to_owned()),
            Value::String(s) if s.eq_ignore_ascii_case("false") => Ok("false".to_owned()),
            _ => Err(invalid("a boolean")),
        },
        // Строки, а также массивы в позиции элемента: числа и логические значения
        // принимаем как их текстовую запись — ID в SourceCraft бывают и числовыми.
        ParamType::String | ParamType::Array => match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(invalid("a string")),
        },
        ParamType::Object | ParamType::File => Err(invalid("a string")),
    }
}

/// Значения query-параметра: массив передаётся повторением ключа.
fn query_values(param: &ParamDef, value: &Value) -> Result<Vec<String>, ToolError> {
    match (param.value_type, value) {
        (ParamType::Array, Value::Array(items)) => {
            items.iter().map(|item| scalar(param, item)).collect()
        }
        _ => scalar(param, value).map(|item| vec![item]),
    }
}

fn form_part(
    param: &ParamDef,
    value: &Value,
    files: &FilePolicy,
) -> Result<MultipartPart, ToolError> {
    if param.value_type == ParamType::File {
        let Value::String(path) = value else {
            return Err(ToolError::new(format!(
                "argument `{}` must be a path to a local file",
                param.name
            )));
        };
        let path = files
            .check_read(path)
            .map_err(|err| ToolError::new(format!("argument `{}`: {err}", param.name)))?;
        return Ok(MultipartPart::File {
            name: param.name.to_owned(),
            path,
            file_name: None,
            content_type: None,
        });
    }
    Ok(MultipartPart::Text {
        name: param.name.to_owned(),
        value: scalar(param, value)?,
    })
}

fn is_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sourcecraft_api::HttpMethod;
    use sourcecraft_api::spec::find;

    use super::*;

    fn op(name: &str) -> &'static OperationDef {
        find(name).unwrap_or_else(|| panic!("нет операции {name}"))
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("не объект: {other}"),
        }
    }

    fn policy() -> FilePolicy {
        FilePolicy::new(&[]).unwrap()
    }

    fn build(name: &str, value: Value) -> Result<ApiRequest, ToolError> {
        build_request(op(name), &args(value), &policy())
    }

    #[test]
    fn slug_variant_with_query() {
        let request = build(
            "list_repository_issues",
            json!({"org_slug": "acme", "repo_slug": "web app", "page_size": 20, "page_token": null}),
        )
        .unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/repos/acme/web%20app/issues");
        assert_eq!(request.query, vec![("page_size".into(), "20".into())]);
        assert!(matches!(request.body, RequestBody::Empty));
    }

    #[test]
    fn id_variant_is_selected_by_id_argument() {
        let request = build(
            "list_repository_issues",
            json!({"repo_id": "r-1", "page_size": "5"}),
        )
        .unwrap();
        assert_eq!(request.path, "/repos/id:r-1/issues");
        assert_eq!(request.query, vec![("page_size".into(), "5".into())]);
    }

    #[test]
    fn mixing_variants_is_rejected() {
        let err = build(
            "list_repository_issues",
            json!({"repo_id": "r-1", "org_slug": "acme"}),
        )
        .unwrap_err();
        assert!(err.message().contains("`org_slug`"), "{err}");
        assert!(err.message().contains("`repo_id`"), "{err}");

        // Не-path параметр варианта slug (`name` у загрузки в релиз) вместе с ID — тоже ошибка.
        let err = build(
            "add_release_asset_attachment",
            json!({"release_id": "1", "name": "a", "file": "/nonexistent"}),
        )
        .unwrap_err();
        assert!(
            err.message().contains("`name` cannot be used with ID"),
            "{err}"
        );
    }

    #[test]
    fn missing_and_unknown_arguments() {
        let err = build("list_repository_issues", json!({"org_slug": "acme"})).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `repo_slug`");

        let err = build(
            "list_repository_issues",
            json!({"org_slug": "acme", "repo_slug": "x", "pageSize": 1, "token": "t"}),
        )
        .unwrap_err();
        assert!(
            err.message()
                .starts_with("unknown argument(s) `pageSize`, `token`"),
            "{err}"
        );
        assert!(err.message().contains("`page_size`"), "{err}");

        let err = build("get_my_profile", json!({"x": 1}));
        assert!(
            err.unwrap_err()
                .message()
                .contains("allowed arguments: none")
        );
    }

    #[test]
    fn query_types() {
        let request = build(
            "list_secrets",
            json!({"org_slug": "acme", "repo_slug": "x", "keys": ["A", "B"]}),
        )
        .unwrap();
        assert_eq!(
            request.query,
            vec![("keys".into(), "A".into()), ("keys".into(), "B".into())]
        );

        let err = build(
            "list_repository_issues",
            json!({"org_slug": "a", "repo_slug": "b", "page_size": 1.5}),
        )
        .unwrap_err();
        assert!(err.message().contains("must be an integer"), "{err}");
        let err = build(
            "list_repository_issues",
            json!({"org_slug": "a", "repo_slug": "b", "page_size": "ten"}),
        )
        .unwrap_err();
        assert!(err.message().contains("must be an integer"), "{err}");

        let bool_op = operations_with(ParamLocation::Query, ParamType::Boolean);
        let bool_param = bool_op
            .params
            .iter()
            .find(|p| p.location == ParamLocation::Query && p.value_type == ParamType::Boolean)
            .unwrap();
        assert_eq!(scalar(bool_param, &json!(true)).unwrap(), "true");
        assert_eq!(scalar(bool_param, &json!("FALSE")).unwrap(), "false");
        assert!(scalar(bool_param, &json!(1)).is_err());
    }

    fn operations_with(location: ParamLocation, value_type: ParamType) -> &'static OperationDef {
        sourcecraft_api::spec::operations()
            .iter()
            .find(|op| {
                op.params
                    .iter()
                    .any(|p| p.location == location && p.value_type == value_type)
            })
            .unwrap()
    }

    #[test]
    fn path_values_are_single_segments() {
        let request = build(
            "list_repository_issues",
            json!({"org_slug": "a/b", "repo_slug": "c"}),
        )
        .unwrap();
        assert_eq!(request.path, "/repos/a%2Fb/c/issues");

        let err = build(
            "list_repository_issues",
            json!({"org_slug": "..", "repo_slug": "c"}),
        )
        .unwrap_err();
        assert!(err.message().contains("valid API path"), "{err}");
        let err = build(
            "list_repository_issues",
            json!({"org_slug": "", "repo_slug": "c"}),
        )
        .unwrap_err();
        assert!(err.message().contains("missing path argument"), "{err}");
        let err = build(
            "list_repository_issues",
            json!({"org_slug": {"a": 1}, "repo_slug": "c"}),
        )
        .unwrap_err();
        assert!(err.message().contains("must be a string"), "{err}");
    }

    #[test]
    fn json_body() {
        let request = build(
            "create_pull_request",
            json!({"org_slug": "a", "repo_slug": "b", "body": {"title": "T"}}),
        )
        .unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/repos/a/b/pulls");
        assert!(matches!(request.body, RequestBody::Json(ref v) if v == &json!({"title": "T"})));
    }

    #[test]
    fn multipart_with_file_policy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("asset.zip");
        std::fs::write(&file, b"zip").unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root)).unwrap();

        let request = build_request(
            op("add_release_asset_attachment"),
            &args(json!({
                "org_slug": "a", "repo_slug": "b", "release_tag": "v1",
                "name": "asset.zip", "file": file.display().to_string()
            })),
            &policy,
        )
        .unwrap();
        assert_eq!(request.path, "/repos/a/b/releases/tag/v1/attachments");
        let RequestBody::Multipart(parts) = request.body else {
            panic!("ожидался multipart");
        };
        assert!(
            matches!(&parts[..], [MultipartPart::File { name, path, .. }, MultipartPart::Text { name: text_name, value }]
                if name == "file" && path == &file && text_name == "name" && value == "asset.zip"),
            "{parts:?}"
        );

        // Существующий файл вне корня: `/etc/hosts` в Windows не абсолютный путь.
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("hosts");
        std::fs::write(&outside, b"x").unwrap();
        let outside = outside.display().to_string();
        let err = build_request(
            op("upload_issue_comment_attachment"),
            &args(json!({"issue_comment_id": "1", "file": outside})),
            &policy,
        )
        .unwrap_err();
        assert!(
            err.message().contains(&format!(
                "argument `file`: `{outside}` is outside the allowed directories"
            )),
            "{err}"
        );
    }

    #[test]
    fn visibility_guard() {
        let update = op(UPDATE_REPOSITORY_TOOL);
        let public = args(json!({"repo_id": "1", "body": {"visibility": "public"}}));
        let public_text = args(json!({"repo_id": "1", "body": "{\"visibility\": \" Public\"}"}));
        let private = args(json!({"repo_id": "1", "body": {"visibility": "private"}}));
        let other = args(json!({"repo_id": "1", "body": {"description": "d"}}));

        let err = check_guards(update, &public, AccessLevel::Write).unwrap_err();
        assert!(err.message().contains("--allow-destructive"), "{err}");
        assert!(check_guards(update, &public_text, AccessLevel::Write).is_err());
        assert!(check_guards(update, &public, AccessLevel::Destructive).is_ok());
        assert!(check_guards(update, &private, AccessLevel::Write).is_ok());
        assert!(check_guards(update, &other, AccessLevel::Write).is_ok());
        // Остальные операции проверка не затрагивает.
        assert!(check_guards(op("create_repository"), &public, AccessLevel::Write).is_ok());
    }
}
