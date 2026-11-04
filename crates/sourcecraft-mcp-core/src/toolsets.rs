//! Наборы инструментов (toolsets).
//!
//! Набор — это тег операции в спецификации SourceCraft, записанный стабильным slug:
//! нижний регистр, каждая серия символов кроме `a-z0-9` заменяется на `-`, крайние `-`
//! отбрасываются. Например, `Repository | PullRequest` → `repository-pullrequest`,
//! `CI/CD` → `ci-cd`. Набор выбирается точным совпадением slug: `repository` не включает
//! `repository-pullrequest`.

use std::collections::BTreeMap;

use sourcecraft_api::spec::operations;

/// Набор инструментов: slug, исходный тег и число операций в спецификации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    pub slug: String,
    pub tag: &'static str,
    pub operations: usize,
}

/// Slug набора по тегу спецификации.
pub fn toolset_slug(tag: &str) -> String {
    let mut slug = String::with_capacity(tag.len());
    let mut pending_dash = false;
    for c in tag.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    slug
}

/// Все наборы инструментов из спецификации, отсортированные по slug.
pub fn available_toolsets() -> Vec<Toolset> {
    let mut by_slug: BTreeMap<String, Toolset> = BTreeMap::new();
    for op in operations() {
        by_slug
            .entry(toolset_slug(op.tag))
            .or_insert_with_key(|slug| Toolset {
                slug: slug.clone(),
                tag: op.tag,
                operations: 0,
            })
            .operations += 1;
    }
    by_slug.into_values().collect()
}

/// Проверяет имена наборов и приводит их к виду slug (без пробелов по краям).
///
/// Пустые элементы пропускаются. Неизвестный набор — ошибка со списком допустимых.
pub fn normalize_toolsets<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>, String> {
    let known = available_toolsets();
    let mut result = Vec::new();
    for name in names {
        let name = name.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if !known.iter().any(|t| t.slug == name) {
            let list: Vec<&str> = known.iter().map(|t| t.slug.as_str()).collect();
            return Err(format!(
                "unknown toolset `{name}`; available toolsets: {}",
                list.join(", ")
            ));
        }
        if !result.iter().any(|known: &String| known == name) {
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_mapping() {
        assert_eq!(
            toolset_slug("Repository | PullRequest"),
            "repository-pullrequest"
        );
        assert_eq!(toolset_slug("CI/CD"), "ci-cd");
        assert_eq!(toolset_slug("Issues | Linked PRs"), "issues-linked-prs");
        assert_eq!(toolset_slug("CodeAssistWorkplaces"), "codeassistworkplaces");
        assert_eq!(toolset_slug("  User | "), "user");
    }

    #[test]
    fn toolsets_cover_all_operations_and_are_unique() {
        let toolsets = available_toolsets();
        let total: usize = toolsets.iter().map(|t| t.operations).sum();
        assert_eq!(total, operations().len());
        for t in &toolsets {
            assert!(!t.slug.is_empty());
            assert!(
                t.slug
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            );
            // Разные теги не должны схлопываться в один slug.
            assert!(
                operations()
                    .iter()
                    .filter(|op| toolset_slug(op.tag) == t.slug)
                    .all(|op| op.tag == t.tag),
                "slug {} объединяет разные теги",
                t.slug
            );
        }
        assert!(toolsets.iter().any(|t| t.slug == "repository-pullrequest"));
        assert!(toolsets.iter().any(|t| t.slug == "issues"));
    }

    #[test]
    fn normalize_validates_names() {
        assert_eq!(
            normalize_toolsets(&[" issues ", "", "repository", "issues"]).unwrap(),
            ["issues", "repository"]
        );
        let err = normalize_toolsets(&["repos"]).unwrap_err();
        assert!(err.contains("unknown toolset `repos`"));
        assert!(err.contains("repository-pullrequest"));
    }
}
