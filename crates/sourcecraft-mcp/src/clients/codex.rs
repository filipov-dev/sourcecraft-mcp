//! Codex: запись `[mcp_servers.sourcecraft]` в `$CODEX_HOME/config.toml`.
//!
//! Файл правится через `toml_edit`, чтобы сохранить комментарии, форматирование и чужие записи.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::{SERVER_NAME, ServerSpec};
use crate::ui::Ui;

/// Переменная окружения Codex с каталогом его конфигурации.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Каталог конфигурации Codex: `$CODEX_HOME`, иначе `~/.codex`.
pub fn codex_home(env_value: Option<OsString>, home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = env_value.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    home.map(|home| home.join(".codex"))
        .context("cannot determine the home directory; set CODEX_HOME")
}

/// Каталог конфигурации Codex текущего пользователя.
pub fn default_codex_home() -> anyhow::Result<PathBuf> {
    codex_home(std::env::var_os(CODEX_HOME_ENV), dirs::home_dir())
}

/// Путь к `config.toml` в каталоге Codex.
pub fn config_path(codex_home: &Path) -> PathBuf {
    codex_home.join("config.toml")
}

/// Записывает `command` и `args` сервера в документ. Остальные записи не меняются; если
/// `[mcp_servers.sourcecraft]` уже есть, таблица очищается и заполняется заново на том же месте
/// (с теми же комментариями над заголовком). Возвращает имена ключей прежней записи.
///
/// Значения прежней записи не возвращаются и не показываются: в старой конфигурации в `env`
/// мог лежать токен.
pub fn apply_entry(
    doc: &mut DocumentMut,
    spec: &ServerSpec,
) -> anyhow::Result<Option<Vec<String>>> {
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = Table::new();
            // Без отдельного пустого заголовка `[mcp_servers]`.
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .context("`mcp_servers` in the Codex config is not a table; edit it manually")?;

    let mut args = Array::new();
    for arg in &spec.args {
        args.push(arg.as_str());
    }

    let previous = match servers.get_mut(SERVER_NAME) {
        Some(Item::Table(table)) => {
            let previous = table.iter().map(|(key, _)| key.to_owned()).collect();
            table.clear();
            table.insert("command", value(spec.command.as_str()));
            table.insert("args", value(args));
            Some(previous)
        }
        Some(other) => {
            let previous = vec![format!("<{}>", other.type_name())];
            *other = Item::Table(new_entry(spec, args));
            Some(previous)
        }
        None => {
            servers.insert(SERVER_NAME, Item::Table(new_entry(spec, args)));
            None
        }
    };
    Ok(previous)
}

fn new_entry(spec: &ServerSpec, args: Array) -> Table {
    let mut table = Table::new();
    table.insert("command", value(spec.command.as_str()));
    table.insert("args", value(args));
    table
}

/// Запись сервера в виде TOML для показа пользователю.
pub fn render_entry(spec: &ServerSpec) -> anyhow::Result<String> {
    let mut doc = DocumentMut::new();
    apply_entry(&mut doc, spec)?;
    Ok(doc.to_string())
}

