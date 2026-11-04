//! Проверка и сборка относительных путей SourceCraft API.
//!
//! Это граница безопасности: всё, что уходит в [`crate::executor::ApiClient::execute`],
//! проходит через [`validate_relative_path`], поэтому запрос не может уйти на другой хост
//! или выйти за пределы API через `..`, в том числе в percent-кодированном виде.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::executor::ApiError;

/// Символы, которые кодируются в значении параметра пути.
///
/// Оставляем только unreserved-символы RFC 3986, поэтому `/`, `?`, `#`, `%`, `:` и прочие
/// разделители внутри значения становятся `%XX` и не могут изменить структуру пути.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Сколько раз раскодировать сегмент при проверке. Защищает от двойного кодирования
/// (`%252e%252e`) на случай, если что-то между нами и API раскодирует путь повторно.
const MAX_DECODE_ROUNDS: usize = 3;

/// Проверяет относительный путь SourceCraft API, например `/repos/org/repo/pulls`.
///
/// Требования:
/// - начинается с `/`, но не с `//` (иначе это адрес другого хоста);
/// - содержит только символы `pchar` из RFC 3986 и `/`: схема, хост, `?`, `#`, `\`,
///   пробелы, управляющие и не-ASCII символы отклоняются (query передаётся отдельно);
/// - `%` всегда образует корректную тройку `%XX`;
/// - нет пустых сегментов (`//`) кроме завершающего `/`;
/// - ни один сегмент ни в исходном, ни в раскодированном виде не равен `.` или `..`,
///   а раскодированный сегмент не содержит `.`/`..` между `/` или `\` и управляющих символов.
pub fn validate_relative_path(path: &str) -> Result<(), ApiError> {
    let invalid = |reason: &str| {
        Err(ApiError::InvalidPath(format!(
            "{reason}: `{}`",
            shorten(path)
        )))
    };

    if !path.starts_with('/') {
        return invalid("путь должен начинаться с `/`");
    }
    if path.starts_with("//") {
        return invalid("путь не может начинаться с `//`");
    }
    if let Some(c) = path.chars().find(|c| !is_allowed_char(*c)) {
        return Err(ApiError::InvalidPath(format!(
            "недопустимый символ {c:?} в пути `{}`; query передаётся отдельно, \
             остальные символы кодируйте как %XX",
            shorten(path)
        )));
    }
    if !has_valid_percent_triplets(path) {
        return invalid("некорректная percent-последовательность");
    }

    let segments: Vec<&str> = path[1..].split('/').collect();
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // Пустой сегмент допустим только в конце (`/repos/`) или для пути `/`.
            if index == last {
                continue;
            }
            return invalid("пустой сегмент пути");
        }
        if !segment_is_safe(segment) {
            return invalid("сегменты `.` и `..` запрещены, в том числе в закодированном виде");
        }
    }
    Ok(())
}

/// Подставляет параметры в шаблон пути вида `/repos/{org_slug}/{repo_slug}` или
/// `/repos/id:{repo_id}` и проверяет результат через [`validate_relative_path`].
///
/// Значение параметра кодируется как один сегмент пути: `a/b` превращается в `a%2Fb`,
/// поэтому значение не может добавить сегменты или выйти из пути. Отсутствующий или
/// пустой параметр даёт [`ApiError::MissingPathParam`]. Значения `.` и `..` отклоняются
/// проверкой итогового пути.
pub fn render_path<F>(template: &str, mut lookup: F) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(ApiError::InvalidPath(format!(
                "лишняя `}}` в шаблоне `{}`",
                shorten(template)
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(ApiError::InvalidPath(format!(
                "незакрытая `{{` в шаблоне `{}`",
                shorten(template)
            )));
        };
        let name = &after[..close];
        if name.is_empty() || name.contains('{') {
            return Err(ApiError::InvalidPath(format!(
                "некорректный параметр в шаблоне `{}`",
                shorten(template)
            )));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::MissingPathParam(name.to_string()))?;
        out.extend(utf8_percent_encode(&value, SEGMENT));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    validate_relative_path(&out)?;
    Ok(out)
}

/// `pchar` из RFC 3986 плюс разделитель `/`; `%` проверяется отдельно.
fn is_allowed_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '/'
                | '%'
        )
}

