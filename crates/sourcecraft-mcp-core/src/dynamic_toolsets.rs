//! Динамические наборы инструментов (`serve --dynamic-toolsets`).
//!
//! Клиенты без отложенной загрузки инструментов (например, Codex) кладут описания всех
//! ~110 инструментов в контекст сразу. В динамическом режиме при запуске регистрируются только
//! `api_search`, `api_request` и инструменты этого модуля: `list_toolsets`, `enable_toolset`,
//! `disable_toolset`, а также пользовательские инструменты: их явно настроил оператор, и ни
//! в один набор они не входят. Агент включает нужный набор, сервер пересобирает реестр
//! ([`ToolRegistry::with_toolsets`]) и сообщает клиенту `notifications/tools/list_changed`.
//!
//! Уровень доступа действует как обычно: включение набора не регистрирует инструменты выше
//! `max_access`. Управление наборами меняет только список инструментов этого сервера и данных
//! SourceCraft не касается, поэтому эти инструменты — уровня чтения.
//!
//! Здесь только описание инструментов, разбор аргументов и тексты ответов; состояние
//! (текущий реестр сессии) и уведомления — в [`crate::server`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use sourcecraft_api::spec::{AccessLevel, find, operations};

use crate::error::ToolError;
use crate::registry::{EXCLUDED_OPERATIONS, ToolKind, ToolRegistry, access_flag};
use crate::secret_copy::SECRET_COPY_TOOL;
use crate::secrets::{SECRET_SET_TOOL, SET_SECRET_OPERATION};
use crate::toolsets::{available_toolsets, normalize_toolsets, toolset_slug};

pub const LIST_TOOLSETS_TOOL: &str = "list_toolsets";
pub const ENABLE_TOOLSET_TOOL: &str = "enable_toolset";
pub const DISABLE_TOOLSET_TOOL: &str = "disable_toolset";

/// Сколько имён инструментов показывать в `list_toolsets` как пример содержимого набора.
const EXAMPLES: usize = 4;

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

fn toolset_schema(verb: &str) -> JsonObject {
    object(json!({
        "type": "object",
        "properties": {
            "toolset": {
                "type": "string",
                "description": format!(
                    "Toolset to {verb} (slug from `{LIST_TOOLSETS_TOOL}`, e.g. `issues`). \
                     Several toolsets can be given separated by commas."
                )
            }
        },
        "required": ["toolset"],
        "additionalProperties": false
    }))
}

fn annotated(name: &'static str, title: &str, description: String, schema: JsonObject) -> Tool {
    let mut tool = Tool::new(name, description, Arc::new(schema));
    tool.title = Some(title.to_owned());
    tool.annotations = Some(ToolAnnotations::new().read_only(true));
    tool
}

/// Описание `list_toolsets`.
pub fn list_toolsets_tool() -> Tool {
    annotated(
        LIST_TOOLSETS_TOOL,
        "List toolsets",
        format!(
            "List the toolsets (groups of SourceCraft API tools) of this server: slug, API tag, \
             number of tools available at the server's access level, example tool names and \
             whether the toolset is enabled. Only the tools of enabled toolsets are loaded; \
             enable a toolset with `{ENABLE_TOOLSET_TOOL}` before using its tools. To find the \
             toolset of a specific operation, use `api_search`.\n\n\
             Access: read-only (does not call the API)."
        ),
        object(json!({"type": "object", "properties": {}, "additionalProperties": false})),
    )
}

/// Описание `enable_toolset`.
pub fn enable_toolset_tool() -> Tool {
    annotated(
        ENABLE_TOOLSET_TOOL,
        "Enable a toolset",
        format!(
            "Load the tools of a toolset (see `{LIST_TOOLSETS_TOOL}`). The server then notifies \
             the client that its tool list changed, and the new tools become available. Tools \
             above the server's access level stay unavailable. If the new tools do not appear \
             in your tool list (the client does not reload it), call the same operations with \
             `api_request`, using the method and path from `api_search`.\n\n\
             Access: read-only (changes only the tool list of this server)."
        ),
        toolset_schema("enable"),
    )
}

