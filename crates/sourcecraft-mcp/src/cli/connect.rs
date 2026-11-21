//! `sourcecraft-mcp connect` — подключение сервера к MCP-клиенту.
//!
//! Сценарий: инструкция по созданию токена и форма создания в браузере → скрытый ввод →
//! проверка `GET /user` → токен в системное хранилище, настройки в `profiles.toml` (права —
//! только если заданы флаги; на macOS повышение прав одобряется Touch ID или паролем) →
//! регистрация сервера в клиенте после подтверждения, без флагов доступа. Токен не попадает
//! ни в аргументы, ни в конфигурацию клиента.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;
use mcp_kit::approval::{self, ApprovalStore, Presence};
use mcp_kit::{AccessLevel, ProfileAccess};
use sourcecraft_api::ApiToken;
use sourcecraft_api::auth::{KEYRING_SERVICE, KeyringStore, TokenStore};
use sourcecraft_mcp_core::normalize_toolsets;

use crate::browser::{BrowserDecision, BrowserEnv, SkipReason, TOKEN_FORM_URL, decide};
use crate::clients::claude::{self, CommandRunner, SystemRunner};
use crate::clients::{ClientKind, EXE_STEM, Scope, ServerSpec, codex};
use crate::profiles::{ProfileRepo, ProfileSettings, validate_profile_name};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

/// Документация SourceCraft о персональных токенах.
pub const PAT_DOCS_URL: &str = "https://sourcecraft.dev/portal/docs/en/sourcecraft/security/pat";

#[derive(Debug, Args)]
#[command(after_help = "\
The token is read with hidden input and stored only in the system keychain \
(macOS Keychain, Windows Credential Manager or Secret Service on Linux) under the profile name. \
The client config gets only the command `sourcecraft-mcp serve --profile <profile> ...`, \
without access flags.

The access is a setting of the profile: --allow-write and --allow-destructive change it \
(without them the profile keeps its access, read-only for a new profile), and \
`sourcecraft-mcp ui` changes it later. On macOS, raising the access asks for Touch ID or the \
macOS password; access that is not approved does not take effect.

When a new token is needed, the token creation page opens in the default browser; over SSH, \
without a graphical session or with --no-browser only its link is printed.

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
    /// Set the access of the profile to write: tools that create or modify data in
    /// SourceCraft.
    #[arg(long)]
    pub allow_write: bool,
    /// Set the access of the profile to destructive: also tools that delete data in
    /// SourceCraft (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (see `sourcecraft-mcp toolsets`). Default: all.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Let the agent ask to see repository secret values (`serve --allow-secret-values`).
    #[arg(long)]
    pub allow_secret_values: bool,
    /// Start with only the search, request and toolset tools; the agent loads toolsets on
    /// demand (`serve --dynamic-toolsets`). Needs a client that reloads the tool list.
    #[arg(long)]
    pub dynamic_toolsets: bool,
    /// Do not ask for confirmation before changing the client config.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Store the token without checking it against the SourceCraft API.
    #[arg(long)]
    pub skip_check: bool,
    /// Do not open the token creation page in a browser; only print its link.
    #[arg(long)]
    pub no_browser: bool,
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot determine the path of sourcecraft-mcp")?;
    let codex_home = match args.client {
        ClientKind::Codex => codex::default_codex_home()?,
        ClientKind::ClaudeCode => PathBuf::new(),
    };
    let approvals = approval::platform_store(EXE_STEM);
    let presence = approval::platform_presence();
    let mut env = ConnectEnv {
        approval: approvals.as_deref().zip(presence.as_deref()),
        store: &KeyringStore,
        repo: &ProfileRepo::open_default()?,
        verifier: &ApiVerifier::new()?,
        runner: &mut SystemRunner,
        ui: &mut TerminalUi,
        exe,
        codex_home,
        browser: decide(args.no_browser, &BrowserEnv::current()),
    };
    connect(&mut env, &args).await
}

