//! `sourcecraft-mcp serve` — запуск MCP-сервера через stdio или HTTP (`--http`).
//!
//! stdio: stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Токен берётся только из системного хранилища по профилю. Права (уровень доступа и
//! каталоги `allow_path`) — из настроек профиля; флаги командной строки их только сужают, а
//! на macOS права сверх чтения ещё и должны быть одобрены владельцем ([`mcp_kit::approval`]).
//! Сборка сервера ([`Setup`]) общая со `call` и `ui`.
//!
//! HTTP ([`http_server`]): профилей и хранилища нет, токен приходит в заголовке
//! `Authorization` каждого запроса агента и нигде не сохраняется. Локальные файлы выключены.
//! Уровень доступа задают флаги `--allow-write` и `--allow-destructive`, как и раньше:
//! одобрение на macOS защищает токен из хранилища этой машины от агента, который правит
//! регистрацию или `profiles.toml`, а у HTTP-сервера своего токена нет — токен приносит сам
//! клиент, и с ним можно обратиться к API и в обход сервера.

mod http_server;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::Args;
use mcp_kit::approval::{self, ApprovalState, ApprovalStore, Resolved};
use mcp_kit::{AccessRequest, ProfileAccess};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use sourcecraft_api::AccessLevel;
use sourcecraft_api::auth::{KeyringStore, TokenStore};
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_api::keychain::SystemKeychain;
use sourcecraft_mcp_core::{
    FilePolicy, JqRunner, ServerOptions, SourceCraftServer, normalize_toolsets,
};
use url::Url;

use crate::base_url::{BASE_URL_ENV, resolve_base_url};
use crate::cli::{jq_worker, tools};
use crate::clients::EXE_STEM;
use crate::profiles::{
    ProfileRepo, ProfileSettings, access_level, explain_load_error, validate_profile_name,
};

pub use crate::profiles::DEFAULT_PROFILE;

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит тела ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug, Args)]
#[command(after_help = "\
Access: the access level and the extra directories for local files (allow_path) are settings \
of the profile; change them with `sourcecraft-mcp ui`. --allow-write, --allow-destructive and \
--allow-path can only narrow them: asking for more than the profile allows is an error. On \
macOS, access above read-only takes effect only after the owner approved it on this Mac \
(`sourcecraft-mcp ui` asks for Touch ID or the macOS password); without the approval the server \
runs read-only and says so in stderr. --allow-destructive implies --allow-write. Without any \
settings the server is read-only.

Toolsets: flags given on the command line win; without them the settings saved for the profile \
are used, otherwise all toolsets (see `sourcecraft-mcp toolsets`).

Dynamic toolsets: with --dynamic-toolsets the server starts with only `api_search`, \
`api_request`, `list_toolsets`, `enable_toolset` and `disable_toolset` (plus the toolsets given \
with --toolsets and the custom tools, which belong to no toolset); the agent enables toolsets on demand and the client is notified that the tool \
list changed. Use it with clients that load all tool descriptions up front and reload the tool \
list on `notifications/tools/list_changed`. Not available in HTTP mode, which keeps no sessions.

Secrets: repository secret values and fields that look like credentials are replaced with \
\"[redacted]\" in every tool response. --allow-secret-values (or the profile setting) lets a \
call ask for secret values explicitly.

Custom tools: tools defined in ~/.config/sourcecraft-mcp/tools.toml are registered when the file exists; files from other places (for example, a project repository) only with --tools-file. Validate them with `sourcecraft-mcp tools check`.

Keychain: `secret_copy` copies repository secrets only. --allow-keychain-source also lets it \
read items of this machine's system keychain (`keychain://<service>/<account>`) and upload them \
to a repository; items with sourcecraft-mcp tokens are never readable. Not available in HTTP mode.

Local files: file arguments of tools must be inside the server's working directory (unless it is \
the filesystem root or the home directory), the system temp directory, or an extra directory of \
the profile (allow_path).

