//! `sourcecraft-mcp ui` — TUI профилей ([`mcp_kit::tui`]): профили и их настройки, права и
//! одобрение на macOS, регистрации в Claude Code и Codex, проверка токена в SourceCraft и
//! инструменты с вызовом.
//!
//! Здесь только сервисная часть ([`SourceCraftAdapter`]): чтение и запись `profiles.toml`,
//! состояние токена (без самого токена), проверка токена и ролей пользователя запросами к API,
//! сборка сервера и повторная регистрация без флагов доступа. Сервер для инструментов
//! собирается так же, как stdio-`serve` без флагов: права профиля, урезанные до одобренных.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;
use mcp_kit::ProfileAccess;
use mcp_kit::approval;
use mcp_kit::call::Session;
use mcp_kit::registrations::{self, Registration, ServerIdentity};
use mcp_kit::tui::{self, Field, ProfileView, ServiceAdapter};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde_json::{Value, json};
use sourcecraft_api::auth::{KeyringStore, TokenStore};

use crate::cli::serve::{ServeArgs, Setup};
use crate::clients::claude::{self, SystemRunner};
use crate::clients::{EXE_STEM, SERVER_NAME, Scope, ServerSpec, codex};
use crate::profiles::{DEFAULT_PROFILE, ProfileRepo, ProfileSettings};
use crate::ui::CaptureUi;

/// Переменная окружения с профилем для [`ServerIdentity`]. `serve` профиль из окружения не
/// читает (только `--profile`), поэтому имя заведомо не встречается в окружении записей.
const SERVE_PROFILE_ENV: &str = "";

/// Сколько организаций проверять на роли пользователя: на каждую — отдельный запрос.
const MAX_ROLE_ORGS: usize = 5;

const UI_HELP: &str = "\
Shows the profiles in profiles.toml with their settings and the state of the token (never \
the token), the access of each profile and whether it is approved, where the server is \
registered in Claude Code and Codex and whether those registrations agree with the profile, \
who the token signs in as and the user's roles in organizations, and the tools the profile \
gives the agent.

The access (read, write or destructive, and the extra directories for local files) is changed \
here: r, w and d set the level, a adds a directory, x removes one, s saves. On macOS, saving \
more access than was approved asks for Touch ID or the macOS password; access that is not \
approved does not take effect. f registers the selected entry again without access flags. \
Press ? for the keys.";

#[derive(Debug, Clone, Args)]
#[command(after_help = UI_HELP)]
pub struct UiArgs {}

pub async fn run(_args: UiArgs) -> anyhow::Result<()> {
    let adapter = SourceCraftAdapter {
        repo: ProfileRepo::open_default()?,
        tokens: &KeyringStore,
        exe: std::env::current_exe()
            .map_err(|err| anyhow!("cannot determine the path of {EXE_STEM}: {err}"))?,
        codex_home: codex::default_codex_home().ok(),
    };
    tui::run(&adapter).await.map_err(|err| anyhow!(err))
}

/// Сервисная часть TUI для SourceCraft.
pub struct SourceCraftAdapter<'a> {
    pub repo: ProfileRepo,
    pub tokens: &'a dyn TokenStore,
    /// Путь к бинарнику для повторной регистрации.
    pub exe: PathBuf,
    /// `$CODEX_HOME`; `None` — не определить (тогда Codex не регистрируется заново).
    pub codex_home: Option<PathBuf>,
}

