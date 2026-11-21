//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов динамический (строится из таблицы операций), поэтому `ServerHandler`
//! реализован вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `SourceCraftServer::new(...)?.serve(stdio())`.
//!
//! Токен SourceCraft для вызова берётся из [`TokenSource`]: у stdio-сервера это токен клиента
//! API из системного хранилища, у HTTP-сервера — токен текущего HTTP-запроса агента, который
//! транспорт кладёт в расширения запроса MCP. Ядро не знает, как устроен HTTP: функцию
//! извлечения токена передаёт бинарник.
//!
//! Состояние сессии — текущий реестр инструментов: в режиме динамических наборов
//! (`ServerOptions::dynamic_toolsets`) `enable_toolset`/`disable_toolset` подменяют его снимок,
//! и клиент получает `notifications/tools/list_changed`: в сессии с `initialize` — сразу после
//! вызова, в протоколе 2026-07-28 — через подписку `subscriptions/listen`. Клоны сервера делят
//! сессию; транспорт с несколькими сессиями должен создавать их через
//! [`SourceCraftServer::new_session`]. Реестр от токена не зависит, поэтому с токенами
//! отдельных запросов (HTTP) наборы переключаются так же.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, RwLock};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Extensions, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerConfig, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use sourcecraft_api::ApiToken;
use sourcecraft_api::executor::{ApiClient, ApiRequest, ApiResponse, ResponseBody};
use sourcecraft_api::spec::{AccessLevel, OperationDef, find};

use crate::ServerOptions;
use crate::api_request;
use crate::api_search;
use crate::custom_tools::access_mismatch;
use crate::dynamic_toolsets;
use crate::error::{ToolError, describe_api_error};
use crate::files::FilePolicy;
use crate::jq::{FILTER_INPUT_LIMIT, JqFilter, take_jq};
use crate::redact::{redact_api_error, redact_response};
use crate::registry::{ToolKind, ToolRegistry, access_hint, access_name};
use crate::request::{build_request, check_guards};
use crate::response::{TRUNCATED_NOTE, body_text, failure, response_text, success, truncate_text};
use crate::secret_copy::{CopyPolicy, copy_secret};
use crate::secrets::{SECRET_SET_TOOL, SET_SECRET_OPERATION, build_secret_set, scrub, take_reveal};

/// Сколько `secret_copy` ждёт завершения записи секрета.
const SECRET_COPY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Имя сервера в `initialize`.
pub const SERVER_NAME: &str = "sourcecraft-mcp";

/// Функция, которая достаёт токен текущего запроса из расширений запроса MCP.
pub type RequestTokenFn = dyn Fn(&Extensions) -> Option<ApiToken> + Send + Sync;

/// Откуда брать токен SourceCraft для вызова инструмента.
#[derive(Clone, Default)]
pub enum TokenSource {
    /// Токен клиента API, заданный при запуске (stdio: из системного хранилища по профилю).
    #[default]
    Client,
    /// Токен текущего запроса (HTTP: заголовок `Authorization` каждого HTTP-запроса).
    ///
    /// Токен читается заново на каждый вызов и не сохраняется: сессия MCP может пережить
    /// токен, а у разных запросов могут быть разные токены. Без токена вызов к API не
    /// выполняется и возвращает ошибку инструмента.
    PerRequest(Arc<RequestTokenFn>),
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client => f.write_str("Client"),
            Self::PerRequest(_) => f.write_str("PerRequest"),
        }
    }
}

/// MCP-сервер SourceCraft. Дёшево клонируется.
#[derive(Debug, Clone)]
pub struct SourceCraftServer {
    inner: Arc<Inner>,
    session: Arc<Session>,
}

#[derive(Debug)]
struct Inner {
    client: ApiClient,
    tokens: TokenSource,
    options: ServerOptions,
    files: FilePolicy,
    /// Реестр при запуске; с него начинается каждая сессия.
    initial: Arc<ToolRegistry>,
    instructions: String,
}

