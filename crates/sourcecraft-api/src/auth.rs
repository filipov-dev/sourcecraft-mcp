//! Хранение токенов SourceCraft в системном хранилище.
//!
//! Каждый профиль (например, `claude-code` или `codex`) хранит свой токен отдельной записью:
//! сервис [`KEYRING_SERVICE`], имя учётной записи — имя профиля. Так токены разных агентов
//! можно отзывать и заменять независимо.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::token::ApiToken;

/// Имя сервиса записей в Keychain / Credential Manager / Secret Service.
pub const KEYRING_SERVICE: &str = "dev.filipov.sourcecraft-mcp";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("токен для профиля `{profile}` не найден; выполните `sourcecraft-mcp connect`")]
    NotFound { profile: String },
    #[error("ошибка системного хранилища токенов: {0}")]
    Backend(String),
}

/// Хранилище токенов по профилям.
pub trait TokenStore: Send + Sync {
    fn get(&self, profile: &str) -> Result<ApiToken, AuthError>;
    fn set(&self, profile: &str, token: &ApiToken) -> Result<(), AuthError>;
    fn delete(&self, profile: &str) -> Result<(), AuthError>;

    /// Есть ли токен у профиля. Значение токена наружу не возвращается.
    fn contains(&self, profile: &str) -> Result<bool, AuthError> {
        match self.get(profile) {
            Ok(_) => Ok(true),
            Err(AuthError::NotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Хранилище на базе системного keyring (Keychain на macOS, Credential Manager на Windows,
/// Secret Service на Linux).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(profile: &str) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYRING_SERVICE, profile).map_err(|err| map_error(profile, err))
    }
}

/// Переводит ошибку keyring в [`AuthError`].
///
/// Значение токена в текст ошибки не попадает.
fn map_error(profile: &str, err: keyring::Error) -> AuthError {
    match err {
        keyring::Error::NoEntry => AuthError::NotFound {
            profile: profile.to_owned(),
        },
        // `BadEncoding` содержит байты сохранённого значения — их не выводим.
        keyring::Error::BadEncoding(_) => {
            AuthError::Backend("сохранённый токен не является корректной строкой UTF-8".into())
        }
        other => AuthError::Backend(other.to_string()),
    }
}

impl TokenStore for KeyringStore {
    fn get(&self, profile: &str) -> Result<ApiToken, AuthError> {
        let password = Self::entry(profile)?
            .get_password()
            .map_err(|err| map_error(profile, err))?;
        Ok(ApiToken::new(password))
    }

    fn set(&self, profile: &str, token: &ApiToken) -> Result<(), AuthError> {
        Self::entry(profile)?
            .set_password(token.expose())
            .map_err(|err| map_error(profile, err))
    }

    fn delete(&self, profile: &str) -> Result<(), AuthError> {
        Self::entry(profile)?
            .delete_credential()
            .map_err(|err| map_error(profile, err))
    }
}

/// Хранилище в памяти для тестов: системный keychain в автотестах не используется.
#[derive(Debug, Default)]
pub struct MemoryStore {
    tokens: Mutex<HashMap<String, ApiToken>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, ApiToken>>, AuthError> {
        self.tokens
            .lock()
            .map_err(|_| AuthError::Backend("хранилище в памяти повреждено".into()))
    }
}

impl TokenStore for MemoryStore {
    fn get(&self, profile: &str) -> Result<ApiToken, AuthError> {
        self.lock()?
            .get(profile)
            .cloned()
            .ok_or_else(|| AuthError::NotFound {
                profile: profile.to_owned(),
            })
    }

    fn set(&self, profile: &str, token: &ApiToken) -> Result<(), AuthError> {
        self.lock()?.insert(profile.to_owned(), token.clone());
        Ok(())
    }

    fn delete(&self, profile: &str) -> Result<(), AuthError> {
        self.lock()?
            .remove(profile)
            .map(|_| ())
            .ok_or_else(|| AuthError::NotFound {
                profile: profile.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::new();
        assert!(!store.contains("codex").unwrap());
        assert!(matches!(
            store.get("codex"),
            Err(AuthError::NotFound { profile }) if profile == "codex"
        ));

        store.set("codex", &ApiToken::new("t1")).unwrap();
        store.set("claude-code", &ApiToken::new("t2")).unwrap();
        assert_eq!(store.get("codex").unwrap().expose(), "t1");
        assert!(store.contains("claude-code").unwrap());

        store.delete("codex").unwrap();
        assert!(!store.contains("codex").unwrap());
        assert!(matches!(
            store.delete("codex"),
            Err(AuthError::NotFound { .. })
        ));
        assert_eq!(store.get("claude-code").unwrap().expose(), "t2");
    }

    #[test]
    fn keyring_errors_are_mapped_without_values() {
        assert!(matches!(
            map_error("p", keyring::Error::NoEntry),
            AuthError::NotFound { profile } if profile == "p"
        ));
        let err = map_error("p", keyring::Error::BadEncoding(b"secret-bytes".to_vec()));
        assert!(!err.to_string().contains("secret"));
    }

    /// Проверка с настоящим системным хранилищем: запускать вручную (`cargo test -- --ignored`).
    #[test]
    #[ignore = "uses the real system keychain"]
    fn keyring_store_round_trip() {
        let store = KeyringStore;
        let profile = "sourcecraft-mcp-test-profile";
        let _ = store.delete(profile);
        assert!(!store.contains(profile).unwrap());
        store.set(profile, &ApiToken::new("test-value")).unwrap();
        assert_eq!(store.get(profile).unwrap().expose(), "test-value");
        store.delete(profile).unwrap();
        assert!(!store.contains(profile).unwrap());
    }
}
