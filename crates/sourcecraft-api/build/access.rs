//! Исключения из уровня доступа по HTTP-методу.
//!
//! По умолчанию `GET` — чтение, `POST`/`PUT`/`PATCH` — запись, `DELETE` — удаление.
//! Здесь единственный список операций, для которых это правило не подходит.
//! Ключ — итоговое имя инструмента; неизвестное имя роняет сборку, чтобы список не устаревал.

/// Уровень доступа в генераторе (зеркало `AccessLevel` из `spec.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Destructive,
}

impl Access {
    pub fn rust_name(self) -> &'static str {
        match self {
            Self::Read => "AccessLevel::Read",
            Self::Write => "AccessLevel::Write",
            Self::Destructive => "AccessLevel::Destructive",
        }
    }
}

pub const ACCESS_EXCEPTIONS: &[(&str, Access)] = &[
    // PUT, но перезаписывает значение секрета без возможности вернуть прежнее,
    // а секреты используются CI/CD: ошибка ломает сборки и может подменить учётные данные.
    ("set_secret", Access::Destructive),
    // Изменение ролей меняет права доступа людей к организации и репозиториям:
    // выдача лишних прав или их отзыв — действие уровня безопасности, а не обычная запись.
    ("add_organization_roles", Access::Destructive),
    ("remove_organization_roles", Access::Destructive),
    ("add_repo_roles", Access::Destructive),
    ("remove_repo_roles", Access::Destructive),
    // Приглашения дают внешним пользователям доступ в организацию и отправляют им письма.
    ("create_organization_invites", Access::Destructive),
    // Рабочие места Code Assist — платные лицензии: изменение их числа влияет на счёт,
    // а освобождение и передача мест отбирают доступ у пользователей.
    ("set_code_assist_workplaces", Access::Destructive),
    ("occupy_code_assist_workplaces_bulk", Access::Destructive),
    ("release_code_assist_workplaces_bulk", Access::Destructive),
    ("transfer_code_assist_workplace", Access::Destructive),
    // DELETE, но удаляет только связь (метку, реакцию, привязку PR к задаче),
    // которую тут же можно вернуть обратной операцией; сами объекты не удаляются.
    ("remove_issue_labels", Access::Write),
    ("remove_pull_request_labels", Access::Write),
    ("remove_linked_prs", Access::Write),
    ("remove_issue_comment_reaction", Access::Write),
    // POST, но только формирует предпросмотр release notes и ничего не создаёт
    // (по описанию спецификации: «without creating a release»).
    ("generate_release_notes", Access::Read),
];

/// Уровень доступа по методу, если инструмента нет в [`ACCESS_EXCEPTIONS`].
pub fn default_access(method: &str) -> Access {
    match method {
        "get" => Access::Read,
        "delete" => Access::Destructive,
        _ => Access::Write,
    }
}

pub fn access_for(tool_name: &str, method: &str) -> Access {
    ACCESS_EXCEPTIONS
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map_or_else(|| default_access(method), |(_, access)| *access)
}
