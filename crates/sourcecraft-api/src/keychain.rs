//! Произвольные записи системного хранилища (Keychain и аналоги) — для копирования секретов
//! (`keychain://<service>/<account>`).
//!
//! Записи с токенами этого сервера (сервис [`KEYRING_SERVICE`]) недоступны ни на чтение, ни на
//! запись: иначе через копирование секрета токен агента можно было бы вынести в репозиторий,
//! а запись — подменить чужой токен. Проверка — и здесь, и в вызывающем коде.
//!
//! Значения не попадают в `Debug`, ошибки и логи.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::auth::KEYRING_SERVICE;

/// Запись системного хранилища: сервис и учётная запись.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeychainItem {
    pub service: String,
    pub account: String,
}

impl KeychainItem {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    /// Запись относится к хранилищу токенов этого сервера. Сравнение без учёта регистра и
    /// пробелов по краям: на некоторых платформах имена сервисов регистронезависимы.
    pub fn is_token_entry(&self) -> bool {
        self.service.trim().eq_ignore_ascii_case(KEYRING_SERVICE)
    }
}

impl fmt::Display for KeychainItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "keychain://{}/{}", self.service, self.account)
    }
}

/// Значение секрета. Не раскрывается в `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretValue([redacted], {} bytes)", self.0.len())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain item `{0}` holds a sourcecraft-mcp token and cannot be read or written here")]
    TokenEntry(KeychainItem),
    #[error("system keychain error for `{item}`: {message}")]
    Backend { item: KeychainItem, message: String },
}

/// Доступ к записям системного хранилища.
pub trait Keychain: Send + Sync + fmt::Debug {
    /// Значение записи; `None` — записи нет.
    fn get(&self, item: &KeychainItem) -> Result<Option<SecretValue>, KeychainError>;
    /// Создаёт или заменяет запись.
    fn set(&self, item: &KeychainItem, value: &[u8]) -> Result<(), KeychainError>;
}

fn guard(item: &KeychainItem) -> Result<(), KeychainError> {
    if item.is_token_entry() {
        return Err(KeychainError::TokenEntry(item.clone()));
    }
    Ok(())
}

/// Системное хранилище (Keychain на macOS, Credential Manager на Windows, Secret Service на
/// Linux).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemKeychain;

impl SystemKeychain {
    fn entry(item: &KeychainItem) -> Result<keyring::Entry, KeychainError> {
        keyring::Entry::new(&item.service, &item.account).map_err(|err| backend(item, err))
    }
}

/// Ошибка keyring без значения записи (`BadEncoding` содержит байты значения).
fn backend(item: &KeychainItem, err: keyring::Error) -> KeychainError {
    let message = match err {
        keyring::Error::BadEncoding(_) => "the stored value has an unexpected encoding".to_owned(),
        other => other.to_string(),
    };
    KeychainError::Backend {
        item: item.clone(),
        message,
    }
}

impl Keychain for SystemKeychain {
    fn get(&self, item: &KeychainItem) -> Result<Option<SecretValue>, KeychainError> {
        guard(item)?;
        match Self::entry(item)?.get_secret() {
            Ok(value) => Ok(Some(SecretValue::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(backend(item, err)),
        }
    }

    fn set(&self, item: &KeychainItem, value: &[u8]) -> Result<(), KeychainError> {
        guard(item)?;
        Self::entry(item)?
            .set_secret(value)
            .map_err(|err| backend(item, err))
    }
}

/// Хранилище в памяти для тестов.
#[derive(Debug, Default)]
pub struct MemoryKeychain {
    items: Mutex<HashMap<KeychainItem, Vec<u8>>>,
}

impl MemoryKeychain {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(
        &self,
        item: &KeychainItem,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<KeychainItem, Vec<u8>>>, KeychainError> {
        self.items.lock().map_err(|_| KeychainError::Backend {
            item: item.clone(),
            message: "in-memory keychain is poisoned".to_owned(),
        })
    }
}

impl Keychain for MemoryKeychain {
    fn get(&self, item: &KeychainItem) -> Result<Option<SecretValue>, KeychainError> {
        guard(item)?;
        Ok(self.lock(item)?.get(item).cloned().map(SecretValue::new))
    }

    fn set(&self, item: &KeychainItem, value: &[u8]) -> Result<(), KeychainError> {
        guard(item)?;
        self.lock(item)?.insert(item.clone(), value.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_entries_are_refused() {
        let keychain = MemoryKeychain::new();
        for service in [KEYRING_SERVICE, " DEV.Filipov.SourceCraft-MCP "] {
            let item = KeychainItem::new(service, "codex");
            assert!(item.is_token_entry());
            assert!(matches!(
                keychain.get(&item),
                Err(KeychainError::TokenEntry(_))
            ));
            assert!(matches!(
                keychain.set(&item, b"x"),
                Err(KeychainError::TokenEntry(_))
            ));
            assert!(matches!(
                SystemKeychain.get(&item),
                Err(KeychainError::TokenEntry(_))
            ));
        }
    }

    #[test]
    fn memory_round_trip_and_debug_hides_value() {
        let keychain = MemoryKeychain::new();
        let item = KeychainItem::new("deploy", "prod");
        assert_eq!(item.to_string(), "keychain://deploy/prod");
        assert!(keychain.get(&item).unwrap().is_none());
        keychain.set(&item, b"s3cret-value").unwrap();
        let value = keychain.get(&item).unwrap().unwrap();
        assert_eq!(value.expose(), b"s3cret-value");
        assert!(!format!("{value:?}").contains("s3cret"));
        let err = backend(&item, keyring::Error::BadEncoding(b"s3cret".to_vec()));
        assert!(!err.to_string().contains("s3cret"));
    }
}
