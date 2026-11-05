//! Пользовательские инструменты: именованные шаблоны запросов из `tools.toml`.
//!
//! Файл читает бинарник (`serve --tools-file`, `tools check`) и передаёт разобранные
//! [`CustomToolDef`] сюда: [`CustomTool::compile`] проверяет определение, а вызов собирает
//! аргументы `api_request` и проходит через [`crate::api_request::prepare`]. Поэтому у
//! пользовательского инструмента те же границы, что у `api_request`:
//!
//! - только относительные пути API, заголовки задать нельзя;
//! - уровень доступа — по совпавшим операциям или по методу. При запуске он вычисляется
//!   по шаблону пути (инструменты выше `max_access` не регистрируются), а при вызове —
//!   по фактическому пути: запрос выше уровня сервера или выше объявленного уровня
//!   инструмента отклоняется;
//! - правила секретов и проверка видимости `update_repository` — те же.
//!
//! Подстановка аргументов:
//!
//! - в путь — значение кодируется как один сегмент ([`render_path`]): `/`, `?`, `#`, `%` и
//!   `..` не меняют структуру пути;
//! - в `query` и `body` — строка, целиком равная `{имя}`, заменяется JSON-значением
//!   аргумента (число остаётся числом); склейки строк нет, поэтому тело всегда валидный
//!   JSON. Необязательный аргумент без значения убирает ключ объекта или элемент массива.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sourcecraft_api::executor::render_path;
use sourcecraft_api::spec::{AccessLevel, HttpMethod, operations};

use crate::api_request::API_REQUEST_TOOL;
use crate::api_search::API_SEARCH_TOOL;
use crate::dynamic_toolsets::{DISABLE_TOOLSET_TOOL, ENABLE_TOOLSET_TOOL, LIST_TOOLSETS_TOOL};
use crate::error::ToolError;
use crate::jq::{JQ_ARG, JqFilter, add_jq_argument};
use crate::registry::access_name;
use crate::routes::required_access;
use crate::secret_copy::SECRET_COPY_TOOL;
use crate::secrets::SECRET_SET_TOOL;

/// Наибольшая длина имени инструмента (ограничение MCP-клиентов).
const MAX_NAME_LEN: usize = 64;

/// Содержимое файла `tools.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolsFile {
    #[serde(default)]
    pub tools: Vec<CustomToolDef>,
}

/// Определение инструмента в файле.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolDef {
    pub name: String,
    pub description: String,
    pub method: String,
    /// Шаблон пути относительно адреса API: `/repos/{org}/{repo}/pulls`.
    pub path: String,
    /// Параметры query; значения — литералы или `"{аргумент}"`.
    #[serde(default)]
    pub query: Option<Map<String, Value>>,
    /// Шаблон JSON-тела; строки `"{аргумент}"` заменяются значениями.
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub arguments: Vec<ArgumentDef>,
    /// Фильтр ответа (см. [`crate::jq`]).
    #[serde(default)]
    pub jq: Option<String>,
}

/// Аргумент инструмента.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgumentDef {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: ArgumentType,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgumentType {
    #[default]
    String,
    Integer,
    Number,
    Boolean,
}