/// Описание `disable_toolset`.
pub fn disable_toolset_tool() -> Tool {
    annotated(
        DISABLE_TOOLSET_TOOL,
        "Disable a toolset",
        format!(
            "Unload the tools of a toolset enabled earlier with `{ENABLE_TOOLSET_TOOL}`, to keep \
             the tool list short. `api_search` and `api_request` stay available.\n\n\
             Access: read-only (changes only the tool list of this server)."
        ),
        toolset_schema("disable"),
    )
}

/// Имена инструментов каждого набора, доступные на уровне `max_access` (без учёта того,
/// включён ли набор). `secret_set` и `secret_copy` относятся к набору операции `set_secret`
/// (`secret_copy` — с уровня записи, как в [`ToolRegistry`]).
fn tools_by_toolset(max_access: AccessLevel) -> BTreeMap<String, Vec<&'static str>> {
    let mut map: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    for op in operations() {
        if !EXCLUDED_OPERATIONS.contains(&op.tool_name) && max_access.allows(op.access) {
            map.entry(toolset_slug(op.tag))
                .or_default()
                .push(op.tool_name);
        }
    }
    if let Some(op) = find(SET_SECRET_OPERATION) {
        if max_access.allows(op.access) {
            map.entry(toolset_slug(op.tag))
                .or_default()
                .push(SECRET_SET_TOOL);
        }
        if max_access.allows(AccessLevel::Write) {
            map.entry(toolset_slug(op.tag))
                .or_default()
                .push(SECRET_COPY_TOOL);
        }
    }
    map
}

/// Ответ `list_toolsets`: компактный JSON.
pub fn list(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    if let Some(key) = args.keys().next() {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; `{LIST_TOOLSETS_TOOL}` takes no arguments"
        )));
    }
    let available = tools_by_toolset(registry.max_access());
    let mut hidden_total = 0;
    let toolsets: Vec<Value> = available_toolsets()
        .into_iter()
        .map(|toolset| {
            let names = available.get(&toolset.slug).cloned().unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("toolset".into(), json!(toolset.slug));
            entry.insert("api_tag".into(), json!(toolset.tag));
            entry.insert("tools".into(), json!(names.len()));
            entry.insert(
                "examples".into(),
                json!(names.iter().take(EXAMPLES).collect::<Vec<_>>()),
            );
            // Операции выше уровня доступа: сколько их и каким флагом они включаются.
            let hidden = toolset.operations.saturating_sub(
                names
                    .iter()
                    .filter(|name| **name != SECRET_SET_TOOL && **name != SECRET_COPY_TOOL)
                    .count(),
            );
            if hidden > 0 {
                hidden_total += hidden;
                entry.insert("above_access_level".into(), json!(hidden));
            }
            entry.insert(
                "enabled".into(),
                json!(registry.toolset_enabled(toolset.tag)),
            );
            Value::Object(entry)
        })
        .collect();
    let mut out = Map::new();
    out.insert("toolsets".into(), Value::Array(toolsets));
    let mut note = format!(
        "Enable a toolset with `{ENABLE_TOOLSET_TOOL}` to load its tools. `api_search` and \
         `api_request` are always available."
    );
    if hidden_total > 0 {
        let flag = match registry.max_access() {
            AccessLevel::Read => access_flag(AccessLevel::Write),
            _ => access_flag(AccessLevel::Destructive),
        };
        note.push_str(&format!(
            " `above_access_level` counts operations this server does not allow at its access \
             level; only the user can allow them by restarting the server (e.g. with {flag})."
        ));
    }
    out.insert("note".into(), json!(note));
    Ok(Value::Object(out).to_string())
}

