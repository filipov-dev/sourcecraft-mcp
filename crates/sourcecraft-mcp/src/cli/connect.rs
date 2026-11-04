//! `sourcecraft-mcp connect` — подключение сервера к MCP-клиенту.
//!
//! Сценарий: инструкция по созданию токена → скрытый ввод → проверка `GET /user` → токен в
//! системное хранилище, настройки в `profiles.toml` → регистрация сервера в клиенте после
//! подтверждения. Токен не попадает ни в аргументы, ни в конфигурацию клиента.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;
use sourcecraft_api::auth::{KEYRING_SERVICE, KeyringStore, TokenStore};
use sourcecraft_mcp_core::normalize_toolsets;

use crate::clients::claude::{self, CommandRunner, SystemRunner};
use crate::clients::{ClientKind, Scope, ServerSpec, codex};
use crate::profiles::{ProfileRepo, ProfileSettings, validate_profile_name};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

/// Документация SourceCraft о персональных токенах.
pub const PAT_DOCS_URL: &str = "https://sourcecraft.dev/portal/docs/en/sourcecraft/security/pat";

#[derive(Debug, Args)]
#[command(after_help = "\
The token is read with hidden input and stored only in the system keychain \
(macOS Keychain, Windows Credential Manager or Secret Service on Linux) under the profile name. \
The client config gets only the command `sourcecraft-mcp serve --profile <profile> ...`.

Use a separate token and profile for every client, so each can be revoked on its own.")]
pub struct ConnectArgs {
    /// MCP client to register the server in.
    #[arg(value_enum)]
    pub client: ClientKind,
    /// Token profile name. Defaults to the client name.
    #[arg(long)]
    pub profile: Option<String>,
    /// Where to register the server in the client. Codex supports only `user`.
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
    /// Allow tools that create or modify data in SourceCraft.
    #[arg(long)]
    pub allow_write: bool,
    /// Allow tools that delete data in SourceCraft.
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (see `sourcecraft-mcp toolsets`). Default: all.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Let the agent ask to see repository secret values (`serve --allow-secret-values`).
    #[arg(long)]
    pub allow_secret_values: bool,
    /// Do not ask for confirmation before changing the client config.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Store the token without checking it against the SourceCraft API.
    #[arg(long)]
    pub skip_check: bool,
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot determine the path of sourcecraft-mcp")?;
    let codex_home = match args.client {
        ClientKind::Codex => codex::default_codex_home()?,
        ClientKind::ClaudeCode => PathBuf::new(),
    };
    let mut env = ConnectEnv {
        store: &KeyringStore,
        repo: &ProfileRepo::open_default()?,
        verifier: &ApiVerifier::new()?,
        runner: &mut SystemRunner,
        ui: &mut TerminalUi,
        exe,
        codex_home,
    };
    connect(&mut env, &args).await
}

/// Зависимости `connect`; в тестах подменяются хранилищем в памяти и временными каталогами.
pub struct ConnectEnv<'a, V> {
    pub store: &'a dyn TokenStore,
    pub repo: &'a ProfileRepo,
    pub verifier: &'a V,
    pub runner: &'a mut dyn CommandRunner,
    pub ui: &'a mut dyn Ui,
    pub exe: PathBuf,
    pub codex_home: PathBuf,
}

/// Инструкция по созданию токена в веб-интерфейсе SourceCraft.
///
/// Прямой адрес страницы токенов в документации SourceCraft не указан, поэтому даём путь
/// по меню, а не угадываем URL.
pub fn token_instructions(profile: &str, allow_write: bool) -> String {
    let roles = if allow_write {
        "the repository roles this agent needs to read and change data"
    } else {
        "read-only repository roles (the server runs read-only)"
    };
    format!(
        "Create a personal access token for profile `{profile}` in SourceCraft:\n\
         \x20 1. Open https://sourcecraft.dev and sign in.\n\
         \x20 2. In the left panel open Home -> Access -> Personal access tokens and click\n\
         \x20    \"Generate new token\".\n\
         \x20 3. Name it `mcp-{profile}`, pick an expiration date, repository access and\n\
         \x20    {roles}.\n\
         \x20 4. Click \"Generate token\" and copy the value: SourceCraft shows it only once.\n\
         \x20 Docs: {PAT_DOCS_URL}"
    )
}

