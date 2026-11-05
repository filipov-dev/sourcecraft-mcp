//! Копирование секрета (`secret_copy` и `sourcecraft-mcp secret copy`): значение читается и
//! записывается сервером и не проходит через контекст агента.
//!
//! Ссылки:
//! - `sc://<org>/<repo>/<KEY>[?group=<name>]` — секрет репозитория SourceCraft;
//! - `keychain://<service>/<account>` — запись системного хранилища этой машины.
//!
//! Правила безопасности:
//! - **Уровень доступа.** Запись нового ключа — `Write`; перезапись существующего секрета —
//!   `Destructive` (как `set_secret`): старое значение не вернуть. Существование проверяется
//!   чтением списка секретов назначения перед записью (проверка и запись не атомарны).
//! - **Значения секретов не раскрываются**, поэтому `allow_secret_values` не нужен: значение
//!   не попадает ни в результат, ни в ошибки (они вычищаются от значения и его base64), ни в
//!   логи — в лог пишутся только ссылки и результат (`created`/`overwritten`). Копировать
//!   можно только между репозиториями, к секретам которых у токена уже есть доступ.
//! - **`keychain://`** читает локальные записи и отправляет их в удалённый репозиторий: для
//!   агента это путь утечки данных с машины. В MCP-инструменте источник `keychain://`
//!   работает, только если сервер передал доступ к хранилищу ([`CopyPolicy::keychain`],
//!   флаг `serve --allow-keychain-source`); назначением `keychain://` агент быть не может
//!   (запись в хранилище машины). CLI запускает человек: там разрешено и то и другое. Записи
//!   с токенами этого сервера недоступны всегда ([`KeychainItem::is_token_entry`]).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use sourcecraft_api::executor::ApiClient;
use sourcecraft_api::keychain::{Keychain, KeychainItem, SecretValue};
use sourcecraft_api::spec::AccessLevel;

use crate::error::ToolError;
use crate::repo_secrets::{RepoTarget, SecretOperation, fetch_secrets, set_secret, wait_operation};
use crate::secrets::sensitive_forms;

/// Имя инструмента.
pub const SECRET_COPY_TOOL: &str = "secret_copy";

const ARGUMENTS: &[&str] = &["source", "destination"];

/// Интервал опроса операции записи.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Ссылка на секрет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// Секрет репозитория SourceCraft.
    Repo {
        org: String,
        repo: String,
        key: String,
        group: Option<String>,
    },
    /// Запись системного хранилища.
    Keychain(KeychainItem),
}

impl SecretRef {
    /// Разбирает `sc://<org>/<repo>/<KEY>[?group=<name>]` или `keychain://<service>/<account>`.
    pub fn parse(text: &str) -> Result<Self, ToolError> {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix("sc://") {
            let (path, query) = match rest.split_once('?') {
                Some((path, query)) => (path, Some(query)),
                None => (rest, None),
            };
            let group = match query {
                None => None,
                Some(query) => match query.strip_prefix("group=") {
                    Some(group) if !group.is_empty() && !group.contains(['&', '=']) => {
                        Some(group.to_owned())
                    }
                    _ => {
                        return Err(ToolError::new(format!(
                            "`{text}`: the only supported query is `?group=<name>`"
                        )));
                    }
                },
            };
            let parts: Vec<&str> = path.split('/').collect();
            return match parts.as_slice() {
                [org, repo, key] if [org, repo, key].iter().all(|p| !p.is_empty()) => {
                    Ok(Self::Repo {
                        org: (*org).to_owned(),
                        repo: (*repo).to_owned(),
                        key: (*key).to_owned(),
                        group,
                    })
                }
                _ => Err(ToolError::new(format!(
                    "`{text}` is not a repository secret reference; expected \
                     `sc://<org>/<repo>/<KEY>` (optionally `?group=<name>`)"
                ))),
            };
        }
        if let Some(rest) = text.strip_prefix("keychain://") {
            return match rest.split_once('/') {
                Some((service, account)) if !service.is_empty() && !account.is_empty() => {
                    Ok(Self::Keychain(KeychainItem::new(service, account)))
                }
                _ => Err(ToolError::new(format!(
                    "`{text}` is not a keychain reference; expected \
                     `keychain://<service>/<account>`"
                ))),
            };
        }
        Err(ToolError::new(format!(
            "`{text}` is not a secret reference; use `sc://<org>/<repo>/<KEY>` for a repository \
             secret or `keychain://<service>/<account>` for a local keychain item"
        )))
    }

