//! `secret copy` — копирование секрета между репозиториями и системным хранилищем без вывода
//! значения.
//!
//! Логика копирования общая с MCP-инструментом `secret_copy` (`sourcecraft_mcp_core::
//! secret_copy`). Отличия CLI: команду запускает человек, поэтому `keychain://` разрешён и как
//! источник, и как назначение (кроме записей с токенами этого сервера), а перезапись
//! существующего секрета включает `--force`, а не уровень доступа сервера.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use clap::Args;
use sourcecraft_api::executor::ApiClient;
use sourcecraft_api::keychain::Keychain;
use sourcecraft_mcp_core::secret_copy::{CopyPolicy, CopyReport, SecretRef, copy_secret};

/// Сколько ждать завершения записи секрета в SourceCraft.
const WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Args)]
pub struct CopyArgs {
    /// Token profile stored in the system keychain.
    #[arg(long, env = super::PROFILE_ENV, default_value = "default")]
    pub profile: String,
    /// SourceCraft API URL [default: $SOURCECRAFT_BASE_URL or https://api.sourcecraft.tech].
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
    /// Overwrite the destination if it already exists.
    #[arg(long)]
    pub force: bool,
    /// Secret to copy: `sc://<org>/<repo>/<KEY>[?group=<name>]` or
    /// `keychain://<service>/<account>`.
    pub source: String,
    /// Where to copy it, in the same forms.
    pub destination: String,
}

/// Разбирает ссылки до обращения к keychain и API.
pub fn parse_refs(args: &CopyArgs) -> anyhow::Result<(SecretRef, SecretRef)> {
    let parse = |text: &str| SecretRef::parse(text).map_err(|err| anyhow!(err.to_string()));
    Ok((parse(&args.source)?, parse(&args.destination)?))
}

/// Копирует секрет. Значение не печатается; ошибки его не содержат.
pub async fn copy(
    client: &ApiClient,
    keychain: Arc<dyn Keychain>,
    source: &SecretRef,
    destination: &SecretRef,
    force: bool,
) -> anyhow::Result<CopyReport> {
    let policy = CopyPolicy {
        allow_overwrite: force,
        overwrite_hint: "pass --force to replace it".to_owned(),
        keychain: Some(keychain),
        keychain_destination: true,
        keychain_hint: String::new(),
        wait: WAIT,
    };
    copy_secret(client, source, destination, &policy)
        .await
        .map_err(|err| anyhow!(err.to_string()))
}

/// Строка итога для пользователя.
pub fn describe(report: &CopyReport) -> String {
    let mut line = format!(
        "Copied {} to {}: {}",
        report.source,
        report.destination,
        report.outcome.as_str()
    );
    if let Some(operation) = &report.operation {
        line.push_str(&format!(
            " (operation {}: {})",
            operation.id.as_deref().unwrap_or("-"),
            if operation.status.is_empty() {
                "unknown"
            } else {
                &operation.status
            }
        ));
        if !operation.is_finished() {
            line.push_str("; the write is still in progress");
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sourcecraft_api::keychain::{KeychainItem, MemoryKeychain};
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::fetch::tests::client;
    use super::*;

    fn r(text: &str) -> SecretRef {
        SecretRef::parse(text).unwrap()
    }

    #[tokio::test]
    async fn copies_keychain_item_to_repository_and_back() {
        let server = MockServer::start().await;
        // Назначение пустое: секрет создаётся.
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/secrets"))
            .and(query_param("keys", "DEPLOY_KEY"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"secrets": []})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/repos/acme/web/secrets/DEPLOY_KEY"))
            .and(body_json(json!({"value": "bG9jYWwtc2VjcmV0"})))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "operation_id": "op-1", "status": "success"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let keychain = Arc::new(MemoryKeychain::new());
        let item = KeychainItem::new("deploy", "prod");
        keychain.set(&item, b"local-secret").unwrap();
        let report = copy(
            &client(&server),
            keychain.clone(),
            &r("keychain://deploy/prod"),
            &r("sc://acme/web/DEPLOY_KEY"),
            false,
        )
        .await
        .unwrap();
        let line = describe(&report);
        assert_eq!(
            line,
            "Copied keychain://deploy/prod to sc://acme/web/DEPLOY_KEY: created \
             (operation op-1: success)"
        );
        assert!(!line.contains("local-secret"));
    }

    #[tokio::test]
    async fn keychain_destination_needs_force_to_overwrite() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/secrets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "secrets": [{"key": "K", "value": "cmVwby12YWx1ZQ=="}]
            })))
            .mount(&server)
            .await;
        let keychain = Arc::new(MemoryKeychain::new());
        let item = KeychainItem::new("svc", "acct");
        keychain.set(&item, b"old-value").unwrap();

        let err = copy(
            &client(&server),
            keychain.clone(),
            &r("sc://acme/web/K"),
            &r("keychain://svc/acct"),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("--force"), "{err}");
        assert!(!err.contains("repo-value"), "{err}");
        assert_eq!(keychain.get(&item).unwrap().unwrap().expose(), b"old-value");

        let report = copy(
            &client(&server),
            keychain.clone(),
            &r("sc://acme/web/K"),
            &r("keychain://svc/acct"),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            describe(&report),
            "Copied sc://acme/web/K to keychain://svc/acct: overwritten"
        );
        assert_eq!(
            keychain.get(&item).unwrap().unwrap().expose(),
            b"repo-value"
        );
    }

    #[tokio::test]
    async fn token_entries_are_never_touched() {
        let server = MockServer::start().await;
        let keychain = Arc::new(MemoryKeychain::new());
        for (source, destination) in [
            (
                "keychain://dev.filipov.sourcecraft-mcp/codex",
                "sc://acme/web/K",
            ),
            (
                "sc://acme/web/K",
                "keychain://dev.filipov.sourcecraft-mcp/codex",
            ),
        ] {
            let err = copy(
                &client(&server),
                keychain.clone(),
                &r(source),
                &r(destination),
                true,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains("API token"), "{err}");
        }
        // Ни одного запроса к API: проверка до чтения источника.
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