In stdio mode the token is read only from the system keychain; store it with \
`sourcecraft-mcp auth login` or `sourcecraft-mcp connect`.

HTTP mode (--http): the server listens on --listen (default 127.0.0.1:8765) and serves MCP at \
/mcp (Streamable HTTP, stateless) and a health check at /health. There are no profiles and no \
stored tokens: every request must carry the agent's SourceCraft token in an \
`Authorization: Bearer <token>` header, which the server forwards to the SourceCraft API only. \
Access level, toolsets and secret flags come from the command line only: --allow-write and \
--allow-destructive enable write and delete tools, and there is no approval on macOS (the \
server keeps no token of its own; the token's roles limit the access). Local files are \
disabled. Browser requests are rejected unless their Origin is allowed with --allowed-origin; \
the Host header must be a loopback name, the --listen address or an --allowed-host. Outside \
loopback, put the server behind a TLS reverse proxy.")]
pub struct ServeArgs {
    /// Token profile stored in the system keychain [default: default].
    #[arg(long, conflicts_with = "http")]
    pub profile: Option<String>,
    /// Limit the access to write (the profile must allow it). With --http: register tools
    /// that create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Limit the access to destructive (the profile must allow it; implies --allow-write).
    /// With --http: register tools that delete data.
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (see `sourcecraft-mcp toolsets`). With
    /// --dynamic-toolsets: the toolsets enabled at start.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Load toolsets on demand: start with only the search, request and toolset tools, and let
    /// the agent enable toolsets with `enable_toolset`. Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub dynamic_toolsets: bool,
    /// Let tools show repository secret values when a call explicitly asks for them
    /// (`reveal_values: true`). Without it, secret values are always hidden.
    #[arg(long)]
    pub allow_secret_values: bool,
    /// Let `secret_copy` read items of the system keychain (`keychain://<service>/<account>`)
    /// and upload them to repository secrets. Off by default: it moves local data to SourceCraft.
    /// Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub allow_keychain_source: bool,
    /// Limit the extra directories of the profile to this one (it must be inside a directory
    /// of the profile). Repeatable.
    #[arg(long, value_name = "DIR", conflicts_with = "http")]
    pub allow_path: Vec<PathBuf>,
    /// SourceCraft API URL [default: $SOURCECRAFT_BASE_URL or https://api.sourcecraft.tech].
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of an API response returned to the agent; larger responses are truncated.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    /// Additional custom tools file (same format as ~/.config/sourcecraft-mcp/tools.toml).
    /// Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Serve MCP over HTTP instead of stdio; each request carries the agent's token.
    #[arg(long)]
    pub http: bool,
    /// Address and port to listen on in HTTP mode [default: 127.0.0.1:8765].
    #[arg(long, value_name = "IP:PORT", requires = "http")]
    pub listen: Option<SocketAddr>,
    /// Extra allowed Host header value (name or name:port) in HTTP mode. Repeatable.
    #[arg(long, value_name = "HOST", requires = "http")]
    pub allowed_host: Vec<String>,
    /// Browser Origin allowed in HTTP mode (e.g. http://localhost:6274). Repeatable.
    /// Requests without an Origin header are always allowed.
    #[arg(long, value_name = "ORIGIN", requires = "http")]
    pub allowed_origin: Vec<String>,
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    if args.http {
        // Сначала проверяем аргументы, чтобы не начинать работу зря.
        let (base_url, cli_toolsets) = check_args(&args)?;
        return run_http(&args, base_url, cli_toolsets).await;
    }
    let repo = ProfileRepo::open_default()?;
    let approvals = approval::platform_store(EXE_STEM);
    let setup = Setup::load(&args, &KeyringStore, &repo, approvals.as_deref())?;
    if let Some(warning) = &setup.warning {
        eprintln!("{EXE_STEM}: {warning}");
    }
    tracing::info!(
        profile = %setup.options.profile.as_deref().unwrap_or_default(),
        access = ?setup.options.max_access,
        toolsets = ?setup.options.toolsets,
        dynamic_toolsets = setup.options.dynamic_toolsets,
        allow_secret_values = setup.options.allow_secret_values,
        allow_keychain_source = setup.options.keychain_source.is_some(),
        roots = %setup.files.describe_roots(),
        custom_tools = setup.options.custom_tools.len(),
        "запуск MCP-сервера SourceCraft"
    );
    let server = setup.server()?;
    let running = server
        .serve(stdio())
        .await
        .context("MCP initialization over stdio failed")?;
    running.waiting().await.context("MCP server task failed")?;
    Ok(())
}