impl ArgumentType {
    fn json_type(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

/// Проверенный пользовательский инструмент.
#[derive(Debug, Clone)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub method: HttpMethod,
    pub path: String,
    query: Option<Value>,
    body: Option<Value>,
    arguments: Vec<ArgumentDef>,
    jq: Option<JqFilter>,
    /// Уровень доступа по шаблону пути: с ним инструмент регистрируется.
    pub access: AccessLevel,
    /// Откуда определение (путь к файлу) — для сообщений об ошибках.
    pub source: String,
}

impl CustomTool {
    /// Проверяет определение. Ошибка называет файл и инструмент.
    pub fn compile(def: CustomToolDef, source: &str) -> Result<Self, String> {
        let name = def.name.trim().to_owned();
        let fail = |message: String| format!("{source}: tool `{name}`: {message}");
        check_identifier(&name, MAX_NAME_LEN).map_err(|err| format!("{source}: {err}"))?;
        if def.description.trim().is_empty() {
            return Err(fail("`description` is empty".into()));
        }
        let method = parse_method(&def.method).map_err(fail)?;

        // Аргументы: имена, дубликаты, зарезервированный `jq`.
        let mut names = HashSet::new();
        for arg in &def.arguments {
            check_identifier(&arg.name, MAX_NAME_LEN)
                .map_err(|err| fail(format!("argument: {err}")))?;
            if arg.name == JQ_ARG {
                return Err(fail(format!(
                    "argument name `{JQ_ARG}` is reserved for the response filter"
                )));
            }
            if !names.insert(arg.name.as_str()) {
                return Err(fail(format!("argument `{}` is declared twice", arg.name)));
            }
        }
        let find = |arg: &str| def.arguments.iter().find(|a| a.name == arg);

        // Путь: каждый параметр — обязательный аргумент; результат — допустимый путь API.
        let path = def.path.trim().to_owned();
        let mut used = BTreeSet::new();
        let sample = render_path(&path, |param| {
            used.insert(param.to_owned());
            Some("x".to_owned())
        })
        .map_err(|err| {
            fail(format!(
                "`path` must be a relative SourceCraft API path like `/repos/{{org}}/{{repo}}/pulls` \
                 without `?` or `#` ({err})"
            ))
        })?;
        for param in &used {
            match find(param) {
                Some(arg) if arg.required => {}
                Some(_) => {
                    return Err(fail(format!(
                        "argument `{param}` is used in `path`, so it must be `required = true`"
                    )));
                }
                None => {
                    return Err(fail(format!(
                        "`path` uses `{{{param}}}`, which is not a declared argument"
                    )));
                }
            }
        }

        let query = match def.query {
            Some(map) => {
                let value = Value::Object(map);
                collect_placeholders(&value, &mut used);
                check_query(&value).map_err(fail)?;
                Some(value)
            }
            None => None,
        };
        if let Some(body) = &def.body {
            if method == HttpMethod::Get {
                return Err(fail("GET requests cannot have a `body`".into()));
            }
            collect_placeholders(body, &mut used);
        }
        for param in &used {
            if find(param).is_none() {
                return Err(fail(format!(
                    "`{{{param}}}` is not a declared argument (a string that is exactly \
                     `{{name}}` is a placeholder)"
                )));
            }
        }
        for arg in &def.arguments {
            if !used.contains(&arg.name) {
                return Err(fail(format!(
                    "argument `{}` is not used in `path`, `query` or `body`",
                    arg.name
                )));
            }
        }

        let jq = match def.jq.as_deref() {
            Some(code) => Some(
                JqFilter::parse(code).map_err(|err| fail(format!("`jq`: {}", err.message())))?,
            ),
            None => None,
        };

        let access = required_access(method, &sample).required;
        Ok(Self {
            name,
            description: def.description.trim().to_owned(),
            method,
            path,
            query,
            body: def.body,
            arguments: def.arguments,
            jq,
            access,
            source: source.to_owned(),
        })
    }