/// Изменяемое состояние одной MCP-сессии.
#[derive(Debug)]
struct Session {
    registry: RwLock<Arc<ToolRegistry>>,
    /// Счётчик изменений списка инструментов: по нему подписки `subscriptions/listen` узнают,
    /// что пора отправить `notifications/tools/list_changed`.
    changes: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry: RwLock::new(registry),
            changes: tokio::sync::watch::Sender::new(0),
        }
    }

    fn registry(&self) -> Arc<ToolRegistry> {
        // Отравленная блокировка не мешает: внутри только `Arc`, его замена атомарна.
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn generation(&self) -> u64 {
        *self.changes.borrow()
    }
}

impl SourceCraftServer {
    /// Создаёт сервер с токеном клиента API ([`TokenSource::Client`]).
    /// Ошибка — неизвестный набор инструментов в `options.toolsets`.
    pub fn new(
        client: ApiClient,
        options: ServerOptions,
        files: FilePolicy,
    ) -> Result<Self, String> {
        Self::with_token_source(client, options, files, TokenSource::Client)
    }

    /// Создаёт сервер с заданным источником токена.
    ///
    /// Без [`ServerOptions::local_machine`] политика файлов принудительно
    /// [`FilePolicy::disabled`], какой бы ни передали: удалённый сервер не работает с файлами
    /// своей машины от имени агента.
    pub fn with_token_source(
        client: ApiClient,
        options: ServerOptions,
        files: FilePolicy,
        tokens: TokenSource,
    ) -> Result<Self, String> {
        let files = if options.local_machine {
            files
        } else {
            FilePolicy::disabled()
        };
        let registry = Arc::new(ToolRegistry::new(&options)?);
        let instructions = server_instructions(&options, &files);
        Ok(Self {
            session: Arc::new(Session::new(registry.clone())),
            inner: Arc::new(Inner {
                client,
                tokens,
                options,
                files,
                initial: registry,
                instructions,
            }),
        })
    }

