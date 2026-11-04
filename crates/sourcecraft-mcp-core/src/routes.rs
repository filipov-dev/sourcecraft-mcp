//! Сопоставление произвольного запроса (`api_request`) с операциями спецификации.
//!
//! Уровень доступа `api_request` определяется не только HTTP-методом: если запрос совпадает
//! с известной операцией, действует её уровень из таблицы операций (исключения генератора:
//! `generate_release_notes` — чтение, роли и приглашения — удаление и т. д.).
//!
//! Правила сопоставления:
//!
//! - метод запроса должен совпадать с методом операции;
//! - сравниваются шаблоны [`OperationDef::path`] и [`OperationDef::id_path`];
//! - один завершающий `/` в пути запроса игнорируется;
//! - число сегментов совпадает; литеральный сегмент шаблона сравнивается без учёта регистра;
//! - `{param}` совпадает с одним непустым сегментом; литеральные префикс и суффикс вокруг
//!   параметра (`id:{repo_id}`, `cloud-id:{cloud_id}`) обязательны, а значение между ними
//!   не может быть пустым;
//! - сегмент запроса сравнивается и в исходном, и в percent-раскодированном виде
//!   (до нескольких раундов), чтобы `secr%65ts` не обходил совпадение с `secrets`.
//!
//! Если совпало несколько операций (например, `/repos/id:x/issues` подходит и под
//! `/repos/id:{repo_id}/issues`, и под `/repos/{org_slug}/{repo_slug}`), требуется
//! максимальный уровень доступа среди них: так решение не зависит от того, как именно
//! маршрутизирует запрос сервер SourceCraft. Если не совпало ничего — уровень по методу.

use std::borrow::Cow;

use percent_encoding::percent_decode_str;
use sourcecraft_api::spec::{AccessLevel, HttpMethod, OperationDef, operations};

/// Сколько раундов percent-раскодирования проверять (как в `validate_relative_path`).
const MAX_DECODE_ROUNDS: usize = 3;

/// Операция, с которой совпал запрос.
#[derive(Debug, Clone, Copy)]
pub struct RouteMatch {
    pub op: &'static OperationDef,
    /// Шаблон, с которым совпал путь (`path` или `id_path`).
    pub template: &'static str,
    /// Число литеральных символов шаблона: чем больше, тем точнее совпадение.
    pub specificity: usize,
}

/// Все операции с методом `method`, шаблон которых совпадает с `path`.
///
/// Порядок: сначала самые точные совпадения (больше литеральных символов), затем по имени.
pub fn match_operations(method: HttpMethod, path: &str) -> Vec<RouteMatch> {
    let segments = request_segments(path);
    let mut matches: Vec<RouteMatch> = Vec::new();
    for op in operations().iter().filter(|op| op.method == method) {
        let best = [Some(op.path), op.id_path]
            .into_iter()
            .flatten()
            .filter_map(|template| {
                template_specificity(template, &segments).map(|specificity| RouteMatch {
                    op,
                    template,
                    specificity,
                })
            })
            .max_by_key(|m| m.specificity);
        if let Some(found) = best {
            matches.push(found);
        }
    }
    matches.sort_by(|a, b| {
        b.specificity
            .cmp(&a.specificity)
            .then_with(|| a.op.tool_name.cmp(b.op.tool_name))
    });
    matches
}

/// Решение о доступе для запроса `api_request`.
#[derive(Debug, Clone)]
pub struct AccessDecision {
    /// Требуемый уровень доступа.
    pub required: AccessLevel,
    /// Совпавшие операции, самые точные первыми; пусто — уровень взят по методу.
    pub matches: Vec<RouteMatch>,
}

impl AccessDecision {
    /// Самая точная совпавшая операция.
    pub fn operation(&self) -> Option<&'static OperationDef> {
        self.matches.first().map(|m| m.op)
    }

    /// Совпала ли операция с именем инструмента `tool_name`.
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        self.matches.iter().any(|m| m.op.tool_name == tool_name)
    }
}

/// Какой уровень доступа нужен запросу `method path`.
pub fn required_access(method: HttpMethod, path: &str) -> AccessDecision {
    let matches = match_operations(method, path);
    let required = matches
        .iter()
        .map(|m| m.op.access)
        .max()
        .unwrap_or_else(|| method.default_access());
    AccessDecision { required, matches }
}

/// Сегменты пути запроса без ведущего и одного завершающего `/`.
fn request_segments(path: &str) -> Vec<&str> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('/').collect()
}