/// Проверка аргументов, общих для stdio и HTTP, до обращения к профилю и keychain.
/// Возвращает адрес API и наборы из `--toolsets`.
fn check_args(args: &ServeArgs) -> anyhow::Result<(Url, Vec<String>)> {
    if args.timeout == 0 {
        bail!("--timeout must be greater than 0");
    }
    if args.max_response_bytes == 0 {
        bail!("--max-response-bytes must be greater than 0");
    }
    let from_env = std::env::var(BASE_URL_ENV).ok();
    let base_url = resolve_base_url(args.base_url.as_deref(), from_env.as_deref())?;
    let cli_toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    Ok((base_url, cli_toolsets))
}

/// Всё, из чего собирается stdio-сервер профиля: настройки ядра, политика файлов, клиент API
/// с токеном. Общая сборка для `serve`, `call` и `ui`.
pub struct Setup {
    pub options: ServerOptions,
    pub files: FilePolicy,
    pub config: ClientConfig,
    /// Предупреждение для stderr: права профиля урезаны до одобренных (macOS).
    pub warning: Option<String>,
}

impl Setup {
    /// Читает профиль из `repo`, одобренные права — из `approvals`, токен — из `store` и
    /// собирает настройки сервера. Токен читается последним: при ошибке в аргументах,
    /// правах или файлах инструментов keychain не трогается.
    pub fn load(
        args: &ServeArgs,
        store: &dyn TokenStore,
        repo: &ProfileRepo,
        approvals: Option<&dyn ApprovalStore>,
    ) -> anyhow::Result<Self> {
        if args.http {
            bail!("HTTP mode has no profiles");
        }
        let (base_url, cli_toolsets) = check_args(args)?;
        let profile = args.profile();
        validate_profile_name(profile)?;
        let settings = repo
            .get(profile)
            .with_context(|| format!("cannot load profile `{profile}`"))?
            .unwrap_or_default();
        let resolved = resolve_access(args, &settings, approvals)?;
        let files =
            FilePolicy::with_defaults(&resolved.access.allow_path).map_err(|err| anyhow!(err))?;
        let mut options = effective_options(args, cli_toolsets, &settings, &resolved.access)?;
        options.custom_tools =
            tools::load_with(Some(&tools::user_tools_file(repo)), &args.tools_file)?;
        let token = store
            .get(profile)
            .map_err(|err| explain_load_error(err.into(), profile))?;
        let config = ClientConfig {
            base_url,
            token: Some(token),
            timeout: Duration::from_secs(args.timeout),
            max_response_bytes: args.max_response_bytes,
        };
        Ok(Self {
            options,
            files,
            config,
            warning: resolved.verified.warning(EXE_STEM, profile),
        })
    }

    /// Сервер MCP с этими настройками.
    pub fn server(self) -> anyhow::Result<SourceCraftServer> {
        let client = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the SourceCraft API client: {err}"))?;
        SourceCraftServer::new(client, self.options, self.files).map_err(|err| anyhow!(err))
    }
}

