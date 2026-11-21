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
//! Пользовательские инструменты ([`crate::custom_tools`]) регистрируются по уровню доступа,
//! без учёта наборов (и в динамическом режиме — сразу при запуске); совпадение имени
//! со встроенным инструментом — ошибка запуска.
//!
//! Без [`ServerOptions::local_machine`] (HTTP-режим) операции с файловыми аргументами
//! (загрузка вложений) не регистрируются, а из схем `api_request` и `secret_set` убираются
//! аргументы с локальными путями.
//!
//! В режиме динамических наборов (`ServerOptions::dynamic_toolsets`) реестр — снимок для
//! текущего списка включённых наборов: сервер пересобирает его через
//! [`ToolRegistry::with_toolsets`] при `enable_toolset`/`disable_toolset`. Пустой список
//! в этом режиме означает «ни одного набора», а не «все».

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use sourcecraft_api::spec::{AccessLevel, OperationDef, ParamType, find, operations};

use crate::ServerOptions;
use crate::api_request::{api_request_tool, without_local_files};
use crate::api_search::api_search_tool;
use crate::custom_tools::check_names;
use crate::dynamic_toolsets::{
    ENABLE_TOOLSET_TOOL, disable_toolset_tool, enable_toolset_tool, list_toolsets_tool,
};
use crate::jq::{JQ_ARG, add_jq_argument};
use crate::secret_copy::{KeychainMode, SECRET_COPY_TOOL, secret_copy_tool};
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
    /// `secret_copy`: копирование секрета без раскрытия значения.
    SecretCopy,
    /// Пользовательский инструмент: индекс в `ServerOptions::custom_tools`.
    Custom(usize),
    /// `list_toolsets`: наборы инструментов и какие из них включены (динамический режим).
    ListToolsets,
    /// `enable_toolset`: включить набор (динамический режим).
    EnableToolset,
    /// `disable_toolset`: выключить набор (динамический режим).
    DisableToolset,
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
    /// Выбранные наборы (slug); `None` — все. В динамическом режиме всегда `Some`.
    toolsets: Option<Vec<String>>,
    max_access: AccessLevel,
    allow_secret_values: bool,
    dynamic: bool,
    /// Пользовательские инструменты не выше уровня сервера. Наборы их не ограничивают,
    /// поэтому при пересборке реестра (`with_toolsets`) они переносятся как есть.
    custom: Vec<RegisteredTool>,
    /// Пользовательские инструменты выше уровня сервера: имя → нужный уровень.
    disabled_custom: HashMap<String, AccessLevel>,
    /// Инструменты могут работать с локальными файлами ([`ServerOptions::local_machine`]).
    local_files: bool,
    /// Источники `keychain://` в `secret_copy` ([`ServerOptions::keychain_mode`]): от этого
    /// зависит описание инструмента.
    keychain_mode: KeychainMode,
}

/// Принимает ли операция локальный файл (загрузка вложений через multipart).
pub fn takes_local_file(op: &OperationDef) -> bool {
    op.params
        .iter()
        .any(|param| param.value_type == ParamType::File)
}

