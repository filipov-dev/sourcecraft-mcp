//! Чтение секретов репозитория из API для команд `secret`.
//!
//! Запрос собирается сгенерированной операцией `list_secrets` через `build_request` ядра,
//! поэтому адресация (slug или ID) и кодирование пути — те же, что у MCP-инструментов.
//! Тексты ошибок API проходят через `redact_api_error`: значения в них не попадают.

use std::collections::BTreeMap;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde_json::{Map, Value};
use sourcecraft_api::executor::{ApiClient, ResponseBody};
use sourcecraft_api::spec::find;
use sourcecraft_mcp_core::FilePolicy;
use sourcecraft_mcp_core::error::describe_api_error;
use sourcecraft_mcp_core::redact::redact_api_error;
use sourcecraft_mcp_core::request::build_request;

/// Операция чтения секретов.
const LIST_SECRETS_OPERATION: &str = "list_secrets";

/// base64 значения секрета: стандартный алфавит, паддинг необязателен.
const LENIENT_STANDARD: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Репозиторий, секреты которого читаются.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoTarget {
    Slug { org: String, repo: String },
    Id(String),
}

impl RepoTarget {
    /// Из `--repo <org>/<repo>` или `--repo-id <ID>` (ровно один из них).
    pub fn from_args(repo: Option<&str>, repo_id: Option<&str>) -> anyhow::Result<Self> {
        match (repo, repo_id) {
            (Some(repo), None) => {
                let parts: Vec<&str> = repo.trim().split('/').collect();
                match parts.as_slice() {
                    [org, name] if !org.is_empty() && !name.is_empty() => Ok(Self::Slug {
                        org: (*org).to_owned(),
                        repo: (*name).to_owned(),
                    }),
                    _ => bail!("--repo must be `<org>/<repo>`, got `{repo}`"),
                }
            }
            (None, Some(id)) if !id.trim().is_empty() => Ok(Self::Id(id.trim().to_owned())),
            (None, Some(_)) => bail!("--repo-id must not be empty"),
            (Some(_), Some(_)) => bail!("pass either --repo or --repo-id, not both"),
            (None, None) => bail!("pass --repo <org>/<repo> or --repo-id <ID>"),
        }
    }

    fn insert_args(&self, args: &mut Map<String, Value>) {
        match self {
            Self::Slug { org, repo } => {
                args.insert("org_slug".to_owned(), Value::String(org.clone()));
                args.insert("repo_slug".to_owned(), Value::String(repo.clone()));
            }
            Self::Id(id) => {
                args.insert("repo_id".to_owned(), Value::String(id.clone()));
            }
        }
    }
}

/// Раскодированные значения секретов по ключам. Не `Debug`: внутри значения.
pub struct Secrets(BTreeMap<String, Vec<u8>>);

impl Secrets {
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.0.get(key).map(Vec::as_slice)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(test)]
    pub fn from_pairs(pairs: &[(&str, &[u8])]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.to_vec()))
                .collect(),
        )
    }
}

/// Читает секреты репозитория. `keys` — фильтр по ключам (пусто — все секреты).
pub async fn fetch_secrets(
    client: &ApiClient,
    target: &RepoTarget,
    group: Option<&str>,
    keys: &[String],
) -> anyhow::Result<Secrets> {
    let op = find(LIST_SECRETS_OPERATION)
        .context("the SourceCraft API specification has no operation to list secrets")?;
    let mut args = Map::new();
    target.insert_args(&mut args);
    if let Some(group) = group {
        args.insert("secret_group".to_owned(), Value::String(group.to_owned()));
    }
    if !keys.is_empty() {
        args.insert(
            "keys".to_owned(),
            Value::Array(keys.iter().cloned().map(Value::String).collect()),
        );
    }
    // Файлы этой операции не нужны: пустая политика.
    let files = FilePolicy::new(&[]).map_err(|err| anyhow!(err))?;
    let request = build_request(op, &args, &files).map_err(|err| anyhow!(err.to_string()))?;
    let path = request.path.clone();

    let response = client
        .execute(request)
        .await
        .map_err(|err| anyhow!(describe_api_error(&redact_api_error(&path, err))))?;
    if response.truncated {
        bail!("the list of secrets is too large to read");
    }
    // Тело ответа не печатается ни в одной ошибке: в нём значения.
    let body = match response.body {
        ResponseBody::Json(body) => body,
        ResponseBody::Empty => Value::Object(Map::new()),
        _ => bail!("unexpected response from the SourceCraft API: not JSON"),
    };
    parse_secrets(&body)
}

