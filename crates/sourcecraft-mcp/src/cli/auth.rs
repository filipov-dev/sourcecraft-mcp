//! `sourcecraft-mcp auth` — управление токенами профилей.
//!
//! Команды, которая печатает значение токена, нет и быть не должно.

use anyhow::{Context, bail};
use clap::Subcommand;
use sourcecraft_api::auth::{AuthError, KEYRING_SERVICE, KeyringStore, TokenStore};

use crate::profiles::{ProfileRepo, ProfileSettings, validate_profile_name};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Show whether a token is stored for the profile and the profile settings.
    ///
    /// The token value is never printed.
    Status {
        /// Token profile name.
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Store a token for the profile (hidden input) without registering any MCP client.
    ///
    /// Replaces the stored token if there is one.
    Login {
        /// Token profile name.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Store the token without checking it against the SourceCraft API.
        #[arg(long)]
        skip_check: bool,
    },
    /// Remove the stored token of the profile from the system keychain.
    Logout {
        /// Token profile name.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Also remove the profile settings from profiles.toml.
        #[arg(long)]
        forget: bool,
    },
}

pub async fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = KeyringStore;
    let repo = ProfileRepo::open_default()?;
    let mut ui = TerminalUi;
    match command {
        AuthCommand::Status { profile } => status(&store, &repo, &mut ui, &profile),
        AuthCommand::Login {
            profile,
            skip_check,
        } => {
            let verifier = (!skip_check).then(ApiVerifier::new).transpose()?;
            login(&store, &repo, verifier.as_ref(), &mut ui, &profile).await
        }
        AuthCommand::Logout { profile, forget } => logout(&store, &repo, &mut ui, &profile, forget),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub fn status(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let token = if store.contains(profile)? {
        format!("stored in the system keychain (service `{KEYRING_SERVICE}`)")
    } else {
        "not found".to_owned()
    };
    ui.say(&format!("Profile:  {profile}"));
    ui.say(&format!("Token:    {token}"));
    match repo.get(profile)? {
        Some(settings) => {
            ui.say(&format!(
                "Client:   {}",
                settings.client.as_deref().unwrap_or("-")
            ));
            ui.say(&format!("Write:    {}", yes_no(settings.allow_write)));
            ui.say(&format!("Delete:   {}", yes_no(settings.allow_destructive)));
            let toolsets = if settings.toolsets.is_empty() {
                "default".to_owned()
            } else {
                settings.toolsets.join(",")
            };
            ui.say(&format!("Toolsets: {toolsets}"));
        }
        None => ui.say("Settings: none (read-only defaults)"),
    }
    ui.say(&format!("Profiles: {}", repo.path().display()));
    Ok(())
}

pub async fn login<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    ui.say(&super::connect::token_instructions(profile, false));
    let token = ui
        .read_token(&format!(
            "Paste the token for profile `{profile}` (input is hidden): "
        ))
        .context("failed to read the token")?;
    if token.expose().is_empty() {
        bail!("empty token; nothing was stored");
    }
    match verifier {
        Some(verifier) => {
            let user = verifier.verify(&token).await?;
            ui.say(&format!(
                "Token is valid: signed in to SourceCraft as `{user}`."
            ));
        }
        None => ui.say("Skipping the token check (--skip-check)."),
    }
    let replaced = store.contains(profile)?;
    store.set(profile, &token)?;
    if repo.get(profile)?.is_none() {
        repo.upsert(profile, ProfileSettings::default())?;
    }
    ui.say(&format!(
        "Token {} in the system keychain (service `{KEYRING_SERVICE}`, account `{profile}`).",
        if replaced { "replaced" } else { "saved" }
    ));
    Ok(())
}

pub fn logout(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "Token of profile `{profile}` removed from the system keychain. \
             Also revoke it in SourceCraft: Home -> Access -> Personal access tokens \
             (`mcp-{profile}`)."
        )),
        Err(AuthError::NotFound { .. }) => {
            ui.say(&format!("No token stored for profile `{profile}`."));
        }
        Err(err) => return Err(err.into()),
    }
    if forget {
        if repo.remove(profile)? {
            ui.say(&format!(
                "Profile `{profile}` removed from {}. The MCP client config still references it; \
                 remove the `sourcecraft` server there if you no longer need it.",
                repo.path().display()
            ));
        } else {
            ui.say(&format!("Profile `{profile}` has no saved settings."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sourcecraft_api::ApiToken;
    use sourcecraft_api::auth::MemoryStore;

    use super::*;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::testing::FakeVerifier;

    fn verifier() -> FakeVerifier {
        FakeVerifier {
            valid_token: "good-token".into(),
        }
    }

    #[tokio::test]
    async fn login_status_logout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let mut ui = ScriptedUi::default();

        status(&store, &repo, &mut ui, "ci").unwrap();
        assert!(ui.text().contains("Token:    not found"));

        ui.tokens = ["good-token".to_owned()].into();
        login(&store, &repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap();
        assert_eq!(store.get("ci").unwrap().expose(), "good-token");
        assert_eq!(repo.get("ci").unwrap(), Some(ProfileSettings::default()));

        let mut ui = ScriptedUi::default();
        status(&store, &repo, &mut ui, "ci").unwrap();
        let text = ui.text();
        assert!(text.contains("Token:    stored"), "{text}");
        assert!(text.contains("Write:    no"), "{text}");
        assert!(!text.contains("good-token"));

        let mut ui = ScriptedUi::default();
        logout(&store, &repo, &mut ui, "ci", false).unwrap();
        assert!(!store.contains("ci").unwrap());
        assert!(repo.get("ci").unwrap().is_some());

        // Повторный logout — не ошибка; --forget удаляет настройки.
        logout(&store, &repo, &mut ui, "ci", true).unwrap();
        assert!(ui.text().contains("No token stored"));
        assert_eq!(repo.get("ci").unwrap(), None);
    }

    #[tokio::test]
    async fn login_keeps_existing_settings_and_replaces_token() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let settings = ProfileSettings {
            client: Some("codex".into()),
            allow_write: true,
            ..Default::default()
        };
        repo.upsert("codex", settings.clone()).unwrap();
        store.set("codex", &ApiToken::new("old")).unwrap();

        let mut ui = ScriptedUi {
            tokens: ["new".to_owned()].into(),
            ..Default::default()
        };
        login::<FakeVerifier>(&store, &repo, None, &mut ui, "codex")
            .await
            .unwrap();
        assert_eq!(store.get("codex").unwrap().expose(), "new");
        assert_eq!(repo.get("codex").unwrap(), Some(settings));
        assert!(ui.text().contains("Token replaced"));
    }

    #[tokio::test]
    async fn login_rejects_invalid_token() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let mut ui = ScriptedUi {
            tokens: ["bad".to_owned()].into(),
            ..Default::default()
        };
        assert!(
            login(&store, &repo, Some(&verifier()), &mut ui, "ci")
                .await
                .is_err()
        );
        assert!(!store.contains("ci").unwrap());
        assert_eq!(repo.get("ci").unwrap(), None);
    }
}
