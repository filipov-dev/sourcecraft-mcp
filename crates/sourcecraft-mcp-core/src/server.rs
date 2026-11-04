//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов динамический (строится из таблицы операций), поэтому `ServerHandler`
//! реализован вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `SourceCraftServer::new(...)?.serve(stdio())`.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use sourcecraft_api::executor::{ApiClient, ApiResponse};
use sourcecraft_api::spec::{AccessLevel, OperationDef, find};

use crate::ServerOptions;
use crate::api_request;
use crate::api_search;
use crate::error::{ToolError, describe_api_error};
use crate::files::FilePolicy;
use crate::redact::{redact_api_error, redact_response};
use crate::registry::{ToolKind, ToolRegistry, access_name};
use crate::request::{build_request, check_guards};
use crate::response::{failure, response_text, success};
use crate::secrets::{SECRET_SET_TOOL, SET_SECRET_OPERATION, build_secret_set, scrub, take_reveal};

/// Имя сервера в `initialize`.
pub const SERVER_NAME: &str = "sourcecraft-mcp";

/// MCP-сервер SourceCraft. Дёшево клонируется.
#[derive(Debug, Clone)]
pub struct SourceCraftServer {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    client: ApiClient,
    options: ServerOptions,
    files: FilePolicy,
    registry: ToolRegistry,
    instructions: String,
}

impl SourceCraftServer {
    /// Создаёт сервер. Ошибка — неизвестный набор инструментов в `options.toolsets`.
    pub fn new(
        client: ApiClient,
        options: ServerOptions,
        files: FilePolicy,
    ) -> Result<Self, String> {
        let registry = ToolRegistry::new(&options)?;
        let instructions = server_instructions(&options, &files);
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                options,
                files,
                registry,
                instructions,
            }),
        })
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.inner.registry
    }

    pub fn instructions(&self) -> &str {
        &self.inner.instructions
    }

    /// Вызов инструмента без транспорта. Ошибки возвращаются как `isError: true`;
    /// `Err` — только для неизвестного инструмента (ошибка протокола).
    pub async fn call(&self, name: &str, args: JsonObject) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let Some(tool) = inner.registry.get(name) else {
            return match inner.registry.unavailable_reason(name) {
                Some(reason) => Ok(failure(&ToolError::new(reason))),
                None => Err(ErrorData::invalid_params(
                    format!("unknown tool `{name}`"),
                    None,
                )),
            };
        };
        let result = match tool.kind {
            ToolKind::Operation(op) => self.call_operation(op, args).await,
            ToolKind::ApiRequest => self.call_api_request(&args).await,
            ToolKind::ApiSearch => api_search::search(&args, &inner.registry),
            ToolKind::SecretSet => self.call_secret_set(&args).await,
        };
        Ok(match result {
            Ok(text) => success(text),
            Err(err) => {
                tracing::debug!(tool = name, error = %err, "инструмент завершился ошибкой");
                failure(&err)
            }
        })
    }

    async fn call_operation(
        &self,
        op: &'static OperationDef,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let reveal = take_reveal(op, &mut args, inner.options.allow_secret_values)?;
        check_guards(op, &args, inner.options.max_access)?;
        let request = build_request(op, &args, &inner.files)?;
        let path = request.path.clone();
        let response = inner
            .client
            .execute(request)
            .await
            .map_err(|err| redact_api_error(&path, err))?;
        Ok(self.render(Some(op), &path, &response, reveal))
    }

    async fn call_api_request(&self, args: &JsonObject) -> Result<String, ToolError> {
        let inner = &self.inner;
        let prepared = api_request::prepare(args, inner.options.max_access, &inner.files)?;
        let path = prepared.request.path.clone();
        let response = inner
            .client
            .execute(prepared.request)
            .await
            .map_err(|err| redact_api_error(&path, err))?;
        // `api_request` значения секретов не раскрывает: для этого есть `list_secrets`.
        Ok(self.render(prepared.operation, &path, &response, false))
    }

    /// `secret_set`: значение кодируется в base64 и вычищается из ответа и текста ошибки.
    async fn call_secret_set(&self, args: &JsonObject) -> Result<String, ToolError> {
        let inner = &self.inner;
        let built = build_secret_set(args, &inner.files).await?;
        let path = built.request.path.clone();
        tracing::debug!(tool = SECRET_SET_TOOL, %path, "запись секрета");
        match inner.client.execute(built.request).await {
            Ok(response) => Ok(scrub(
                &self.render(find(SET_SECRET_OPERATION), &path, &response, false),
                &built.sensitive,
            )),
            Err(err) => Err(ToolError::new(scrub(
                &describe_api_error(&redact_api_error(&path, err)),
                &built.sensitive,
            ))),
        }
    }

    /// Единая точка постобработки успешных ответов API — сгенерированных инструментов,
    /// `api_request` и `secret_set`.
    ///
    /// `op` — операция инструмента или самая точная операция, с которой совпал запрос
    /// `api_request` (`None` — путь не совпал ни с одной операцией); `path` — фактический
    /// относительный путь запроса. Значения секретов и учётных данных вырезаются по `path`
    /// до превращения ответа в текст ([`crate::redact`]); `reveal_secret_values` (только при
    /// `ServerOptions::allow_secret_values`) оставляет значения секретов для одного вызова.
    fn render(
        &self,
        _op: Option<&OperationDef>,
        path: &str,
        response: &ApiResponse,
        reveal_secret_values: bool,
    ) -> String {
        let mut response = response.clone();
        let redactions = redact_response(path, &mut response, reveal_secret_values);
        let mut text = response_text(&response);
        if let Some(note) = redactions.note(self.inner.options.allow_secret_values) {
            text.push_str("\n\n");
            text.push_str(&note);
        }
        text
    }
}

