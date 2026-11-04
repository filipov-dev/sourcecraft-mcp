//! Имена операций: ключ пары slug/id и имя MCP-инструмента.

/// Суффиксы `operationId`, которые отличают варианты одной операции.
///
/// `ByRepoID`, `ByOrgID`, `ByID` — вариант адресации по id; `ByTag` — slug-вариант релизов
/// (`GetByTag` в паре с `GetByID`). `ByCloudID` намеренно не входит в список: поиск
/// пользователя по облачному id — отдельный инструмент, а не вариант `GetProfile`.
const VARIANT_SUFFIXES: &[&str] = &["ByRepoID", "ByOrgID", "ByID", "ById", "ByTag"];

/// Переименования, где `operationId` слишком общий или неоднозначный для имени инструмента.
///
/// Ключ — `operationId` без суффикса варианта, значение — новое имя в CamelCase.
/// Каждый ключ обязан существовать в спецификации, иначе сборка падает.
pub const TOOL_NAME_OVERRIDES: &[(&str, &str)] = &[
    // Операции релизов названы без существительного (`List`, `Create`, `Get`...).
    ("Create", "CreateRelease"),
    ("Delete", "DeleteRelease"),
    ("Discard", "DiscardRelease"),
    ("Get", "GetRelease"),
    ("GetLatest", "GetLatestRelease"),
    ("List", "ListReleases"),
    ("Publish", "PublishRelease"),
    ("Update", "UpdateRelease"),
    // Метки задачи легко спутать с метками репозитория (`GetLabel`, `CreateLabel`).
    ("AddLabels", "AddIssueLabels"),
    ("GetLabels", "GetIssueLabels"),
    ("RemoveLabels", "RemoveIssueLabels"),
    ("ReplaceLabels", "ReplaceIssueLabels"),
    // Реакции есть только у комментариев задач.
    ("AddReaction", "AddIssueCommentReaction"),
    ("RemoveReaction", "RemoveIssueCommentReaction"),
    // Ревьюеры и решение ревью пул-реквеста.
    ("AutoAssign", "AutoAssignReviewers"),
    ("SetDecision", "SetReviewDecision"),
];

/// Ключ логической операции: `operationId` без суффикса варианта адресации.
pub fn base_key(operation_id: &str) -> &str {
    VARIANT_SUFFIXES
        .iter()
        .find_map(|suffix| operation_id.strip_suffix(suffix))
        .filter(|base| !base.is_empty())
        .unwrap_or(operation_id)
}

/// Имя инструмента для ключа операции с учётом [`TOOL_NAME_OVERRIDES`].
pub fn tool_name(key: &str) -> String {
    let camel = TOOL_NAME_OVERRIDES
        .iter()
        .find(|(from, _)| *from == key)
        .map_or(key, |(_, to)| to);
    to_snake_case(camel)
}

/// CamelCase → snake_case с учётом аббревиатур: `GetLinkedPRs` → `get_linked_prs`,
/// `GetProfileByCloudID` → `get_profile_by_cloud_id`.
pub fn to_snake_case(name: &str) -> String {
    // Множественное число аббревиатуры (`PRs`) иначе разбилось бы на `p_rs`.
    let name = name.replace("PRs", "Prs");
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(chars.len() + 8);
    for (index, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            let prev = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(|c| c.is_ascii_lowercase());
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_is_lower)
            {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Имя MCP-инструмента должно соответствовать `^[a-zA-Z0-9_-]{1,64}$`.
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