/// Регистрирует сервер в Codex. Возвращает `false`, если пользователь отказался.
pub fn register(
    ui: &mut dyn Ui,
    codex_home: &Path,
    spec: &ServerSpec,
    yes: bool,
) -> anyhow::Result<bool> {
    let path = config_path(codex_home);
    let original = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let previous = apply_entry(&mut doc, spec)?;

    let mut message = format!(
        "Codex config {} will get:\n\n{}",
        path.display(),
        render_entry(spec)?.trim_end()
    );
    if let Some(previous) = &previous {
        message.push_str(&format!(
            "\n\nIt replaces the existing `{SERVER_NAME}` entry (keys: {}).",
            previous.join(", ")
        ));
    }
    ui.say(&message);
    if !yes && !ui.confirm("Write the Codex config?")? {
        return Ok(false);
    }

    let updated = doc.to_string();
    if updated == original {
        ui.say("Codex config already up to date.");
        return Ok(true);
    }
    if !codex_home.exists() {
        fs::create_dir_all(codex_home)
            .with_context(|| format!("failed to create {}", codex_home.display()))?;
    }
    // Запись по исходному пути (а не через rename), чтобы не сломать symlink на dotfiles.
    fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

/// Проверка, которую делаем до ввода токена: Codex поддерживает только пользовательскую область.
pub fn ensure_scope(scope: super::Scope) -> anyhow::Result<()> {
    if scope != super::Scope::User {
        bail!("Codex supports only `--scope user` (config in $CODEX_HOME/config.toml)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testing::ScriptedUi;

    fn spec() -> ServerSpec {
        ServerSpec {
            command: "/usr/local/bin/sourcecraft-mcp".into(),
            args: vec!["serve".into(), "--profile".into(), "codex".into()],
        }
    }

    const EXISTING: &str = r#"# Codex config
model = "gpt-5"  # inline comment

[mcp_servers.other]
command = "other-mcp"
args = ["--flag"]

# our server
[mcp_servers.sourcecraft]
url = "http://localhost:8080/mcp"
env = { TOKEN = "old-secret-value" }

[profiles.fast]
model = "gpt-5-mini"
"#;

    #[test]
    fn codex_home_resolution() {
        assert_eq!(
            codex_home(Some("/tmp/codex".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/tmp/codex")
        );
        assert_eq!(
            codex_home(Some("".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.codex")
        );
        assert!(codex_home(None, None).is_err());
    }

    #[test]
    fn adds_entry_to_empty_config() {
        let mut doc = DocumentMut::new();
        assert_eq!(apply_entry(&mut doc, &spec()).unwrap(), None);
        assert_eq!(
            doc.to_string(),
            "[mcp_servers.sourcecraft]\ncommand = \"/usr/local/bin/sourcecraft-mcp\"\nargs = [\"serve\", \"--profile\", \"codex\"]\n"
        );
    }

    #[test]
    fn replaces_entry_and_preserves_everything_else() {
        let mut doc: DocumentMut = EXISTING.parse().unwrap();
        let previous = apply_entry(&mut doc, &spec()).unwrap().unwrap();
        assert_eq!(previous, ["url", "env"]);

        let text = doc.to_string();
        assert_eq!(
            text,
            r#"# Codex config
model = "gpt-5"  # inline comment

[mcp_servers.other]
command = "other-mcp"
args = ["--flag"]

# our server
[mcp_servers.sourcecraft]
command = "/usr/local/bin/sourcecraft-mcp"
args = ["serve", "--profile", "codex"]

[profiles.fast]
model = "gpt-5-mini"
"#
        );
    }

    #[test]
    fn register_writes_file_in_temp_codex_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        let mut ui = ScriptedUi {
            answers: [true].into(),
            ..Default::default()
        };
        assert!(register(&mut ui, &home, &spec(), false).unwrap());
        let text = fs::read_to_string(config_path(&home)).unwrap();
        assert!(text.contains("[mcp_servers.sourcecraft]"));
        assert!(
            ui.text()
                .contains("args = [\"serve\", \"--profile\", \"codex\"]")
        );

        // Повторный запуск ничего не меняет.
        assert!(register(&mut ui, &home, &spec(), true).unwrap());
        assert_eq!(fs::read_to_string(config_path(&home)).unwrap(), text);
    }

    #[test]
    fn register_keeps_file_when_declined() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(config_path(dir.path()), EXISTING).unwrap();
        let mut ui = ScriptedUi {
            answers: [false].into(),
            ..Default::default()
        };
        assert!(!register(&mut ui, dir.path(), &spec(), false).unwrap());
        assert_eq!(
            fs::read_to_string(config_path(dir.path())).unwrap(),
            EXISTING
        );
        let text = ui.text();
        assert!(text.contains("replaces the existing `sourcecraft` entry (keys: url, env)"));
        // Значения прежней записи (в том числе `env`) не показываются.
        assert!(!text.contains("old-secret-value"), "{text}");
    }

    #[test]
    fn rejects_non_table_mcp_servers() {
        let mut doc: DocumentMut = "mcp_servers = 1\n".parse().unwrap();
        assert!(apply_entry(&mut doc, &spec()).is_err());
    }
}
