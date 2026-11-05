//! `sourcecraft-mcp serve` — запуск MCP-сервера через stdio или HTTP (`--http`).
//!
//! stdio: stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Токен берётся только из системного хранилища по профилю.
//!
//! HTTP ([`http_server`]): профилей и хранилища нет, токен приходит в заголовке
//! `Authorization` каждого запроса агента и нигде не сохраняется. Локальные файлы выключены.

mod http_server;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_api::keychain::SystemKeychain;
use sourcecraft_mcp_core::{
    FilePolicy, JqRunner, ServerOptions, SourceCraftServer, normalize_toolsets,
};
use url::Url;

use crate::base_url::{BASE_URL_ENV, resolve_base_url};
use crate::cli::{jq_worker, tools};
use crate::profiles::{ProfileSettings, access_level, load_profile_explained};

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит тела ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Профиль по умолчанию (stdio).
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Args)]
#[command(after_help = "\
Access level and toolsets: flags given on the command line win; without them the settings \
saved for the profile by `sourcecraft-mcp connect` are used. Without any, the server is read-only \
and exposes all toolsets (see `sourcecraft-mcp toolsets`).

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
the filesystem root or the home directory), the system temp directory, or a directory given with \
--allow-path.

In stdio mode the token is read only from the system keychain; store it with \
`sourcecraft-mcp auth login` or `sourcecraft-mcp connect`.

HTTP mode (--http): the server listens on --listen (default 127.0.0.1:8765) and serves MCP at \
/mcp (Streamable HTTP, stateless) and a health check at /health. There are no profiles and no \
stored tokens: every request must carry the agent's SourceCraft token in an \
`Authorization: Bearer <token>` header, which the server forwards to the SourceCraft API only. \
Access level, toolsets and secret flags come from the command line only. Local files are \
disabled. Browser requests are rejected unless their Origin is allowed with --allowed-origin; \
the Host header must be a loopback name, the --listen address or an --allowed-host. Outside \
loopback, put the server behind a TLS reverse proxy.")]
pub struct ServeArgs {
    /// Token profile stored in the system keychain [default: default].
    #[arg(long, conflicts_with = "http")]
    pub profile: Option<String>,
    /// Register tools that create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Register tools that delete data (implies --allow-write).
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
    /// Extra directory that tools may read files from or save files to. Repeatable.
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
    // Сначала проверяем аргументы, чтобы не обращаться к keychain зря.
    if args.timeout == 0 {
        bail!("--timeout must be greater than 0");
    }
    if args.max_response_bytes == 0 {
        bail!("--max-response-bytes must be greater than 0");
    }
    let from_env = std::env::var(BASE_URL_ENV).ok();
    let base_url = resolve_base_url(args.base_url.as_deref(), from_env.as_deref())?;
    let cli_toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    if args.http {
        return run_http(&args, base_url, cli_toolsets).await;
    }
    let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;
    let custom_tools = tools::load(&args.tools_file)?;

    let profile = args.profile.as_deref().unwrap_or(DEFAULT_PROFILE);
    let (settings, token) = load_profile_explained(profile)?;

    let mut options = effective_options(&args, cli_toolsets, &settings)?;
    options.custom_tools = custom_tools;
    tracing::info!(
        profile = %profile,
        access = ?options.max_access,
        toolsets = ?options.toolsets,
        dynamic_toolsets = options.dynamic_toolsets,
        allow_secret_values = options.allow_secret_values,
        allow_keychain_source = options.keychain_source.is_some(),
        roots = %files.describe_roots(),
        custom_tools = options.custom_tools.len(),
        "запуск MCP-сервера SourceCraft"
    );

    let client = ApiClient::new(ClientConfig {
        base_url,
        token: Some(token),
        timeout: Duration::from_secs(args.timeout),
        max_response_bytes: args.max_response_bytes,
    })
    .map_err(|err| anyhow!("cannot create the SourceCraft API client: {err}"))?;
    let server = SourceCraftServer::new(client, options, files).map_err(|err| anyhow!(err))?;

    let running = server
        .serve(stdio())
        .await
        .context("MCP initialization over stdio failed")?;
    running.waiting().await.context("MCP server task failed")?;
    Ok(())
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

/// Уровень доступа и наборы: флаги командной строки, если заданы, иначе настройки профиля.
fn effective_options(
    args: &ServeArgs,
    cli_toolsets: Vec<String>,
    settings: &ProfileSettings,
) -> anyhow::Result<ServerOptions> {
    let max_access = if args.allow_write || args.allow_destructive {
        access_level(args.allow_write, args.allow_destructive)
    } else {
        settings.access_level()
    };
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
        // Как и флаги доступа: флаг командной строки включает, без него — настройка профиля.
        allow_secret_values: args.allow_secret_values || settings.allow_secret_values,
        // Профиль и путь к бинарнику попадают в примеры `sourcecraft-mcp secret …`
        // в инструкциях: агент вызывает CLI с тем же профилем, что и сервер.
        profile: Some(
            args.profile
                .clone()
                .unwrap_or_else(|| DEFAULT_PROFILE.to_owned()),
        ),
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
    use sourcecraft_api::AccessLevel;

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
            toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            allow_secret_values: false,
            dynamic_toolsets: false,
        }
    }

    fn options(args: &[&str], settings: &ProfileSettings) -> ServerOptions {
        let args = parse(args);
        let cli = normalize_toolsets(&args.toolsets).unwrap();
        effective_options(&args, cli, settings).unwrap()
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
    fn cli_flags_win_over_profile() {
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

        let opts = options(&["--allow-destructive"], &profile(false, false, &[]));
        assert_eq!(opts.max_access, AccessLevel::Destructive);
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
        let args = parse(&[]);
        let err =
            effective_options(&args, Vec::new(), &profile(false, false, &["repos"])).unwrap_err();
        assert!(err.to_string().contains("unknown toolset `repos`"));
    }
}
