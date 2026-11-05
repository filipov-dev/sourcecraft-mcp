//! Инструмент `api_search`: поиск по всем операциям SourceCraft API.
//!
//! Ищет и по незарегистрированным операциям (выше `max_access` или вне `--toolsets`), чтобы
//! агент мог узнать, что операция существует и какой флаг её включает, а не изобретал обходные
//! пути. Работает локально по таблице операций, в API не обращается.
//!
//! Оценка простая: запрос делится на слова (всё, что не буква и не цифра, — разделитель),
//! каждое слово ищется подстрокой без учёта регистра в полях операции с весами; слово
//! получает вес лучшего поля. Сначала идут операции, совпавшие по большему числу слов,
//! затем по сумме весов, затем с более коротким именем, затем по алфавиту.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use sourcecraft_api::spec::{OperationDef, ParamVariant, find, operations};

use crate::dynamic_toolsets::ENABLE_TOOLSET_TOOL;
use crate::error::ToolError;
use crate::registry::{
    EXCLUDED_OPERATIONS, ToolRegistry, access_flag, access_name, takes_local_file,
};
use crate::toolsets::{available_toolsets, toolset_slug};

/// Имя инструмента.
pub const API_SEARCH_TOOL: &str = "api_search";

/// Сколько результатов по умолчанию и максимум.
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

const ARGUMENTS: &[&str] = &["query", "tool_name", "toolset", "limit"];

/// Бонус за точное совпадение всего запроса с именем инструмента или `operationId`.
const EXACT_BONUS: u32 = 100;

/// Описание инструмента.
pub fn api_search_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to search for in operation names, summaries, descriptions, \
                    paths and tags, e.g. `pull request merge` or `release notes`."
            },
            "tool_name": {
                "type": "string",
                "description": "Exact tool name of one operation (from a previous search) to get \
                    its full input schema instead of searching. Mutually exclusive with `query`."
            },
            "toolset": {
                "type": "string",
                "description": "Limit the search to one toolset (tag slug, e.g. `issues`). \
                    Without `query`, lists all operations of the toolset."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": format!("Maximum number of results (default {DEFAULT_LIMIT}, max {MAX_LIMIT}).")
            }
        },
        "additionalProperties": false
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    let description = "Search all SourceCraft API operations, including those whose tools are \
        not enabled on this server. Each result shows the tool name, HTTP method and path (also \
        the `id:` path form when the operation supports addressing by ID), access level, toolset, \
        required arguments, whether the tool is registered here and, if not, which server flag \
        enables it. Pass `tool_name` to get the full input schema of one operation.\n\n\
        If the tool is registered, call it directly. If only its toolset is disabled, enable it \
        with `enable_toolset` when the server offers that tool, or call the same path with `api_request`. If it needs a higher access level, ask the user to \
        restart the server with the named flag.\n\nAccess: read-only (does not call the API).";
    let mut tool = Tool::new(API_SEARCH_TOOL, description, Arc::new(schema));
    tool.title = Some("Search SourceCraft API operations".to_owned());
    tool.annotations = Some(ToolAnnotations::new().read_only(true));
    tool
}

/// Выполняет поиск; результат — компактный JSON.
pub fn search(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    reject_unknown(args)?;
    let query = optional_string(args, "query")?.unwrap_or_default();
    let tool_name = optional_string(args, "tool_name")?;
    let toolset = optional_string(args, "toolset")?;
    let limit = parse_limit(args.get("limit"))?;

    if let Some(name) = tool_name.filter(|name| !name.is_empty()) {
        if !query.is_empty() {
            return Err(ToolError::new(
                "`query` and `tool_name` are mutually exclusive; pass `tool_name` to inspect one \
                 operation or `query` to search",
            ));
        }
        return describe(name, registry);
    }

    if let Some(slug) = toolset
        && !available_toolsets().iter().any(|t| t.slug == slug)
    {
        let list: Vec<String> = available_toolsets().into_iter().map(|t| t.slug).collect();
        return Err(ToolError::new(format!(
            "unknown toolset `{slug}`; available toolsets: {}",
            list.join(", ")
        )));
    }
    if query.is_empty() && toolset.is_none() {
        return Err(ToolError::new(
            "pass `query` (words to search for), `toolset`, or `tool_name`",
        ));
    }

    let hits = rank(query, toolset);
    let total = hits.len();
    let results: Vec<Value> = hits
        .into_iter()
        .take(limit)
        .map(|(op, _)| summary_entry(op, registry))
        .collect();
    let mut out = Map::new();
    out.insert("total_matches".into(), json!(total));
    out.insert("returned".into(), json!(results.len()));
    out.insert("results".into(), Value::Array(results));
    if total > limit {
        out.insert(
            "note".into(),
            json!("More operations match: refine the query, filter by `toolset` or raise `limit`."),
        );
    }
    Ok(Value::Object(out).to_string())
}