    fn repo_target(org: &str, repo: &str) -> RepoTarget {
        RepoTarget::Slug {
            org: org.to_owned(),
            repo: repo.to_owned(),
        }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repo {
                org,
                repo,
                key,
                group,
            } => {
                write!(f, "sc://{org}/{repo}/{key}")?;
                if let Some(group) = group {
                    write!(f, "?group={group}")?;
                }
                Ok(())
            }
            Self::Keychain(item) => item.fmt(f),
        }
    }
}

/// Что разрешено при копировании.
#[derive(Debug, Clone)]
pub struct CopyPolicy {
    /// Можно перезаписать существующий секрет (`Destructive` или `--force` в CLI).
    pub allow_overwrite: bool,
    /// Подсказка, как разрешить перезапись.
    pub overwrite_hint: String,
    /// Доступ к системному хранилищу; `None` — ссылки `keychain://` отклоняются.
    pub keychain: Option<Arc<dyn Keychain>>,
    /// Можно ли записывать в `keychain://` (только CLI).
    pub keychain_destination: bool,
    /// Подсказка, как разрешить `keychain://`, когда хранилище недоступно.
    pub keychain_hint: String,
    /// Сколько ждать завершения операции записи в SourceCraft.
    pub wait: Duration,
}

/// Результат записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    Created,
    Overwritten,
}

impl CopyOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Overwritten => "overwritten",
        }
    }
}

/// Итог копирования (без значения).
#[derive(Debug, Clone)]
pub struct CopyReport {
    pub source: String,
    pub destination: String,
    pub outcome: CopyOutcome,
    /// Операция записи в SourceCraft; `None` для `keychain://`.
    pub operation: Option<SecretOperation>,
}

impl CopyReport {
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "source": self.source,
            "destination": self.destination,
            "result": self.outcome.as_str(),
        });
        if let (Some(operation), Some(map)) = (&self.operation, out.as_object_mut()) {
            map.insert("operation".into(), operation.to_json());
            if !operation.is_finished() {
                map.insert(
                    "note".into(),
                    json!(
                        "The secret is still being written; poll the operation with \
                         `get_secret_operation`."
                    ),
                );
            }
        }
        out
    }
}

fn keychain<'a>(
    policy: &'a CopyPolicy,
    item: &KeychainItem,
) -> Result<&'a dyn Keychain, ToolError> {
    if item.is_token_entry() {
        return Err(ToolError::new(format!(
            "`{item}` holds a sourcecraft-mcp API token; it cannot be copied or overwritten"
        )));
    }
    policy
        .keychain
        .as_deref()
        .ok_or_else(|| ToolError::new(policy.keychain_hint.clone()))
}

/// Читает значение источника.
async fn read_source(
    client: &ApiClient,
    source: &SecretRef,
    policy: &CopyPolicy,
) -> Result<SecretValue, ToolError> {
    match source {
        SecretRef::Repo {
            org,
            repo,
            key,
            group,
        } => {
            let secrets = fetch_secrets(
                client,
                &SecretRef::repo_target(org, repo),
                group.as_deref(),
                std::slice::from_ref(key),
            )
            .await?;
            secrets
                .get(key)
                .map(SecretValue::new)
                .ok_or_else(|| ToolError::new(format!("source secret `{source}` does not exist")))
        }
        SecretRef::Keychain(item) => keychain(policy, item)?
            .get(item)
            .map_err(|err| ToolError::new(err.to_string()))?
            .ok_or_else(|| ToolError::new(format!("keychain item `{item}` does not exist"))),
    }
}

/// Есть ли уже секрет назначения.
async fn destination_exists(
    client: &ApiClient,
    destination: &SecretRef,
    policy: &CopyPolicy,
) -> Result<bool, ToolError> {
    match destination {
        SecretRef::Repo {
            org,
            repo,
            key,
            group,
        } => {
            // Значение существующего секрета приходит в ответе и сразу отбрасывается.
            let secrets = fetch_secrets(
                client,
                &SecretRef::repo_target(org, repo),
                group.as_deref(),
                std::slice::from_ref(key),
            )
            .await?;
            Ok(secrets.contains(key))
        }
        SecretRef::Keychain(item) => Ok(keychain(policy, item)?
            .get(item)
            .map_err(|err| ToolError::new(err.to_string()))?
            .is_some()),
    }
}