impl ServeArgs {
    /// Профиль stdio-сервера.
    pub fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(DEFAULT_PROFILE)
    }

    /// Что просят флаги доступа.
    fn access_request(&self) -> AccessRequest {
        AccessRequest::from_flags(
            self.allow_write,
            self.allow_destructive,
            self.allow_path.clone(),
        )
    }

    /// Аргументы stdio-сервера профиля без флагов: права профиля, все наборы по умолчанию.
    /// Основа для `call` и `ui`.
    pub fn for_profile(profile: &str) -> Self {
        Self {
            profile: Some(profile.to_owned()),
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            dynamic_toolsets: false,
            allow_secret_values: false,
            allow_keychain_source: false,
            allow_path: Vec::new(),
            base_url: None,
            timeout: DEFAULT_TIMEOUT_SECS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            tools_file: Vec::new(),
            http: false,
            listen: None,
            allowed_host: Vec::new(),
            allowed_origin: Vec::new(),
        }
    }
}

/// Права stdio-сервера: права профиля, урезанные до одобренных (macOS), затем суженные
/// флагами. Флаг сверх прав профиля — ошибка с подсказкой, где меняются права.
pub fn resolve_access(
    args: &ServeArgs,
    settings: &ProfileSettings,
    approvals: Option<&dyn ApprovalStore>,
) -> anyhow::Result<Resolved> {
    let profile = args.profile();
    approval::resolve_with(
        approvals,
        profile,
        &settings.access(),
        &args.access_request(),
    )
    .map_err(|err| {
        anyhow!(
            "{err}. The access of a profile is changed with `{EXE_STEM} ui` (profile `{profile}`); \
             registrations made by older versions with access flags are updated by \
             `{EXE_STEM} connect`"
        )
    })
}

/// Уровень доступа ядра по уровню из `mcp-kit`.
pub fn api_level(level: mcp_kit::AccessLevel) -> AccessLevel {
    match level {
        mcp_kit::AccessLevel::Read => AccessLevel::Read,
        mcp_kit::AccessLevel::Write => AccessLevel::Write,
        mcp_kit::AccessLevel::Destructive => AccessLevel::Destructive,
    }
}

/// Состояние одобрения прав для `auth status`.
pub fn approval_text(state: &ApprovalState) -> String {
    match state {
        ApprovalState::NotRequired => {
            "not used on this platform (the token's roles limit the access)".to_owned()
        }
        ApprovalState::ReadOnly => "not needed (read-only)".to_owned(),
        ApprovalState::Approved => "approved on this Mac".to_owned(),
        ApprovalState::Missing => "not approved on this Mac".to_owned(),
        ApprovalState::Exceeds { approved } => {
            format!("only `{}` is approved on this Mac", approved.summary())
        }
        ApprovalState::Unavailable(reason) => format!("cannot read the keychain: {reason}"),
    }
}

/// `serve --http`: без профиля и хранилища; токен — из каждого HTTP-запроса агента.
async fn run_http(
    args: &ServeArgs,
    base_url: Url,
    cli_toolsets: Vec<String>,
) -> anyhow::Result<()> {
    let mut options = http_options(args, cli_toolsets);
    // Файлы инструментов читает оператор при запуске; агенты по HTTP их не передают,
    // а сами инструменты не работают с локальными файлами (политика файлов отключена).
    options.custom_tools = tools::load(&args.tools_file)?;
    let listen = match args.listen {
        Some(listen) => listen,
        None => http_server::DEFAULT_LISTEN
            .parse()
            .context("invalid default listen address")?,
    };
    let http = http_server::HttpOptions {
        allowed_hosts: http_server::allowed_hosts(listen, &args.allowed_host),
        allowed_origins: args.allowed_origin.clone(),
    };
    tracing::info!(
        access = ?options.max_access,
        toolsets = ?options.toolsets,
        allow_secret_values = options.allow_secret_values,
        base_url = %base_url,
        "настройки MCP-сервера SourceCraft (HTTP)"
    );
    // У общего клиента нет токена: на каждый вызов ядро делает копию с токеном запроса.
    let client = ApiClient::new(ClientConfig {
        base_url,
        token: None,
        timeout: Duration::from_secs(args.timeout),
        max_response_bytes: args.max_response_bytes,
    })
    .map_err(|err| anyhow!("cannot create the SourceCraft API client: {err}"))?;
    let server = SourceCraftServer::with_token_source(
        client,
        options,
        FilePolicy::disabled(),
        http_server::token_source(),
    )
    .map_err(|err| anyhow!(err))?;
    http_server::run(server, listen, http).await
}