impl SourceCraftAdapter<'_> {
    /// Сервер с правами профиля без флагов (как stdio-`serve` без флагов), со всеми наборами и
    /// без наборов по запросу.
    fn setup(&self, profile: &str) -> anyhow::Result<Setup> {
        let approvals = approval::platform_store(EXE_STEM);
        let mut setup = Setup::load(
            &ServeArgs::for_profile(profile),
            self.tokens,
            &self.repo,
            approvals.as_deref(),
        )?;
        setup.options.toolsets = None;
        setup.options.dynamic_toolsets = false;
        Ok(setup)
    }

    async fn session(&self, profile: &str) -> Result<Session, String> {
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        let server = setup.server().map_err(|err| format!("{err:#}"))?;
        Session::start(server).await.map_err(|err| err.to_string())
    }

    fn profile_fields(&self, name: &str, settings: &ProfileSettings) -> Vec<Field> {
        let token = match self.tokens.contains(name) {
            Ok(true) => "stored in the system keychain".to_owned(),
            Ok(false) => format!("missing: run `{EXE_STEM} auth login --profile {name}`"),
            Err(err) => format!("cannot check the keychain: {err}"),
        };
        let toolsets = if settings.toolsets.is_empty() {
            "all".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        let on_off = |value: bool| if value { "on" } else { "off" };
        vec![
            Field::new("Token", token),
            Field::new("Toolsets", toolsets),
            Field::new("Dynamic toolsets", on_off(settings.dynamic_toolsets)),
            Field::new(
                "Secret values",
                if settings.allow_secret_values {
                    "shown when a call asks for them"
                } else {
                    "always hidden"
                },
            ),
            Field::new(
                "Connected to",
                settings.client.clone().unwrap_or_else(|| "-".into()),
            ),
        ]
    }
}

impl ServiceAdapter for SourceCraftAdapter<'_> {
    fn bin(&self) -> &str {
        EXE_STEM
    }

    fn service_name(&self) -> &str {
        "SourceCraft"
    }

    fn identity(&self) -> ServerIdentity<'_> {
        ServerIdentity {
            exe_stem: EXE_STEM,
            default_profile: DEFAULT_PROFILE,
            profile_env: SERVE_PROFILE_ENV,
        }
    }

    fn load_profiles(&self) -> Result<Vec<ProfileView>, String> {
        let profiles = self.repo.list().map_err(|err| format!("{err:#}"))?;
        Ok(profiles
            .iter()
            .map(|(name, settings)| ProfileView {
                name: name.clone(),
                access: settings.access(),
                fields: self.profile_fields(name, settings),
            })
            .collect())
    }

    fn save_access(&self, profile: &str, access: &ProfileAccess) -> Result<(), String> {
        let mut settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        settings.set_access(access);
        self.repo
            .upsert(profile, settings)
            .map_err(|err| format!("{err:#}"))
    }

    async fn check_account(&self, profile: &str) -> Result<Vec<Field>, String> {
        let session = self.session(profile).await?;
        let fields = account_fields(&session, profile).await;
        session.close().await;
        Ok(fields)
    }

    async fn tools(&self, profile: &str) -> Result<Vec<Tool>, String> {
        let session = self.session(profile).await?;
        let tools = session.tools().await.map_err(|err| err.to_string());
        session.close().await;
        tools
    }

    async fn call_tool(
        &self,
        profile: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let session = self.session(profile).await?;
        let result = session
            .call(tool, arguments)
            .await
            .map_err(|err| err.to_string());
        session.close().await;
        result
    }

    fn reregister(&self, registration: &Registration) -> Result<String, String> {
        reregister(
            registration,
            &self.exe,
            self.codex_home.as_deref(),
            std::env::current_dir().ok().as_deref(),
        )
    }
}

/// Что видно токену: пользователь, организации из его pull request'ов и задач, роли
/// пользователя в них. Роли самого токена SourceCraft не сообщает.
async fn account_fields(session: &Session, profile: &str) -> Vec<Field> {
    let mut fields = Vec::new();
    let token_roles = Field::new(
        "Token roles",
        format!(
            "SourceCraft does not report them; see Home > Access > Personal access tokens \
             (`mcp-{profile}`)"
        ),
    );
    let user = match request(session, "/user", json!({}), "{id, username}").await {
        Ok(user) => user,
        Err(err) => {
            fields.push(Field::new(
                "Token",
                format!("rejected or unreachable: {err}"),
            ));
            fields.push(token_roles);
            return fields;
        }
    };
    let username = user["username"].as_str().unwrap_or("?").to_owned();
    fields.push(Field::new(
        "Token",
        format!("valid: signs in as `{username}`"),
    ));

    let mut orgs = Vec::new();
    for (path, query, filter) in [
        (
            "/me/pulls",
            json!({"role": "any", "page_size": 50}),
            "[.pull_requests[]?.repository.organization.slug // empty]",
        ),
        (
            "/me/issues",
            json!({"page_size": 50}),
            "[.issues[]?.repository.organization.slug // empty]",
        ),
    ] {
        if let Ok(Value::Array(found)) = request(session, path, query, filter).await {
            for org in found.iter().filter_map(Value::as_str) {
                if is_plain_slug(org) && !orgs.iter().any(|known| known == org) {
                    orgs.push(org.to_owned());
                }
            }
        }
    }
    match user["id"].as_str() {
        Some(id) if !orgs.is_empty() => {
            orgs.sort();
            for org in orgs.iter().take(MAX_ROLE_ORGS) {
                fields.push(Field::new(
                    format!("Roles in {org}"),
                    org_roles(session, org, id).await,
                ));
            }
            if orgs.len() > MAX_ROLE_ORGS {
                fields.push(Field::new(
                    "Other organizations",
                    orgs[MAX_ROLE_ORGS..].join(", "),
                ));
            }
        }
        _ => fields.push(Field::new(
            "Roles",
            "no organizations found in the user's pull requests and issues",
        )),
    }
    fields.push(token_roles);
    fields
}