/// Копирует секрет `source` в `destination`. Значение не попадает ни в результат, ни в
/// ошибки, ни в логи.
pub async fn copy_secret(
    client: &ApiClient,
    source: &SecretRef,
    destination: &SecretRef,
    policy: &CopyPolicy,
) -> Result<CopyReport, ToolError> {
    if source == destination {
        return Err(ToolError::new(
            "the source and the destination are the same secret",
        ));
    }
    if let SecretRef::Keychain(item) = destination {
        if !policy.keychain_destination {
            return Err(ToolError::new(format!(
                "`{item}`: copying into the local keychain is not available here; the \
                 destination must be a repository secret (`sc://<org>/<repo>/<KEY>`)"
            )));
        }
        // Раньше чтения источника: запись с токеном не трогаем ни при каких флагах.
        keychain(policy, item)?;
    }

    let value = read_source(client, source, policy).await?;
    if value.expose().is_empty() {
        return Err(ToolError::new(format!(
            "source secret `{source}` is empty; SourceCraft does not store empty secrets"
        )));
    }
    let sensitive = sensitive_forms(value.expose());
    let scrubbed =
        |err: ToolError| ToolError::new(crate::secrets::scrub(err.message(), &sensitive));

    let exists = destination_exists(client, destination, policy)
        .await
        .map_err(scrubbed)?;
    if exists && !policy.allow_overwrite {
        return Err(ToolError::new(format!(
            "destination secret `{destination}` already exists and overwriting it is not \
             allowed: {}",
            policy.overwrite_hint
        )));
    }
    let outcome = if exists {
        CopyOutcome::Overwritten
    } else {
        CopyOutcome::Created
    };

    let operation = match destination {
        SecretRef::Repo {
            org,
            repo,
            key,
            group,
        } => {
            let target = SecretRef::repo_target(org, repo);
            let started = set_secret(client, &target, group.as_deref(), key, value.expose())
                .await
                .map_err(scrubbed)?;
            let operation = wait_operation(client, started, &sensitive, POLL_INTERVAL, policy.wait)
                .await
                .map_err(scrubbed)?;
            if matches!(operation.status.as_str(), "failed" | "cancel") {
                return Err(ToolError::new(format!(
                    "writing `{destination}` failed: {}",
                    operation.error.as_deref().unwrap_or(&operation.status)
                )));
            }
            Some(operation)
        }
        SecretRef::Keychain(item) => {
            keychain(policy, item)?
                .set(item, value.expose())
                .map_err(|err| scrubbed(ToolError::new(err.to_string())))?;
            None
        }
    };
    drop(value);

    tracing::info!(
        source = %source,
        destination = %destination,
        result = outcome.as_str(),
        "секрет скопирован"
    );
    Ok(CopyReport {
        source: source.to_string(),
        destination: destination.to_string(),
        outcome,
        operation,
    })
}

/// Источники `keychain://` в `secret_copy` на этом сервере.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeychainMode {
    /// Разрешены (`serve --allow-keychain-source`).
    Allowed,
    /// Выключены (по умолчанию); пользователь может включить флагом.
    Disabled,
    /// Недоступны: сервер удалённый (HTTP), хранилище его машины не имеет отношения к агенту.
    /// В описании инструмента `keychain://` не упоминается.
    Unavailable,
}

