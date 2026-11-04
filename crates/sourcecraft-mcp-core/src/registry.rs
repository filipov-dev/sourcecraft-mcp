//! Реестр MCP-инструментов сервера.
//!
//! Строится один раз при запуске из `sourcecraft_api::spec::operations()` с учётом
//! [`ServerOptions`]: инструменты выше `max_access` и вне выбранных наборов не регистрируются,
//! поэтому не видны в `tools/list` и не вызываются. Для незарегистрированных, но известных
//! инструментов [`ToolRegistry::unavailable_reason`] объясняет, какой флаг их включает.
//!
//! Инструменты, не связанные с одной операцией (`api_request`, `api_search`, `secret_set`),
//! добавляются вариантами [`ToolKind`] и регистрируются здесь же.
//! `api_request` и `api_search` регистрируются всегда: наборы инструментов их не ограничивают.
//! `secret_set` — только при уровне и наборе заменяемой им операции `set_secret`.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use sourcecraft_api::spec::{AccessLevel, OperationDef, ParamType, find, operations};

use crate::ServerOptions;
use crate::api_request::api_request_tool;
use crate::api_search::api_search_tool;
use crate::secrets::{
    SECRET_SET_TOOL, SET_SECRET_OPERATION, annotate_operation_tool, secret_set_tool,
};
use crate::toolsets::{normalize_toolsets, toolset_slug};

/// Сгенерированные операции, которые не регистрируются как инструменты.
///
/// `set_secret` принимает значение секрета уже в base64 — агенту легко ошибиться и записать
/// неверное значение. Вместо него регистрируется `secret_set` (см. [`crate::secrets`]), который
/// принимает открытый текст или файл и кодирует значение сам.
pub const EXCLUDED_OPERATIONS: &[&str] = &["set_secret"];

/// Что выполняет инструмент.
#[derive(Debug, Clone, Copy)]
pub enum ToolKind {
    /// Сгенерированная операция SourceCraft API.
    Operation(&'static OperationDef),
    /// Произвольный запрос к API (`api_request`).
    ApiRequest,
    /// Поиск по операциям API (`api_search`).
    ApiSearch,
    /// `secret_set`: запись секрета из открытого текста или файла.
    SecretSet,
}

/// Зарегистрированный инструмент: описание для `tools/list` и обработчик.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub tool: Tool,
    pub kind: ToolKind,
    pub access: AccessLevel,
}

/// Набор инструментов, доступных при заданных [`ServerOptions`].
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    index: HashMap<String, usize>,
    /// Выбранные наборы (slug); `None` — все.
    toolsets: Option<Vec<String>>,
    max_access: AccessLevel,
}

impl ToolRegistry {
    /// Строит реестр. Ошибка — неизвестный набор инструментов или некорректная схема
    /// в сгенерированной таблице (последнее — ошибка генератора).
    pub fn new(options: &ServerOptions) -> Result<Self, String> {
        let toolsets = match &options.toolsets {
            Some(names) => {
                let names = normalize_toolsets(names)?;
                (!names.is_empty()).then_some(names)
            }
            None => None,
        };

        let mut registry = Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets,
            max_access: options.max_access,
        };
        for op in operations() {
            if registry.operation_enabled(op) {
                let mut tool = operation_tool(op)?;
                annotate_operation_tool(op, &mut tool, options.allow_secret_values);
                registry.push(RegisteredTool {
                    tool,
                    kind: ToolKind::Operation(op),
                    access: op.access,
                });
            }
        }
        // `secret_set` требует того же уровня и набора, что и заменяемый им `set_secret`.
        if let Some(op) = find(SET_SECRET_OPERATION)
            && registry.max_access.allows(op.access)
            && registry.toolset_enabled(op.tag)
        {
            registry.push(RegisteredTool {
                tool: secret_set_tool(op)?,
                kind: ToolKind::SecretSet,
                access: op.access,
            });
        }
        registry.push(RegisteredTool {
            tool: api_search_tool(),
            kind: ToolKind::ApiSearch,
            access: AccessLevel::Read,
        });
        registry.push(RegisteredTool {
            tool: api_request_tool(options.max_access),
            kind: ToolKind::ApiRequest,
            access: options.max_access,
        });
        Ok(registry)
    }

    fn operation_enabled(&self, op: &OperationDef) -> bool {
        !EXCLUDED_OPERATIONS.contains(&op.tool_name)
            && self.max_access.allows(op.access)
            && self.toolset_enabled(op.tag)
    }

    /// Включён ли набор инструментов с тегом `tag`.
    pub fn toolset_enabled(&self, tag: &str) -> bool {
        match &self.toolsets {
            Some(names) => {
                let slug = toolset_slug(tag);
                names.contains(&slug)
            }
            None => true,
        }
    }

    /// Максимальный уровень доступа сервера.
    pub fn max_access(&self) -> AccessLevel {
        self.max_access
    }

    fn push(&mut self, tool: RegisteredTool) {
        self.index
            .insert(tool.tool.name.to_string(), self.tools.len());
        self.tools.push(tool);
    }

    /// Зарегистрированные инструменты в порядке регистрации.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.iter()
    }

    /// Описания инструментов для `tools/list`.
    pub fn tools(&self) -> Vec<Tool> {
        self.tools.iter().map(|t| t.tool.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.index.get(name).and_then(|&i| self.tools.get(i))
    }

    /// Почему известный инструмент не зарегистрирован; `None` — инструмент неизвестен.
    pub fn unavailable_reason(&self, name: &str) -> Option<String> {
        if name == SECRET_SET_TOOL {
            return self.disabled_reason(name, find(SET_SECRET_OPERATION)?);
        }
        let op = find(name)?;
        if EXCLUDED_OPERATIONS.contains(&op.tool_name) {
            return Some(format!(
                "tool `{name}` is not exposed by this server: it expects a base64-encoded value \
                 and is easy to misuse; use `{SECRET_SET_TOOL}` instead, which takes plain text \
                 or a local file and encodes the value itself"
            ));
        }
        self.disabled_reason(name, op)
    }

    /// Почему инструмент с уровнем и набором операции `op` не зарегистрирован.
    fn disabled_reason(&self, name: &str, op: &OperationDef) -> Option<String> {
        if !self.max_access.allows(op.access) {
            let flag = match op.access {
                AccessLevel::Destructive => "--allow-destructive",
                _ => "--allow-write",
            };
            return Some(format!(
                "tool `{name}` is disabled: it {} data in SourceCraft and the server runs with {} \
                 access. Ask the user to restart the server with {flag} if this change is intended",
                access_verb(op.access),
                access_name(self.max_access),
            ));
        }
        if !self.toolset_enabled(op.tag) {
            return Some(format!(
                "tool `{name}` belongs to toolset `{}`, which is not enabled; ask the user to add \
                 it to --toolsets",
                toolset_slug(op.tag)
            ));
        }
        None
    }
}