    /// Тот же сервер с новой сессией: наборы инструментов — как при запуске. Клон
    /// (`clone()`) делит сессию с исходным сервером.
    pub fn new_session(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            session: Arc::new(Session::new(self.inner.initial.clone())),
        }
    }

    /// Текущий реестр инструментов сессии (снимок).
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.session.registry()
    }

    pub fn instructions(&self) -> &str {
        &self.inner.instructions
    }

    /// Вызов инструмента без транспорта. Ошибки возвращаются как `isError: true`;
    /// `Err` — только для неизвестного инструмента (ошибка протокола).
    pub async fn call(&self, name: &str, args: JsonObject) -> Result<CallToolResult, ErrorData> {
        self.call_with_extensions(name, args, &Extensions::new())
            .await
    }

    /// Вызов инструмента с расширениями запроса MCP, из которых [`TokenSource::PerRequest`]
    /// берёт токен.
    pub async fn call_with_extensions(
        &self,
        name: &str,
        args: JsonObject,
        extensions: &Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        let registry = self.registry();
        let Some(tool) = registry.get(name) else {
            return match registry.unavailable_reason(name) {
                Some(reason) => Ok(failure(&ToolError::new(reason))),
                None => Err(ErrorData::invalid_params(
                    format!("unknown tool `{name}`"),
                    None,
                )),
            };
        };
        // Поиск по операциям и управление наборами не обращаются к API, токен им не нужен.
        let local = match tool.kind {
            ToolKind::ApiSearch => Some(api_search::search(&args, &registry)),
            ToolKind::ListToolsets => Some(dynamic_toolsets::list(&args, &registry)),
            ToolKind::EnableToolset => Some(self.toggle_toolsets(&args, true)),
            ToolKind::DisableToolset => Some(self.toggle_toolsets(&args, false)),
            ToolKind::Operation(_)
            | ToolKind::ApiRequest
            | ToolKind::SecretSet
            | ToolKind::SecretCopy
            | ToolKind::Custom(_) => None,
        };
        if let Some(result) = local {
            return Ok(match result {
                Ok(text) => success(text),
                Err(err) => failure(&err),
            });
        }
        let client = match self.client_for(extensions) {
            Ok(client) => client,
            Err(err) => return Ok(failure(&err)),
        };
        let result = match tool.kind {
            ToolKind::Operation(op) => self.call_operation(&client, op, args).await,
            ToolKind::ApiRequest => self.call_api_request(&client, args).await,
            ToolKind::SecretSet => self.call_secret_set(&client, &args).await,
            ToolKind::SecretCopy => self.call_secret_copy(&client, &args).await,
            // Пользовательские инструменты идут с клиентом этого вызова: в HTTP-режиме —
            // с токеном текущего запроса.
            ToolKind::Custom(index) => self.call_custom(&client, index, args).await,
            ToolKind::ApiSearch
            | ToolKind::ListToolsets
            | ToolKind::EnableToolset
            | ToolKind::DisableToolset => {
                // Обработаны выше, без токена.
                return Ok(failure(&ToolError::new(format!(
                    "tool `{name}` is not an API call"
                ))));
            }
        };
        // Защита в глубину: токен не попадает в ответ агенту, даже если API его отразил.
        Ok(match result {
            Ok(text) => success(client.scrub_token(text)),
            Err(err) => {
                let err = ToolError::new(client.scrub_token(err.message().to_owned()));
                tracing::debug!(tool = name, error = %err, "инструмент завершился ошибкой");
                failure(&err)
            }
        })
    }

    /// `enable_toolset`/`disable_toolset`: пересобирает реестр сессии. Замена делается под
    /// блокировкой записи от текущего реестра, чтобы параллельные вызовы не потеряли изменения.
    fn toggle_toolsets(&self, args: &JsonObject, enable: bool) -> Result<String, ToolError> {
        let mut guard = self
            .session
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let toggled = dynamic_toolsets::toggle(&guard, args, enable)?;
        if let Some(next) = toggled.registry {
            tracing::info!(
                toolsets = ?next.enabled_toolsets(),
                tools = next.len(),
                "наборы инструментов изменены"
            );
            *guard = Arc::new(next);
            drop(guard);
            self.session
                .changes
                .send_modify(|generation| *generation += 1);
        }
        Ok(toggled.text)
    }

    /// Клиент API с токеном для этого вызова. В HTTP-режиме — копия общего клиента
    /// с токеном текущего запроса; токен живёт только до конца вызова.
    fn client_for(&self, extensions: &Extensions) -> Result<Cow<'_, ApiClient>, ToolError> {
        let client = &self.inner.client;
        match &self.inner.tokens {
            TokenSource::Client => Ok(Cow::Borrowed(client)),
            TokenSource::PerRequest(token_of) => {
                let token = token_of(extensions).ok_or_else(|| {
                    ToolError::new(
                        "this request carries no SourceCraft token: the MCP client must send it \
                         in the `Authorization: Bearer <token>` HTTP header",
                    )
                })?;
                client.with_token(token).map(Cow::Owned).map_err(|_| {
                    ToolError::new("the SourceCraft token in the request is malformed")
                })
            }
        }
    }

    async fn call_operation(
        &self,
        client: &ApiClient,
        op: &'static OperationDef,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        // Фильтр проверяется до запроса: ошибка в нём не должна появиться после записи.
        let jq = take_jq(&mut args)?;
        let reveal = take_reveal(op, &mut args, inner.options.allow_secret_values)?;
        check_guards(
            op,
            &args,
            inner.options.max_access,
            inner.options.local_machine,
        )?;
        let request = build_request(op, &args, &inner.files)?;
        let path = request.path.clone();
        let response = execute(client, request, jq.is_some()).await?;
        self.render(client, Some(op), &path, &response, reveal, jq.as_ref())
            .await
    }

    async fn call_api_request(
        &self,
        client: &ApiClient,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let jq = take_jq(&mut args)?;
        let prepared = api_request::prepare(&args, inner.options.max_access, &inner.files)?;
        let path = prepared.request.path.clone();
        let response = execute(client, prepared.request, jq.is_some()).await?;
        // `api_request` значения секретов не раскрывает: для этого есть `list_secrets`.
        self.render(
            client,
            prepared.operation,
            &path,
            &response,
            false,
            jq.as_ref(),
        )
        .await
    }

    /// Пользовательский инструмент: шаблон превращается в аргументы `api_request` и
    /// проходит те же проверки (путь, уровень доступа, секреты, политика файлов). Запрос
    /// идёт с токеном вызова (`client`), в HTTP-режиме — с токеном агента. Дополнительно
    /// вызов не может требовать уровня выше объявленного у инструмента.
    async fn call_custom(
        &self,
        client: &ApiClient,
        index: usize,
        args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let Some(tool) = inner.options.custom_tools.get(index) else {
            return Err(ToolError::new("custom tool is not available"));
        };
        let (request_args, jq) = tool.build_call(args)?;
        let prepared = api_request::prepare(&request_args, inner.options.max_access, &inner.files)?;
        if !tool.access.allows(prepared.access) {
            return Err(access_mismatch(tool, prepared.access));
        }
        let path = prepared.request.path.clone();
        let response = execute(client, prepared.request, jq.is_some()).await?;
        self.render(
            client,
            prepared.operation,
            &path,
            &response,
            false,
            jq.as_ref(),
        )
        .await
    }

    /// `secret_set`: значение кодируется в base64 и вычищается из ответа и текста ошибки.
    async fn call_secret_set(
        &self,
        client: &ApiClient,
        args: &JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let built = build_secret_set(args, &inner.files).await?;
        let path = built.request.path.clone();
        tracing::debug!(tool = SECRET_SET_TOOL, %path, "запись секрета");
        match client.execute(built.request).await {
            Ok(response) => Ok(scrub(
                &self
                    .render(
                        client,
                        find(SET_SECRET_OPERATION),
                        &path,
                        &response,
                        false,
                        None,
                    )
                    .await?,
                &built.sensitive,
            )),
            Err(err) => Err(ToolError::new(scrub(
                &describe_api_error(&redact_api_error(&path, err)),
                &built.sensitive,
            ))),
        }
    }

    /// `secret_copy`: значение читает и пишет сервер; в результате только ссылки и итог.
    /// Значения секретов не раскрываются, поэтому `allow_secret_values` не требуется.
    async fn call_secret_copy(
        &self,
        client: &ApiClient,
        args: &JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let (source, destination) = crate::secret_copy::parse_args(args)?;
        let policy = CopyPolicy {
            allow_overwrite: inner.options.max_access.allows(AccessLevel::Destructive),
            overwrite_hint: format!(
                "the server runs without `destructive` access; ask the user to {}, or copy to a \
                 new key",
                access_hint(AccessLevel::Destructive, inner.options.local_machine)
            ),
            // Удалённому серверу (HTTP) хранилище недоступно, даже если его передали.
            keychain: inner.options.keychain_source().cloned(),
            keychain_destination: false,
            keychain_hint: if inner.options.local_machine {
                "local keychain sources (`keychain://…`) are disabled on this server: they would \
                 upload data from this machine. The user can run `sourcecraft-mcp secret copy` \
                 in a terminal, or restart the server with --allow-keychain-source"
            } else {
                "keychain sources (`keychain://…`) are not available: this server runs on \
                 another machine (HTTP mode); copy between repositories (`sc://…`) only"
            }
            .to_owned(),
            wait: SECRET_COPY_WAIT,
        };
        let report = copy_secret(client, &source, &destination, &policy).await?;
        Ok(report.to_json().to_string())
    }

    /// Единая точка постобработки успешных ответов API — сгенерированных инструментов,
    /// `api_request` и `secret_set`.
    ///
    /// `op` — операция инструмента или самая точная операция, с которой совпал запрос
    /// `api_request` (`None` — путь не совпал ни с одной операцией); `path` — фактический
    /// относительный путь запроса. Значения секретов и учётных данных вырезаются по `path`
    /// до превращения ответа в текст ([`crate::redact`]); `reveal_secret_values` (только при
    /// `ServerOptions::allow_secret_values`) оставляет значения секретов для одного вызова.
    ///
    /// Порядок: вырезание секретов и токена → фильтр `jq` → лимит размера. Фильтр получает
    /// только очищенный ответ, поэтому не может раскрыть вырезанные значения, в том числе
    /// преобразованием (`@base64`), которое не поймала бы итоговая `scrub_token`.
    async fn render(
        &self,
        client: &ApiClient,
        _op: Option<&OperationDef>,
        path: &str,
        response: &ApiResponse,
        reveal_secret_values: bool,
        jq: Option<&JqFilter>,
    ) -> Result<String, ToolError> {
        let mut response = response.clone();
        let redactions = redact_response(path, &mut response, reveal_secret_values);
        let mut text = match jq {
            None => response_text(&response),
            Some(filter) => self.filter(client, filter, response).await?,
        };
        if let Some(note) = redactions.note(self.inner.options.allow_secret_values) {
            text.push_str("\n\n");
            text.push_str(&note);
        }
        Ok(text)
    }

    /// Применяет фильтр к очищенному ответу и ограничивает результат лимитом ответа сервера.
    /// Ответ не в JSON возвращается без фильтра, с пояснением.
    async fn filter(
        &self,
        client: &ApiClient,
        filter: &JqFilter,
        response: ApiResponse,
    ) -> Result<String, ToolError> {
        let max = client.config().max_response_bytes;
        let status = response.status;
        let ApiResponse {
            body: ResponseBody::Json(mut value),
            ..
        } = response
        else {
            let mut text = body_text(&response);
            if truncate_text(&mut text, max) || response.truncated {
                text.push_str(TRUNCATED_NOTE);
            }
            text.push_str(if response.truncated {
                "\n\n[The `jq` filter was not applied: the response exceeds the size limit for \
                 filtering.]"
            } else {
                "\n\n[The `jq` filter was not applied: the response is not JSON.]"
            });
            return Ok(text);
        };
        scrub_token_json(client, &mut value);
        let output = self
            .inner
            .options
            .jq
            .run(filter, value, max)
            .await
            .map_err(|err| {
                ToolError::new(format!(
                    "the API request succeeded (HTTP {status}), but the `jq` filter failed: {err}. \
                     Fix the filter; repeating a request that changes data would apply the change \
                     again"
                ))
            })?;
        let mut text = output.text;
        if output.truncated {
            text.push_str(TRUNCATED_NOTE);
        }
        Ok(text)
    }
}