/// Наборы из аргумента `toolset` (через запятую), проверенные и приведённые к slug.
fn parse_toolsets(args: &JsonObject) -> Result<Vec<String>, ToolError> {
    if let Some(key) = args.keys().find(|key| key.as_str() != "toolset") {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; the only argument is `toolset`"
        )));
    }
    let raw = match args.get("toolset") {
        Some(Value::String(text)) => text,
        None | Some(Value::Null) => {
            return Err(ToolError::new(format!(
                "missing required argument `toolset`; see `{LIST_TOOLSETS_TOOL}` for the slugs"
            )));
        }
        Some(_) => return Err(ToolError::new("argument `toolset` must be a string")),
    };
    let names: Vec<&str> = raw.split(',').collect();
    let toolsets = normalize_toolsets(&names).map_err(ToolError::new)?;
    if toolsets.is_empty() {
        return Err(ToolError::new(format!(
            "argument `toolset` is empty; see `{LIST_TOOLSETS_TOOL}` for the slugs"
        )));
    }
    Ok(toolsets)
}

/// Имена зарегистрированных инструментов наборов: операций, `secret_set` и `secret_copy`.
/// Пользовательские инструменты ни в один набор не входят.
fn toolset_tools(registry: &ToolRegistry) -> BTreeSet<String> {
    registry
        .iter()
        .filter(|tool| {
            matches!(
                tool.kind,
                ToolKind::Operation(_) | ToolKind::SecretSet | ToolKind::SecretCopy
            )
        })
        .map(|tool| tool.tool.name.to_string())
        .collect()
}

/// Результат `enable_toolset`/`disable_toolset`: новый реестр (`None` — список инструментов
/// не изменился) и текст ответа.
#[derive(Debug)]
pub struct Toggle {
    pub registry: Option<ToolRegistry>,
    pub text: String,
}

