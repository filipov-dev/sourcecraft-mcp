//! Инварианты таблицы операций, сгенерированной `build.rs`, и снапшоты характерных операций.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use sourcecraft_api::spec::{
    AccessLevel, HttpMethod, OperationDef, ParamLocation, ParamType, ParamVariant, PathVariant,
    find, operations,
};

fn schema(op: &OperationDef) -> Value {
    serde_json::from_str(op.input_schema)
        .unwrap_or_else(|err| panic!("{}: input_schema не JSON: {err}", op.tool_name))
}

fn placeholders(path: &str) -> BTreeSet<&str> {
    path.split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect()
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

#[test]
fn operation_count_is_sane() {
    // 229 операций в спецификации: 10 Withdrawn пропущены, ~100 пар slug/id объединены.
    let count = operations().len();
    assert!(
        (100..=150).contains(&count),
        "неожиданное число операций: {count}"
    );
    let merged = operations()
        .iter()
        .filter(|op| op.id_path.is_some())
        .count();
    assert!(merged >= 80, "мало объединённых пар: {merged}");
}

#[test]
fn tool_names_are_unique_sorted_and_valid() {
    let names: Vec<&str> = operations().iter().map(|op| op.tool_name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "имена должны быть уникальны и отсортированы");
    for name in names {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "недопустимое имя инструмента {name}"
        );
        assert_eq!(find(name).map(|op| op.tool_name), Some(name));
    }
    assert!(find("no_such_tool").is_none());
}

#[test]
fn withdrawn_operations_are_skipped() {
    for op in operations() {
        assert_ne!(op.tag, "Withdrawn", "{}", op.tool_name);
        assert!(!op.operation_id.ends_with("Deprecated"), "{}", op.tool_name);
    }
    assert!(find("run_workflow").is_none());
}