/// Выполняет запрос; ошибка API очищается от секретов по пути запроса.
///
/// С фильтром ответ читается до [`FILTER_INPUT_LIMIT`]: фильтру нужен JSON целиком,
/// а лимит ответа сервера применяется к результату фильтра в `SourceCraftServer::render`.
async fn execute(
    client: &ApiClient,
    request: ApiRequest,
    filtered: bool,
) -> Result<ApiResponse, ToolError> {
    let path = request.path.clone();
    let result = if filtered {
        let limit = FILTER_INPUT_LIMIT.max(client.config().max_response_bytes);
        client.execute_with_limit(request, limit).await
    } else {
        client.execute(request).await
    };
    result.map_err(|err| redact_api_error(&path, err).into())
}

/// Заменяет токен клиента во всех строках и ключах JSON до фильтра `jq`: итоговая
/// `scrub_token` ищет токен только в исходном виде, а фильтр мог бы его преобразовать.
fn scrub_token_json(client: &ApiClient, value: &mut Value) {
    match value {
        Value::String(text) => {
            let scrubbed = client.scrub_token(std::mem::take(text));
            *text = scrubbed;
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| scrub_token_json(client, item)),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                scrub_token_json(client, &mut item);
                map.insert(client.scrub_token(key), item);
            }
        }
        _ => {}
    }
}

