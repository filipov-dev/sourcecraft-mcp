//! Перевод схем Swagger 2.0 в самодостаточную JSON Schema для аргументов MCP-инструмента.
//!
//! `$ref` раскрываются на месте: MCP-клиенты не получают `definitions`, поэтому ссылки
//! в схеме инструмента были бы неразрешимы.

use serde_json::{Map, Value, json};

/// Максимальная вложенность раскрытия `$ref`. Глубже схема заменяется разрешающим объектом.
const MAX_REF_DEPTH: usize = 8;

/// Ключи схемы, которые переносятся как есть (без рекурсии).
const PLAIN_KEYS: &[&str] = &[
    "type",
    "format",
    "enum",
    "default",
    "required",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "readOnly",
];

pub struct Resolver<'a> {
    definitions: &'a Map<String, Value>,
    /// Цепочка раскрываемых определений — защита от рекурсивных схем.
    stack: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub fn new(definitions: &'a Map<String, Value>) -> Self {
        Self {
            definitions,
            stack: Vec::new(),
        }
    }

    /// Раскрывает схему Swagger в JSON Schema без `$ref`, `title` и расширений `x-*`.
    pub fn resolve(&mut self, schema: &Value) -> Value {
        let Some(object) = schema.as_object() else {
            return schema.clone();
        };

        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            // Описание рядом с `$ref` относится к конкретному полю и точнее описания типа.
            let local_description = description_of(object);
            let mut resolved = self.resolve_ref(reference);
            if let (Some(text), Some(target)) = (local_description, resolved.as_object_mut()) {
                target.insert("description".into(), Value::String(text));
            }
            return resolved;
        }

        let mut out = Map::new();
        for key in PLAIN_KEYS {
            if let Some(value) = object.get(*key) {
                out.insert((*key).into(), value.clone());
            }
        }
        if let Some(text) = description_of(object) {
            out.insert("description".into(), Value::String(text));
        }
        if let Some(items) = object.get("items") {
            out.insert("items".into(), self.resolve(items));
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            let resolved: Map<String, Value> = properties
                .iter()
                .map(|(name, property)| (name.clone(), self.resolve(property)))
                .collect();
            out.insert("properties".into(), Value::Object(resolved));
        }
        if let Some(additional) = object.get("additionalProperties") {
            let value = if additional.is_object() {
                self.resolve(additional)
            } else {
                additional.clone()
            };
            out.insert("additionalProperties".into(), value);
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(variants) = object.get(key).and_then(Value::as_array) {
                let resolved = variants.iter().map(|v| self.resolve(v)).collect();
                out.insert(key.into(), Value::Array(resolved));
            }
        }
        Value::Object(out)
    }

    fn resolve_ref(&mut self, reference: &str) -> Value {
        let name = reference
            .strip_prefix("#/definitions/")
            .unwrap_or_else(|| panic!("неподдерживаемая ссылка {reference}"));
        if self.stack.iter().any(|seen| seen == name) || self.stack.len() >= MAX_REF_DEPTH {
            // Рекурсия или слишком глубокая вложенность: структура не раскрывается,
            // агент может передать объект произвольной формы.
            return json!({
                "type": "object",
                "description": format!("`{name}` object (nested structure is not expanded)"),
            });
        }
        let definition = self
            .definitions
            .get(name)
            .unwrap_or_else(|| panic!("в спецификации нет определения {name}"));
        self.stack.push(name.to_owned());
        let resolved = self.resolve(definition);
        self.stack.pop();
        resolved
    }
}

/// Описание схемы из `title` и `description`.
///
/// grpc-gateway кладёт первый абзац комментария в `title`, остальное — в `description`,
/// а однострочные комментарии — только в `title`. Для агента это один текст.
pub fn description_of(object: &Map<String, Value>) -> Option<String> {
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (title, description) {
        (Some(title), Some(description)) if title != description => {
            Some(format!("{title}\n\n{description}"))
        }
        (Some(text), _) | (None, Some(text)) => Some(text.to_owned()),
        (None, None) => None,
    }
}