#[test]
fn input_schemas_are_objects_with_all_params() {
    for op in operations() {
        let schema = schema(op);
        assert_eq!(schema["type"], "object", "{}", op.tool_name);
        assert_eq!(schema["additionalProperties"], false, "{}", op.tool_name);
        assert!(
            !op.input_schema.contains("$ref"),
            "{}: $ref не раскрыт",
            op.tool_name
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: нет properties", op.tool_name));
        let param_names: BTreeSet<&str> = op.params.iter().map(|p| p.name).collect();
        let property_names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(param_names, property_names, "{}", op.tool_name);
        assert_eq!(
            param_names.len(),
            op.params.len(),
            "{}: повтор параметра",
            op.tool_name
        );

        for param in op.params {
            assert!(
                !param.description.is_empty(),
                "{}: {}",
                op.tool_name,
                param.name
            );
            assert_eq!(
                properties[param.name]["description"], param.description,
                "{}: описание {} в схеме и ParamDef различается",
                op.tool_name, param.name
            );
        }

        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let expected: BTreeSet<&str> = op
            .params
            .iter()
            .filter(|p| p.required && p.variant == ParamVariant::Common)
            .map(|p| p.name)
            .collect();
        assert_eq!(required, expected, "{}", op.tool_name);
    }
}

#[test]
fn path_params_match_templates() {
    for op in operations() {
        let variants: &[PathVariant] = if op.id_path.is_some() {
            &[PathVariant::Slug, PathVariant::Id]
        } else {
            &[PathVariant::Slug]
        };
        for &variant in variants {
            let path = op.path_for(variant).expect("путь варианта");
            let declared: BTreeSet<&str> = op
                .params_for(variant)
                .filter(|p| p.location == ParamLocation::Path)
                .map(|p| p.name)
                .collect();
            assert_eq!(placeholders(path), declared, "{} {path}", op.tool_name);
            assert!(
                op.params_for(variant)
                    .filter(|p| p.location == ParamLocation::Path)
                    .all(|p| p.required),
                "{}: path-параметр необязателен",
                op.tool_name
            );
        }
    }
}

#[test]
fn slug_id_pairs_are_consistent() {
    for op in operations() {
        assert_eq!(
            op.id_path.is_some(),
            op.id_operation_id.is_some(),
            "{}",
            op.tool_name
        );
        if let Some(id_path) = op.id_path {
            assert!(id_path.contains("id:{"), "{}: {id_path}", op.tool_name);
            assert!(!op.path.contains("id:{"), "{}: {}", op.tool_name, op.path);
            assert!(
                op.params.iter().any(|p| p.variant == ParamVariant::Id),
                "{}",
                op.tool_name
            );
            let schema = schema(op);
            assert!(
                schema["description"]
                    .as_str()
                    .is_some_and(|d| d.contains("either by slug")),
                "{}: нет пояснения про варианты адресации",
                op.tool_name
            );
        } else {
            assert!(
                op.params.iter().all(|p| p.variant == ParamVariant::Common),
                "{}",
                op.tool_name
            );
        }
        let bodies = op
            .params
            .iter()
            .filter(|p| p.location == ParamLocation::Body)
            .count();
        assert!(bodies <= 1, "{}", op.tool_name);
        assert!(
            op.params
                .iter()
                .filter(|p| p.location == ParamLocation::Body)
                .all(|p| p.name == "body" && p.value_type == ParamType::Object),
            "{}",
            op.tool_name
        );
    }
}

#[test]
fn access_levels_follow_method_with_exceptions() {
    let exceptions = [
        ("set_secret", AccessLevel::Destructive),
        ("delete_secret", AccessLevel::Destructive),
        ("add_repo_roles", AccessLevel::Destructive),
        ("remove_repo_roles", AccessLevel::Destructive),
        ("add_organization_roles", AccessLevel::Destructive),
        ("remove_organization_roles", AccessLevel::Destructive),
        ("create_organization_invites", AccessLevel::Destructive),
        ("set_code_assist_workplaces", AccessLevel::Destructive),
        ("remove_issue_labels", AccessLevel::Write),
        ("remove_pull_request_labels", AccessLevel::Write),
        ("generate_release_notes", AccessLevel::Read),
    ];
    for (name, access) in exceptions {
        assert_eq!(op(name).access, access, "{name}");
    }
    for op in operations() {
        if op.method == HttpMethod::Get {
            assert_eq!(op.access, AccessLevel::Read, "{}", op.tool_name);
        }
        if op.access == AccessLevel::Read {
            assert!(
                op.method == HttpMethod::Get || op.tool_name == "generate_release_notes",
                "{}: чтение с методом {:?}",
                op.tool_name,
                op.method
            );
        }
    }
    assert_eq!(op("create_pull_request").access, AccessLevel::Write);
    assert_eq!(op("delete_repository").access, AccessLevel::Destructive);
}

#[test]
fn upload_params_are_files() {
    for name in [
        "upload_issue_attachment",
        "upload_issue_comment_attachment",
        "add_release_asset_attachment",
    ] {
        let file = op(name)
            .params
            .iter()
            .find(|p| p.name == "file")
            .unwrap_or_else(|| panic!("{name}: нет параметра file"));
        assert_eq!(file.location, ParamLocation::FormData, "{name}");
        assert_eq!(file.value_type, ParamType::File, "{name}");
        assert!(file.required, "{name}");
    }
}

/// Снапшот операции. Ключи сортируются, чтобы порядок не зависел от фичи
/// `preserve_order` у `serde_json`, которую может включить любая зависимость.
macro_rules! snapshot {
    ($name:expr) => {
        insta::with_settings!({ sort_maps => true }, {
            insta::assert_json_snapshot!(snapshot_view(op($name)));
        })
    };
}

/// Представление операции для снапшота: все поля плюс разобранная JSON Schema.
fn snapshot_view(op: &OperationDef) -> Value {
    let params: Vec<Value> = op
        .params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "location": format!("{:?}", p.location),
                "required": p.required,
                "variant": format!("{:?}", p.variant),
                "value_type": format!("{:?}", p.value_type),
                "description": p.description,
            })
        })
        .collect();
    json!({
        "tool_name": op.tool_name,
        "operation_id": op.operation_id,
        "id_operation_id": op.id_operation_id,
        "tag": op.tag,
        "summary": op.summary,
        "description": op.description,
        "method": op.method.as_str(),
        "path": op.path,
        "id_path": op.id_path,
        "access": format!("{:?}", op.access),
        "params": params,
        "input_schema": schema(op),
    })
}

#[test]
fn snapshot_create_pull_request() {
    snapshot!("create_pull_request");
}

#[test]
fn snapshot_get_pull_request() {
    // Пара, в которой grpc-gateway дублирует идентификаторы в query (`pull_request_id`,
    // `pull_request_slug`): дубли должны быть убраны.
    snapshot!("get_pull_request");
}

#[test]
fn snapshot_add_release_asset_attachment() {
    // multipart-загрузка, где поле `name` есть только в slug-варианте.
    snapshot!("add_release_asset_attachment");
}

#[test]
fn snapshot_upload_issue_comment_attachment() {
    // Операция только в форме id, без пары.
    snapshot!("upload_issue_comment_attachment");
}

#[test]
fn snapshot_set_secret() {
    snapshot!("set_secret");
}

#[test]
fn snapshot_list_secrets() {
    // Query-параметр-массив `keys`.
    snapshot!("list_secrets");
}