impl ServerHandler for SourceCraftServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = if self.inner.options.dynamic_toolsets {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };
        ServerConfig::new(capabilities)
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
        Ok(ListToolsResult::with_all_items(self.registry().tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let before = self.session.generation();
        let result = self
            .call_with_extensions(&request.name, args, &context.extensions)
            .await;
        // В сессии с `initialize` уведомление уходит сразу; в протоколе 2026-07-28 клиент
        // получает его через подписку (`listen`), а не запрошенные уведомления отклоняет.
        if self.session.generation() != before
            && uses_legacy_session(&context)
            && let Err(err) = context.peer.notify_tool_list_changed().await
        {
            tracing::warn!(error = %err, "не удалось отправить tools/list_changed");
        }
        result.map(Into::into)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        // Подписка нужна только для смены списка инструментов в динамическом режиме.
        self.inner
            .options
            .dynamic_toolsets
            .then(|| requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        if context.accepted().tools_list_changed != Some(true) {
            context.cancelled().await;
            return Ok(());
        }
        let mut changes = self.session.changes.subscribe();
        changes.mark_unchanged();
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    changes.mark_unchanged();
                    if let Err(err) = context.sink().notify_tool_list_changed().await {
                        tracing::debug!(error = %err, "подписка на tools/list_changed закрыта");
                        return Ok(());
                    }
                }
            }
        }
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяет `build_request` с понятными агенту сообщениями;
        // проверку схемы на стороне rmcp не включаем.
        None
    }
}