/// Операции, отсортированные по релевантности, с оценкой.
pub fn rank(query: &str, toolset: Option<&str>) -> Vec<(&'static OperationDef, u32)> {
    let tokens = tokenize(query);
    let whole = query.trim().to_ascii_lowercase();
    let mut hits: Vec<(&'static OperationDef, usize, u32)> = operations()
        .iter()
        .filter(|op| toolset.is_none_or(|slug| toolset_slug(op.tag) == slug))
        .filter_map(|op| {
            if tokens.is_empty() {
                return Some((op, 0, 0));
            }
            let (matched, mut score) = score(op, &tokens);
            if matched == 0 {
                return None;
            }
            if whole == op.tool_name || whole == op.operation_id.to_ascii_lowercase() {
                score += EXACT_BONUS;
            }
            Some((op, matched, score))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            // При равной оценке короче имя — меньше посторонних слов. Без запроса (только
            // набор) список идёт по алфавиту.
            .then_with(|| {
                if tokens.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    a.0.tool_name.len().cmp(&b.0.tool_name.len())
                }
            })
            .then_with(|| a.0.tool_name.cmp(b.0.tool_name))
    });
    hits.into_iter().map(|(op, _, score)| (op, score)).collect()
}

/// Слова запроса в нижнем регистре без повторов.
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for token in query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let token = token.to_lowercase();
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}

/// Сколько слов совпало и сумма весов лучших полей.
fn score(op: &OperationDef, tokens: &[String]) -> (usize, u32) {
    let fields: [(u32, String); 7] = [
        (6, op.tool_name.to_lowercase()),
        (
            4,
            format!(
                "{} {}",
                op.operation_id,
                op.id_operation_id.unwrap_or_default()
            )
            .to_lowercase(),
        ),
        (
            3,
            format!("{} {}", op.tag, toolset_slug(op.tag)).to_lowercase(),
        ),
        (
            3,
            format!("{} {}", op.path, op.id_path.unwrap_or_default()).to_lowercase(),
        ),
        (2, op.summary.to_lowercase()),
        (1, op.description.to_lowercase()),
        (1, op.method.as_str().to_lowercase()),
    ];
    let mut matched = 0;
    let mut total = 0;
    for token in tokens {
        let best = fields
            .iter()
            .filter(|(_, text)| text.contains(token.as_str()))
            .map(|(weight, _)| *weight)
            .max()
            .unwrap_or(0);
        if best > 0 {
            matched += 1;
            total += best;
        }
    }
    (matched, total)
}

/// Краткая запись операции для результатов поиска.
fn summary_entry(op: &'static OperationDef, registry: &ToolRegistry) -> Value {
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(op.tool_name));
    entry.insert("method".into(), json!(op.method.as_str()));
    entry.insert("path".into(), json!(op.path));
    if let Some(id_path) = op.id_path {
        entry.insert("id_path".into(), json!(id_path));
    }
    entry.insert("summary".into(), json!(op.summary.trim()));
    entry.insert("access".into(), json!(access_name(op.access)));
    entry.insert("toolset".into(), json!(toolset_slug(op.tag)));
    entry.insert("required_params".into(), json!(required_params(op, false)));
    if op.id_path.is_some() {
        entry.insert(
            "id_required_params".into(),
            json!(required_params(op, true)),
        );
    }
    insert_availability(&mut entry, op, registry);
    Value::Object(entry)
}

