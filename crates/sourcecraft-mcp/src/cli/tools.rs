//! Пользовательские инструменты: загрузка `tools.toml` для `serve` и `sourcecraft-mcp tools check`.
//!
//! Файл пользователя `~/.config/sourcecraft-mcp/tools.toml` читается всегда, если он есть.
//! Другие файлы (например, из репозитория проекта) — только по явному `--tools-file`: файл
//! из клонированного репозитория — недоверенный ввод и не должен молча добавлять инструменты.
//! Проверку определений и имён выполняет ядро (`sourcecraft_mcp_core::custom_tools`).

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use sourcecraft_api::AccessLevel;
use sourcecraft_mcp_core::custom_tools::check_names;
use sourcecraft_mcp_core::{CustomTool, CustomToolsFile};

use crate::profiles::{ProfileRepo, config_dir};

/// Имя файла пользовательских инструментов в каталоге конфигурации.
pub const TOOLS_FILE: &str = "tools.toml";

#[derive(Debug, Subcommand)]
pub enum ToolsCommand {
    /// Validate custom tool files and list the tools they define.
    Check(CheckArgs),
}

#[derive(Debug, Args)]
#[command(after_help = "\
The user file ~/.config/sourcecraft-mcp/tools.toml is always checked when it exists. Pass the \
same --tools-file options as to `serve` to check additional files.")]
pub struct CheckArgs {
    /// Additional custom tools file, as passed to `serve --tools-file`. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
}

pub fn run(command: ToolsCommand) -> anyhow::Result<()> {
    match command {
        ToolsCommand::Check(args) => {
            let tools = load(&args.tools_file)?;
            print!("{}", render(&tools));
            Ok(())
        }
    }
}

/// Путь к файлу пользователя; `None` — каталог конфигурации неизвестен.
pub fn default_tools_file() -> Option<PathBuf> {
    config_dir().ok().map(|dir| dir.join(TOOLS_FILE))
}

/// Файл пользователя рядом с файлом профилей `repo` (в тестах — во временном каталоге).
pub fn user_tools_file(repo: &ProfileRepo) -> PathBuf {
    repo.path()
        .parent()
        .map(|dir| dir.join(TOOLS_FILE))
        .unwrap_or_else(|| PathBuf::from(TOOLS_FILE))
}

/// Загружает инструменты из файла пользователя (если он есть) и из `extra` (обязательны).
pub fn load(extra: &[PathBuf]) -> anyhow::Result<Vec<CustomTool>> {
    load_with(default_tools_file().as_deref(), extra)
}

/// То же, что [`load`], с явным путём файла пользователя: файла может и не быть.
pub fn load_with(default: Option<&Path>, extra: &[PathBuf]) -> anyhow::Result<Vec<CustomTool>> {
    let default = default.filter(|path| path.exists());
    load_files(default, extra)
}

fn load_files(default: Option<&Path>, extra: &[PathBuf]) -> anyhow::Result<Vec<CustomTool>> {
    let mut tools = Vec::new();
    // Один и тот же файл, переданный дважды (например, файл пользователя ещё и через
    // `--tools-file`), читается один раз.
    let mut seen = Vec::new();
    for path in default
        .into_iter()
        .chain(extra.iter().map(PathBuf::as_path))
    {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        tools.extend(load_file(path)?);
    }
    check_names(&tools).map_err(|err| anyhow!(err))?;
    Ok(tools)
}

fn load_file(path: &Path) -> anyhow::Result<Vec<CustomTool>> {
    let source = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the custom tools file {source}"))?;
    let file: CustomToolsFile =
        toml::from_str(&text).map_err(|err| anyhow!("{source}: invalid tools file: {err}"))?;
    file.tools
        .into_iter()
        .map(|def| CustomTool::compile(def, &source).map_err(|err| anyhow!(err)))
        .collect()
}

/// Таблица инструментов для `tools check`.
fn render(tools: &[CustomTool]) -> String {
    if tools.is_empty() {
        let hint = default_tools_file()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| format!("~/.config/sourcecraft-mcp/{TOOLS_FILE}"));
        return format!("No custom tools defined (user file: {hint}).\n");
    }
    let width = tools
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0)
        .max("TOOL".len());
    let mut out = format!("{:<width$}  {:<11}  REQUEST\n", "TOOL", "ACCESS");
    for tool in tools {
        let access = match tool.access {
            AccessLevel::Read => "read",
            AccessLevel::Write => "write",
            AccessLevel::Destructive => "destructive",
        };
        out.push_str(&format!(
            "{:<width$}  {:<11}  {} {}  ({})\n",
            tool.name,
            access,
            tool.method.as_str(),
            tool.path,
            tool.source
        ));
    }
    out.push_str(
        "\nTools above the access of the server's profile (`sourcecraft-mcp ui`) are not \
         registered.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[tools]]
name = "my_open_prs"
description = "My open pull requests in the acme organization"
method = "GET"
path = "/orgs/acme/pulls"
query = { state = "open", page_size = "{limit}" }
jq = ".pull_requests[] | {id, title}"

[[tools.arguments]]
name = "limit"
type = "integer"
description = "How many pull requests to return"

[[tools]]
name = "comment_issue"
description = "Comment on an issue"
method = "POST"
path = "/repos/{org}/{repo}/issues/{issue}/comments"
body = { text = "{text}" }
arguments = [
  { name = "org", required = true },
  { name = "repo", required = true },
  { name = "issue", required = true },
  { name = "text", required = true },
]
"#;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn loads_and_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "tools.toml", SAMPLE);
        let tools = load_files(Some(&path), &[]).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].access, AccessLevel::Read);
        assert_eq!(tools[1].access, AccessLevel::Write);
        let table = render(&tools);
        assert!(table.contains("my_open_prs"), "{table}");
        assert!(table.contains("write"), "{table}");
        assert!(table.contains("POST /repos/{org}/{repo}/issues/{issue}/comments"));
        assert!(render(&[]).contains("No custom tools"));
    }

    #[test]
    fn reports_errors_with_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let bad_toml = write(dir.path(), "a.toml", "[[tools]\nname = 1");
        let err = load_files(None, &[bad_toml]).unwrap_err().to_string();
        assert!(err.contains("a.toml: invalid tools file"), "{err}");

        let unknown = write(
            dir.path(),
            "b.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\nmethod = \"GET\"\npath = \"/user\"\n\
             headers = { Authorization = \"x\" }\n",
        );
        let err = load_files(None, &[unknown]).unwrap_err().to_string();
        assert!(err.contains("unknown field `headers`"), "{err}");

        let missing = dir.path().join("missing.toml");
        let err = load_files(None, &[missing]).unwrap_err().to_string();
        assert!(err.contains("cannot read"), "{err}");

        let user = write(dir.path(), "tools.toml", SAMPLE);
        assert_eq!(
            load_files(Some(&user), std::slice::from_ref(&user))
                .unwrap()
                .len(),
            2
        );
        let project = write(dir.path(), "project.toml", SAMPLE);
        let err = load_files(Some(&user), &[project]).unwrap_err().to_string();
        assert!(err.contains("defined more than once"), "{err}");
    }
}