pub async fn connect<V: TokenVerifier>(
    env: &mut ConnectEnv<'_, V>,
    args: &ConnectArgs,
) -> anyhow::Result<()> {
    let client = args.client;
    let profile = args
        .profile
        .clone()
        .unwrap_or_else(|| client.name().to_owned());
    validate_profile_name(&profile)?;
    let toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow::anyhow!(err))?;
    let settings = ProfileSettings {
        client: Some(client.name().to_owned()),
        allow_write: args.allow_write,
        allow_destructive: args.allow_destructive,
        toolsets,
        allow_secret_values: args.allow_secret_values,
    };
    let spec = ServerSpec::new(&env.exe, &profile, &settings)?;

    // Проверки клиента — до ввода токена, чтобы не просить токен зря.
    match client {
        ClientKind::ClaudeCode => claude::ensure_available(env.runner)?,
        ClientKind::Codex => codex::ensure_scope(args.scope)?,
    }

    let stored = env.store.contains(&profile)?;
    let token = if stored {
        env.ui.say(&format!(
            "Using the token already stored for profile `{profile}`. \
             To replace it run `sourcecraft-mcp auth login --profile {profile}`."
        ));
        env.store.get(&profile)?
    } else {
        env.ui.say(&token_instructions(
            &profile,
            args.allow_write || args.allow_destructive,
        ));
        let token = env
            .ui
            .read_token(&format!(
                "Paste the token for profile `{profile}` (input is hidden): "
            ))
            .context("failed to read the token")?;
        if token.expose().is_empty() {
            bail!("empty token; nothing was stored");
        }
        token
    };

    if args.skip_check {
        env.ui.say("Skipping the token check (--skip-check).");
    } else {
        let user = env.verifier.verify(&token).await?;
        env.ui.say(&format!(
            "Token is valid: signed in to SourceCraft as `{user}`."
        ));
    }

    if !stored {
        env.store.set(&profile, &token)?;
        env.ui.say(&format!(
            "Token saved in the system keychain (service `{KEYRING_SERVICE}`, account `{profile}`)."
        ));
    }
    env.repo.upsert(&profile, settings)?;
    env.ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        env.repo.path().display()
    ));

    let registered = match client {
        ClientKind::ClaudeCode => {
            claude::register(env.runner, env.ui, args.scope, &spec, args.yes)?
        }
        ClientKind::Codex => codex::register(env.ui, &env.codex_home, &spec, args.yes)?,
    };
    if registered {
        env.ui.say(&format!(
            "Done: {} now runs `{}`. Restart the client to pick it up.",
            client.display_name(),
            spec.display()
        ));
    } else {
        env.ui.say(&format!(
            "{} config was not changed. The token and profile are kept; re-run \
             `sourcecraft-mcp connect {}` to register later.",
            client.display_name(),
            client.name()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sourcecraft_api::ApiToken;
    use sourcecraft_api::auth::MemoryStore;

    use super::*;
    use crate::clients::claude::testing::FakeRunner;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::testing::FakeVerifier;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: MemoryStore,
        repo: ProfileRepo,
        verifier: FakeVerifier,
        runner: FakeRunner,
        ui: ScriptedUi,
        codex_home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self {
                store: MemoryStore::new(),
                repo: ProfileRepo::in_dir(dir.path().join("config")),
                verifier: FakeVerifier {
                    valid_token: "good-token".into(),
                },
                runner: FakeRunner::default(),
                ui: ScriptedUi::default(),
                codex_home: dir.path().join("codex"),
                _dir: dir,
            }
        }

        async fn connect(&mut self, args: &ConnectArgs) -> anyhow::Result<()> {
            let mut env = ConnectEnv {
                store: &self.store,
                repo: &self.repo,
                verifier: &self.verifier,
                runner: &mut self.runner,
                ui: &mut self.ui,
                exe: PathBuf::from("/usr/local/bin/sourcecraft-mcp"),
                codex_home: self.codex_home.clone(),
            };
            connect(&mut env, args).await
        }
    }

    fn args(client: ClientKind) -> ConnectArgs {
        ConnectArgs {
            client,
            profile: None,
            scope: Scope::User,
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            allow_secret_values: false,
            yes: false,
            skip_check: false,
        }
    }

    #[tokio::test]
    async fn codex_flow_stores_token_profile_and_config() {
        let mut fx = Fixture::new();
        fx.ui.tokens = [" good-token \n".to_owned()].into();
        fx.ui.answers = [true].into();
        let mut args = args(ClientKind::Codex);
        args.allow_write = true;
        args.toolsets = vec!["repository".into(), " ".into()];

        fx.connect(&args).await.unwrap();

        assert_eq!(fx.store.get("codex").unwrap().expose(), "good-token");
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert_eq!(settings.client.as_deref(), Some("codex"));
        assert!(settings.allow_write);
        assert_eq!(settings.toolsets, ["repository"]);

        let config = fs::read_to_string(codex::config_path(&fx.codex_home)).unwrap();
        assert!(config.contains(
            r#"args = ["serve", "--profile", "codex", "--allow-write", "--toolsets", "repository"]"#
        ));
        let output = fx.ui.text();
        assert!(output.contains("mcp-codex"));
        assert!(output.contains("test-user"));
        // Токен не выводится и не попадает в конфигурацию клиента.
        assert!(!output.contains("good-token"));
        assert!(!config.contains("good-token"));
        assert!(fx.runner.calls.is_empty());
    }

    #[tokio::test]
    async fn invalid_token_is_not_stored() {
        let mut fx = Fixture::new();
        fx.ui.tokens = ["bad-token".to_owned()].into();
        let err = fx.connect(&args(ClientKind::Codex)).await.unwrap_err();
        assert!(err.to_string().contains("rejected the token"));
        assert!(!fx.store.contains("codex").unwrap());
        assert_eq!(fx.repo.get("codex").unwrap(), None);
        assert!(!codex::config_path(&fx.codex_home).exists());
    }

    #[tokio::test]
    async fn skip_check_stores_unverified_token() {
        let mut fx = Fixture::new();
        fx.ui.tokens = ["bad-token".to_owned()].into();
        let mut args = args(ClientKind::Codex);
        args.skip_check = true;
        args.yes = true;
        args.profile = Some("codex-ci".into());
        fx.connect(&args).await.unwrap();
        assert!(fx.store.contains("codex-ci").unwrap());
        assert!(fx.ui.questions.is_empty());
        let config = fs::read_to_string(codex::config_path(&fx.codex_home)).unwrap();
        assert!(config.contains(r#"args = ["serve", "--profile", "codex-ci"]"#));
    }

    #[tokio::test]
    async fn empty_token_is_rejected() {
        let mut fx = Fixture::new();
        fx.ui.tokens = ["  ".to_owned()].into();
        let err = fx.connect(&args(ClientKind::Codex)).await.unwrap_err();
        assert!(err.to_string().contains("empty token"));
        assert!(!fx.store.contains("codex").unwrap());
    }

    #[tokio::test]
    async fn claude_flow_reuses_stored_token() {
        let mut fx = Fixture::new();
        fx.store
            .set("claude-code", &ApiToken::new("good-token"))
            .unwrap();
        fx.ui.answers = [true].into();
        let mut args = args(ClientKind::ClaudeCode);
        args.scope = Scope::Project;
        fx.connect(&args).await.unwrap();

        // Токен не запрашивался повторно.
        assert!(fx.ui.tokens.is_empty());
        assert_eq!(
            fx.runner.calls,
            [
                vec!["--version".to_owned()],
                claude::add_args(
                    Scope::Project,
                    &ServerSpec {
                        command: "/usr/local/bin/sourcecraft-mcp".into(),
                        args: vec!["serve".into(), "--profile".into(), "claude-code".into()],
                    }
                )
            ]
        );
        assert!(fx.repo.get("claude-code").unwrap().is_some());
    }

    #[tokio::test]
    async fn codex_rejects_project_scope_before_token_input() {
        let mut fx = Fixture::new();
        let mut args = args(ClientKind::Codex);
        args.scope = Scope::Project;
        assert!(fx.connect(&args).await.is_err());
        assert!(fx.ui.output.is_empty());
    }

    #[tokio::test]
    async fn invalid_profile_name_is_rejected() {
        let mut fx = Fixture::new();
        let mut args = args(ClientKind::Codex);
        args.profile = Some("--evil".into());
        assert!(fx.connect(&args).await.is_err());
    }
}