    /// Описание MCP-инструмента.
    pub fn tool(&self) -> Tool {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for arg in &self.arguments {
            let mut schema = json!({"type": arg.kind.json_type()});
            if !arg.description.trim().is_empty() {
                schema["description"] = json!(arg.description.trim());
            }
            properties.insert(arg.name.clone(), schema);
            if arg.required {
                required.push(json!(arg.name));
            }
        }
        let mut schema = JsonObject::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("properties".into(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        schema.insert("additionalProperties".into(), json!(false));
        add_jq_argument(&mut schema);

        let mut description = format!(
            "{}\n\nCustom tool defined by the user: {} {}",
            self.description,
            self.method.as_str(),
            self.path
        );
        if let Some(jq) = &self.jq {
            description.push_str(&format!(", response filtered with `{}`", jq.as_str()));
        }
        description.push_str(".\n\nAccess: ");
        description.push_str(match self.access {
            AccessLevel::Read => "read-only.",
            AccessLevel::Write => "write (creates or changes data).",
            AccessLevel::Destructive => "destructive (deletes data or makes irreversible changes).",
        });
        let annotations = match self.access {
            AccessLevel::Read => ToolAnnotations::new().read_only(true),
            AccessLevel::Write => ToolAnnotations::new().read_only(false).destructive(false),
            AccessLevel::Destructive => ToolAnnotations::new().read_only(false).destructive(true),
        };
        let mut tool = Tool::new(self.name.clone(), description, Arc::new(schema));
        tool.annotations = Some(annotations);
        tool
    }

    /// Аргументы `api_request` для вызова и итоговый фильтр (фильтр инструмента, затем
    /// фильтр из вызова).
    pub fn build_call(
        &self,
        mut args: JsonObject,
    ) -> Result<(JsonObject, Option<JqFilter>), ToolError> {
        let call_jq = crate::jq::take_jq(&mut args)?;
        let jq = match (&self.jq, call_jq) {
            (Some(own), Some(extra)) => Some(JqFilter::parse(&format!(
                "({}) | ({})",
                own.as_str(),
                extra.as_str()
            ))?),
            (own, extra) => extra.or_else(|| own.clone()),
        };

        let unknown: Vec<String> = args
            .keys()
            .filter(|key| !self.arguments.iter().any(|a| &a.name == *key))
            .map(|key| format!("`{key}`"))
            .collect();
        if !unknown.is_empty() {
            return Err(ToolError::new(format!(
                "unknown argument(s) {}",
                unknown.join(", ")
            )));
        }
        let mut values = Map::new();
        let mut missing = Vec::new();
        for arg in &self.arguments {
            match args.remove(&arg.name).filter(|v| !v.is_null()) {
                Some(value) => {
                    values.insert(arg.name.clone(), coerce(arg, value)?);
                }
                None if arg.required => missing.push(format!("`{}`", arg.name)),
                None => {}
            }
        }
        if !missing.is_empty() {
            return Err(ToolError::new(format!(
                "missing required argument(s): {}",
                missing.join(", ")
            )));
        }

        let path =
            render_path(&self.path, |param| values.get(param).map(scalar_text)).map_err(|_| {
                ToolError::new(
                    "cannot build the request path from the arguments: path arguments must not \
                     be empty, `.` or `..`, and must not contain `/../`",
                )
            })?;
        let mut request = JsonObject::new();
        request.insert("method".into(), json!(self.method.as_str()));
        request.insert("path".into(), json!(path));
        if let Some(query) = &self.query
            && let Some(query) = substitute(query, &values)
        {
            request.insert("query".into(), query);
        }
        if let Some(body) = &self.body
            && let Some(body) = substitute(body, &values)
        {
            request.insert("body".into(), body);
        }
        Ok((request, jq))
    }
}

/// Проверяет набор пользовательских инструментов: имена уникальны и не совпадают со
/// встроенными. Встроенные имена берутся из всей таблицы операций, а не только из
/// зарегистрированных, чтобы смысл файла не зависел от флагов доступа и наборов.
pub fn check_names(tools: &[CustomTool]) -> Result<(), String> {
    let builtin: HashSet<&str> = operations()
        .iter()
        .map(|op| op.tool_name)
        .chain([
            API_REQUEST_TOOL,
            API_SEARCH_TOOL,
            SECRET_SET_TOOL,
            SECRET_COPY_TOOL,
            LIST_TOOLSETS_TOOL,
            ENABLE_TOOLSET_TOOL,
            DISABLE_TOOLSET_TOOL,
        ])
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    for tool in tools {
        if builtin.contains(tool.name.as_str()) {
            return Err(format!(
                "{}: tool `{}` has the same name as a built-in tool; rename it",
                tool.source, tool.name
            ));
        }
        if !seen.insert(tool.name.as_str()) {
            return Err(format!(
                "{}: tool `{}` is defined more than once",
                tool.source, tool.name
            ));
        }
    }
    Ok(())
}

fn check_identifier(name: &str, max: usize) -> Result<(), String> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= max;
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid name `{name}`: use lowercase letters, digits and `_`, starting with a \
             letter, at most {max} characters"
        ))
    }
}

