//! Описание операций SourceCraft API.
//!
//! Таблица [`operations`] генерируется `build.rs` из `spec/sourcecraft.swagger.json`.
//! Типы этого модуля — контракт между генератором, исполнителем запросов и MCP-сервером.
//!
//! # Пары slug/id
//!
//! Многие операции SourceCraft существуют в двух вариантах адресации: по slug
//! (`/repos/{org_slug}/{repo_slug}/pulls`) и по id (`/repos/id:{repo_id}/pulls`).
//! Генератор объединяет такую пару в одну операцию (один MCP-инструмент):
//!
//! - [`OperationDef::path`] — путь варианта [`PathVariant::Slug`], у операций без пары —
//!   единственный путь (он может быть и в форме `id:{...}`, например `/issue_comments/id:{issue_comment_id}`);
//! - [`OperationDef::id_path`] — путь варианта [`PathVariant::Id`], если пара есть;
//! - [`ParamDef::variant`] показывает, в каком варианте используется параметр.
//!
//! Как MCP-сервер строит запрос из аргументов инструмента:
//!
//! 1. Выбрать вариант через [`OperationDef::select_variant`]: если передан хотя бы один
//!    path-параметр варианта `Id`, используется `id_path`, иначе — `path`.
//! 2. Взять параметры варианта через [`OperationDef::params_for`] и проверить, что переданы
//!    все с `required: true`. Аргументы другого варианта считаются ошибкой.
//! 3. Подставить path-параметры в [`OperationDef::path_for`], query-параметры передать в строке
//!    запроса (массивы — повторением ключа), `body` — JSON-телом, `FormData` — multipart
//!    (параметры с [`ParamType::File`] содержат путь к локальному файлу).

/// HTTP-метод операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    /// Уровень доступа по умолчанию для метода. Исключения задаёт генератор.
    pub fn default_access(self) -> AccessLevel {
        match self {
            Self::Get => AccessLevel::Read,
            Self::Post | Self::Put | Self::Patch => AccessLevel::Write,
            Self::Delete => AccessLevel::Destructive,
        }
    }
}

/// Уровень доступа операции. Уровни упорядочены: `Read < Write < Destructive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessLevel {
    Read,
    Write,
    Destructive,
}

impl AccessLevel {
    /// Разрешает ли текущий уровень операцию с уровнем `required`.
    pub fn allows(self, required: AccessLevel) -> bool {
        required <= self
    }
}

/// Где передаётся параметр операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    /// Подставляется в шаблон пути (`{name}`).
    Path,
    /// Передаётся в строке запроса.
    Query,
    /// JSON-тело запроса; у операции не больше одного такого параметра, его имя — `body`.
    Body,
    /// Часть multipart/form-data тела.
    FormData,
}

/// Тип значения параметра в аргументах инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    /// Целое число. В пути и строке запроса передаётся десятичной строкой
    /// (в спецификации такие параметры — строки с форматом `int64`/`uint64`).
    Integer,
    Number,
    Boolean,
    /// Массив; в строке запроса передаётся повторением ключа (`keys=a&keys=b`).
    Array,
    /// JSON-объект (тело запроса).
    Object,
    /// Путь к локальному файлу, содержимое которого отправляется частью multipart.
    File,
}

/// Вариант адресации ресурса — какой шаблон пути используется для запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathVariant {
    /// [`OperationDef::path`]: адресация по slug или единственный путь операции без пары.
    Slug,
    /// [`OperationDef::id_path`]: адресация по id.
    Id,
}

/// К какому варианту адресации относится параметр.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamVariant {
    /// Используется в обоих вариантах (и во всех параметрах операций без пары).
    Common,
    /// Только в варианте [`PathVariant::Slug`].
    Slug,
    /// Только в варианте [`PathVariant::Id`].
    Id,
}

impl ParamVariant {
    /// Используется ли параметр в варианте `variant`.
    pub fn applies_to(self, variant: PathVariant) -> bool {
        match self {
            Self::Common => true,
            Self::Slug => variant == PathVariant::Slug,
            Self::Id => variant == PathVariant::Id,
        }
    }
}

/// Параметр операции.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// Имя параметра: ключ в аргументах инструмента, имя в пути, строке запроса или части multipart.
    pub name: &'static str,
    pub location: ParamLocation,
    /// Обязателен в своём варианте адресации. В JSON Schema инструмента в `required`
    /// попадают только обязательные параметры [`ParamVariant::Common`].
    pub required: bool,
    pub description: &'static str,
    pub variant: ParamVariant,
    pub value_type: ParamType,
}