/// Зависимости `connect`; в тестах подменяются хранилищем в памяти и временными каталогами.
pub struct ConnectEnv<'a, V> {
    /// Одобрение прав (macOS); `None` — платформа без одобрения.
    pub approval: Option<(&'a dyn ApprovalStore, &'a dyn Presence)>,
    pub store: &'a dyn TokenStore,
    pub repo: &'a ProfileRepo,
    pub verifier: &'a V,
    pub runner: &'a mut dyn CommandRunner,
    pub ui: &'a mut dyn Ui,
    pub exe: PathBuf,
    pub codex_home: PathBuf,
    /// Открывать ли форму создания токена в браузере (решается в `run` по флагу и окружению).
    pub browser: BrowserDecision,
}

/// Инструкция по заполнению формы создания токена.
///
/// Форма не принимает параметров URL, а по умолчанию предлагает пустое имя, срок 1 день,
/// доступ ко всем репозиториям и роль по умолчанию — поэтому подсказываем, что поменять.
pub fn token_instructions(profile: &str, allow_write: bool) -> String {
    let roles = if allow_write {
        "the repository roles this agent needs to read and change data"
    } else {
        "read-only repository roles (the server runs read-only)"
    };
    format!(
        "Create a personal access token for profile `{profile}` in SourceCraft:\n\
         \x20 {TOKEN_FORM_URL}\n\
         \x20 - Name: mcp-{profile}\n\
         \x20 - Expiration: longer than the default 1 day (up to 1 year)\n\
         \x20 - Roles: {roles}\n\
         \x20 - Repositories: optionally limit access to the ones the agent needs\n\
         \x20 Click \"Generate token\" and copy the value: SourceCraft shows it only once.\n\
         \x20 Roles reference: {PAT_DOCS_URL}"
    )
}