/// Роли пользователя `user_id` в организации: роль в организации и роли в репозиториях,
/// заданные на уровне организации. Роли через команды не показываются.
async fn org_roles(session: &Session, org: &str, user_id: &str) -> String {
    let id = Value::String(user_id.to_owned());
    let filter = format!(
        "[((.org_subject_roles // [])[], (.repo_subject_roles // [])[]) \
         | select(.subject.id == {id}) | .role] | unique"
    );
    let path = format!("/orgs/{org}/roles");
    match request(session, &path, json!({"page_size": 100}), &filter).await {
        Ok(Value::Array(roles)) if !roles.is_empty() => roles
            .iter()
            .map(|role| {
                role.as_str()
                    .map_or_else(|| role.to_string(), str::to_owned)
            })
            .collect::<Vec<_>>()
            .join(", "),
        Ok(_) => "none set directly (roles through teams are not shown)".to_owned(),
        Err(err) => format!("cannot list: {err}"),
    }
}

/// `GET path` через `api_request` с фильтром `jq`: значение или первая строка ошибки.
async fn request(session: &Session, path: &str, query: Value, jq: &str) -> Result<Value, String> {
    let arguments = object(json!({
        "method": "GET",
        "path": path,
        "query": query,
        "jq": jq,
    }));
    let result = session
        .call("api_request", arguments)
        .await
        .map_err(|err| err.to_string())?;
    let text = result_text(&result);
    if result.is_error == Some(true) {
        let first: String = text
            .lines()
            .next()
            .unwrap_or("error")
            .chars()
            .take(160)
            .collect();
        return Err(first);
    }
    // После JSON может идти пояснение сервера (например, о вырезанных значениях).
    serde_json::Deserializer::from_str(&text)
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok)
        .ok_or_else(|| "unexpected response".to_owned())
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Slug организации без символов, которые пришлось бы кодировать в пути.
fn is_plain_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !slug.starts_with('.')
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