/// Точность совпадения шаблона с сегментами запроса; `None` — не совпадает.
fn template_specificity(template: &str, segments: &[&str]) -> Option<usize> {
    let template_segments = request_segments(template);
    if template_segments.len() != segments.len() {
        return None;
    }
    let mut specificity = 0;
    for (pattern, segment) in template_segments.iter().zip(segments) {
        specificity += segment_specificity(pattern, segment)?;
    }
    Some(specificity)
}

/// Совпадение одного сегмента шаблона: литерал или `префикс{param}суффикс`.
fn segment_specificity(pattern: &str, segment: &str) -> Option<usize> {
    if segment.is_empty() {
        return None;
    }
    let variants = decoded_variants(segment);
    let Some((prefix, rest)) = pattern.split_once('{') else {
        return variants
            .iter()
            .any(|value| value.eq_ignore_ascii_case(pattern))
            .then_some(pattern.len());
    };
    // В шаблонах спецификации не больше одного параметра на сегмент (проверяется тестом).
    let suffix = rest.split_once('}').map_or("", |(_, suffix)| suffix);
    variants
        .iter()
        .any(|value| {
            value.len() > prefix.len() + suffix.len()
                && starts_with_ignore_case(value, prefix)
                && ends_with_ignore_case(value, suffix)
        })
        .then_some(prefix.len() + suffix.len())
}