/// Операция SourceCraft API, доступная как MCP-инструмент.
#[derive(Debug, Clone, Copy)]
pub struct OperationDef {
    /// Имя MCP-инструмента (snake_case из `operationId` без суффикса `ByID`), уникально.
    pub tool_name: &'static str,
    /// Исходный `operationId` из спецификации (для пары — варианта `path`).
    pub operation_id: &'static str,
    /// `operationId` варианта `id_path`, если операция объединена из пары.
    pub id_operation_id: Option<&'static str>,
    /// Тег спецификации; определяет набор инструментов (toolset).
    pub tag: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub method: HttpMethod,
    /// Шаблон пути варианта [`PathVariant::Slug`], например `/repos/{org_slug}/{repo_slug}/pulls`.
    /// У операции без пары — единственный путь, в том числе в форме `id:{...}`.
    pub path: &'static str,
    /// Шаблон того же пути с адресацией по id, если он есть (`/repos/id:{repo_id}/pulls`).
    pub id_path: Option<&'static str>,
    pub access: AccessLevel,
    pub params: &'static [ParamDef],
    /// JSON Schema аргументов инструмента в виде JSON-текста.
    ///
    /// Объект с `type: object` и `additionalProperties: false`: path- и query-параметры,
    /// `body` (JSON-тело с раскрытыми `$ref`) и поля multipart. Для пар slug/id параметры
    /// обоих вариантов необязательны, а `description` схемы объясняет, какой набор передать.
    pub input_schema: &'static str,
}

impl OperationDef {
    /// Шаблон пути для варианта; `None`, если у операции нет варианта `Id`.
    pub fn path_for(&self, variant: PathVariant) -> Option<&'static str> {
        match variant {
            PathVariant::Slug => Some(self.path),
            PathVariant::Id => self.id_path,
        }
    }

    /// Параметры, которые используются в варианте `variant`.
    pub fn params_for(&self, variant: PathVariant) -> impl Iterator<Item = &'static ParamDef> {
        self.params
            .iter()
            .filter(move |param| param.variant.applies_to(variant))
    }

    /// Выбирает вариант адресации по переданным аргументам.
    ///
    /// `has_arg(name)` сообщает, передан ли аргумент. Вариант `Id` выбирается, если у операции
    /// есть `id_path` и передан хотя бы один path-параметр варианта `Id`; иначе — `Slug`.
    /// Проверку обязательных параметров выбранного варианта выполняет вызывающий код.
    pub fn select_variant(&self, has_arg: impl Fn(&str) -> bool) -> PathVariant {
        let uses_id = self.id_path.is_some()
            && self.params.iter().any(|param| {
                param.variant == ParamVariant::Id
                    && param.location == ParamLocation::Path
                    && has_arg(param.name)
            });
        if uses_id {
            PathVariant::Id
        } else {
            PathVariant::Slug
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/operations.rs"));

/// Все операции API, отсортированные по имени инструмента.
pub fn operations() -> &'static [OperationDef] {
    OPERATIONS
}

/// Операция по имени MCP-инструмента.
pub fn find(tool_name: &str) -> Option<&'static OperationDef> {
    OPERATIONS
        .binary_search_by(|op| op.tool_name.cmp(tool_name))
        .ok()
        .and_then(|index| OPERATIONS.get(index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_are_ordered() {
        assert!(AccessLevel::Destructive.allows(AccessLevel::Write));
        assert!(AccessLevel::Write.allows(AccessLevel::Read));
        assert!(!AccessLevel::Read.allows(AccessLevel::Write));
    }

    #[test]
    fn method_default_access() {
        assert_eq!(HttpMethod::Get.default_access(), AccessLevel::Read);
        assert_eq!(HttpMethod::Patch.default_access(), AccessLevel::Write);
        assert_eq!(
            HttpMethod::Delete.default_access(),
            AccessLevel::Destructive
        );
    }

    #[test]
    fn param_variant_applies() {
        assert!(ParamVariant::Common.applies_to(PathVariant::Id));
        assert!(ParamVariant::Slug.applies_to(PathVariant::Slug));
        assert!(!ParamVariant::Slug.applies_to(PathVariant::Id));
        assert!(!ParamVariant::Id.applies_to(PathVariant::Slug));
    }

    #[test]
    fn select_variant_by_arguments() {
        let op = find("create_pull_request").expect("операция create_pull_request");
        assert_eq!(op.select_variant(|name| name == "repo_id"), PathVariant::Id);
        assert_eq!(
            op.select_variant(|name| name == "org_slug" || name == "repo_slug"),
            PathVariant::Slug
        );
        assert_eq!(op.select_variant(|_| false), PathVariant::Slug);
        assert_eq!(
            op.path_for(PathVariant::Id),
            Some("/repos/id:{repo_id}/pulls")
        );

        let id_names: Vec<_> = op.params_for(PathVariant::Id).map(|p| p.name).collect();
        assert!(id_names.contains(&"repo_id"));
        assert!(id_names.contains(&"body"));
        assert!(!id_names.contains(&"org_slug"));
    }

    #[test]
    fn select_variant_without_pair_is_slug() {
        let op = find("get_issue_comment").expect("операция get_issue_comment");
        assert!(op.id_path.is_none());
        assert_eq!(op.select_variant(|_| true), PathVariant::Slug);
        assert_eq!(op.path_for(PathVariant::Id), None);
    }
}
