//! Профили токенов: настройки в `~/.config/sourcecraft-mcp/profiles.toml`, токен — в системном
//! хранилище под именем профиля.
//!
//! В файле профилей нет токенов и других секретов: только то, что `connect` зарегистрировал
//! в MCP-клиенте.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sourcecraft_api::ApiToken;
use sourcecraft_api::auth::{AuthError, KeyringStore, TokenStore};

/// Переменная окружения, которая переопределяет каталог конфигурации (нужна для тестов).
pub const CONFIG_DIR_ENV: &str = "SOURCECRAFT_MCP_CONFIG_DIR";

/// Имя файла профилей в каталоге конфигурации.
pub const PROFILES_FILE: &str = "profiles.toml";

/// Максимальная длина имени профиля.
const MAX_PROFILE_LEN: usize = 64;

/// Настройки профиля. Токена здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// MCP-клиент, в котором зарегистрирован профиль (`claude-code`, `codex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Разрешены операции записи (`serve --allow-write`).
    pub allow_write: bool,
    /// Разрешены удаляющие операции (`serve --allow-destructive`).
    pub allow_destructive: bool,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Разрешено раскрывать значения секретов по явному запросу (`serve --allow-secret-values`).
    pub allow_secret_values: bool,
}

/// Содержимое `profiles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfilesDocument {
    profiles: BTreeMap<String, ProfileSettings>,
}

/// Файл профилей.
#[derive(Debug, Clone)]
pub struct ProfileRepo {
    path: PathBuf,
}

impl ProfileRepo {
    /// Файл профилей в каталоге `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(PROFILES_FILE),
        }
    }

    /// Файл профилей в каталоге конфигурации пользователя.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::in_dir(config_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<ProfilesDocument> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        toml::from_str(&text).with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, document: &ProfilesDocument) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(document).context("failed to serialize profiles")?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Настройки профиля, если он есть в файле.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<ProfileSettings>> {
        Ok(self.load()?.profiles.remove(name))
    }

    /// Создаёт или заменяет профиль.
    pub fn upsert(&self, name: &str, settings: ProfileSettings) -> anyhow::Result<()> {
        validate_profile_name(name)?;
        let mut document = self.load()?;
        document.profiles.insert(name.to_owned(), settings);
        self.save(&document)
    }

    /// Удаляет профиль. Возвращает `true`, если профиль был.
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut document = self.load()?;
        let removed = document.profiles.remove(name).is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

/// Каталог конфигурации: `SOURCECRAFT_MCP_CONFIG_DIR`, иначе `~/.config/sourcecraft-mcp`.
///
/// На macOS тоже используется `~/.config`, а не `~/Library/Application Support`: так путь
/// одинаков на macOS и Linux. На Linux учитывается `XDG_CONFIG_HOME`.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join(".config"))
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("sourcecraft-mcp"))
        .context("cannot determine the user configuration directory")
}

/// Проверяет имя профиля. Имя попадает в аргументы команд MCP-клиентов, ключи TOML и имя
/// записи в keychain, поэтому допускаются только `[A-Za-z0-9._-]` и имя не начинается с `-`.
pub fn validate_profile_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid profile name `{name}`: use 1-{MAX_PROFILE_LEN} characters from A-Z, a-z, 0-9, `.`, `_`, `-`, not starting with `-`"
        );
    }
    Ok(())
}

/// Загружает профиль для `serve`: настройки из `profiles.toml` и токен из системного хранилища.
///
/// Если профиля нет в файле, но токен сохранён (например, через `auth login`), возвращаются
/// настройки по умолчанию (только чтение).
pub fn load_profile(name: &str) -> anyhow::Result<(ProfileSettings, ApiToken)> {
    load_profile_with(&ProfileRepo::open_default()?, &KeyringStore, name)
}

/// То же, что [`load_profile`], но отсутствие токена превращается в понятную пользователю
/// ошибку с подсказкой, как его сохранить (для `serve` и `secret`).
pub fn load_profile_explained(name: &str) -> anyhow::Result<(ProfileSettings, ApiToken)> {
    load_profile(name).map_err(|err| match err.downcast_ref::<AuthError>() {
        Some(AuthError::NotFound { .. }) => anyhow::anyhow!(
            "no SourceCraft token is stored for profile `{name}`. Store one with \
             `sourcecraft-mcp auth login --profile {name}` or register a client with \
             `sourcecraft-mcp connect <client> --profile {name}`"
        ),
        _ => err.context(format!("cannot load profile `{name}`")),
    })
}

/// То же, что [`load_profile`], с явными файлом профилей и хранилищем токенов.
pub fn load_profile_with(
    repo: &ProfileRepo,
    store: &dyn TokenStore,
    name: &str,
) -> anyhow::Result<(ProfileSettings, ApiToken)> {
    validate_profile_name(name)?;
    let settings = repo.get(name)?.unwrap_or_default();
    let token = store.get(name)?;
    Ok((settings, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcecraft_api::auth::MemoryStore;

    fn sample() -> ProfileSettings {
        ProfileSettings {
            client: Some("codex".into()),
            allow_write: true,
            allow_destructive: false,
            toolsets: vec!["repos".into(), "issues".into()],
            allow_secret_values: true,
        }
    }

    #[test]
    fn round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("nested"));
        assert_eq!(repo.get("codex").unwrap(), None);

        repo.upsert("codex", sample()).unwrap();
        repo.upsert("claude-code", ProfileSettings::default())
            .unwrap();
        assert_eq!(repo.get("codex").unwrap(), Some(sample()));
        assert_eq!(
            repo.get("claude-code").unwrap(),
            Some(ProfileSettings::default())
        );

        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("[profiles.codex]"), "{text}");
        assert!(text.contains("allow_write = true"), "{text}");

        assert!(repo.remove("codex").unwrap());
        assert!(!repo.remove("codex").unwrap());
        assert_eq!(repo.get("codex").unwrap(), None);
        assert!(repo.get("claude-code").unwrap().is_some());
    }

    #[test]
    fn parses_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(
            repo.path(),
            "# comment\n[profiles.ci]\nallow_write = true\n\n[profiles.empty]\n",
        )
        .unwrap();
        let ci = repo.get("ci").unwrap().unwrap();
        assert!(ci.allow_write);
        assert!(!ci.allow_destructive);
        assert!(!ci.allow_secret_values);
        assert!(ci.toolsets.is_empty());
        assert_eq!(repo.get("empty").unwrap(), Some(ProfileSettings::default()));
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(repo.path(), "[profiles.ci]\ntoken = \"x\"\n").unwrap();
        assert!(repo.get("ci").is_err());
    }

    #[test]
    fn validates_profile_names() {
        for good in ["codex", "claude-code", "ci_1", "a.b"] {
            validate_profile_name(good).unwrap();
        }
        for bad in ["", "-x", "a b", "a/b", "проф", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn load_profile_requires_token() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        repo.upsert("codex", sample()).unwrap();

        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(err.to_string().contains("не найден"), "{err}");

        store.set("codex", &ApiToken::new("t")).unwrap();
        let (settings, token) = load_profile_with(&repo, &store, "codex").unwrap();
        assert_eq!(settings, sample());
        assert_eq!(token.expose(), "t");

        // Токен без записи в profiles.toml — настройки по умолчанию.
        store.set("bare", &ApiToken::new("t2")).unwrap();
        let (settings, _) = load_profile_with(&repo, &store, "bare").unwrap();
        assert_eq!(settings, ProfileSettings::default());
    }
}