/// Описание MCP-инструмента для операции.
pub fn operation_tool(op: &'static OperationDef) -> Result<Tool, String> {
    let schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;

    let mut description = op.summary.trim().to_owned();
    let details = op.description.trim();
    if !details.is_empty() && details != description {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(details);
    }
    if let Some(addressing) = schema.get("description").and_then(|v| v.as_str()) {
        description.push_str("\n\n");
        description.push_str(addressing);
    }
    if op
        .params
        .iter()
        .any(|param| param.value_type == ParamType::File)
    {
        description.push_str(
            "\n\nFile arguments take a path to a local file inside the directories allowed by the \
             server (see the server instructions).",
        );
    }
    description.push_str("\n\nAccess: ");
    description.push_str(match op.access {
        AccessLevel::Read => "read-only.",
        AccessLevel::Write => "write (creates or changes data).",
        AccessLevel::Destructive => "destructive (deletes data or makes irreversible changes).",
    });

    let annotations = match op.access {
        AccessLevel::Read => ToolAnnotations::new().read_only(true),
        AccessLevel::Write => ToolAnnotations::new().read_only(false).destructive(false),
        AccessLevel::Destructive => ToolAnnotations::new().read_only(false).destructive(true),
    };

    let mut tool = Tool::new(op.tool_name, description, Arc::new(schema));
    let title = op.summary.trim();
    if !title.is_empty() {
        tool.title = Some(title.to_owned());
    }
    tool.annotations = Some(annotations);
    Ok(tool)
}

fn access_verb(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "reads",
        AccessLevel::Write => "creates or changes",
        AccessLevel::Destructive => "deletes or irreversibly changes",
    }
}

/// Название уровня доступа для сообщений и инструкций.
pub fn access_name(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only",
        AccessLevel::Write => "write",
        AccessLevel::Destructive => "destructive",
    }
}