fn parse_method(method: &str) -> Result<HttpMethod, String> {
    let method = method.trim();
    [
        HttpMethod::Get,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Patch,
        HttpMethod::Delete,
    ]
    .into_iter()
    .find(|m| m.as_str().eq_ignore_ascii_case(method))
    .ok_or_else(|| format!("`method` must be GET, POST, PUT, PATCH or DELETE, got `{method}`"))
}

/// Имя аргумента, если строка целиком — подстановка `{имя}`.
fn placeholder(text: &str) -> Option<&str> {
    let name = text.strip_prefix('{')?.strip_suffix('}')?;
    check_identifier(name, MAX_NAME_LEN).ok().map(|()| name)
}

fn collect_placeholders(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            if let Some(name) = placeholder(text) {
                out.insert(name.to_owned());
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_placeholders(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_placeholders(v, out)),
        _ => {}
    }
}

/// `query` — объект скаляров или массивов скаляров, как у `api_request`.
fn check_query(query: &Value) -> Result<(), String> {
    let scalar = |v: &Value| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_));
    let Value::Object(map) = query else {
        return Err("`query` must be a table".into());
    };
    for (key, value) in map {
        let ok = match value {
            Value::Array(items) => items.iter().all(scalar),
            other => scalar(other),
        };
        if !ok {
            return Err(format!(
                "query parameter `{key}` must be a string, number, boolean or an array of them"
            ));
        }
    }
    Ok(())
}

/// Подставляет значения аргументов; `None` — подстановка без значения (ключ убирается).
fn substitute(template: &Value, values: &Map<String, Value>) -> Option<Value> {
    match template {
        Value::String(text) => match placeholder(text) {
            Some(name) => values.get(name).cloned(),
            None => Some(template.clone()),
        },
        Value::Array(items) => Some(Value::Array(
            items.iter().filter_map(|v| substitute(v, values)).collect(),
        )),
        Value::Object(map) => Some(Value::Object(
            map.iter()
                .filter_map(|(k, v)| substitute(v, values).map(|v| (k.clone(), v)))
                .collect(),
        )),
        other => Some(other.clone()),
    }
}

/// Значение аргумента для пути: строка как есть, остальное — JSON-текст.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Проверяет тип аргумента; числа и логические значения можно передать строкой.
fn coerce(arg: &ArgumentDef, value: Value) -> Result<Value, ToolError> {
    let bad = || {
        ToolError::new(format!(
            "argument `{}` must be {}",
            arg.name,
            match arg.kind {
                ArgumentType::String => "a string",
                ArgumentType::Integer => "an integer",
                ArgumentType::Number => "a number",
                ArgumentType::Boolean => "a boolean",
            }
        ))
    };
    match (arg.kind, value) {
        (ArgumentType::String, Value::String(s)) => Ok(Value::String(s)),
        (ArgumentType::Integer, Value::Number(n)) if n.is_i64() || n.is_u64() => {
            Ok(Value::Number(n))
        }
        (ArgumentType::Integer, Value::String(s)) => {
            s.trim().parse::<i64>().map(Value::from).map_err(|_| bad())
        }
        (ArgumentType::Number, Value::Number(n)) => Ok(Value::Number(n)),
        (ArgumentType::Number, Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(bad),
        (ArgumentType::Boolean, Value::Bool(b)) => Ok(Value::Bool(b)),
        (ArgumentType::Boolean, Value::String(s)) => match s.trim() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(bad()),
        },
        _ => Err(bad()),
    }
}