/// Описание инструмента `secret_copy`. `max_access` — уровень сервера: от него зависит,
/// может ли инструмент перезаписать секрет; `keychain` — разрешён ли `keychain://`.
pub fn secret_copy_tool(max_access: AccessLevel, keychain: KeychainMode) -> Tool {
    let overwrite = if max_access.allows(AccessLevel::Destructive) {
        "If the destination secret exists, it is overwritten (the old value cannot be restored): \
         confirm with the user first."
    } else {
        "Only new destination keys can be created: overwriting an existing secret needs the \
         server to run with --allow-destructive."
    };
    let keychain = match keychain {
        KeychainMode::Allowed => {
            " The source may also be a local keychain item, `keychain://<service>/<account>`; \
             use it only when the user asked to upload that item."
        }
        KeychainMode::Disabled => {
            " Local keychain sources (`keychain://…`) are disabled on this server."
        }
        KeychainMode::Unavailable => "",
    };
    let schema = json!({
        "type": "object",
        "properties": {
            "source": {
                "type": "string",
                "description": "Secret to copy: `sc://<org>/<repo>/<KEY>` (optionally \
                    `?group=<name>` for a non-default secret group)."
            },
            "destination": {
                "type": "string",
                "description": "Repository secret to write: `sc://<org>/<repo>/<KEY>` \
                    (optionally `?group=<name>`). The key may differ from the source key."
            }
        },
        "required": ["source", "destination"],
        "additionalProperties": false
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    let description = format!(
        "Copy a repository secret to another repository (or to another key) without revealing \
         it: the server reads the value and writes it, the value never enters the conversation \
         and is not shown in the result or errors. The result says whether the destination was \
         `created` or `overwritten` and the state of the write operation. \
         {overwrite}{keychain}\n\n\
         Access: write (destructive when it overwrites an existing secret)."
    );
    let mut tool = Tool::new(SECRET_COPY_TOOL, description, Arc::new(schema));
    tool.title = Some("Copy a secret between repositories".to_owned());
    tool.annotations = Some(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(max_access.allows(AccessLevel::Destructive))
            .idempotent(true),
    );
    tool
}

/// Ссылки из аргументов инструмента.
pub fn parse_args(args: &JsonObject) -> Result<(SecretRef, SecretRef), ToolError> {
    if let Some(key) = args.keys().find(|key| !ARGUMENTS.contains(&key.as_str())) {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; allowed arguments: `source`, `destination`"
        )));
    }
    let get = |name: &str| match args.get(name) {
        Some(Value::String(text)) => SecretRef::parse(text),
        None | Some(Value::Null) => Err(ToolError::new(format!(
            "missing required argument `{name}`"
        ))),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    };
    Ok((get("source")?, get("destination")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_references() {
        assert_eq!(
            SecretRef::parse(" sc://acme/web/API_TOKEN ").unwrap(),
            SecretRef::Repo {
                org: "acme".into(),
                repo: "web".into(),
                key: "API_TOKEN".into(),
                group: None
            }
        );
        let grouped = SecretRef::parse("sc://acme/web/K?group=ci").unwrap();
        assert_eq!(grouped.to_string(), "sc://acme/web/K?group=ci");
        let item = SecretRef::parse("keychain://deploy.example/prod/key").unwrap();
        assert_eq!(
            item,
            SecretRef::Keychain(KeychainItem::new("deploy.example", "prod/key"))
        );
        assert_eq!(item.to_string(), "keychain://deploy.example/prod/key");

        for bad in [
            "acme/web/K",
            "sc://acme/web",
            "sc://acme/web/K/x",
            "sc://acme//K",
            "sc://acme/web/K?x=1",
            "sc://acme/web/K?group=",
            "keychain://service",
            "keychain:///account",
            "https://example.com/a/b/c",
        ] {
            assert!(SecretRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_tool_arguments() {
        let args = |v: Value| match v {
            Value::Object(map) => map,
            _ => JsonObject::new(),
        };
        let (s, d) = parse_args(&args(
            json!({"source": "sc://a/b/K", "destination": "sc://c/d/K"}),
        ))
        .unwrap();
        assert_eq!(s.to_string(), "sc://a/b/K");
        assert_eq!(d.to_string(), "sc://c/d/K");
        for (input, expected) in [
            (
                json!({"source": "sc://a/b/K"}),
                "missing required argument `destination`",
            ),
            (
                json!({"source": "sc://a/b/K", "destination": 1}),
                "must be a string",
            ),
            (
                json!({"source": "sc://a/b/K", "destination": "sc://c/d/K", "value": "x"}),
                "unknown argument `value`",
            ),
        ] {
            let err = parse_args(&args(input.clone())).unwrap_err();
            assert!(err.message().contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn tool_annotations_follow_access() {
        let write = secret_copy_tool(AccessLevel::Write, KeychainMode::Disabled);
        let annotations = write.annotations.unwrap();
        assert_eq!(annotations.destructive_hint, Some(false));
        let description = write.description.unwrap();
        assert!(description.contains("--allow-destructive"), "{description}");
        assert!(description.contains("disabled"), "{description}");

        let all = secret_copy_tool(AccessLevel::Destructive, KeychainMode::Allowed);
        assert_eq!(all.annotations.unwrap().destructive_hint, Some(true));
        assert!(all.description.unwrap().contains("keychain://<service>"));

        // Удалённый сервер (HTTP) не упоминает хранилище вовсе.
        let remote = secret_copy_tool(AccessLevel::Destructive, KeychainMode::Unavailable);
        let description = remote.description.unwrap();
        assert!(!description.contains("keychain"), "{description}");
    }
}