/// Флаг сервера, который включает уровень доступа `access`.
pub fn access_flag(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "no extra flag",
        AccessLevel::Write => "--allow-write",
        AccessLevel::Destructive => "--allow-destructive",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
        ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            allow_secret_values: false,
            ..ServerOptions::default()
        }
    }

    /// Инструменты, не связанные с одной операцией: регистрируются всегда.
    /// Инструменты, которые есть всегда: `api_search` и `api_request` (`secret_set` — нет).
    const EXTRA_TOOLS: usize = 2;

    fn operation_tools(registry: &ToolRegistry) -> usize {
        registry
            .iter()
            .filter(|t| matches!(t.kind, ToolKind::Operation(_)))
            .count()
    }

    fn count(access: AccessLevel) -> usize {
        operations()
            .iter()
            .filter(|op| access.allows(op.access) && !EXCLUDED_OPERATIONS.contains(&op.tool_name))
            .count()
    }

    #[test]
    fn all_schemas_parse() {
        assert!(!operations().is_empty());
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            assert_eq!(tool.name, op.tool_name);
            assert_eq!(
                tool.input_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{}",
                op.tool_name
            );
            assert!(tool.description.is_some_and(|d| d.contains("Access: ")));
        }
    }

    #[test]
    fn access_filtering() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        assert_eq!(read.len(), count(AccessLevel::Read) + EXTRA_TOOLS);
        assert_eq!(operation_tools(&read), count(AccessLevel::Read));
        assert!(read.iter().all(|t| t.access == AccessLevel::Read));
        assert!(read.iter().all(|t| {
            let a = t.tool.annotations.as_ref().unwrap();
            a.read_only_hint == Some(true)
        }));
        assert!(read.get("get_repository").is_some());
        assert!(read.get("create_pull_request").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, None)).unwrap();
        assert_eq!(operation_tools(&write), count(AccessLevel::Write));
        assert!(write.get("create_pull_request").is_some());
        assert!(write.get("delete_repository").is_none());

        let all = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        // Плюс `secret_set`, который регистрируется только при `Destructive`.
        assert_eq!(
            all.len(),
            operations().len() - EXCLUDED_OPERATIONS.len() + EXTRA_TOOLS + 1
        );
        let delete = all.get("delete_repository").unwrap();
        let annotations = delete.tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(true));
        assert!(read.len() < write.len() && write.len() < all.len());
    }

    #[test]
    fn set_secret_is_never_registered() {
        let all = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        assert!(find("set_secret").is_some());
        assert!(all.get("set_secret").is_none());
        let reason = all.unavailable_reason("set_secret").unwrap();
        assert!(reason.contains("base64"), "{reason}");
        assert!(reason.contains("secret_set"), "{reason}");
    }

    #[test]
    fn secret_set_requires_destructive_access_and_secrets_toolset() {
        let all = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        let tool = all.get("secret_set").unwrap();
        assert!(matches!(tool.kind, ToolKind::SecretSet));
        assert_eq!(tool.access, AccessLevel::Destructive);
        assert!(all.unavailable_reason("secret_set").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, None)).unwrap();
        assert!(write.get("secret_set").is_none());
        let reason = write.unavailable_reason("secret_set").unwrap();
        assert!(reason.contains("--allow-destructive"), "{reason}");

        let secrets =
            ToolRegistry::new(&options(AccessLevel::Destructive, Some(&["secrets"]))).unwrap();
        assert!(secrets.get("secret_set").is_some());
        let issues =
            ToolRegistry::new(&options(AccessLevel::Destructive, Some(&["issues"]))).unwrap();
        assert!(issues.get("secret_set").is_none());
        let reason = issues.unavailable_reason("secret_set").unwrap();
        assert!(reason.contains("toolset `secrets`"), "{reason}");
    }

    #[test]
    fn reveal_argument_follows_the_flag() {
        let hidden = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        let list = &hidden.get("list_secrets").unwrap().tool;
        assert!(
            list.input_schema["properties"]
                .get("reveal_values")
                .is_none()
        );

        let mut opts = options(AccessLevel::Read, None);
        opts.allow_secret_values = true;
        let revealing = ToolRegistry::new(&opts).unwrap();
        let list = &revealing.get("list_secrets").unwrap().tool;
        assert!(
            list.input_schema["properties"]
                .get("reveal_values")
                .is_some()
        );
        let repo = &revealing.get("get_repository").unwrap().tool;
        assert!(
            repo.input_schema["properties"]
                .get("reveal_values")
                .is_none()
        );
    }

    #[test]
    fn unavailable_reasons_name_the_flag() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        let reason = read.unavailable_reason("create_pull_request").unwrap();
        assert!(reason.contains("--allow-write"), "{reason}");
        let reason = read.unavailable_reason("delete_repository").unwrap();
        assert!(reason.contains("--allow-destructive"), "{reason}");
        assert!(read.unavailable_reason("get_repository").is_none());
        assert!(read.unavailable_reason("no_such_tool").is_none());

        let issues = ToolRegistry::new(&options(AccessLevel::Read, Some(&["issues"]))).unwrap();
        let reason = issues.unavailable_reason("get_repository").unwrap();
        assert!(reason.contains("toolset `repository`"), "{reason}");
    }

    #[test]
    fn toolset_filtering() {
        let registry = ToolRegistry::new(&options(
            AccessLevel::Destructive,
            Some(&["repository-pullrequest", " issues "]),
        ))
        .unwrap();
        assert!(!registry.is_empty());
        for tool in registry.iter() {
            let ToolKind::Operation(op) = tool.kind else {
                continue;
            };
            let slug = toolset_slug(op.tag);
            assert!(
                slug == "repository-pullrequest" || slug == "issues",
                "{slug}"
            );
        }
        assert!(registry.get("create_pull_request").is_some());
        assert!(registry.get("get_issue").is_some());
        assert!(registry.get("get_repository").is_none());
        // Наборы не ограничивают api_request и api_search.
        assert!(registry.get("api_request").is_some());
        assert!(registry.get("api_search").is_some());

        // Пустой список равен «все наборы».
        let empty = ToolRegistry::new(&options(AccessLevel::Read, Some(&[]))).unwrap();
        assert_eq!(operation_tools(&empty), count(AccessLevel::Read));

        let err = ToolRegistry::new(&options(AccessLevel::Read, Some(&["repos"]))).unwrap_err();
        assert!(err.contains("unknown toolset `repos`"), "{err}");
    }
}