/// Сообщение для вызова, которому нужен уровень выше объявленного у инструмента.
pub fn access_mismatch(tool: &CustomTool, needed: AccessLevel) -> ToolError {
    ToolError::new(format!(
        "custom tool `{}` is registered with {} access, but this call needs {} access \
         (the arguments select a different API operation); use `api_request` or fix the tool \
         definition",
        tool.name,
        access_name(tool.access),
        access_name(needed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(toml_like: Value) -> CustomToolDef {
        serde_json::from_value(toml_like).unwrap()
    }

    fn compile(value: Value) -> Result<CustomTool, String> {
        CustomTool::compile(def(value), "tools.toml")
    }

    fn open_prs() -> Value {
        json!({
            "name": "open_prs",
            "description": "Open pull requests",
            "method": "GET",
            "path": "/repos/{org}/{repo}/pulls",
            "query": {"state": "open", "page_size": "{limit}"},
            "jq": ".pull_requests[] | {id, title}",
            "arguments": [
                {"name": "org", "description": "Organization", "required": true},
                {"name": "repo", "required": true},
                {"name": "limit", "type": "integer"}
            ]
        })
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("не объект"),
        }
    }

    #[test]
    fn compiles_and_describes() {
        let tool = compile(open_prs()).unwrap();
        assert_eq!(tool.access, AccessLevel::Read);
        let mcp = tool.tool();
        assert_eq!(mcp.name, "open_prs");
        let schema = &mcp.input_schema;
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["required"], json!(["org", "repo"]));
        assert!(schema["properties"].get("jq").is_some());
        let description = mcp.description.unwrap();
        assert!(description.contains("GET /repos/{org}/{repo}/pulls"));
        assert!(description.contains("Access: read-only."));
        assert_eq!(mcp.annotations.unwrap().read_only_hint, Some(true));
    }

    #[test]
    fn builds_requests_with_encoded_path_and_typed_values() {
        let tool = compile(open_prs()).unwrap();
        let (request, jq) = tool
            .build_call(args(
                json!({"org": "a/b?x#y%", "repo": "web", "limit": "5"}),
            ))
            .unwrap();
        assert_eq!(request["path"], "/repos/a%2Fb%3Fx%23y%25/web/pulls");
        assert_eq!(request["query"], json!({"state": "open", "page_size": 5}));
        assert_eq!(jq.unwrap().as_str(), ".pull_requests[] | {id, title}");

        // Необязательный аргумент без значения убирает параметр.
        let (request, _) = tool
            .build_call(args(json!({"org": "a", "repo": "web", "jq": "length"})))
            .unwrap();
        assert_eq!(request["query"], json!({"state": "open"}));

        // `..` — целиком или внутри значения — отклоняется.
        for org in ["..", "a/../b", "", "%2e%2e"] {
            let err = tool
                .build_call(args(json!({"org": org, "repo": "web"})))
                .unwrap_err();
            assert!(
                err.message().contains("path arguments"),
                "{}",
                err.message()
            );
        }

        for (bad, needle) in [
            (json!({"org": "a"}), "missing required argument(s): `repo`"),
            (
                json!({"org": "a", "repo": "b", "extra": 1}),
                "unknown argument(s) `extra`",
            ),
            (
                json!({"org": "a", "repo": "b", "limit": "x"}),
                "must be an integer",
            ),
            (json!({"org": 1, "repo": "b"}), "must be a string"),
        ] {
            let err = tool.build_call(args(bad)).unwrap_err();
            assert!(err.message().contains(needle), "{}", err.message());
        }
    }

    #[test]
    fn combines_filters() {
        let tool = compile(open_prs()).unwrap();
        let (_, jq) = tool
            .build_call(args(json!({"org": "a", "repo": "b", "jq": ".id"})))
            .unwrap();
        assert_eq!(
            jq.unwrap().as_str(),
            "(.pull_requests[] | {id, title}) | (.id)"
        );
    }

    #[test]
    fn body_is_substituted_as_json() {
        let tool = compile(json!({
            "name": "comment",
            "description": "Comment on an issue",
            "method": "POST",
            "path": "/repos/{org}/{repo}/issues/{issue}/comments",
            "body": {"text": "{text}", "meta": {"n": "{n}", "flag": true}, "tags": ["{tag}", "x"]},
            "arguments": [
                {"name": "org", "required": true},
                {"name": "repo", "required": true},
                {"name": "issue", "required": true},
                {"name": "text", "required": true},
                {"name": "n", "type": "number"},
                {"name": "tag"}
            ]
        }))
        .unwrap();
        assert_eq!(tool.access, AccessLevel::Write);
        let (request, _) = tool
            .build_call(args(json!({
                "org": "a", "repo": "b", "issue": "1",
                "text": "\"}, {\"evil\": true", "n": 2.5
            })))
            .unwrap();
        assert_eq!(
            request["body"],
            json!({"text": "\"}, {\"evil\": true", "meta": {"n": 2.5, "flag": true}, "tags": ["x"]})
        );
    }

    #[test]
    fn rejects_invalid_definitions() {
        let cases = [
            (json!({"name": "Bad-Name"}), "invalid name"),
            (json!({"description": " "}), "`description` is empty"),
            (json!({"method": "HEAD"}), "`method` must be"),
            (
                json!({"path": "https://evil.example/x"}),
                "relative SourceCraft API path",
            ),
            (
                json!({"path": "/repos/{org}/{repo}/pulls?x=1"}),
                "relative SourceCraft API path",
            ),
            (
                json!({"path": "/repos/../{org}/{repo}"}),
                "relative SourceCraft API path",
            ),
            (
                json!({"path": "/repos/{org}/{nope}/pulls"}),
                "not a declared argument",
            ),
            (json!({"query": {"x": "{nope}"}}), "not a declared argument"),
            (json!({"query": {"x": {"a": 1}}}), "query parameter `x`"),
            (
                json!({"body": {"a": 1}}),
                "GET requests cannot have a `body`",
            ),
            (json!({"jq": ".a |"}), "`jq`"),
        ];
        for (patch, needle) in cases {
            let mut value = open_prs();
            for (k, v) in patch.as_object().unwrap() {
                value[k] = v.clone();
            }
            let err = compile(value).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
            assert!(err.starts_with("tools.toml"), "{err}");
        }

        let mut unused = open_prs();
        unused["arguments"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "spare"}));
        assert!(compile(unused).unwrap_err().contains("`spare` is not used"));

        let mut optional_in_path = open_prs();
        optional_in_path["arguments"][0]["required"] = json!(false);
        assert!(
            compile(optional_in_path)
                .unwrap_err()
                .contains("must be `required = true`")
        );

        let mut reserved = open_prs();
        reserved["arguments"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "jq"}));
        assert!(compile(reserved).unwrap_err().contains("reserved"));

        let mut duplicate = open_prs();
        duplicate["arguments"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "org"}));
        assert!(compile(duplicate).unwrap_err().contains("declared twice"));

        assert!(
            serde_json::from_value::<CustomToolDef>(json!({"name": "x", "headers": {}})).is_err()
        );
    }

    #[test]
    fn access_follows_the_path() {
        let delete = compile(json!({
            "name": "drop_repo",
            "description": "Delete a repository",
            "method": "DELETE",
            "path": "/repos/{org}/{repo}",
            "arguments": [{"name": "org", "required": true}, {"name": "repo", "required": true}]
        }))
        .unwrap();
        assert_eq!(delete.access, AccessLevel::Destructive);
    }

    #[test]
    fn names_must_not_clash() {
        let mut builtin = open_prs();
        builtin["name"] = json!("get_repository");
        let tools = [compile(builtin).unwrap()];
        assert!(check_names(&tools).unwrap_err().contains("built-in"));
        let mut builtin = open_prs();
        builtin["name"] = json!("api_request");
        assert!(check_names(&[compile(builtin).unwrap()]).is_err());

        let twice = [compile(open_prs()).unwrap(), compile(open_prs()).unwrap()];
        assert!(check_names(&twice).unwrap_err().contains("more than once"));
        assert!(check_names(&[compile(open_prs()).unwrap()]).is_ok());
    }
}