/// Исходный сегмент и его percent-раскодированные варианты.
fn decoded_variants(segment: &str) -> Vec<Cow<'_, str>> {
    let mut variants = vec![Cow::Borrowed(segment)];
    for _ in 0..MAX_DECODE_ROUNDS {
        let Some(last) = variants.last() else { break };
        let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
        if decoded == **last {
            break;
        }
        variants.push(Cow::Owned(decoded));
    }
    variants
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| value.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(method: HttpMethod, path: &str) -> Vec<&'static str> {
        match_operations(method, path)
            .iter()
            .map(|m| m.op.tool_name)
            .collect()
    }

    fn first(method: HttpMethod, path: &str) -> Option<&'static str> {
        required_access(method, path)
            .operation()
            .map(|op| op.tool_name)
    }

    #[test]
    fn templates_have_one_param_per_segment() {
        for op in operations() {
            for template in [Some(op.path), op.id_path].into_iter().flatten() {
                for segment in request_segments(template) {
                    assert!(
                        segment.matches('{').count() <= 1,
                        "{}: {template}",
                        op.tool_name
                    );
                }
            }
        }
    }

    #[test]
    fn every_operation_matches_its_own_templates() {
        for op in operations() {
            for template in [Some(op.path), op.id_path].into_iter().flatten() {
                let path = template.replace(['{', '}'], "");
                let decision = required_access(op.method, &path);
                assert!(
                    decision.matches_tool(op.tool_name),
                    "{} не совпала с {path}",
                    op.tool_name
                );
                assert!(decision.required >= op.access, "{}", op.tool_name);
            }
        }
    }

    #[test]
    fn slug_and_id_paths() {
        assert_eq!(
            first(HttpMethod::Post, "/repos/acme/web/pulls"),
            Some("create_pull_request")
        );
        assert_eq!(
            first(HttpMethod::Post, "/repos/id:r-1/pulls"),
            Some("create_pull_request")
        );
        assert_eq!(
            first(HttpMethod::Get, "/pulls/id:42"),
            Some("get_pull_request")
        );
        // Путь без пары slug/id в форме `id:{...}`.
        assert_eq!(
            first(HttpMethod::Patch, "/issue_comments/id:7"),
            Some("update_issue_comment")
        );
        assert_eq!(
            first(HttpMethod::Get, "/users/cloud-id:abc"),
            Some("get_profile_by_cloud_id")
        );
        // `/repos/id:x/issues` подходит и под get_repository (`org_slug=id:x`), но точнее —
        // list_repository_issues, она идёт первой.
        let all = names(HttpMethod::Get, "/repos/id:x/issues");
        assert_eq!(all, ["list_repository_issues", "get_repository"]);
    }

    #[test]
    fn literal_prefix_must_match_and_value_is_not_empty() {
        assert_eq!(first(HttpMethod::Get, "/pulls/42"), None);
        assert_eq!(first(HttpMethod::Get, "/pulls/id:"), None);
        // `cloud-id:` без значения не совпадает с `cloud-id:{cloud_id}` (но это допустимый
        // slug пользователя для `/users/{user_slug}`).
        assert!(
            !required_access(HttpMethod::Get, "/users/cloud-id:")
                .matches_tool("get_profile_by_cloud_id")
        );
        // `ID:` и закодированный `:` — то же, что `id:`.
        assert_eq!(
            first(HttpMethod::Get, "/pulls/ID:42"),
            Some("get_pull_request")
        );
        assert_eq!(
            first(HttpMethod::Get, "/pulls/id%3A42"),
            Some("get_pull_request")
        );
    }

    #[test]
    fn unknown_paths_and_methods() {
        assert!(names(HttpMethod::Get, "/no/such/endpoint").is_empty());
        assert!(names(HttpMethod::Get, "/").is_empty());
        assert!(names(HttpMethod::Get, "/repos/acme/web/issues/1/extra/deep").is_empty());
        // Путь операции, но другой метод: совпадения нет.
        assert!(names(HttpMethod::Get, "/repos/acme/web/pulls/1/merge").is_empty());
    }

    #[test]
    fn trailing_slash_and_case() {
        assert_eq!(
            first(HttpMethod::Post, "/repos/acme/web/pulls/"),
            Some("create_pull_request")
        );
        assert_eq!(
            first(HttpMethod::Put, "/Repos/acme/web/SECRETS/KEY/"),
            Some("set_secret")
        );
        // Только один завершающий `/`: `//` в конце не схлопывается в совпадение.
        assert_eq!(first(HttpMethod::Get, "/user//"), None);
        assert_eq!(first(HttpMethod::Get, "/user/"), Some("get_my_profile"));
    }

    #[test]
    fn percent_encoded_literals_still_match() {
        assert_eq!(
            first(HttpMethod::Put, "/repos/acme/web/secr%65ts/KEY"),
            Some("set_secret")
        );
        assert_eq!(
            first(HttpMethod::Put, "/repos/acme/web/secr%2565ts/KEY"),
            Some("set_secret")
        );
    }

    #[test]
    fn access_comes_from_operation_then_method() {
        // Обычная запись.
        assert_eq!(
            required_access(HttpMethod::Post, "/repos/a/b/pulls").required,
            AccessLevel::Write
        );
        // POST, но только чтение: исключение генератора.
        assert_eq!(
            required_access(HttpMethod::Post, "/repos/a/b/releases/generate-notes").required,
            AccessLevel::Read
        );
        assert_eq!(
            required_access(HttpMethod::Post, "/repos/id:r/releases/generate-notes").required,
            AccessLevel::Read
        );
        // DELETE связи — запись, а не удаление.
        assert_eq!(
            required_access(HttpMethod::Delete, "/repos/a/b/issues/1/labels").required,
            AccessLevel::Write
        );
        // PUT/POST, повышенные до удаления (рабочие места, роли).
        assert_eq!(
            required_access(HttpMethod::Put, "/orgs/a/code-assist-workplaces").required,
            AccessLevel::Destructive
        );
        assert_eq!(
            required_access(HttpMethod::Post, "/repos/a/b/roles").required,
            AccessLevel::Destructive
        );
        // GET по пути операции записи не совпадает с ней: уровень по методу — чтение.
        assert_eq!(
            required_access(HttpMethod::Get, "/repos/a/b/pulls/1/merge").required,
            AccessLevel::Read
        );
        // Неизвестные пути — по методу.
        assert_eq!(
            required_access(HttpMethod::Get, "/x").required,
            AccessLevel::Read
        );
        assert_eq!(
            required_access(HttpMethod::Post, "/x").required,
            AccessLevel::Write
        );
        assert_eq!(
            required_access(HttpMethod::Patch, "/x").required,
            AccessLevel::Write
        );
        assert_eq!(
            required_access(HttpMethod::Put, "/x").required,
            AccessLevel::Write
        );
        assert_eq!(
            required_access(HttpMethod::Delete, "/x").required,
            AccessLevel::Destructive
        );
    }

    #[test]
    fn ambiguous_matches_take_the_highest_access() {
        // PATCH `/repos/id:x/issues` не совпадает с операцией задач, но совпадает с
        // update_repository (`org_slug=id:x`, `repo_slug=issues`).
        let decision = required_access(HttpMethod::Patch, "/repos/id:x/issues");
        assert!(decision.matches_tool("update_repository"));
        assert_eq!(decision.required, AccessLevel::Write);
    }
}
