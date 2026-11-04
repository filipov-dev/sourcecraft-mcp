//! Генерация таблицы операций из Swagger-спецификации SourceCraft.
//!
//! Результат — `$OUT_DIR/operations.rs` со статической таблицей `OPERATIONS`,
//! которую подключает `src/spec.rs`. Подробности:
//!
//! - `build/generate.rs` — разбор операций, объединение пар slug/id, вывод кода;
//! - `build/naming.rs` — имена инструментов;
//! - `build/access.rs` — исключения из уровня доступа по HTTP-методу;
//! - `build/schema.rs` — JSON Schema аргументов с раскрытыми `$ref`.

#[path = "build/access.rs"]
mod access;
#[path = "build/generate.rs"]
mod generate;
#[path = "build/naming.rs"]
mod naming;
#[path = "build/schema.rs"]
mod schema;

use std::{env, fs, path::PathBuf};

const SPEC_PATH: &str = "spec/sourcecraft.swagger.json";

fn main() {
    println!("cargo:rerun-if-changed={SPEC_PATH}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build");

    let raw = fs::read_to_string(SPEC_PATH).expect("не удалось прочитать спецификацию SourceCraft");
    let spec: serde_json::Value =
        serde_json::from_str(&raw).expect("спецификация SourceCraft не является корректным JSON");

    let code = generate::generate(&spec);

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR не задан")).join("operations.rs");
    fs::write(out, code).expect("не удалось записать сгенерированные операции");
}
