//! `sourcecraft-mcp serve` — запуск MCP-сервера через stdio.
//!
//! stdout занят протоколом MCP: всё остальное (логи, ошибки запуска) — только в stderr.
//! Токен берётся только из системного хранилища по профилю.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use sourcecraft_api::AccessLevel;
use sourcecraft_api::executor::{ApiClient, ClientConfig};
use sourcecraft_mcp_core::{FilePolicy, ServerOptions, SourceCraftServer, normalize_toolsets};

use crate::base_url::{BASE_URL_ENV, resolve_base_url};
use crate::profiles::{ProfileSettings, load_profile_explained};

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит тела ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug, Args)]
#[command(after_help = "\
Access level and toolsets: flags given on the command line win; without them the settings \
saved for the profile by `sourcecraft-mcp connect` are used. Without any, the server is read-only \
and exposes all toolsets (see `sourcecraft-mcp toolsets`).

Secrets: repository secret values and fields that look like credentials are replaced with \
\"[redacted]\" in every tool response. --allow-secret-values (or the profile setting) lets a \
call ask for secret values explicitly.

Local files: file arguments of tools must be inside the server's working directory (unless it is \
the filesystem root or the home directory), the system temp directory, or a directory given with \
--allow-path.

The token is read only from the system keychain; store it with `sourcecraft-mcp auth login` or \
`sourcecraft-mcp connect`.")]
pub struct ServeArgs {
    /// Token profile stored in the system keychain.
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Register tools that create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Register tools that delete data (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (see `sourcecraft-mcp toolsets`).
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Let tools show repository secret values when a call explicitly asks for them
    /// (`reveal_values: true`). Without it, secret values are always hidden.
    #[arg(long)]
    pub allow_secret_values: bool,
    /// Extra directory that tools may read files from or save files to. Repeatable.
    #[arg(long, value_name = "DIR")]
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
    let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;

    let (settings, token) = load_profile_explained(&args.profile)?;

    let options = effective_options(&args, cli_toolsets, &settings)?;
    tracing::info!(
        profile = %args.profile,
        access = ?options.max_access,
        toolsets = ?options.toolsets,
        allow_secret_values = options.allow_secret_values,
        roots = %files.describe_roots(),
        "запуск MCP-сервера SourceCraft"
    );

    let client = ApiClient::new(ClientConfig {
        base_url,
        token,
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

/// Уровень доступа и наборы: флаги командной строки, если заданы, иначе настройки профиля.
fn effective_options(
    args: &ServeArgs,
    cli_toolsets: Vec<String>,
    settings: &ProfileSettings,
) -> anyhow::Result<ServerOptions> {
    let max_access = if args.allow_write || args.allow_destructive {
        access_level(args.allow_write, args.allow_destructive)
    } else {
        access_level(settings.allow_write, settings.allow_destructive)
    };
    let toolsets = if cli_toolsets.is_empty() {
        normalize_toolsets(&settings.toolsets).map_err(|err| anyhow!("profile settings: {err}"))?
    } else {
        cli_toolsets
    };
    Ok(ServerOptions {
        max_access,
        toolsets: (!toolsets.is_empty()).then_some(toolsets),
        // Как и флаги доступа: флаг командной строки включает, без него — настройка профиля.
        allow_secret_values: args.allow_secret_values || settings.allow_secret_values,
        // Профиль и путь к бинарнику попадают в примеры `sourcecraft-mcp secret …`
        // в инструкциях: агент вызывает CLI с тем же профилем, что и сервер.
        profile: Some(args.profile.clone()),
        executable: std::env::current_exe()
            .ok()
            .map(|path| path.display().to_string()),
    })
}

fn access_level(allow_write: bool, allow_destructive: bool) -> AccessLevel {
    if allow_destructive {
        AccessLevel::Destructive
    } else if allow_write {
        AccessLevel::Write
    } else {
        AccessLevel::Read
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn parse(args: &[&str]) -> ServeArgs {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).unwrap().serve
    }

    fn profile(write: bool, destructive: bool, toolsets: &[&str]) -> ProfileSettings {
        ProfileSettings {
            client: None,
            allow_write: write,
            allow_destructive: destructive,
            toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            allow_secret_values: false,
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
        assert_eq!(args.profile, "default");
        assert_eq!(args.timeout, 60);
        assert_eq!(args.max_response_bytes, 256 * 1024);
        assert!(args.allow_path.is_empty());
        let opts = options(&[], &ProfileSettings::default());
        assert_eq!(opts.max_access, AccessLevel::Read);
        assert_eq!(opts.toolsets, None);
        assert!(!opts.allow_secret_values);
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
