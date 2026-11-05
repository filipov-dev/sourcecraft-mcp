//! Чтение секретов репозитория для команд `secret`.
//!
//! Сам запрос и разбор ответа — в ядре (`sourcecraft_mcp_core::repo_secrets`, общее с
//! инструментом `secret_copy`); здесь — разбор `--repo`/`--repo-id` и перевод ошибок в `anyhow`.
//! Тексты ошибок ядра значений не содержат.

use anyhow::{anyhow, bail};
use sourcecraft_api::executor::ApiClient;
pub use sourcecraft_mcp_core::repo_secrets::{RepoTarget, Secrets};

/// Репозиторий из `--repo <org>/<repo>` или `--repo-id <ID>` (ровно один из них).
pub fn repo_target(repo: Option<&str>, repo_id: Option<&str>) -> anyhow::Result<RepoTarget> {
    match (repo, repo_id) {
        (Some(repo), None) => {
            let parts: Vec<&str> = repo.trim().split('/').collect();
            match parts.as_slice() {
                [org, name] if !org.is_empty() && !name.is_empty() => Ok(RepoTarget::Slug {
                    org: (*org).to_owned(),
                    repo: (*name).to_owned(),
                }),
                _ => bail!("--repo must be `<org>/<repo>`, got `{repo}`"),
            }
        }
        (None, Some(id)) if !id.trim().is_empty() => Ok(RepoTarget::Id(id.trim().to_owned())),
        (None, Some(_)) => bail!("--repo-id must not be empty"),
        (Some(_), Some(_)) => bail!("pass either --repo or --repo-id, not both"),
        (None, None) => bail!("pass --repo <org>/<repo> or --repo-id <ID>"),
    }
}

/// Читает секреты репозитория. `keys` — фильтр по ключам (пусто — все секреты).
pub async fn fetch_secrets(
    client: &ApiClient,
    target: &RepoTarget,
    group: Option<&str>,
    keys: &[String],
) -> anyhow::Result<Secrets> {
    sourcecraft_mcp_core::repo_secrets::fetch_secrets(client, target, group, keys)
        .await
        .map_err(|err| anyhow!(err.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sourcecraft_api::ApiToken;
    use sourcecraft_api::executor::ClientConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    pub(crate) fn client(server: &MockServer) -> ApiClient {
        ApiClient::new(ClientConfig {
            base_url: url::Url::parse(&server.uri()).unwrap(),
            token: Some(ApiToken::new("test-token")),
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
        })
        .unwrap()
    }

    #[test]
    fn parses_repo_target() {
        assert_eq!(
            repo_target(Some("acme/web"), None).unwrap(),
            RepoTarget::Slug {
                org: "acme".into(),
                repo: "web".into()
            }
        );
        assert_eq!(
            repo_target(None, Some(" 42 ")).unwrap(),
            RepoTarget::Id("42".into())
        );
        for bad in ["acme", "acme/", "/web", "a/b/c", ""] {
            assert!(repo_target(Some(bad), None).is_err(), "{bad}");
        }
        assert!(repo_target(None, None).is_err());
        assert!(repo_target(Some("a/b"), Some("1")).is_err());
    }

    #[tokio::test]
    async fn api_errors_hide_values() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/secrets"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"message": "denied", "value": "czNjcmV0"})),
            )
            .mount(&server)
            .await;
        let target = repo_target(Some("acme/web"), None).unwrap();
        let err = fetch_secrets(&client(&server), &target, None, &[])
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(!err.contains("czNjcmV0"), "{err}");
    }
}