/// Полное описание одной операции со схемой аргументов.
fn describe(name: &str, registry: &ToolRegistry) -> Result<String, ToolError> {
    let Some(op) = find(name) else {
        let similar: Vec<&str> = rank(name, None)
            .into_iter()
            .take(5)
            .map(|(op, _)| op.tool_name)
            .collect();
        let hint = if similar.is_empty() {
            "use `query` to search".to_owned()
        } else {
            format!("similar: {}", similar.join(", "))
        };
        return Err(ToolError::new(format!(
            "unknown operation `{name}`; {hint}"
        )));
    };
    let schema: Value = serde_json::from_str(op.input_schema).unwrap_or(Value::Null);
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(op.tool_name));
    entry.insert("operation_id".into(), json!(op.operation_id));
    if let Some(id) = op.id_operation_id {
        entry.insert("id_operation_id".into(), json!(id));
    }
    entry.insert("method".into(), json!(op.method.as_str()));
    entry.insert("path".into(), json!(op.path));
    if let Some(id_path) = op.id_path {
        entry.insert("id_path".into(), json!(id_path));
    }
    entry.insert("summary".into(), json!(op.summary.trim()));
    entry.insert("description".into(), json!(op.description.trim()));
    entry.insert("access".into(), json!(access_name(op.access)));
    entry.insert("toolset".into(), json!(toolset_slug(op.tag)));
    insert_availability(&mut entry, op, registry);
    entry.insert("input_schema".into(), schema);
    Ok(Value::Object(entry).to_string())
}

/// `registered`, а для незарегистрированных — флаги и что делать.
fn insert_availability(entry: &mut Map<String, Value>, op: &OperationDef, registry: &ToolRegistry) {
    let registered = registry.get(op.tool_name).is_some();
    entry.insert("registered".into(), json!(registered));
    if registered {
        return;
    }
    if EXCLUDED_OPERATIONS.contains(&op.tool_name) {
        entry.insert(
            "note".into(),
            json!("Not exposed by this server: the raw API expects base64-encoded secret values. Use the `secret_set` tool."),
        );
        return;
    }
    if !registry.local_files() && takes_local_file(op) {
        entry.insert(
            "note".into(),
            json!("Not available on this server: it uploads a local file, and this server runs on another machine (HTTP mode). Ask the user to upload the file in SourceCraft or to use a local (stdio) server."),
        );
        return;
    }
    let mut flags = Vec::new();
    let access_ok = registry.max_access().allows(op.access);
    if !access_ok {
        flags.push(access_flag(op.access).to_owned());
    }
    if registry.is_dynamic() {
        // Набор агент включает сам; флаг нужен только для уровня доступа.
        if access_ok {
            entry.insert(
                "enable_with".into(),
                json!(format!(
                    "{ENABLE_TOOLSET_TOOL} {{\"toolset\": \"{}\"}}",
                    toolset_slug(op.tag)
                )),
            );
            entry.insert(
                "note".into(),
                json!(format!(
                    "Its toolset is not enabled: call `{ENABLE_TOOLSET_TOOL}` with this toolset, \
                     or call the same method and path with `api_request`."
                )),
            );
            return;
        }
    } else if !registry.toolset_enabled(op.tag) {
        flags.push(format!("--toolsets {}", toolset_slug(op.tag)));
    }
    entry.insert("enable_with".into(), json!(flags.join(" ")));
    let note = if access_ok {
        "Its toolset is not enabled: call the same method and path with `api_request`, or ask \
         the user to add the toolset."
    } else {
        "Needs a higher access level than this server allows: ask the user to restart the server \
         with the flag in `enable_with` if the change is intended."
    };
    entry.insert("note".into(), json!(note));
}