/// Настройки ядра в HTTP-режиме: только флаги командной строки (профилей нет),
/// без локальных файлов и CLI в инструкциях.
fn http_options(args: &ServeArgs, cli_toolsets: Vec<String>) -> ServerOptions {
    ServerOptions {
        max_access: access_level(args.allow_write, args.allow_destructive),
        toolsets: (!cli_toolsets.is_empty()).then_some(cli_toolsets),
        // HTTP-режим без сессий: включённые наборы негде хранить между запросами
        // (флаг `--dynamic-toolsets` с `--http` отклоняет clap).
        dynamic_toolsets: false,
        allow_secret_values: args.allow_secret_values,
        profile: None,
        executable: None,
        // Фильтры `jq` и здесь выполняются в отдельном процессе: это не локальные файлы
        // агента, а защита сервера от зависших фильтров.
        jq: jq_runner(std::env::current_exe().ok().as_deref()),
        // Заполняет `run_http` из файлов инструментов.
        custom_tools: Vec::new(),
        local_machine: false,
        // Хранилище машины HTTP-сервера агенту не принадлежит (флаг с `--http` отклоняет clap).
        keychain_source: None,
    }
}

/// Настройки ядра stdio-сервера: уровень доступа — уже разрешённые права `access`, наборы —
/// флаги командной строки, если заданы, иначе настройки профиля.
fn effective_options(
    args: &ServeArgs,
    cli_toolsets: Vec<String>,
    settings: &ProfileSettings,
    access: &ProfileAccess,
) -> anyhow::Result<ServerOptions> {
    let max_access = api_level(access.level);
    let toolsets = if cli_toolsets.is_empty() {
        normalize_toolsets(&settings.toolsets).map_err(|err| anyhow!("profile settings: {err}"))?
    } else {
        cli_toolsets
    };
    let executable = std::env::current_exe().ok();
    Ok(ServerOptions {
        max_access,
        toolsets: (!toolsets.is_empty()).then_some(toolsets),
        // Как `allow_secret_values`: флаг включает, без него — настройка профиля.
        dynamic_toolsets: args.dynamic_toolsets || settings.dynamic_toolsets,
        // Флаг командной строки включает, без него — настройка профиля. В отличие от уровня
        // доступа одобрением на macOS не проверяется (см. `docs/security.md`).
        allow_secret_values: args.allow_secret_values || settings.allow_secret_values,
        // Профиль и путь к бинарнику попадают в примеры `sourcecraft-mcp secret …`
        // в инструкциях: агент вызывает CLI с тем же профилем, что и сервер.
        profile: Some(args.profile().to_owned()),
        executable: executable.as_ref().map(|path| path.display().to_string()),
        jq: jq_runner(executable.as_deref()),
        // Заполняет `run` из файлов инструментов.
        custom_tools: Vec::new(),
        local_machine: true,
        // Только по явному флагу и только здесь, в локальном stdio-сервере: агент получает
        // путь от записей хранилища этой машины к удалённому репозиторию.
        keychain_source: args
            .allow_keychain_source
            .then(|| Arc::new(SystemKeychain) as Arc<dyn sourcecraft_api::keychain::Keychain>),
    })
}