/// Запрашивает новый токен: инструкция, форма создания в браузере (если `browser` разрешает),
/// скрытый ввод. Общий шаг `connect` и `auth login`.
pub fn prompt_new_token(
    ui: &mut dyn Ui,
    profile: &str,
    allow_write: bool,
    browser: BrowserDecision,
) -> anyhow::Result<ApiToken> {
    // Ссылка печатается всегда, чтобы её можно было скопировать.
    ui.say(&token_instructions(profile, allow_write));
    match browser {
        BrowserDecision::Open => match ui.open_url(TOKEN_FORM_URL) {
            Ok(()) => ui.say("Opened the token creation page in your browser."),
            Err(err) => ui.say(&format!(
                "Could not open a browser ({err}); open the link above yourself."
            )),
        },
        BrowserDecision::Skip(SkipReason::Ssh) => {
            ui.say("SSH session: open the link above in a browser on your computer.");
        }
        BrowserDecision::Skip(SkipReason::NoDisplay) => {
            ui.say("No graphical session: open the link above in a browser.");
        }
        BrowserDecision::Skip(SkipReason::Flag) => {}
    }
    let token = ui
        .read_token(&format!(
            "Paste the token for profile `{profile}` (input is hidden): "
        ))
        .context("failed to read the token")?;
    if token.expose().is_empty() {
        bail!("empty token; nothing was stored");
    }
    Ok(token)
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
    let requested_level = AccessLevel::requested_by_flags(args.allow_write, args.allow_destructive);
    // Регистрация не зависит от прав профиля: флагов доступа в ней нет.
    let spec = ServerSpec::new(
        &env.exe,
        &profile,
        &ProfileSettings {
            toolsets: toolsets.clone(),
            allow_secret_values: args.allow_secret_values,
            dynamic_toolsets: args.dynamic_toolsets,
            ..ProfileSettings::default()
        },
    )?;

    // Проверки клиента — до ввода токена, чтобы не просить токен зря.
    match client {
        ClientKind::ClaudeCode => claude::ensure_available(env.runner)?,
        ClientKind::Codex => codex::ensure_scope(args.scope)?,
    }
    let mut settings = env.repo.get(&profile)?.unwrap_or_default();
    // Роли в подсказке о токене — по уровню, который будет у профиля.
    let level = requested_level.unwrap_or(settings.access().level);

    let stored = env.store.contains(&profile)?;
    let token = if stored {
        env.ui.say(&format!(
            "Using the token already stored for profile `{profile}`. \
             To replace it run `sourcecraft-mcp auth login --profile {profile}`."
        ));
        env.store.get(&profile)?
    } else {
        prompt_new_token(env.ui, &profile, level != AccessLevel::Read, env.browser)?
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
    settings.client = Some(client.name().to_owned());
    settings.toolsets = toolsets;
    settings.allow_secret_values = args.allow_secret_values;
    settings.dynamic_toolsets = args.dynamic_toolsets;
    if let Some(level) = requested_level {
        change_access(env.ui, env.approval, &profile, &mut settings, level);
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

/// Меняет уровень доступа профиля (каталоги профиля остаются). На macOS повышение сначала
/// одобряется владельцем; отказ не останавливает `connect`: профиль сохраняет прежние права.
fn change_access(
    ui: &mut dyn Ui,
    approval: Option<(&dyn ApprovalStore, &dyn Presence)>,
    profile: &str,
    settings: &mut ProfileSettings,
    level: AccessLevel,
) {
    let before = settings.access();
    let access = ProfileAccess::new(level, before.allow_path.clone());
    let what = format!("{EXE_STEM} profile \"{profile}\"");
    match approval::save_access_with(approval, profile, &what, &access, |access| {
        settings.set_access(access);
        Ok(())
    }) {
        Ok(saved) if saved.prompted => ui.say(&format!(
            "Access `{}` approved on this Mac.",
            saved.access.summary()
        )),
        Ok(_) => {}
        Err(err) => ui.say(&format!(
            "The access was not changed: {err}. The profile keeps `{}` access; change it later \
             with `{EXE_STEM} ui` (profile `{profile}`).",
            before.summary()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use mcp_kit::approval::testing::{FakePresence, MemoryStore as MemoryApprovals};
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
        browser: BrowserDecision,
        /// Одобрение прав, как на macOS; `None` — как на Linux и Windows.
        approval: Option<(MemoryApprovals, FakePresence)>,
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
                browser: BrowserDecision::Skip(SkipReason::Flag),
                approval: None,
                _dir: dir,
            }
        }

        async fn connect(&mut self, args: &ConnectArgs) -> anyhow::Result<()> {
            let approval = self
                .approval
                .as_ref()
                .map(|(store, presence)| (store as &dyn ApprovalStore, presence as &dyn Presence));
            let mut env = ConnectEnv {
                approval,
                store: &self.store,
                repo: &self.repo,
                verifier: &self.verifier,
                runner: &mut self.runner,
                ui: &mut self.ui,
                exe: PathBuf::from(crate::clients::TEST_EXE),
                codex_home: self.codex_home.clone(),
                browser: self.browser,
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
            dynamic_toolsets: false,
            yes: false,
            skip_check: false,
            no_browser: false,
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
        assert!(
            config
                .contains(r#"args = ["serve", "--profile", "codex", "--toolsets", "repository"]"#)
        );
        let output = fx.ui.text();
        assert!(output.contains("mcp-codex"));
        assert!(output.contains("test-user"));
        // Токен не выводится и не попадает в конфигурацию клиента.
        assert!(!output.contains("good-token"));
        assert!(!config.contains("good-token"));
        assert!(fx.runner.calls.is_empty());
    }

    #[tokio::test]
    async fn access_flags_change_the_profile_not_the_registration() {
        // (--allow-write, --allow-destructive) -> сохранённые флаги; регистрация без флагов.
        let cases = [(false, false), (true, false), (false, true), (true, true)];
        for (write, destructive) in cases {
            let mut fx = Fixture::new();
            fx.ui.tokens = ["good-token".to_owned()].into();
            let mut args = args(ClientKind::Codex);
            args.allow_write = write;
            args.allow_destructive = destructive;
            args.yes = true;
            fx.connect(&args).await.unwrap();

            let settings = fx.repo.get("codex").unwrap().unwrap();
            assert_eq!(settings.allow_write, write || destructive);
            assert_eq!(settings.allow_destructive, destructive);

            let expected = r#"args = ["serve", "--profile", "codex"]"#;
            let config = fs::read_to_string(codex::config_path(&fx.codex_home)).unwrap();
            assert!(config.contains(expected), "{config}\nexpected: {expected}");

            let mut ui = ScriptedUi::default();
            crate::cli::auth::status(&fx.store, &fx.repo, None, &mut ui, "codex").unwrap();
            let text = ui.text();
            let yes_no = |value: bool| if value { "yes" } else { "no" };
            assert!(text.contains(&format!("Write:    {}", yes_no(write || destructive))));
            assert!(text.contains(&format!("Delete:   {}", yes_no(destructive))));
        }

        // Без флагов права профиля не меняются, каталоги профиля сохраняются.
        let mut fx = Fixture::new();
        fx.store.set("codex", &ApiToken::new("good-token")).unwrap();
        let root = fx._dir.path().join("files");
        fx.repo
            .upsert(
                "codex",
                ProfileSettings {
                    allow_write: true,
                    allow_path: vec![root.clone()],
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert!(settings.allow_write && !settings.allow_destructive);
        assert_eq!(settings.allow_path, vec![root.clone()]);
        // С флагом меняется только уровень.
        args.allow_destructive = true;
        fx.connect(&args).await.unwrap();
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert!(settings.allow_destructive);
        assert_eq!(settings.allow_path, vec![root]);
    }

    #[tokio::test]
    async fn raising_access_needs_approval_on_macos() {
        let mut fx = Fixture::new();
        fx.store.set("codex", &ApiToken::new("good-token")).unwrap();
        fx.approval = Some((MemoryApprovals::default(), FakePresence::new(false)));
        let mut args = args(ClientKind::Codex);
        args.allow_destructive = true;
        args.yes = true;
        fx.connect(&args).await.unwrap();
        // Отказ: права прежние, но профиль и регистрация сохранены.
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert!(!settings.allow_write && !settings.allow_destructive);
        let text = fx.ui.text();
        assert!(text.contains("The access was not changed"), "{text}");
        assert!(
            text.contains("`sourcecraft-mcp ui` (profile `codex`)"),
            "{text}"
        );
        assert!(codex::config_path(&fx.codex_home).exists());

        fx.approval = Some((MemoryApprovals::default(), FakePresence::new(true)));
        fx.ui = ScriptedUi::default();
        fx.connect(&args).await.unwrap();
        assert!(fx.repo.get("codex").unwrap().unwrap().allow_destructive);
        let (approvals, presence) = fx.approval.as_ref().unwrap();
        assert_eq!(presence.asked.get(), 1);
        assert_eq!(
            approvals.load("codex").unwrap().unwrap().level,
            AccessLevel::Destructive
        );
        assert!(
            fx.ui.text().contains("approved on this Mac"),
            "{}",
            fx.ui.text()
        );
    }

    #[tokio::test]
    async fn opens_token_form_in_browser() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        fx.ui.tokens = ["good-token".to_owned()].into();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert_eq!(fx.ui.opened, [TOKEN_FORM_URL]);
        let text = fx.ui.text();
        assert!(text.contains(TOKEN_FORM_URL), "{text}");
        assert!(text.contains("Name: mcp-codex"), "{text}");
        assert!(text.contains("Opened the token creation page"), "{text}");
    }

    #[tokio::test]
    async fn browser_failure_or_ssh_only_prints_link() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        fx.ui.open_fails = true;
        fx.ui.tokens = ["good-token".to_owned()].into();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let text = fx.ui.text();
        assert!(text.contains(TOKEN_FORM_URL), "{text}");
        assert!(text.contains("Could not open a browser"), "{text}");
        assert!(fx.store.contains("codex").unwrap());

        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Skip(SkipReason::Ssh);
        fx.ui.tokens = ["good-token".to_owned()].into();
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.opened.is_empty());
        let text = fx.ui.text();
        assert!(text.contains(TOKEN_FORM_URL), "{text}");
        assert!(text.contains("SSH session"), "{text}");
    }

    #[tokio::test]
    async fn stored_token_does_not_open_browser() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        fx.store.set("codex", &ApiToken::new("good-token")).unwrap();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.opened.is_empty());
    }

    #[test]
    fn instructions_name_roles_and_expiry() {
        let read = token_instructions("codex", false);
        assert!(read.contains("mcp-codex"));
        assert!(read.contains("read-only repository roles"));
        assert!(read.contains("1 day"));
        assert!(read.contains(PAT_DOCS_URL));
        let write = token_instructions("codex", true);
        assert!(write.contains("read and change data"));
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
                        command: crate::clients::TEST_EXE.into(),
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