/// Регистрирует запись заново: те же аргументы без флагов доступа и с текущим путём к
/// бинарнику. `connect` пишет только запись `sourcecraft`; запись проекта (Claude Code) — только
/// из каталога проекта.
fn reregister(
    registration: &Registration,
    exe: &Path,
    codex_home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<String, String> {
    let serve = &registration.serve;
    if serve.subcommand.as_deref() != Some("serve") || serve.http {
        return Err(format!(
            "`{}` is not a stdio `serve` command; edit it in the client config",
            registration.name
        ));
    }
    if registration.name != SERVER_NAME {
        return Err(format!(
            "`{}` is not the `{SERVER_NAME}` entry that `{EXE_STEM} connect` writes; remove the \
             access flags from it in the client config, or run `{EXE_STEM} connect` with \
             `--profile {}`",
            registration.name, serve.profile
        ));
    }
    let spec = ServerSpec::without_access_flags(exe, &registration.args)
        .map_err(|err| format!("{err:#}"))?;
    let mut ui = CaptureUi::default();
    let done = match registration.client {
        registrations::Client::ClaudeCode => {
            let scope = match &registration.scope {
                registrations::Scope::User => Scope::User,
                registrations::Scope::Project(dir) if Some(dir.as_path()) == cwd => Scope::Project,
                other => {
                    return Err(format!(
                        "`{}` is registered for {other}; run `{EXE_STEM} connect claude-code \
                         --scope project --profile {}` in that project",
                        registration.name, serve.profile
                    ));
                }
            };
            claude::register(&mut SystemRunner, &mut ui, scope, &spec, true)
        }
        registrations::Client::Codex => {
            let home = codex_home.ok_or("cannot determine $CODEX_HOME")?;
            codex::register(&mut ui, home, &spec, true)
        }
    }
    .map_err(|err| format!("{err:#}"))?;
    if !done {
        return Err(ui
            .lines
            .last()
            .cloned()
            .unwrap_or_else(|| "not changed".into()));
    }
    Ok(format!(
        "{} now runs `{}`; restart the client to pick it up",
        registration.client.display_name(),
        spec.display()
    ))
}

#[cfg(test)]
mod tests {
    use sourcecraft_api::ApiToken;
    use sourcecraft_api::auth::MemoryStore;
    use sourcecraft_mcp_core::JqRunner;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::clients::TEST_EXE;

    fn adapter<'a>(dir: &Path, tokens: &'a MemoryStore) -> SourceCraftAdapter<'a> {
        SourceCraftAdapter {
            repo: ProfileRepo::in_dir(dir.join("config")),
            tokens,
            exe: PathBuf::from(TEST_EXE),
            codex_home: Some(dir.join("codex")),
        }
    }

    #[test]
    fn shows_profiles_without_the_token_and_saves_access() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        tokens.set("work", &ApiToken::new("secret-token")).unwrap();
        let adapter = adapter(dir.path(), &tokens);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    client: Some("codex".into()),
                    allow_write: true,
                    toolsets: vec!["issues".into()],
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        adapter
            .repo
            .upsert("empty", ProfileSettings::default())
            .unwrap();

        let profiles = adapter.load_profiles().unwrap();
        assert_eq!(profiles.len(), 2);
        let work = profiles.iter().find(|p| p.name == "work").unwrap();
        assert_eq!(work.access.level, mcp_kit::AccessLevel::Write);
        let text = format!("{:?}", work.fields);
        assert!(text.contains("stored in the system keychain"), "{text}");
        assert!(text.contains("issues"), "{text}");
        assert!(!text.contains("secret-token"), "{text}");
        let empty = profiles.iter().find(|p| p.name == "empty").unwrap();
        assert!(format!("{:?}", empty.fields).contains("auth login --profile empty"));

        let root = dir.path().join("files");
        adapter
            .save_access(
                "work",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![root.clone()]),
            )
            .unwrap();
        let saved = adapter.repo.get("work").unwrap().unwrap();
        assert!(saved.allow_destructive);
        assert_eq!(saved.allow_path, vec![root]);
        // Остальные настройки профиля не тронуты.
        assert_eq!(saved.client.as_deref(), Some("codex"));
        assert_eq!(saved.toolsets, ["issues"]);
    }

    /// Сессия с сервером профиля `ci`, который ходит в `api`.
    async fn session(api: &MockServer) -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        tokens.set("ci", &ApiToken::new("test-token")).unwrap();
        let adapter = adapter(dir.path(), &tokens);
        adapter
            .repo
            .upsert("ci", ProfileSettings::default())
            .unwrap();
        let mut setup = adapter.setup("ci").unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        setup.options.jq = JqRunner::in_process();
        let session = Session::start(setup.server().unwrap()).await.unwrap();
        (dir, session)
    }

    #[tokio::test]
    async fn account_shows_the_user_and_roles() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1", "username": "test-user"
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/me/pulls"))
            .and(query_param("role", "any"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pull_requests": [
                    {"repository": {"slug": "web", "organization": {"slug": "acme"}}},
                    {"repository": {"slug": "x", "organization": {"slug": "bad/slug"}}}
                ]
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/me/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{"repository": {"organization": {"slug": "beta"}}}]
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/orgs/acme/roles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "org_subject_roles": [
                    {"role": "org_admin", "subject": {"type": "user", "id": "u1"}},
                    {"role": "org_owner", "subject": {"type": "user", "id": "u2"}}
                ],
                "repo_subject_roles": [
                    {"role": "developer", "subject": {"type": "user", "id": "u1"}}
                ]
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/orgs/beta/roles"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message": "forbidden"
            })))
            .mount(&api)
            .await;

        let (_dir, session) = session(&api).await;
        let fields = account_fields(&session, "ci").await;
        session.close().await;
        let text = format!("{fields:?}");
        assert!(text.contains("signs in as `test-user`"), "{text}");
        assert!(text.contains("Roles in acme"), "{text}");
        assert!(text.contains("developer, org_admin"), "{text}");
        assert!(!text.contains("org_owner"), "{text}");
        assert!(text.contains("Roles in beta"), "{text}");
        assert!(text.contains("cannot list"), "{text}");
        assert!(!text.contains("bad/slug"), "{text}");
        assert!(text.contains("mcp-ci"), "{text}");
        assert!(!text.contains("test-token"), "{text}");
    }

    #[tokio::test]
    async fn a_rejected_token_is_reported() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "no"})))
            .mount(&api)
            .await;
        let (_dir, session) = session(&api).await;
        let fields = account_fields(&session, "ci").await;
        session.close().await;
        assert_eq!(fields[0].label, "Token");
        assert!(fields[0].value.contains("rejected"), "{fields:?}");
    }

    fn registration(
        client: registrations::Client,
        scope: registrations::Scope,
        args: &[&str],
    ) -> Registration {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        Registration {
            client,
            scope,
            name: SERVER_NAME.into(),
            command: PathBuf::from(TEST_EXE),
            serve: registrations::parse_serve(&args, None, DEFAULT_PROFILE),
            args,
            env_names: vec![],
            enabled: true,
        }
    }

    #[test]
    fn reregisters_codex_without_access_flags() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            codex::config_path(&home),
            format!(
                "[mcp_servers.sourcecraft]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"work\", \"--allow-write\", \"--allow-destructive\", \"--toolsets\", \"issues\", \"--allow-secret-values\"]\n"
            ),
        )
        .unwrap();
        let reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &[
                "serve",
                "--profile",
                "work",
                "--allow-write",
                "--allow-destructive",
                "--toolsets",
                "issues",
                "--allow-secret-values",
            ],
        );
        assert!(reg.serve.has_access_flags());
        let message = reregister(&reg, Path::new(TEST_EXE), Some(&home), None).unwrap();
        assert!(message.contains("Codex now runs"), "{message}");
        let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
        assert!(!config.contains("--allow-write"), "{config}");
        assert!(!config.contains("--allow-destructive"), "{config}");
        assert!(
            config.contains(
                r#"args = ["serve", "--profile", "work", "--toolsets", "issues", "--allow-secret-values"]"#
            ),
            "{config}"
        );
    }

    #[test]
    fn other_entries_are_explained() {
        let elsewhere = PathBuf::from(if cfg!(windows) {
            r"C:\elsewhere"
        } else {
            "/elsewhere"
        });
        let here = PathBuf::from(if cfg!(windows) { r"C:\here" } else { "/here" });
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Project(elsewhere),
            &["serve", "--profile", "work", "--allow-write"],
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, Some(&here)).unwrap_err();
        assert!(err.contains("--scope project"), "{err}");

        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Local(here.clone()),
            &["serve", "--profile", "work"],
        );
        assert!(reregister(&reg, Path::new(TEST_EXE), None, Some(&here)).is_err());

        let mut reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &["serve", "--profile", "work", "--allow-write"],
        );
        reg.name = "sourcecraft-work".into();
        let err = reregister(&reg, Path::new(TEST_EXE), Some(&here), None).unwrap_err();
        assert!(err.contains("not the `sourcecraft` entry"), "{err}");

        let reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &["serve", "--http"],
        );
        let err = reregister(&reg, Path::new(TEST_EXE), Some(&here), None).unwrap_err();
        assert!(err.contains("not a stdio"), "{err}");
    }

    #[test]
    fn plain_slugs_only() {
        assert!(is_plain_slug("acme-dev_1.x"));
        for bad in ["", "a/b", "a b", "..", ".x", "a?b"] {
            assert!(!is_plain_slug(bad), "{bad}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn approval_service_matches_mcp_kit() {
        // Запись одобрения защищена от `secret copy` по имени сервиса из sourcecraft-api.
        assert_eq!(
            approval::KeychainApprovalStore::new(EXE_STEM).service(),
            sourcecraft_api::keychain::APPROVAL_SERVICE
        );
    }
}
