//! Адрес SourceCraft API.
//!
//! Адрес можно переопределить переменной окружения `SOURCECRAFT_BASE_URL` (например, для
//! тестового стенда) или флагом `serve --base-url`. Токен из окружения не читается никогда.

use anyhow::Context;
use sourcecraft_api::DEFAULT_BASE_URL;

/// Переменная окружения с адресом API.
pub const BASE_URL_ENV: &str = "SOURCECRAFT_BASE_URL";

/// Адрес API: `SOURCECRAFT_BASE_URL`, если задана и не пуста, иначе [`DEFAULT_BASE_URL`].
pub fn default_base_url() -> anyhow::Result<url::Url> {
    let from_env = std::env::var(BASE_URL_ENV).ok();
    resolve_base_url(None, from_env.as_deref())
}

/// Выбор адреса: явный (флаг) → из окружения → по умолчанию.
pub fn resolve_base_url(
    explicit: Option<&str>,
    from_env: Option<&str>,
) -> anyhow::Result<url::Url> {
    let (value, source) = match (
        explicit.map(str::trim).filter(|v| !v.is_empty()),
        from_env.map(str::trim).filter(|v| !v.is_empty()),
    ) {
        (Some(value), _) => (value, "--base-url"),
        (None, Some(value)) => (value, BASE_URL_ENV),
        (None, None) => (DEFAULT_BASE_URL, "the default"),
    };
    url::Url::parse(value).with_context(|| format!("invalid SourceCraft API URL from {source}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence() {
        assert_eq!(
            resolve_base_url(None, None).unwrap().as_str(),
            "https://api.sourcecraft.tech/"
        );
        assert_eq!(
            resolve_base_url(None, Some("http://127.0.0.1:9/api"))
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9/api"
        );
        assert_eq!(
            resolve_base_url(Some("https://example.test"), Some("http://127.0.0.1:9"))
                .unwrap()
                .as_str(),
            "https://example.test/"
        );
        assert_eq!(
            resolve_base_url(Some(" "), Some("")).unwrap().as_str(),
            "https://api.sourcecraft.tech/"
        );
        let err = resolve_base_url(Some("not a url"), None).unwrap_err();
        assert!(err.to_string().contains("--base-url"));
    }
}