/// Включает (`enable = true`) или выключает наборы из аргументов относительно реестра `current`.
pub fn toggle(
    current: &ToolRegistry,
    args: &JsonObject,
    enable: bool,
) -> Result<Toggle, ToolError> {
    let requested = parse_toolsets(args)?;
    let mut enabled: Vec<String> = current.enabled_toolsets().unwrap_or_default().to_vec();
    let mut changed_sets = Vec::new();
    let mut unchanged_sets = Vec::new();
    for slug in requested {
        let present = enabled.contains(&slug);
        if present == enable {
            unchanged_sets.push(slug);
        } else {
            if enable {
                enabled.push(slug.clone());
            } else {
                enabled.retain(|name| name != &slug);
            }
            changed_sets.push(slug);
        }
    }

    let next = if changed_sets.is_empty() {
        None
    } else {
        Some(current.with_toolsets(&enabled).map_err(ToolError::new)?)
    };
    let before = toolset_tools(current);
    let after = next
        .as_ref()
        .map(toolset_tools)
        .unwrap_or_else(|| before.clone());
    let (diff_key, diff): (&str, Vec<&String>) = if enable {
        ("added_tools", after.difference(&before).collect())
    } else {
        ("removed_tools", before.difference(&after).collect())
    };

    let mut out = Map::new();
    let (changed_key, unchanged_key) = if enable {
        ("enabled", "already_enabled")
    } else {
        ("disabled", "not_enabled")
    };
    out.insert(changed_key.into(), json!(changed_sets));
    if !unchanged_sets.is_empty() {
        out.insert(unchanged_key.into(), json!(unchanged_sets));
    }
    out.insert(diff_key.into(), json!(diff));
    out.insert("enabled_toolsets".into(), json!(enabled));
    let note = if enable && !changed_sets.is_empty() && diff.is_empty() {
        Some(
            "No tools were added: all operations of this toolset are above the server's access \
             level. Only the user can allow them by restarting the server."
                .to_owned(),
        )
    } else if enable && !diff.is_empty() {
        Some(
            "The client was notified that the tool list changed. If these tools do not appear \
             in your tool list, call the same operations with `api_request` (method and path \
             from `api_search`)."
                .to_owned(),
        )
    } else {
        None
    };
    if let Some(note) = note {
        out.insert("note".into(), json!(note));
    }
    Ok(Toggle {
        registry: next,
        text: Value::Object(out).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerOptions;

    fn registry(max_access: AccessLevel, toolsets: &[&str]) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access,
            toolsets: Some(toolsets.iter().map(|s| s.to_string()).collect()),
            dynamic_toolsets: true,
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn args(value: Value) -> JsonObject {
        object(value)
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn initial_registry_has_only_core_tools() {
        let reg = registry(AccessLevel::Read, &[]);
        let mut names: Vec<String> = reg.iter().map(|t| t.tool.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "api_request",
                "api_search",
                DISABLE_TOOLSET_TOOL,
                ENABLE_TOOLSET_TOOL,
                LIST_TOOLSETS_TOOL
            ]
        );
        assert!(reg.iter().all(|t| t.access == AccessLevel::Read));
    }

    #[test]
    fn list_shows_counts_and_state() {
        let reg = registry(AccessLevel::Read, &["issues"]);
        let out = parse(&list(&JsonObject::new(), &reg).unwrap());
        let toolsets = out["toolsets"].as_array().unwrap();
        assert_eq!(toolsets.len(), available_toolsets().len());
        let issues = toolsets.iter().find(|t| t["toolset"] == "issues").unwrap();
        assert_eq!(issues["enabled"], true);
        assert!(issues["tools"].as_u64().unwrap() > 0);
        let secrets = toolsets.iter().find(|t| t["toolset"] == "secrets").unwrap();
        assert_eq!(secrets["enabled"], false);
        // `delete_secret` и `set_secret` выше уровня чтения.
        assert!(secrets["above_access_level"].as_u64().unwrap() >= 1);
        assert!(out["note"].as_str().unwrap().contains("--allow-write"));

        let err = list(&args(json!({"x": 1})), &reg).unwrap_err();
        assert!(err.message().contains("no arguments"));
    }

    #[test]
    fn enable_and_disable() {
        let reg = registry(AccessLevel::Read, &[]);
        let toggled = toggle(&reg, &args(json!({"toolset": "issues"})), true).unwrap();
        let next = toggled.registry.unwrap();
        assert!(next.get("get_issue").is_some());
        assert!(next.get("create_issue").is_none(), "выше уровня доступа");
        let out = parse(&toggled.text);
        assert_eq!(out["enabled"], json!(["issues"]));
        assert!(
            out["added_tools"]
                .as_array()
                .unwrap()
                .contains(&json!("get_issue"))
        );

        // Повторное включение ничего не меняет.
        let again = toggle(&next, &args(json!({"toolset": "issues"})), true).unwrap();
        assert!(again.registry.is_none());
        assert_eq!(parse(&again.text)["already_enabled"], json!(["issues"]));

        let off = toggle(
            &next,
            &args(json!({"toolset": " issues ,issues-labels"})),
            false,
        )
        .unwrap();
        let after = off.registry.unwrap();
        assert!(after.get("get_issue").is_none());
        let out = parse(&off.text);
        assert_eq!(out["disabled"], json!(["issues"]));
        assert_eq!(out["not_enabled"], json!(["issues-labels"]));
        assert_eq!(out["enabled_toolsets"], json!([]));
    }

    #[test]
    fn secret_set_follows_secrets_toolset_and_access() {
        let reg = registry(AccessLevel::Destructive, &[]);
        let next = toggle(&reg, &args(json!({"toolset": "secrets"})), true)
            .unwrap()
            .registry
            .unwrap();
        assert!(next.get(SECRET_SET_TOOL).is_some());

        let read = registry(AccessLevel::Read, &[]);
        let next = toggle(&read, &args(json!({"toolset": "secrets"})), true)
            .unwrap()
            .registry
            .unwrap();
        assert!(next.get(SECRET_SET_TOOL).is_none());
        assert!(next.get("list_secrets").is_some());
    }

    #[test]
    fn secret_copy_follows_secrets_toolset_and_access() {
        let write = registry(AccessLevel::Write, &[]);
        assert!(write.get(SECRET_COPY_TOOL).is_none());
        let reason = write.unavailable_reason(SECRET_COPY_TOOL).unwrap();
        assert!(reason.contains("`enable_toolset`"), "{reason}");
        assert!(reason.contains("toolset: \"secrets\""), "{reason}");
        // У `secret_copy` нет пути API, поэтому `api_request` не предлагается.
        assert!(!reason.contains("api_request"), "{reason}");

        let listed = parse(&list(&JsonObject::new(), &write).unwrap());
        let secrets = listed["toolsets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["toolset"] == "secrets")
            .unwrap()
            .clone();
        // Число инструментов набора учитывает `secret_copy`, а операций выше уровня —
        // только операции API.
        let ops: Vec<_> = operations()
            .iter()
            .filter(|op| toolset_slug(op.tag) == "secrets")
            .collect();
        let allowed = ops
            .iter()
            .filter(|op| {
                AccessLevel::Write.allows(op.access) && !EXCLUDED_OPERATIONS.contains(&op.tool_name)
            })
            .count();
        assert_eq!(secrets["tools"], json!(allowed + 1));
        let above = ops
            .iter()
            .filter(|op| !AccessLevel::Write.allows(op.access))
            .count();
        assert_eq!(secrets["above_access_level"], json!(above));

        let toggled = toggle(&write, &args(json!({"toolset": "secrets"})), true).unwrap();
        let next = toggled.registry.unwrap();
        let tool = next.get(SECRET_COPY_TOOL).unwrap();
        assert!(matches!(tool.kind, ToolKind::SecretCopy));
        assert_eq!(tool.access, AccessLevel::Write);
        assert!(next.get(SECRET_SET_TOOL).is_none(), "нужен Destructive");
        let added = parse(&toggled.text)["added_tools"].clone();
        assert!(
            added.as_array().unwrap().contains(&json!(SECRET_COPY_TOOL)),
            "{added}"
        );
        let removed = toggle(&next, &args(json!({"toolset": "secrets"})), false).unwrap();
        assert!(removed.registry.unwrap().get(SECRET_COPY_TOOL).is_none());
        assert!(
            parse(&removed.text)["removed_tools"]
                .as_array()
                .unwrap()
                .contains(&json!(SECRET_COPY_TOOL))
        );

        // Уровень чтения: `secret_copy` не появляется и с включённым набором.
        let read = registry(AccessLevel::Read, &["secrets"]);
        assert!(read.get(SECRET_COPY_TOOL).is_none());
        assert!(
            read.unavailable_reason(SECRET_COPY_TOOL)
                .unwrap()
                .contains("--allow-write")
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        let reg = registry(AccessLevel::Read, &[]);
        for (input, expected) in [
            (json!({}), "missing required argument"),
            (json!({"toolset": "repos"}), "unknown toolset `repos`"),
            (json!({"toolset": " , "}), "is empty"),
            (json!({"toolset": 1}), "must be a string"),
            (json!({"toolset": "issues", "x": 1}), "unknown argument `x`"),
        ] {
            let err = toggle(&reg, &args(input.clone()), true).unwrap_err();
            assert!(err.message().contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn disabled_reason_points_to_enable_toolset() {
        let reg = registry(AccessLevel::Read, &[]);
        let reason = reg.unavailable_reason("get_issue").unwrap();
        assert!(reason.contains("`enable_toolset`"), "{reason}");
        assert!(reason.contains("toolset: \"issues\""), "{reason}");
        let reason = reg.unavailable_reason("create_issue").unwrap();
        assert!(reason.contains("--allow-write"), "{reason}");
    }
}