impl ServerHandler for SourceCraftServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
                    .with_title("SourceCraft"),
            )
            .with_instructions(self.inner.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.inner.registry.tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        self.call(&request.name, args).await.map(Into::into)
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяет `build_request` с понятными агенту сообщениями;
        // проверку схемы на стороне rmcp не включаем.
        None
    }
}

/// Инструкции сервера для агента (поле `instructions` в `initialize`).
pub fn server_instructions(options: &ServerOptions, files: &FilePolicy) -> String {
    let access = match options.max_access {
        AccessLevel::Read => {
            "This server is read-only: only tools that read data are available. Tools that \
             create or change data are enabled when the user starts the server with \
             --allow-write, and tools that delete data with --allow-destructive. If a task needs \
             them, ask the user instead of looking for workarounds."
        }
        AccessLevel::Write => {
            "This server can read, create and change data, but not delete it: deleting tools \
             (and making a repository public) need the user to start the server with \
             --allow-destructive. Confirm changes with the user when they were not explicitly \
             requested."
        }
        AccessLevel::Destructive => {
            "This server can read, change and delete data. Deleting is irreversible: confirm \
             with the user before calling destructive tools unless they explicitly asked for it."
        }
    };
    let toolsets = match &options.toolsets {
        Some(names) if !names.iter().all(|n| n.trim().is_empty()) => format!(
            " Only these toolsets are enabled: {}.",
            names
                .iter()
                .map(|n| n.trim())
                .filter(|n| !n.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => String::new(),
    };
    format!(
        "SourceCraft MCP server ({level} access). Every tool calls one SourceCraft REST API \
         operation and returns its JSON response.{toolsets}\n\
         \n\
         - Access: {access}\n\
         - Identifiers: many tools accept either slugs (`org_slug`, `repo_slug`, …) or IDs \
         (`repo_id`, …). Pass exactly one of these sets, as described in the tool. IDs and \
         integers may be passed as strings.\n\
         - Request bodies go in the `body` argument as a JSON object.\n\
         - Lists are paginated: pass `page_size`, then `page_token` from the previous response.\n\
         - Large responses are truncated; narrow the request when that happens.\n\
         - Finding operations: `api_search` searches all API operations, including tools not \
         enabled here, and tells which flag enables them. `api_request` calls any relative API \
         path within the server's access level (toolsets do not limit it); prefer the dedicated \
         tool when one exists.\n\
         - Local files: file arguments take a path (absolute, or relative to the server's \
         working directory) inside the allowed directories: {roots}. Other paths are rejected; \
         the user can allow more with --allow-path.\n\
         - Secrets: never ask the user to paste tokens or secret values into the conversation. \
         The API token is managed by the server and is never available to tools. {secrets}\n\
         - Secrets in shell commands: {shell}",
        level = access_name(options.max_access),
        roots = files.describe_roots(),
        secrets = secrets_instructions(options),
        shell = shell_secrets_instructions(options),
    )
}

/// Часть инструкций об использовании секретов в командах оболочки через CLI
/// `sourcecraft-mcp secret`: значение попадает в окружение одной команды, а не в контекст агента.
fn shell_secrets_instructions(options: &ServerOptions) -> String {
    let exe = options
        .executable
        .as_deref()
        .map(shell_quote)
        .unwrap_or_else(|| "sourcecraft-mcp".to_owned());
    let profile = options
        .profile
        .as_deref()
        .map(shell_quote)
        .unwrap_or_else(|| "<profile>".to_owned());
    let target = format!("--profile {profile} --repo <org>/<repo>");
    format!(
        "when a command needs a repository secret (a deploy key, a server password), do not \
         read the value. Run the command through the CLI, which puts the secret into an \
         environment variable of that command only and replaces the value with `***` in its \
         output: `{exe} secret exec {target} --env NAME=KEY -- <command> [args...]`. \
         `--env NAME=KEY` is repeatable; `--all` injects every secret under its own key. No \
         shell is involved: to use `$NAME`, run `-- sh -c '...'` in single quotes so that the \
         child shell expands it, not yours. To copy a secret to a server without a local file: \
         `{exe} secret exec {target} --env V=KEY -- sh -c 'printf %s \"$V\" | ssh <host> \
         \"cat > <path>\"'`. When a program needs the secret as a file (for example `ssh -i`), \
         `{exe} secret write {target} KEY --to <path>` writes it with mode 0600; delete the \
         file when done. `{exe} secret list {target}` shows the keys. Never echo, print or log \
         secret variables, never write them into files that you then read, and never encode or \
         transform them in output: masking catches only the value and its base64 form. Run \
         `{exe} secret get {target} KEY --reveal` (it prints the value into your context) only \
         when the user explicitly asks to see the value."
    )
}

/// Экранирует слово для POSIX-оболочки, если в нём есть символы кроме `[A-Za-z0-9/._-]`.
fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Часть инструкций о секретах репозитория.
fn secrets_instructions(options: &ServerOptions) -> String {
    let reveal = if options.allow_secret_values {
        "This server can reveal them: pass `reveal_values: true` to `list_secrets` only when the \
         user explicitly needs to see the values, and never copy revealed values into files, \
         commands, commit messages or other tools."
    } else {
        "This server does not reveal them; work with keys only."
    };
    let set = if options.max_access.allows(AccessLevel::Destructive) {
        "To set a secret use `secret_set`: it takes plain text and base64-encodes it itself. \
         Prefer its `value_file` argument (a local file with the value, for example one the user \
         prepared), so the value never enters the conversation."
    } else {
        "Setting secrets (`secret_set`) needs the server to run with --allow-destructive."
    };
    format!(
        "Repository secret values (used by CI) and fields that look like credentials \
         (passwords, tokens, private keys) are replaced with \"[redacted]\" in every response, \
         including errors; a note says when that happened. {reveal} {set} To use a secret, \
         reference it by key (for example in CI configuration) or use the CLI described below \
         instead of reading its value; do not print secret values in commands, logs, files or \
         messages."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_depend_on_access() {
        let files = FilePolicy::new(&[]).unwrap();
        let read = server_instructions(&ServerOptions::default(), &files);
        assert!(read.contains("read-only access"));
        assert!(read.contains("--allow-write"));
        assert!(read.contains("--allow-destructive"));
        assert!(read.contains("allowed directories: none"));
        assert!(read.contains("`api_search`") && read.contains("`api_request`"));

        let write = server_instructions(
            &ServerOptions {
                max_access: AccessLevel::Write,
                toolsets: Some(vec!["issues".into(), " ".into()]),
                allow_secret_values: false,
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(write.contains("(write access)"));
        assert!(write.contains("Only these toolsets are enabled: issues."));
    }

    #[test]
    fn instructions_explain_secrets() {
        let files = FilePolicy::new(&[]).unwrap();
        let read = server_instructions(&ServerOptions::default(), &files);
        assert!(read.contains("[redacted]"), "{read}");
        assert!(read.contains("does not reveal"), "{read}");
        assert!(read.contains("`secret_set`) needs the server to run with --allow-destructive"));
        assert!(!read.contains("reveal_values"));

        let all = server_instructions(
            &ServerOptions {
                max_access: AccessLevel::Destructive,
                toolsets: None,
                allow_secret_values: true,
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(all.contains("`reveal_values: true`"), "{all}");
        assert!(all.contains("`value_file`"), "{all}");
    }

    #[test]
    fn instructions_explain_secret_cli() {
        let files = FilePolicy::new(&[]).unwrap();
        let plain = server_instructions(&ServerOptions::default(), &files);
        assert!(
            plain.contains(
                "`sourcecraft-mcp secret exec --profile <profile> --repo <org>/<repo> \
                 --env NAME=KEY -- <command> [args...]`"
            ),
            "{plain}"
        );
        assert!(plain.contains("Never echo, print or log secret variables"));
        assert!(plain.contains("--reveal"));

        let exact = server_instructions(
            &ServerOptions {
                profile: Some("codex".into()),
                executable: Some("/opt/my tools/sourcecraft-mcp".into()),
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(
            exact.contains(
                "`'/opt/my tools/sourcecraft-mcp' secret exec --profile codex --repo <org>/<repo>"
            ),
            "{exact}"
        );
        assert!(exact.contains("secret write --profile codex"), "{exact}");
    }

    #[test]
    fn quotes_shell_words() {
        assert_eq!(
            shell_quote("/usr/local/bin/sourcecraft-mcp"),
            "/usr/local/bin/sourcecraft-mcp"
        );
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