/// Фильтры `jq` выполняются в дочернем процессе этого же бинарника: зависший или
/// переполнивший стек фильтр не роняет сервер. Без пути к бинарнику — в потоке сервера.
fn jq_runner(executable: Option<&Path>) -> JqRunner {
    match executable {
        Some(path) => JqRunner::subprocess(path, [jq_worker::SUBCOMMAND]),
        None => {
            tracing::warn!(
                "не удалось определить путь к бинарнику: фильтры jq выполняются в процессе сервера"
            );
            JqRunner::in_process()
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use sourcecraft_api::auth::MemoryStore;
    use sourcecraft_api::{AccessLevel, ApiToken};

    use super::*;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).map(|w| w.serve)
    }

    fn parse(args: &[&str]) -> ServeArgs {
        try_parse(args).unwrap()
    }

    fn profile(write: bool, destructive: bool, toolsets: &[&str]) -> ProfileSettings {
        ProfileSettings {
            client: None,
            allow_write: write,
            allow_destructive: destructive,
            allow_path: Vec::new(),
            toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            allow_secret_values: false,
            dynamic_toolsets: false,
        }
    }

    fn try_options(args: &[&str], settings: &ProfileSettings) -> anyhow::Result<ServerOptions> {
        let args = parse(args);
        let cli = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
        let resolved = resolve_access(&args, settings, None)?;
        effective_options(&args, cli, settings, &resolved.access)
    }

    fn options(args: &[&str], settings: &ProfileSettings) -> ServerOptions {
        try_options(args, settings).unwrap()
    }

    /// Профиль `ci` во временном каталоге и токен в памяти: настоящий keychain не трогаем.
    fn fixture(settings: ProfileSettings) -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        repo.upsert("ci", settings).unwrap();
        let store = MemoryStore::new();
        store.set("ci", &ApiToken::new("test-token")).unwrap();
        (dir, repo, store)
    }

    #[test]
    fn defaults() {
        let args = parse(&[]);
        assert_eq!(args.profile, None);
        assert!(!args.http);
        assert_eq!(args.listen, None);
        assert_eq!(args.timeout, 60);
        assert_eq!(args.max_response_bytes, 256 * 1024);
        assert!(args.allow_path.is_empty());
        let opts = options(&[], &ProfileSettings::default());
        assert_eq!(opts.max_access, AccessLevel::Read);
        assert_eq!(opts.toolsets, None);
        assert!(!opts.allow_secret_values);
        assert_eq!(opts.profile.as_deref(), Some("default"));
        assert!(opts.local_machine);
    }

    #[test]
    fn http_flags() {
        let args = parse(&[
            "--http",
            "--listen",
            "0.0.0.0:9000",
            "--allowed-host",
            "mcp.example.com",
            "--allowed-origin",
            "http://localhost:6274",
            "--allow-write",
            "--toolsets",
            "issues",
        ]);
        assert!(args.http);
        assert_eq!(args.listen, Some("0.0.0.0:9000".parse().unwrap()));
        let opts = http_options(&args, normalize_toolsets(&args.toolsets).unwrap());
        assert_eq!(opts.max_access, AccessLevel::Write);
        assert_eq!(opts.toolsets, Some(vec!["issues".to_owned()]));
        assert!(!opts.local_machine);
        assert_eq!(opts.profile, None);
        assert_eq!(opts.executable, None);
    }

    #[test]
    fn http_rejects_local_only_flags() {
        for extra in [
            ["--profile", "codex"],
            ["--allow-path", "/tmp"],
            ["--allow-keychain-source", "--allow-write"],
        ] {
            let mut argv = vec!["--http"];
            argv.extend_from_slice(&extra);
            let err = try_parse(&argv).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{err}"
            );
            assert!(err.to_string().contains(extra[0]), "{err}");
        }
        let err = try_parse(&["--http", "--dynamic-toolsets"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{err}"
        );
        // Флаги HTTP без --http — ошибка, а не молчаливое игнорирование.
        for flag in [
            ["--listen", "127.0.0.1:1"],
            ["--allowed-origin", "http://x"],
            ["--allowed-host", "x"],
        ] {
            let err = try_parse(&flag).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{err}"
            );
        }
        assert!(try_parse(&["--listen", "localhost:80", "--http"]).is_err());
    }

    #[test]
    fn secret_values_flag() {
        assert!(
            options(&["--allow-secret-values"], &ProfileSettings::default()).allow_secret_values
        );
        let saved = ProfileSettings {
            allow_secret_values: true,
            ..ProfileSettings::default()
        };
        assert!(options(&[], &saved).allow_secret_values);
    }

    #[test]
    fn flags_narrow_access_and_win_for_toolsets() {
        let saved = profile(true, true, &["issues"]);
        let opts = options(&[], &saved);
        assert_eq!(opts.max_access, AccessLevel::Destructive);
        assert_eq!(opts.toolsets, Some(vec!["issues".to_owned()]));

        let opts = options(&["--allow-write", "--toolsets", "repository,ci-cd"], &saved);
        assert_eq!(opts.max_access, AccessLevel::Write);
        assert_eq!(
            opts.toolsets,
            Some(vec!["repository".to_owned(), "ci-cd".to_owned()])
        );

        // Флаги не расширяют права профиля.
        let err = try_options(&["--allow-destructive"], &profile(false, false, &[])).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("profile allows only `read`"), "{text}");
        assert!(
            text.contains("`sourcecraft-mcp ui` (profile `default`)"),
            "{text}"
        );
        assert!(try_options(&["--allow-destructive"], &profile(true, false, &[])).is_err());
        // Старая регистрация с флагами в пределах профиля запускается.
        let opts = options(&["--allow-write", "--allow-destructive"], &saved);
        assert_eq!(opts.max_access, AccessLevel::Destructive);
    }

    #[test]
    fn allow_path_comes_from_profile_and_flags_narrow_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let inner = root.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let saved = ProfileSettings {
            allow_path: vec![root.clone()],
            ..ProfileSettings::default()
        };
        let resolved = resolve_access(&parse(&[]), &saved, None).unwrap();
        assert_eq!(resolved.access.allow_path, vec![root.clone()]);
        let args = parse(&["--allow-path", inner.to_str().unwrap()]);
        let resolved = resolve_access(&args, &saved, None).unwrap();
        assert_eq!(resolved.access.allow_path, vec![inner]);
        let other = tempfile::tempdir().unwrap();
        let args = parse(&["--allow-path", other.path().to_str().unwrap()]);
        let err = resolve_access(&args, &saved, None).unwrap_err();
        assert!(
            err.to_string().contains("not inside a directory allowed"),
            "{err}"
        );
        // Без каталогов в профиле `--allow-path` ничего не добавляет: ошибка.
        let args = parse(&["--allow-path", root.to_str().unwrap()]);
        assert!(resolve_access(&args, &ProfileSettings::default(), None).is_err());

        // Каталоги профиля попадают в политику файлов сервера.
        let (_cfg, repo, store) = fixture(saved);
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert!(
            setup
                .files
                .describe_roots()
                .contains(&root.display().to_string())
        );
    }

    #[test]
    fn unapproved_access_runs_read_only_with_a_warning() {
        use mcp_kit::approval::testing::MemoryStore as MemoryApprovals;

        let (_dir, repo, store) = fixture(profile(true, true, &[]));
        let approvals = MemoryApprovals::default();
        let load = |args: &[&str]| {
            Setup::load(
                &parse(args),
                &store,
                &repo,
                Some(&approvals as &dyn ApprovalStore),
            )
            .unwrap()
        };
        let setup = load(&["--profile", "ci"]);
        assert_eq!(setup.options.max_access, AccessLevel::Read);
        let warning = setup.warning.unwrap();
        assert!(warning.contains("not approved on this Mac"), "{warning}");
        assert!(warning.contains("sourcecraft-mcp ui"), "{warning}");
        // Старая регистрация с флагами в пределах профиля запускается, но на чтение.
        let setup = load(&["--profile", "ci", "--allow-destructive"]);
        assert_eq!(setup.options.max_access, AccessLevel::Read);

        approvals
            .save(
                "ci",
                &ProfileAccess::new(mcp_kit::AccessLevel::Write, vec![]),
            )
            .unwrap();
        let setup = load(&["--profile", "ci"]);
        assert_eq!(setup.options.max_access, AccessLevel::Write);
        assert!(setup.warning.unwrap().contains("only `write` is approved"));

        approvals
            .save(
                "ci",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![]),
            )
            .unwrap();
        let setup = load(&["--profile", "ci"]);
        assert_eq!(setup.options.max_access, AccessLevel::Destructive);
        assert!(setup.warning.is_none());
    }

    #[test]
    fn setup_reads_the_token_last_and_explains_a_missing_one() {
        let (_dir, repo, _store) = fixture(profile(false, false, &[]));
        let empty = MemoryStore::new();
        let err = Setup::load(&parse(&["--profile", "ci"]), &empty, &repo, None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("auth login --profile ci"), "{err}");
        // Ошибка прав — до обращения к хранилищу токенов.
        let err = Setup::load(
            &parse(&["--profile", "ci", "--allow-write"]),
            &empty,
            &repo,
            None,
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("profile allows only"), "{err}");
        let err = Setup::load(&parse(&["--profile=a/b"]), &empty, &repo, None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("invalid profile name"), "{err}");

        let (_dir, repo, store) = fixture(profile(true, false, &["issues"]));
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Write);
        assert_eq!(setup.options.profile.as_deref(), Some("ci"));
        assert_eq!(setup.config.token.as_ref().unwrap().expose(), "test-token");
        assert!(setup.warning.is_none());
        assert!(setup.server().is_ok());
    }

    #[test]
    fn approval_texts() {
        assert!(approval_text(&ApprovalState::NotRequired).contains("token's roles"));
        assert_eq!(
            approval_text(&ApprovalState::Exceeds {
                approved: ProfileAccess::new(mcp_kit::AccessLevel::Write, vec![])
            }),
            "only `write` is approved on this Mac"
        );
    }

    #[test]
    fn dynamic_toolsets_flag() {
        assert!(!options(&[], &ProfileSettings::default()).dynamic_toolsets);
        let opts = options(
            &["--dynamic-toolsets", "--toolsets", "issues"],
            &ProfileSettings::default(),
        );
        assert!(opts.dynamic_toolsets);
        assert_eq!(opts.toolsets, Some(vec!["issues".to_owned()]));
        let saved = ProfileSettings {
            dynamic_toolsets: true,
            ..ProfileSettings::default()
        };
        assert!(options(&[], &saved).dynamic_toolsets);
    }

    #[test]
    fn keychain_source_needs_the_flag() {
        assert!(
            options(&[], &ProfileSettings::default())
                .keychain_source
                .is_none()
        );
        assert!(
            options(&["--allow-keychain-source"], &ProfileSettings::default())
                .keychain_source
                .is_some()
        );
    }

    #[test]
    fn profile_access_combinations() {
        let cases = [
            (false, false, AccessLevel::Read),
            (true, false, AccessLevel::Write),
            // Старый профиль без allow_write: удаление всё равно подразумевает запись.
            (false, true, AccessLevel::Destructive),
            (true, true, AccessLevel::Destructive),
        ];
        for (write, destructive, level) in cases {
            let opts = options(&[], &profile(write, destructive, &[]));
            assert_eq!(
                opts.max_access, level,
                "write={write} destructive={destructive}"
            );
        }
    }

    #[test]
    fn repeatable_allow_path() {
        let args = parse(&["--allow-path", "/a", "--allow-path", "/b"]);
        assert_eq!(args.allow_path, [PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn invalid_profile_toolsets_are_reported() {
        let err = try_options(&[], &profile(false, false, &["repos"])).unwrap_err();
        assert!(err.to_string().contains("unknown toolset `repos`"));
    }
}