/// Сессия с `initialize` (протокол до 2026-07-28): уведомления отправляются без подписки.
fn uses_legacy_session(context: &RequestContext<RoleServer>) -> bool {
    // Версии `YYYY-MM-DD` сравниваются лексикографически так же, как по времени.
    context
        .protocol_version()
        .is_none_or(|version| version.as_str() < ProtocolVersion::V_2026_07_28.as_str())
}

/// Инструкции сервера для агента (поле `instructions` в `initialize`).
pub fn server_instructions(options: &ServerOptions, files: &FilePolicy) -> String {
    let access = match (options.max_access, options.local_machine) {
        (AccessLevel::Read, true) => {
            "This server is read-only: only tools that read data are available. Tools that \
             create or change data are enabled when the user allows `write` access for the \
             server's profile, and tools that delete data with `destructive` access (the user \
             does it with `sourcecraft-mcp ui`, then restarts the client). If a task needs \
             them, ask the user instead of looking for workarounds."
        }
        (AccessLevel::Read, false) => {
            "This server is read-only: only tools that read data are available. Tools that \
             create or change data are enabled when the user starts the server with \
             --allow-write, and tools that delete data with --allow-destructive. If a task needs \
             them, ask the user instead of looking for workarounds."
        }
        (AccessLevel::Write, true) => {
            "This server can read, create and change data, but not delete it: deleting tools \
             (and making a repository public) need `destructive` access for the server's profile \
             (`sourcecraft-mcp ui`). Confirm changes with the user when they were not explicitly \
             requested."
        }
        (AccessLevel::Write, false) => {
            "This server can read, create and change data, but not delete it: deleting tools \
             (and making a repository public) need the user to start the server with \
             --allow-destructive. Confirm changes with the user when they were not explicitly \
             requested."
        }
        (AccessLevel::Destructive, _) => {
            "This server can read, change and delete data. Deleting is irreversible: confirm \
             with the user before calling destructive tools unless they explicitly asked for it."
        }
    };
    let names: Vec<&str> = options
        .toolsets
        .iter()
        .flatten()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .collect();
    let toolsets = if names.is_empty() {
        String::new()
    } else if options.dynamic_toolsets {
        format!(" Toolsets enabled at start: {}.", names.join(", "))
    } else {
        format!(" Only these toolsets are enabled: {}.", names.join(", "))
    };
    let dynamic = if options.dynamic_toolsets {
        "\n- Toolsets: tools are loaded on demand, by toolset. Call `list_toolsets` to see the \
         toolsets, then `enable_toolset` to load the tools you need (`disable_toolset` unloads \
         them); `api_search` shows the toolset of an operation. If enabled tools do not appear \
         in your tool list, call the same operations with `api_request`."
    } else {
        ""
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
         - Filtering: API tools and `api_request` accept an optional `jq` argument, a jq filter \
         (jaq dialect) applied to the JSON response, for example `.pull_requests[] | {{id, title}}` \
         or `[.items[] | select(.state == \"open\") | .id]`. Use it to keep large responses \
         small. Secrets are redacted before the filter runs; the size limit applies to the \
         filtered result. Several outputs are returned as a JSON array, no output as `[]`. \
         Filters cannot read the environment, files or other inputs. A broken filter is \
         reported before the request is sent; a filter that fails at runtime (after the \
         request succeeded) returns an error that says so.\n\
         - Finding operations: `api_search` searches all API operations, including tools not \
         enabled here, and tells which flag enables them. `api_request` calls any relative API \
         path within the server's access level (toolsets do not limit it); prefer the dedicated \
         tool when one exists.{dynamic}\n\
         {machine}",
        level = access_name(options.max_access),
        machine = if options.local_machine {
            local_instructions(options, files)
        } else {
            remote_instructions(options)
        },
    )
}

