//! Построение таблицы операций из спецификации и генерация Rust-кода.
//!
//! Этапы: разбор операций → пропуск `Withdrawn` → объединение пар slug/id →
//! уровни доступа и JSON Schema → сортировка по имени инструмента → вывод `OPERATIONS`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::access::{ACCESS_EXCEPTIONS, Access, access_for};
use crate::naming::{TOOL_NAME_OVERRIDES, base_key, is_valid_tool_name, tool_name};
use crate::schema::{Resolver, description_of};

const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Тег, которым SourceCraft помечает выведенные из употребления операции.
const WITHDRAWN_TAG: &str = "Withdrawn";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Body,
    FormData,
}

impl Location {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Path => "ParamLocation::Path",
            Self::Query => "ParamLocation::Query",
            Self::Body => "ParamLocation::Body",
            Self::FormData => "ParamLocation::FormData",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Common,
    Slug,
    Id,
}

impl Variant {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Common => "ParamVariant::Common",
            Self::Slug => "ParamVariant::Slug",
            Self::Id => "ParamVariant::Id",
        }
    }
}

#[derive(Debug, Clone)]
struct Param {
    name: String,
    location: Location,
    required: bool,
    description: String,
    /// Имя варианта `ParamType` в `spec.rs`.
    value_type: &'static str,
    /// JSON Schema значения без описания.
    schema: Value,
    variant: Variant,
}

#[derive(Debug, Clone)]
struct RawOperation {
    operation_id: String,
    method: &'static str,
    path: String,
    tag: String,
    summary: String,
    description: String,
    params: Vec<Param>,
}

#[derive(Debug)]
struct Operation {
    tool_name: String,
    operation_id: String,
    id_operation_id: Option<String>,
    tag: String,
    summary: String,
    description: String,
    method: &'static str,
    path: String,
    id_path: Option<String>,
    access: Access,
    params: Vec<Param>,
    input_schema: String,
}

/// Генерирует исходный код `operations.rs` из разобранной спецификации.
pub fn generate(spec: &Value) -> String {
    let definitions = spec
        .get("definitions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut resolver = Resolver::new(&definitions);

    let raw = parse_operations(spec, &mut resolver);

    // Группы логических операций: ключ без суффикса варианта + метод.
    let mut groups: BTreeMap<(String, &'static str), Vec<RawOperation>> = BTreeMap::new();
    for op in raw {
        let key = base_key(&op.operation_id).to_owned();
        groups.entry((key, op.method)).or_default().push(op);
    }

    let mut operations = Vec::new();
    for ((key, _), group) in groups {
        let operation = match <[RawOperation; 1]>::try_from(group) {
            Ok([single]) => single_operation(&key, single),
            Err(group) => merged_operation(&key, group),
        };
        operations.push(operation);
    }
    operations.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));

    check_invariants(&operations);

    emit(&operations)
}

fn parse_operations(spec: &Value, resolver: &mut Resolver<'_>) -> Vec<RawOperation> {
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .expect("в спецификации нет paths");
    let mut operations = Vec::new();
    for (path, item) in paths {
        let item = item
            .as_object()
            .expect("элемент paths не является объектом");
        for key in item.keys() {
            assert!(
                METHODS.contains(&key.as_str()),
                "неподдерживаемый ключ {key} в пути {path}"
            );
        }
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            let tags: Vec<&str> = op
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| tags.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let deprecated = op.get("deprecated").and_then(Value::as_bool) == Some(true);
            if tags.contains(&WITHDRAWN_TAG) || deprecated {
                continue;
            }
            let operation_id = str_field(op, "operationId")
                .unwrap_or_else(|| panic!("у {method} {path} нет operationId"));
            let tag = tags
                .first()
                .unwrap_or_else(|| panic!("у {operation_id} нет тегов"));
            operations.push(RawOperation {
                operation_id: operation_id.clone(),
                method,
                path: path.clone(),
                tag: (*tag).to_owned(),
                summary: str_field(op, "summary").unwrap_or_default(),
                description: str_field(op, "description").unwrap_or_default(),
                params: parse_params(op, &operation_id, resolver),
            });
        }
    }
    operations
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
}