/// Имена обязательных параметров варианта slug (`id = false`) или id.
fn required_params(op: &OperationDef, id: bool) -> Vec<&'static str> {
    op.params
        .iter()
        .filter(|param| param.required)
        .filter(|param| match param.variant {
            ParamVariant::Common => true,
            ParamVariant::Slug => !id,
            ParamVariant::Id => id,
        })
        .map(|param| param.name)
        .collect()
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
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: `query`, `tool_name`, `toolset`, `limit`",
        unknown.join(", ")
    )))
}

fn optional_string<'a>(args: &'a JsonObject, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    }
}

fn parse_limit(value: Option<&Value>) -> Result<usize, ToolError> {
    let limit = match value {
        None | Some(Value::Null) => return Ok(DEFAULT_LIMIT),
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        Some(_) => None,
    };
    match limit {
        Some(0) | None => Err(ToolError::new(format!(
            "argument `limit` must be a positive integer (max {MAX_LIMIT})"
        ))),
        Some(n) => Ok(usize::try_from(n).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sourcecraft_api::AccessLevel;

    use super::*;
    use crate::ServerOptions;

    fn registry(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            allow_secret_values: false,
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn run(value: Value, registry: &ToolRegistry) -> Result<Value, ToolError> {
        let Value::Object(args) = value else {
            panic!("не объект")
        };
        search(&args, registry).map(|text| serde_json::from_str(&text).unwrap())
    }

    fn names(result: &Value) -> Vec<&str> {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tool_name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn tokenize_splits_and_dedupes() {
        assert_eq!(
            tokenize("Pull-Request  merge pull_request"),
            ["pull", "request", "merge"]
        );
        assert!(tokenize("  ").is_empty());
    }

    #[test]
    fn ranking_prefers_all_tokens_and_names() {
        let hits = rank("merge pull request", None);
        assert_eq!(
            hits.first().map(|(op, _)| op.tool_name),
            Some("merge_pull_request")
        );

        // Точное имя инструмента — первым.
        let hits = rank("get_repository", None);
        assert_eq!(
            hits.first().map(|(op, _)| op.tool_name),
            Some("get_repository")
        );
        assert!(hits[0].1 >= EXACT_BONUS);

        // Поиск по пути.
        let hits = rank("/releases/generate-notes", None);
        assert_eq!(
            hits.first().map(|(op, _)| op.tool_name),
            Some("generate_release_notes")
        );

        assert!(rank("zzzqqq", None).is_empty());
    }

    #[test]
    fn dynamic_mode_suggests_enable_toolset() {
        let dynamic = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Read,
            dynamic_toolsets: true,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(json!({"tool_name": "get_issue"}), &dynamic).unwrap();
        assert_eq!(result["registered"], false);
        assert_eq!(
            result["enable_with"],
            r#"enable_toolset {"toolset": "issues"}"#
        );
        assert!(
            result["note"]
                .as_str()
                .unwrap()
                .contains("`enable_toolset`")
        );

        // Выше уровня доступа — только флаг сервера, без `--toolsets`.
        let result = run(json!({"tool_name": "create_issue"}), &dynamic).unwrap();
        assert_eq!(result["enable_with"], "--allow-write");
    }

    #[test]
    fn results_show_registration_and_flags() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"query": "create pull request", "limit": 3}), &read).unwrap();
        let first = &result["results"][0];
        assert_eq!(first["tool_name"], "create_pull_request");
        assert_eq!(first["method"], "POST");
        assert_eq!(first["path"], "/repos/{org_slug}/{repo_slug}/pulls");
        assert_eq!(first["id_path"], "/repos/id:{repo_id}/pulls");
        assert_eq!(first["access"], "write");
        assert_eq!(first["toolset"], "repository-pullrequest");
        assert_eq!(first["registered"], false);
        assert_eq!(first["enable_with"], "--allow-write");
        assert_eq!(
            first["required_params"],
            json!(["org_slug", "repo_slug", "body"])
        );
        assert_eq!(first["id_required_params"], json!(["body", "repo_id"]));
        assert_eq!(result["returned"], 3);

        let issues = registry(AccessLevel::Write, Some(&["issues"]));
        let result = run(json!({"query": "create_pull_request"}), &issues).unwrap();
        let first = &result["results"][0];
        assert_eq!(first["registered"], false);
        assert_eq!(first["enable_with"], "--toolsets repository-pullrequest");
        assert!(first["note"].as_str().unwrap().contains("api_request"));

        let result = run(json!({"query": "create issue"}), &issues).unwrap();
        assert_eq!(result["results"][0]["tool_name"], "create_issue");
        assert_eq!(result["results"][0]["registered"], true);
        assert!(result["results"][0].get("enable_with").is_none());

        let result = run(
            json!({"query": "set secret"}),
            &registry(AccessLevel::Destructive, None),
        )
        .unwrap();
        let secret = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool_name"] == "set_secret")
            .unwrap();
        assert_eq!(secret["registered"], false);
        assert!(secret["note"].as_str().unwrap().contains("secret_set"));
    }

    #[test]
    fn limits_and_toolset_filter() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"query": "repos"}), &read).unwrap();
        assert_eq!(result["returned"], DEFAULT_LIMIT);
        assert!(result["total_matches"].as_u64().unwrap() > DEFAULT_LIMIT as u64);
        assert!(result["note"].is_string());

        let result = run(json!({"query": "repos", "limit": 1000}), &read).unwrap();
        assert_eq!(
            result["returned"].as_u64(),
            Some(
                result["total_matches"]
                    .as_u64()
                    .unwrap()
                    .min(MAX_LIMIT as u64)
            )
        );
        let result = run(json!({"query": "repos", "limit": "2"}), &read).unwrap();
        assert_eq!(result["returned"], 2);

        for bad in [json!(0), json!(-1), json!(1.5), json!("x"), json!(true)] {
            let err = run(json!({"query": "repos", "limit": bad}), &read).unwrap_err();
            assert!(err.message().contains("`limit`"), "{err}");
        }

        // Только набор: все его операции по имени.
        let result = run(json!({"toolset": "issues", "limit": 100}), &read).unwrap();
        let listed = names(&result);
        assert!(!listed.is_empty());
        assert!(listed.windows(2).all(|w| w[0] <= w[1]));
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["toolset"] == "issues")
        );

        let result = run(json!({"query": "labels", "toolset": "issues"}), &read).unwrap();
        assert!(names(&result).iter().all(|n| !n.contains("pull_request")));

        let err = run(json!({"toolset": "repos"}), &read).unwrap_err();
        assert!(err.message().contains("unknown toolset `repos`"), "{err}");
    }

    #[test]
    fn tool_name_returns_full_schema() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"tool_name": "update_repository"}), &read).unwrap();
        assert_eq!(result["tool_name"], "update_repository");
        assert_eq!(result["method"], "PATCH");
        assert_eq!(result["registered"], false);
        assert_eq!(result["enable_with"], "--allow-write");
        assert_eq!(result["input_schema"]["type"], "object");
        assert!(result["input_schema"]["properties"]["body"].is_object());

        let err = run(json!({"tool_name": "get_repo"}), &read).unwrap_err();
        assert!(
            err.message().contains("unknown operation `get_repo`"),
            "{err}"
        );
        assert!(err.message().contains("get_repository"), "{err}");
    }

    #[test]
    fn argument_errors() {
        let read = registry(AccessLevel::Read, None);
        assert!(
            run(json!({}), &read)
                .unwrap_err()
                .message()
                .contains("pass `query`")
        );
        assert!(
            run(json!({"query": "x", "tool_name": "get_repository"}), &read)
                .unwrap_err()
                .message()
                .contains("mutually exclusive")
        );
        assert!(
            run(json!({"query": 1}), &read)
                .unwrap_err()
                .message()
                .contains("must be a string")
        );
        assert!(
            run(json!({"q": "x"}), &read)
                .unwrap_err()
                .message()
                .contains("unknown argument(s) `q`")
        );
    }
}