impl ToolRegistry {
    /// Строит реестр. Ошибка — неизвестный набор инструментов или некорректная схема
    /// в сгенерированной таблице (последнее — ошибка генератора).
    pub fn new(options: &ServerOptions) -> Result<Self, String> {
        let names = match &options.toolsets {
            Some(names) => normalize_toolsets(names)?,
            None => Vec::new(),
        };
        let toolsets = if options.dynamic_toolsets {
            Some(names)
        } else {
            (!names.is_empty()).then_some(names)
        };
        check_names(&options.custom_tools)?;
        let mut custom = Vec::new();
        let mut disabled_custom = HashMap::new();
        for (index, tool) in options.custom_tools.iter().enumerate() {
            if options.max_access.allows(tool.access) {
                custom.push(RegisteredTool {
                    tool: tool.tool(),
                    kind: ToolKind::Custom(index),
                    access: tool.access,
                });
            } else {
                disabled_custom.insert(tool.name.clone(), tool.access);
            }
        }
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets,
            max_access: options.max_access,
            allow_secret_values: options.allow_secret_values,
            dynamic: options.dynamic_toolsets,
            custom,
            disabled_custom,
            local_files: options.local_machine,
            keychain_mode: options.keychain_mode(),
        }
        .populate()
    }

    /// Тот же реестр с другим списком включённых наборов (динамический режим).
    /// Ошибка — неизвестный набор.
    pub fn with_toolsets(&self, toolsets: &[String]) -> Result<Self, String> {
        let toolsets = normalize_toolsets(toolsets)?;
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets: if self.dynamic {
                Some(toolsets)
            } else {
                (!toolsets.is_empty()).then_some(toolsets)
            },
            max_access: self.max_access,
            allow_secret_values: self.allow_secret_values,
            dynamic: self.dynamic,
            custom: self.custom.clone(),
            disabled_custom: self.disabled_custom.clone(),
            local_files: self.local_files,
            keychain_mode: self.keychain_mode,
        }
        .populate()
    }

    /// Заполняет пустой реестр инструментами по его настройкам.
    fn populate(self) -> Result<Self, String> {
        let mut registry = self;
        let max_access = registry.max_access;
        for op in operations() {
            if registry.operation_enabled(op) {
                let mut tool = operation_tool(op)?;
                annotate_operation_tool(op, &mut tool, registry.allow_secret_values);
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
                tool: secret_set_tool(op, registry.local_files)?,
                kind: ToolKind::SecretSet,
                access: op.access,
            });
        }
        // `secret_copy` создаёт новые ключи на уровне записи (перезапись проверяется при вызове)
        // и относится к набору секретов, как `set_secret`.
        if let Some(op) = find(SET_SECRET_OPERATION)
            && registry.max_access.allows(AccessLevel::Write)
            && registry.toolset_enabled(op.tag)
        {
            registry.push(RegisteredTool {
                tool: secret_copy_tool(max_access, registry.keychain_mode),
                kind: ToolKind::SecretCopy,
                access: AccessLevel::Write,
            });
        }
        registry.push(RegisteredTool {
            tool: api_search_tool(),
            kind: ToolKind::ApiSearch,
            access: AccessLevel::Read,
        });
        let mut api_request = api_request_tool(max_access);
        if !registry.local_files {
            without_local_files(&mut api_request);
        }
        registry.push(RegisteredTool {
            tool: api_request,
            kind: ToolKind::ApiRequest,
            access: max_access,
        });
        if registry.dynamic {
            // Управление наборами меняет только список инструментов этого сервера,
            // данных SourceCraft не касается — уровень чтения.
            for (tool, kind) in [
                (list_toolsets_tool(), ToolKind::ListToolsets),
                (enable_toolset_tool(), ToolKind::EnableToolset),
                (disable_toolset_tool(), ToolKind::DisableToolset),
            ] {
                registry.push(RegisteredTool {
                    tool,
                    kind,
                    access: AccessLevel::Read,
                });
            }
        }
        // Пользовательские инструменты оператор настроил явно: они регистрируются всегда,
        // в том числе в динамическом режиме, и не входят ни в один набор.
        for tool in registry.custom.clone() {
            registry.push(tool);
        }
        Ok(registry)
    }

    fn operation_enabled(&self, op: &OperationDef) -> bool {
        !EXCLUDED_OPERATIONS.contains(&op.tool_name)
            && self.max_access.allows(op.access)
            && self.toolset_enabled(op.tag)
            && (self.local_files || !takes_local_file(op))
    }

    /// Могут ли инструменты работать с локальными файлами (см. [`ServerOptions::local_machine`]).
    pub fn local_files(&self) -> bool {
        self.local_files
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

    /// Включённые наборы (slug); `None` — все (только в обычном режиме).
    pub fn enabled_toolsets(&self) -> Option<&[String]> {
        self.toolsets.as_deref()
    }

    /// Режим динамических наборов: наборы включаются инструментом `enable_toolset`.
    pub fn is_dynamic(&self) -> bool {
        self.dynamic
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
        if let Some(&access) = self.disabled_custom.get(name) {
            return Some(format!(
                "custom tool `{name}` is disabled: it {} data in SourceCraft and the server runs \
                 with {} access. Ask the user to {} if this is intended",
                access_verb(access),
                access_name(self.max_access),
                access_hint(access, self.local_files),
            ));
        }
        if name == SECRET_SET_TOOL {
            return self.disabled_reason(name, find(SET_SECRET_OPERATION)?);
        }
        if name == SECRET_COPY_TOOL {
            let op = find(SET_SECRET_OPERATION)?;
            if !self.max_access.allows(AccessLevel::Write) {
                return Some(format!(
                    "tool `{name}` is disabled: it writes secrets in SourceCraft and the server \
                     runs with read-only access. Ask the user to {} if this change is intended",
                    access_hint(AccessLevel::Write, self.local_files)
                ));
            }
            return self.toolset_reason(name, op.tag);
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
        if !self.local_files && takes_local_file(op) {
            return Some(format!(
                "tool `{name}` is not available on this server: it uploads a local file, and this \
                 server runs on another machine (HTTP mode) and cannot read the agent's files. \
                 Ask the user to upload the file in SourceCraft or to use a local (stdio) \
                 `sourcecraft-mcp serve` for uploads"
            ));
        }
        if !self.max_access.allows(op.access) {
            return Some(format!(
                "tool `{name}` is disabled: it {} data in SourceCraft and the server runs with {} \
                 access. Ask the user to {} if this change is intended",
                access_verb(op.access),
                access_name(self.max_access),
                access_hint(op.access, self.local_files),
            ));
        }
        self.toolset_reason(name, op.tag)
    }

    /// Почему инструмент набора с тегом `tag` не зарегистрирован, если набор выключен.
    fn toolset_reason(&self, name: &str, tag: &str) -> Option<String> {
        if self.toolset_enabled(tag) {
            return None;
        }
        let slug = toolset_slug(tag);
        if !self.dynamic {
            return Some(format!(
                "tool `{name}` belongs to toolset `{slug}`, which is not enabled; ask the user to \
                 add it to --toolsets"
            ));
        }
        // `secret_set` и `secret_copy` — не операции API, у них нет пути для `api_request`.
        let fallback = if find(name).is_some() {
            ", or call the same operation with `api_request`"
        } else {
            ""
        };
        Some(format!(
            "tool `{name}` belongs to toolset `{slug}`, which is not enabled: call \
             `{ENABLE_TOOLSET_TOOL}` with `toolset: \"{slug}\"` first{fallback}"
        ))
    }
}

/// Описание MCP-инструмента для операции.
pub fn operation_tool(op: &'static OperationDef) -> Result<Tool, String> {
    let mut schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;
    if schema
        .get("properties")
        .and_then(|p| p.get(JQ_ARG))
        .is_some()
    {
        return Err(format!(
            "{}: parameter `{JQ_ARG}` clashes with the response filter argument",
            op.tool_name
        ));
    }
    add_jq_argument(&mut schema);

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
    if takes_local_file(op) {
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

/// Флаг HTTP-сервера, который включает уровень доступа `access`.
pub fn access_flag(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "no extra flag",
        AccessLevel::Write => "--allow-write",
        AccessLevel::Destructive => "--allow-destructive",
    }
}

/// Что пользователь делает, чтобы включить уровень `access`; фраза продолжает «Ask the user
/// to …». Локальный сервер (`local_machine`, stdio): права — настройка профиля, флаги
/// регистрации их не расширяют, а на macOS повышение ещё и одобряется Touch ID. HTTP-сервер:
/// уровень задают флаги запуска.
pub fn access_hint(access: AccessLevel, local_machine: bool) -> String {
    match (access, local_machine) {
        (AccessLevel::Read, _) => "keep the read-only access".to_owned(),
        (level, true) => format!(
            "allow `{}` access for the server's profile with `sourcecraft-mcp ui` and restart \
             the MCP client",
            access_name(level)
        ),
        (level, false) => format!("restart the server with {}", access_flag(level)),
    }
}

/// Короткая форма [`access_hint`] для поля `enable_with` в `api_search`.
pub fn access_enable_with(access: AccessLevel, local_machine: bool) -> String {
    if local_machine {
        format!(
            "`{}` access for the server's profile (`sourcecraft-mcp ui`)",
            access_name(access)
        )
    } else {
        access_flag(access).to_owned()
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
            assert!(
                tool.input_schema["properties"].get(JQ_ARG).is_some(),
                "{}",
                op.tool_name
            );
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
        // Плюс `secret_set` (только при `Destructive`) и `secret_copy` (с `Write`).
        assert_eq!(
            all.len(),
            operations().len() - EXCLUDED_OPERATIONS.len() + EXTRA_TOOLS + 2
        );
        assert_eq!(write.len(), count(AccessLevel::Write) + EXTRA_TOOLS + 1);
        let delete = all.get("delete_repository").unwrap();
        let annotations = delete.tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(true));
        assert!(read.len() < write.len() && write.len() < all.len());
    }

    #[test]
    fn remote_server_has_no_local_file_tools() {
        let remote = ToolRegistry::new(&ServerOptions {
            local_machine: false,
            ..options(AccessLevel::Destructive, None)
        })
        .unwrap();
        assert!(!remote.local_files());
        let uploads: Vec<&str> = operations()
            .iter()
            .filter(|op| takes_local_file(op))
            .map(|op| op.tool_name)
            .collect();
        assert!(!uploads.is_empty());
        for name in &uploads {
            assert!(remote.get(name).is_none(), "{name}");
            let reason = remote.unavailable_reason(name).unwrap();
            assert!(reason.contains("HTTP mode"), "{reason}");
        }
        assert_eq!(
            operation_tools(&remote),
            count(AccessLevel::Destructive) - uploads.len()
        );
        // Ни в одной схеме нет аргументов с локальными путями.
        for tool in remote.iter() {
            let schema = serde_json::Value::Object((*tool.tool.input_schema).clone()).to_string();
            for arg in ["body_file", "save_to", "value_file"] {
                assert!(!schema.contains(arg), "{}: {arg}", tool.tool.name);
            }
            let description = tool.tool.description.as_deref().unwrap_or_default();
            assert!(!description.contains("local file"), "{}", tool.tool.name);
        }
        assert!(remote.get("secret_set").is_some());
        assert!(remote.get("api_request").is_some());

        let local = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        assert!(local.local_files());
        assert!(uploads.iter().all(|name| local.get(name).is_some()));
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
        assert!(reason.contains("`destructive` access"), "{reason}");

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
    fn secret_copy_requires_write_access_and_secrets_toolset() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        assert!(read.get("secret_copy").is_none());
        let reason = read.unavailable_reason("secret_copy").unwrap();
        assert!(reason.contains("`write` access"), "{reason}");

        let write = ToolRegistry::new(&options(AccessLevel::Write, None)).unwrap();
        let tool = write.get("secret_copy").unwrap();
        assert!(matches!(tool.kind, ToolKind::SecretCopy));
        assert_eq!(tool.access, AccessLevel::Write);
        assert!(write.unavailable_reason("secret_copy").is_none());

        let issues = ToolRegistry::new(&options(AccessLevel::Write, Some(&["issues"]))).unwrap();
        assert!(issues.get("secret_copy").is_none());
        let reason = issues.unavailable_reason("secret_copy").unwrap();
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
        assert!(reason.contains("`write` access"), "{reason}");
        assert!(reason.contains("sourcecraft-mcp ui"), "{reason}");
        let reason = read.unavailable_reason("delete_repository").unwrap();
        assert!(reason.contains("`destructive` access"), "{reason}");
        assert!(read.unavailable_reason("get_repository").is_none());

        // HTTP-сервер: уровень задают флаги запуска, а не профиль.
        let remote = ToolRegistry::new(&ServerOptions {
            local_machine: false,
            ..options(AccessLevel::Read, None)
        })
        .unwrap();
        let reason = remote.unavailable_reason("create_pull_request").unwrap();
        assert!(
            reason.contains("restart the server with --allow-write"),
            "{reason}"
        );
        assert!(!reason.contains("sourcecraft-mcp ui"), "{reason}");
        let reason = remote.unavailable_reason("delete_repository").unwrap();
        assert!(reason.contains("--allow-destructive"), "{reason}");
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