fn parse_params(op: &Value, operation_id: &str, resolver: &mut Resolver<'_>) -> Vec<Param> {
    let mut params = Vec::new();
    for raw in op
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let object = raw.as_object().expect("параметр не является объектом");
        let name = str_field(raw, "name").expect("у параметра нет имени");
        let location = match str_field(raw, "in").as_deref() {
            Some("path") => Location::Path,
            Some("query") => Location::Query,
            Some("body") => Location::Body,
            Some("formData") => Location::FormData,
            other => panic!("{operation_id}: неподдерживаемое расположение параметра {other:?}"),
        };
        let required = raw.get("required").and_then(Value::as_bool) == Some(true)
            || location == Location::Path;
        let mut description = description_of(object).unwrap_or_default();
        let (schema, value_type) = match location {
            Location::Body => {
                assert_eq!(name, "body", "{operation_id}: тело должно называться body");
                let schema = resolver.resolve(raw.get("schema").expect("у body нет schema"));
                let own = schema
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|s| format!(" {s}"))
                    .unwrap_or_default();
                description = format!("JSON request body.{own}");
                (schema, "Object")
            }
            _ => simple_schema(object),
        };
        if value_type == "File" {
            description = file_description(&name, &description);
        }
        params.push(Param {
            name,
            location,
            required,
            description,
            value_type,
            schema,
            variant: Variant::Common,
        });
    }

    // В спецификации загрузка файлов описана в стиле OpenAPI 3 (`requestBody`), хотя документ
    // Swagger 2.0. Поддерживается только multipart/form-data: его поля становятся FormData.
    if let Some(body) = op.get("requestBody") {
        let content = body
            .get("content")
            .and_then(Value::as_object)
            .expect("requestBody без content");
        for (media_type, media) in content {
            assert_eq!(
                media_type, "multipart/form-data",
                "{operation_id}: неподдерживаемый тип requestBody"
            );
            let schema = media.get("schema").expect("requestBody без schema");
            let schema = resolver.resolve(schema);
            let required: BTreeSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("multipart без properties");
            for (name, property) in properties {
                let property = property.as_object().expect("поле multipart не объект");
                let (schema, value_type) = simple_schema(property);
                let mut description = description_of(property).unwrap_or_default();
                if value_type == "File" {
                    description = file_description(name, &description);
                }
                params.push(Param {
                    name: name.clone(),
                    location: Location::FormData,
                    required: required.contains(name.as_str()),
                    description,
                    value_type,
                    schema,
                    variant: Variant::Common,
                });
            }
        }
    }
    params
}

/// Описание файлового поля: агент передаёт путь к файлу, а не его содержимое.
fn file_description(name: &str, description: &str) -> String {
    finish_description(
        name,
        description,
        &format!(
            "Absolute path to a local file; the server reads it and uploads \
             its content as multipart field `{name}`."
        ),
    )
}

/// Схема и тип простого параметра (path, query, formData, поле multipart).
fn simple_schema(object: &Map<String, Value>) -> (Value, &'static str) {
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    let format = object.get("format").and_then(Value::as_str);
    // Файл: `type: file` (Swagger formData) или `format: binary` (OpenAPI 3 multipart).
    if kind == "file" || format == Some("binary") {
        return (json!({ "type": "string" }), "File");
    }
    // grpc-gateway описывает 64-битные целые строками; агенту удобнее передать число,
    // а в путь и строку запроса оно всё равно уходит текстом.
    let integer_format = matches!(format, Some("int32" | "int64" | "uint32" | "uint64"));
    let (json_type, value_type) = match kind {
        "string" if integer_format => ("integer", "Integer"),
        "string" => ("string", "String"),
        "integer" => ("integer", "Integer"),
        "number" => ("number", "Number"),
        "boolean" => ("boolean", "Boolean"),
        "array" => ("array", "Array"),
        other => panic!("неподдерживаемый тип простого параметра {other}"),
    };
    let mut schema = Map::new();
    schema.insert("type".into(), json!(json_type));
    if json_type == "integer" && format.is_some_and(|f| f.starts_with('u')) {
        schema.insert("minimum".into(), json!(0));
    }
    for key in ["enum", "default"] {
        if let Some(value) = object.get(key) {
            schema.insert(key.into(), value.clone());
        }
    }
    if let Some(items) = object.get("items").and_then(Value::as_object) {
        schema.insert("items".into(), simple_schema(items).0);
    }
    (Value::Object(schema), value_type)
}

fn is_id_form(path: &str) -> bool {
    path.contains("/id:{")
}

fn single_operation(key: &str, op: RawOperation) -> Operation {
    let tool_name = tool_name(key);
    let access = access_for(&tool_name, op.method);
    let params: Vec<Param> = op
        .params
        .into_iter()
        .map(|mut param| {
            param.description = finish_description(&param.name, &param.description, "");
            param
        })
        .collect();
    check_path_params(&op.operation_id, &op.path, &params, Variant::Slug);
    let input_schema = input_schema(&params, None);
    Operation {
        tool_name,
        operation_id: op.operation_id,
        id_operation_id: None,
        tag: op.tag,
        summary: op.summary,
        description: op.description,
        method: op.method,
        path: op.path,
        id_path: None,
        access,
        params,
        input_schema,
    }
}