/// Часть инструкций сервера на машине агента (stdio): локальные файлы, токен из хранилища,
/// CLI `sourcecraft-mcp secret`.
fn local_instructions(options: &ServerOptions, files: &FilePolicy) -> String {
    format!(
        "- Local files: file arguments take a path (absolute, or relative to the server's \
         working directory) inside the allowed directories: {roots}. Other paths are rejected; \
         the user can allow more directories for the server's profile with `sourcecraft-mcp ui`.\n\
         - Secrets: never ask the user to paste tokens or secret values into the conversation. \
         The API token is managed by the server and is never available to tools; if the API \
         rejects it (HTTP 401), ask the user to store a new one with `{exe} auth login \
         --profile {profile}`. {secrets}\n\
         - Secrets in shell commands: {shell}",
        roots = files.describe_roots(),
        exe = executable(options),
        profile = profile(options),
        secrets = secrets_instructions(options),
        shell = shell_secrets_instructions(options),
    )
}

/// Часть инструкций удалённого сервера (HTTP): файлов агента нет, токен приходит от
/// MCP-клиента в каждом запросе, CLI на машине агента может не быть.
fn remote_instructions(options: &ServerOptions) -> String {
    format!(
        "- Remote server: this server runs on another machine (HTTP mode) and cannot read or \
         write files on your machine, so there are no file arguments and no file upload tools. \
         To attach a file, ask the user to upload it in SourceCraft.\n\
         - Token: the MCP client sends the user's SourceCraft token with every request (the \
         `Authorization` header from its configuration); the server does not store it. Never \
         read the token from the client configuration, print it, or pass it to tools, commands \
         or files. Its roles in SourceCraft are the real limit of what this server can do. If \
         the API rejects it (HTTP 401), ask the user to replace the token in the MCP client \
         configuration.\n\
         - Secrets: never ask the user to paste tokens or secret values into the conversation. \
         {secrets}",
        secrets = secrets_instructions(options),
    )
}

/// Бинарник для примеров команд: путь из настроек или `sourcecraft-mcp` из `PATH`.
fn executable(options: &ServerOptions) -> String {
    options
        .executable
        .as_deref()
        .map(shell_quote)
        .unwrap_or_else(|| "sourcecraft-mcp".to_owned())
}

/// Профиль для примеров команд: профиль сервера или заглушка `<profile>`.
fn profile(options: &ServerOptions) -> String {
    options
        .profile
        .as_deref()
        .map(shell_quote)
        .unwrap_or_else(|| "<profile>".to_owned())
}