fn has_valid_percent_triplets(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let ok = bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !ok {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Проверяет сегмент и его раскодированные варианты.
///
/// Закодированный `/` внутри значения (`a%2Fb`) допустим — так `render_path` передаёт
/// значения со слешем. Но если сервер раскодирует `%2F` до маршрутизации, `..%2F..`
/// превратится в обход каталога, поэтому раскодированный сегмент дополнительно делим
/// по `/` и `\` и ищем `.`/`..`.
fn segment_is_safe(segment: &str) -> bool {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if current
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            return false;
        }
        if current.chars().any(char::is_control) {
            return false;
        }
        if !current.contains('%') {
            return true;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    // Слишком глубокое вложенное кодирование — подозрительно, отклоняем.
    false
}

/// Укорачивает путь для текста ошибки, чтобы не раздувать ответ MCP.
fn shorten(path: &str) -> String {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path.escape_debug().to_string();
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", path[..end].escape_debug())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn accepts_valid_paths() {
        for path in [
            "/",
            "/me",
            "/repos/org/repo/pulls",
            "/repos/id:0190-abc/trees",
            "/repos/org/repo/secrets/MY_KEY",
            "/repos/org/repo/branches/feature%2Fx",
            "/repos/org/repo/",
            "/a/b..c/.hidden/d.",
            "/issues/id:1@x",
        ] {
            assert!(
                validate_relative_path(path).is_ok(),
                "{path} должен проходить"
            );
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "repos/org",
            "https://evil.example/repos",
            "http://evil.example",
            "//evil.example/repos",
            "///evil",
            "/repos//org",
            "/repos/../admin",
            "/repos/./x",
            "/..",
            "/repos/..",
            "/repos/%2e%2e/admin",
            "/repos/%2E%2E/admin",
            "/repos/.%2e/admin",
            "/repos/%2e/admin",
            "/repos/..%2fadmin",
            "/repos/x%2F..%2Fadmin",
            "/repos/..%5cadmin",
            "/repos/%252e%252e/admin",
            "/repos/%25252e%25252e/admin",
            "/repos?x=1",
            "/repos#frag",
            "/repos\\..\\admin",
            "/repos/a b",
            "/repos/\n",
            "/repos/%00",
            "/repos/%0a",
            "/repos/%zz",
            "/repos/%2",
            "/repos/{org}",
            "/repos/кириллица",
            "/repos/x\u{0}",
        ] {
            assert!(
                matches!(validate_relative_path(path), Err(ApiError::InvalidPath(_))),
                "{path:?} должен отклоняться"
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_and_encodes() {
        let p = params(&[
            ("org_slug", "my org"),
            ("repo_slug", "a/b"),
            ("key", "K?#%"),
        ]);
        let rendered = render_path("/repos/{org_slug}/{repo_slug}/secrets/{key}", |n| {
            p.get(n).cloned()
        })
        .unwrap();
        assert_eq!(rendered, "/repos/my%20org/a%2Fb/secrets/K%3F%23%25");
    }

    #[test]
    fn render_handles_literal_prefix_in_segment() {
        let p = params(&[("repo_id", "0190:x")]);
        let rendered = render_path("/repos/id:{repo_id}/trees", |n| p.get(n).cloned()).unwrap();
        assert_eq!(rendered, "/repos/id:0190%3Ax/trees");
    }

    #[test]
    fn render_encodes_non_ascii() {
        let p = params(&[("branch", "ветка")]);
        let rendered = render_path("/b/{branch}", |n| p.get(n).cloned()).unwrap();
        assert_eq!(rendered, "/b/%D0%B2%D0%B5%D1%82%D0%BA%D0%B0");
    }

    #[test]
    fn render_reports_missing_and_empty_params() {
        let p = params(&[("org_slug", "org"), ("repo_slug", "")]);
        let lookup = |n: &str| p.get(n).cloned();
        assert!(matches!(
            render_path("/repos/{org_slug}/{missing}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "missing"
        ));
        assert!(matches!(
            render_path("/repos/{org_slug}/{repo_slug}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "repo_slug"
        ));
    }

    #[test]
    fn render_rejects_traversal_values() {
        for value in ["..", ".", "../admin", "..\\x"] {
            let result = render_path("/repos/{x}/pulls", |_| Some(value.to_string()));
            assert!(
                matches!(result, Err(ApiError::InvalidPath(_))),
                "значение {value:?} должно отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_malformed_templates() {
        for template in ["/repos/{org", "/repos/org}", "/repos/{}", "/repos/{{x}}"] {
            assert!(
                matches!(
                    render_path(template, |_| Some("v".into())),
                    Err(ApiError::InvalidPath(_))
                ),
                "{template} должен отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_absolute_template() {
        assert!(render_path("https://evil/{x}", |_| Some("v".into())).is_err());
    }
}