fn merged_operation(key: &str, group: Vec<RawOperation>) -> Operation {
    let names: Vec<&str> = group.iter().map(|op| op.operation_id.as_str()).collect();
    let (mut ids, mut slugs): (Vec<_>, Vec<_>) =
        group.iter().cloned().partition(|op| is_id_form(&op.path));
    assert!(
        ids.len() == 1 && slugs.len() == 1,
        "группа {key} {names:?} не является парой slug/id"
    );
    let (id, slug) = (ids.remove(0), slugs.remove(0));
    assert_eq!(
        slug.tag, id.tag,
        "у пары {names:?} разные теги — проверь объединение"
    );

    let slug_path_params = path_param_names(&slug.params);
    let id_path_params = path_param_names(&id.params);

    // grpc-gateway дублирует идентификатор другого варианта в query (`org_id` в slug-варианте,
    // `pull_request_slug` в id-варианте). При объединении он конфликтует с path-параметром
    // другого варианта и не нужен: вариант задаётся выбором пути.
    let is_echo = |param: &Param, other_path_params: &[String]| {
        param.location == Location::Query && other_path_params.contains(&param.name)
    };
    let slug_params: Vec<Param> = slug
        .params
        .iter()
        .filter(|p| !is_echo(p, &id_path_params))
        .cloned()
        .collect();
    let id_params: Vec<Param> = id
        .params
        .iter()
        .filter(|p| !is_echo(p, &slug_path_params))
        .cloned()
        .collect();

    // Наборы адресации — только path-параметры, которых нет в другом варианте
    // (`key` в `/secrets/{key}` общий и в наборы не входит).
    let only = |own: &[String], other: &[String]| -> Vec<String> {
        own.iter().filter(|n| !other.contains(n)).cloned().collect()
    };
    let slug_set = code_list(&only(&slug_path_params, &id_path_params));
    let id_set = code_list(&only(&id_path_params, &slug_path_params));
    let mut params = Vec::new();
    for param in &slug_params {
        let mut param = param.clone();
        match id_params.iter().find(|p| p.name == param.name) {
            Some(twin) => {
                assert_eq!(
                    (twin.location, twin.required),
                    (param.location, param.required),
                    "{names:?}: параметр {} различается в вариантах",
                    param.name
                );
                param.variant = Variant::Common;
                param.description = finish_description(&param.name, &param.description, "");
            }
            None => {
                param.variant = Variant::Slug;
                let note = variant_note(&param, "slug", &slug_set, &id_set);
                param.description = finish_description(&param.name, &param.description, &note);
            }
        }
        params.push(param);
    }
    for param in &id_params {
        if slug_params.iter().any(|p| p.name == param.name) {
            continue;
        }
        let mut param = param.clone();
        param.variant = Variant::Id;
        let note = variant_note(&param, "ID", &id_set, &slug_set);
        param.description = finish_description(&param.name, &param.description, &note);
        params.push(param);
    }

    check_path_params(&slug.operation_id, &slug.path, &params, Variant::Slug);
    check_path_params(&id.operation_id, &id.path, &params, Variant::Id);

    let tool_name = tool_name(key);
    let access = access_for(&tool_name, slug.method);
    let addressing = format!(
        "Identify the target either by slug ({slug_set}) or by ID ({id_set}); \
         pass exactly one of these sets."
    );
    let input_schema = input_schema(&params, Some(&addressing));
    let description = if slug.description.is_empty() {
        id.description
    } else {
        slug.description
    };
    Operation {
        tool_name,
        operation_id: slug.operation_id,
        id_operation_id: Some(id.operation_id),
        tag: slug.tag,
        summary: slug.summary,
        description,
        method: slug.method,
        path: slug.path,
        id_path: Some(id.path),
        access,
        params,
        input_schema,
    }
}

fn path_param_names(params: &[Param]) -> Vec<String> {
    params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.name.clone())
        .collect()
}

fn code_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Пояснение для параметра, который есть только в одном варианте адресации.
fn variant_note(param: &Param, variant: &str, own_set: &str, other_set: &str) -> String {
    if param.location == Location::Path {
        let label = humanize(variant);
        format!("{label} addressing ({own_set}); omit when using {other_set} instead.")
    } else {
        format!("Used only with {variant} addressing ({own_set}).")
    }
}

/// Итоговое описание параметра: текст спецификации или имя по-человечески плюс пояснение варианта.
fn finish_description(name: &str, description: &str, note: &str) -> String {
    let mut text = if description.trim().is_empty() {
        humanize(name)
    } else {
        description.trim().to_owned()
    };
    if !note.is_empty() {
        if !text.ends_with(['.', '!', '?', ':']) {
            text.push('.');
        }
        text.push(' ');
        text.push_str(note);
    }
    text
}