fn parse_secrets(body: &Value) -> anyhow::Result<Secrets> {
    let entries = match body.get("secrets") {
        None | Some(Value::Null) => return Ok(Secrets(BTreeMap::new())),
        Some(Value::Array(entries)) => entries,
        Some(_) => bail!("unexpected response from the SourceCraft API: `secrets` is not a list"),
    };
    let mut secrets = BTreeMap::new();
    for entry in entries {
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            bail!("unexpected response from the SourceCraft API: a secret without a key");
        };
        let value = match entry.get("value") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(encoded)) => LENIENT_STANDARD
                .decode(encoded.trim())
                .map_err(|_| anyhow!("the value of secret `{key}` is not valid base64"))?,
            Some(_) => bail!("the value of secret `{key}` is not a string"),
        };
        secrets.insert(key.to_owned(), value);
    }
    Ok(Secrets(secrets))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sourcecraft_api::ApiToken;
    use sourcecraft_api::executor::ClientConfig;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    pub(crate) fn client(server: &MockServer) -> ApiClient {
        ApiClient::new(ClientConfig {
            base_url: url::Url::parse(&server.uri()).unwrap(),
            token: ApiToken::new("test-token"),
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
        })
        .unwrap()
    }

    #[test]
    fn parses_repo_target() {
        assert_eq!(
            RepoTarget::from_args(Some("acme/web"), None).unwrap(),
            RepoTarget::Slug {
                org: "acme".into(),
                repo: "web".into()
            }
        );
        assert_eq!(
            RepoTarget::from_args(None, Some(" 42 ")).unwrap(),
            RepoTarget::Id("42".into())
        );
        for bad in ["acme", "acme/", "/web", "a/b/c", ""] {
            assert!(RepoTarget::from_args(Some(bad), None).is_err(), "{bad}");
        }
        assert!(RepoTarget::from_args(None, None).is_err());
        assert!(RepoTarget::from_args(Some("a/b"), Some("1")).is_err());
    }

    #[tokio::test]
    async fn fetches_and_decodes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/secrets"))
            .and(query_param("secret_group", "ci"))
            .and(query_param("keys", "API_TOKEN"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "secrets": [
                    {"key": "API_TOKEN", "value": "czNjcmV0"},
                    {"key": "UNPADDED", "value": "aGk"},
                    {"key": "EMPTY"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let target = RepoTarget::from_args(Some("acme/web"), None).unwrap();
        let secrets = fetch_secrets(
            &client(&server),
            &target,
            Some("ci"),
            &["API_TOKEN".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(secrets.get("API_TOKEN"), Some(&b"s3cret"[..]));
        assert_eq!(secrets.get("UNPADDED"), Some(&b"hi"[..]));
        assert_eq!(secrets.get("EMPTY"), Some(&b""[..]));
        assert_eq!(
            secrets.keys().collect::<Vec<_>>(),
            ["API_TOKEN", "EMPTY", "UNPADDED"]
        );
    }

    #[tokio::test]
    async fn fetches_by_repo_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/id:42/secrets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let target = RepoTarget::Id("42".into());
        let secrets = fetch_secrets(&client(&server), &target, None, &[])
            .await
            .unwrap();
        assert!(secrets.is_empty());
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
        let target = RepoTarget::from_args(Some("acme/web"), None).unwrap();
        let err = fetch_secrets(&client(&server), &target, None, &[])
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(!err.contains("czNjcmV0"), "{err}");
        assert!(!err.contains("test-token"), "{err}");
    }

    #[test]
    fn rejects_bad_responses() {
        for (body, expected) in [
            (json!({"secrets": "x"}), "not a list"),
            (json!({"secrets": [{"value": "eA=="}]}), "without a key"),
            (
                json!({"secrets": [{"key": "K", "value": "%%%"}]}),
                "not valid base64",
            ),
            (
                json!({"secrets": [{"key": "K", "value": 1}]}),
                "not a string",
            ),
        ] {
            let err = parse_secrets(&body).err().unwrap().to_string();
            assert!(err.contains(expected), "{body}: {err}");
            assert!(!err.contains("%%%"), "{err}");
        }
    }
}