/// Часть инструкций об использовании секретов в командах оболочки через CLI
/// `sourcecraft-mcp secret`: значение попадает в окружение одной команды, а не в контекст агента.
fn shell_secrets_instructions(options: &ServerOptions) -> String {
    let exe = executable(options);
    let profile = profile(options);
    let target = format!("--profile {profile} --repo <org>/<repo>");
    // Сервер в stdio-режиме работает на машине агента, поэтому платформа сборки — это
    // платформа, где агент запускает команды. Примеры ниже написаны для POSIX-оболочки
    // (Git Bash у Claude Code на Windows их понимает); для PowerShell — отдельная подсказка.
    let windows = if cfg!(windows) {
        " On Windows: the examples are for a POSIX shell such as Git Bash. In PowerShell run \
         the quoted path with `&` (`& <path> secret ...`) and, instead of `sh -c`, use \
         `-- powershell -NoProfile -Command '... $env:NAME ...'`. `secret write` cannot set a \
         file mode there: the file inherits the permissions of its folder, so write it under \
         your user profile, not into a shared folder."
    } else {
        ""
    };
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
         when the user explicitly asks to see the value.{windows}"
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
    let set = match (
        options.max_access.allows(AccessLevel::Destructive),
        options.local_machine,
    ) {
        (true, true) => {
            "To set a secret use `secret_set`: it takes plain text and base64-encodes it itself. \
             Prefer its `value_file` argument (a local file with the value, for example one the \
             user prepared), so the value never enters the conversation."
        }
        (true, false) => {
            "To set a secret use `secret_set`: it takes plain text and base64-encodes it itself. \
             The value passes through the conversation, so set a secret only with a value the \
             user gave you for this purpose; otherwise ask the user to set it in SourceCraft."
        }
        (false, true) => {
            "Setting secrets (`secret_set`) needs `destructive` access for the server's profile \
             (`sourcecraft-mcp ui`)."
        }
        (false, false) => {
            "Setting secrets (`secret_set`) needs the server to run with --allow-destructive."
        }
    };
    let usage = if options.local_machine {
        "To use a secret, reference it by key (for example in CI configuration) or use the CLI \
         described below instead of reading its value"
    } else {
        "To use a secret, reference it by key (for example in CI configuration) instead of \
         reading its value"
    };
    let copy = if options.max_access.allows(AccessLevel::Write) {
        " To copy a secret to another repository use `secret_copy`: the server copies the \
         value without showing it."
    } else {
        ""
    };
    format!(
        "Repository secret values (used by CI) and fields that look like credentials \
         (passwords, tokens, private keys) are replaced with \"[redacted]\" in every response, \
         including errors; a note says when that happened. {reveal} {set}{copy} {usage}; do not \
         print secret values in commands, logs, files or messages."
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
        assert!(read.contains("`write` access"));
        assert!(read.contains("`destructive` access"));
        assert!(read.contains("allowed directories: none"));
        assert!(read.contains("`api_search`") && read.contains("`api_request`"));
        assert!(read.contains("optional `jq` argument"));

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
    fn instructions_explain_dynamic_toolsets() {
        let files = FilePolicy::new(&[]).unwrap();
        let plain = server_instructions(&ServerOptions::default(), &files);
        assert!(!plain.contains("enable_toolset"));

        let dynamic = server_instructions(
            &ServerOptions {
                dynamic_toolsets: true,
                toolsets: Some(vec!["issues".into()]),
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(
            dynamic.contains("Toolsets enabled at start: issues."),
            "{dynamic}"
        );
        assert!(dynamic.contains("`list_toolsets`"), "{dynamic}");
        assert!(dynamic.contains("`enable_toolset`"), "{dynamic}");
        assert!(!dynamic.contains("Only these toolsets"), "{dynamic}");
    }

    #[test]
    fn instructions_explain_secrets() {
        let files = FilePolicy::new(&[]).unwrap();
        let read = server_instructions(&ServerOptions::default(), &files);
        assert!(read.contains("[redacted]"), "{read}");
        assert!(read.contains("does not reveal"), "{read}");
        assert!(read.contains("`secret_set`) needs `destructive` access for the server's profile"));
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
    fn remote_instructions_have_no_local_machine_features() {
        let remote = ServerOptions {
            max_access: AccessLevel::Destructive,
            local_machine: false,
            // Даже если профиль и бинарник заданы, удалённые инструкции их не используют.
            profile: Some("codex".into()),
            executable: Some("/usr/bin/sourcecraft-mcp".into()),
            ..ServerOptions::default()
        };
        let text = server_instructions(&remote, &FilePolicy::disabled());
        let read_only = server_instructions(
            &ServerOptions {
                max_access: AccessLevel::Read,
                ..remote.clone()
            },
            &FilePolicy::disabled(),
        );
        assert!(read_only.contains("--allow-write"), "{read_only}");
        assert!(!read_only.contains("sourcecraft-mcp ui"), "{read_only}");
        for absent in [
            "secret exec",
            "secret write",
            "secret get",
            "auth login",
            "--allow-path",
            "value_file",
            "allowed directories",
            "CLI",
        ] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        assert!(text.contains("HTTP mode"), "{text}");
        assert!(text.contains("`Authorization` header"), "{text}");
        assert!(text.contains("`secret_set`"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");

        let local = server_instructions(&ServerOptions::default(), &FilePolicy::new(&[]).unwrap());
        assert!(local.contains("auth login --profile <profile>"), "{local}");
        assert!(!local.contains("HTTP mode"), "{local}");
        assert!(local.contains("sourcecraft-mcp ui"), "{local}");
        assert!(!local.contains("--allow-write"), "{local}");
        assert!(!local.contains("--allow-path"), "{local}");
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