/// `pull_request_id` → `Pull request ID`, `org_slug` → `Organization slug`.
fn humanize(name: &str) -> String {
    let words: Vec<&str> = name
        .split('_')
        .map(|word| match word {
            "org" => "organization",
            "repo" => "repository",
            "id" => "ID",
            other => other,
        })
        .collect();
    let text = words.join(" ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// Проверяет, что плейсхолдеры `{name}` пути совпадают с path-параметрами варианта.
fn check_path_params(operation_id: &str, path: &str, params: &[Param], variant: Variant) {
    let placeholders: BTreeSet<&str> = path
        .split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect();
    let declared: BTreeSet<&str> = params
        .iter()
        .filter(|p| p.location == Location::Path)
        .filter(|p| p.variant == Variant::Common || p.variant == variant)
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        placeholders, declared,
        "{operation_id}: плейсхолдеры пути {path} не совпадают с path-параметрами"
    );
}

/// JSON Schema аргументов инструмента.
fn input_schema(params: &[Param], addressing: Option<&str>) -> String {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in params {
        let mut schema = param.schema.clone();
        let object = schema
            .as_object_mut()
            .expect("схема параметра не является объектом");
        object.insert(
            "description".into(),
            Value::String(param.description.clone()),
        );
        assert!(
            properties.insert(param.name.clone(), schema).is_none(),
            "параметр {} повторяется",
            param.name
        );
        if param.required && param.variant == Variant::Common {
            required.push(Value::String(param.name.clone()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    if let Some(text) = addressing {
        schema.insert("description".into(), json!(text));
    }
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    serde_json::to_string(&Value::Object(schema)).expect("сериализация JSON Schema")
}

fn check_invariants(operations: &[Operation]) {
    let mut names = BTreeSet::new();
    for op in operations {
        assert!(
            is_valid_tool_name(&op.tool_name),
            "недопустимое имя инструмента {}",
            op.tool_name
        );
        assert!(
            names.insert(op.tool_name.as_str()),
            "имя инструмента {} повторяется",
            op.tool_name
        );
    }
    for (name, _) in ACCESS_EXCEPTIONS {
        assert!(
            names.contains(name),
            "исключение доступа для неизвестного инструмента {name}"
        );
    }
    for (_, to) in TOOL_NAME_OVERRIDES {
        let name = crate::naming::to_snake_case(to);
        assert!(
            names.contains(name.as_str()),
            "переименование в {to} не применилось ни к одной операции"
        );
    }
}

fn rust_method(method: &str) -> &'static str {
    match method {
        "get" => "HttpMethod::Get",
        "post" => "HttpMethod::Post",
        "put" => "HttpMethod::Put",
        "patch" => "HttpMethod::Patch",
        "delete" => "HttpMethod::Delete",
        other => panic!("неподдерживаемый метод {other}"),
    }
}

/// Строковые литералы выводятся через `{:?}`: это корректное экранирование для Rust.
fn emit(operations: &[Operation]) -> String {
    let mut out = String::new();
    out.push_str("// Сгенерировано build.rs из spec/sourcecraft.swagger.json. Не редактировать.\n");
    out.push_str("pub static OPERATIONS: &[OperationDef] = &[\n");
    for op in operations {
        let _ = writeln!(out, "    OperationDef {{");
        let _ = writeln!(out, "        tool_name: {:?},", op.tool_name);
        let _ = writeln!(out, "        operation_id: {:?},", op.operation_id);
        let _ = writeln!(
            out,
            "        id_operation_id: {},",
            option_literal(&op.id_operation_id)
        );
        let _ = writeln!(out, "        tag: {:?},", op.tag);
        let _ = writeln!(out, "        summary: {:?},", op.summary);
        let _ = writeln!(out, "        description: {:?},", op.description);
        let _ = writeln!(out, "        method: {},", rust_method(op.method));
        let _ = writeln!(out, "        path: {:?},", op.path);
        let _ = writeln!(out, "        id_path: {},", option_literal(&op.id_path));
        let _ = writeln!(out, "        access: {},", op.access.rust_name());
        let _ = writeln!(out, "        params: &[");
        for param in &op.params {
            let _ = writeln!(
                out,
                "            ParamDef {{ name: {:?}, location: {}, required: {}, description: {:?}, variant: {}, value_type: ParamType::{} }},",
                param.name,
                param.location.rust_name(),
                param.required,
                param.description,
                param.variant.rust_name(),
                param.value_type,
            );
        }
        let _ = writeln!(out, "        ],");
        let _ = writeln!(out, "        input_schema: {:?},", op.input_schema);
        let _ = writeln!(out, "    }},");
    }
    out.push_str("];\n");
    out
}

fn option_literal(value: &Option<String>) -> String {
    match value {
        Some(text) => format!("Some({text:?})"),
        None => "None".to_owned(),
    }
}
