//! Регистрация MCP-сервера в клиентах (Claude Code, Codex).

pub mod claude;
pub mod codex;

use std::path::Path;

use anyhow::{Context, bail};
use clap::ValueEnum;

use crate::profiles::ProfileSettings;

/// Имя сервера в конфигурации MCP-клиентов.
pub const SERVER_NAME: &str = "sourcecraft";

/// Поддерживаемые MCP-клиенты.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClientKind {
    /// Claude Code (`claude mcp add`).
    ClaudeCode,
    /// OpenAI Codex (`$CODEX_HOME/config.toml`).
    Codex,
}

impl ClientKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// Область конфигурации клиента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Scope {
    /// Available in all projects of the current user.
    #[default]
    User,
    /// Shared with the project (Claude Code: `.mcp.json` in the current directory).
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Команда запуска сервера, которую получает MCP-клиент. Токена в ней нет: `serve` берёт его
/// из системного хранилища по профилю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub command: String,
    pub args: Vec<String>,
}

impl ServerSpec {
    /// `exe` — абсолютный путь к бинарнику. Путь не канонизируется: при установке через
    /// Homebrew symlink в `bin/` переживает обновление версии, а путь в `Cellar` — нет.
    pub fn new(exe: &Path, profile: &str, settings: &ProfileSettings) -> anyhow::Result<Self> {
        if !exe.is_absolute() {
            bail!("executable path `{}` is not absolute", exe.display());
        }
        let command = exe
            .to_str()
            .with_context(|| format!("executable path `{}` is not valid UTF-8", exe.display()))?
            .to_owned();

        let mut args = vec![
            "serve".to_owned(),
            "--profile".to_owned(),
            profile.to_owned(),
        ];
        if settings.allow_write {
            args.push("--allow-write".to_owned());
        }
        if settings.allow_destructive {
            args.push("--allow-destructive".to_owned());
        }
        if !settings.toolsets.is_empty() {
            args.push("--toolsets".to_owned());
            args.push(settings.toolsets.join(","));
        }
        if settings.allow_secret_values {
            args.push("--allow-secret-values".to_owned());
        }
        Ok(Self { command, args })
    }

    /// Команда целиком для показа пользователю.
    pub fn display(&self) -> String {
        shell_join(
            std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)),
        )
    }
}

/// Склеивает аргументы в строку для показа, экранируя их как в POSIX shell.
pub fn shell_join<'a>(args: impl IntoIterator<Item = &'a str>) -> String {
    args.into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ',' | ':' | '=' | '@' | '+')
        });
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn server_spec_args() {
        let exe = PathBuf::from("/opt/homebrew/bin/sourcecraft-mcp");
        let spec = ServerSpec::new(&exe, "codex", &ProfileSettings::default()).unwrap();
        assert_eq!(spec.command, "/opt/homebrew/bin/sourcecraft-mcp");
        assert_eq!(spec.args, ["serve", "--profile", "codex"]);

        let settings = ProfileSettings {
            client: None,
            allow_write: true,
            allow_destructive: true,
            toolsets: vec!["repos".into(), "issues".into()],
            allow_secret_values: true,
        };
        let spec = ServerSpec::new(&exe, "ci", &settings).unwrap();
        assert_eq!(
            spec.args,
            [
                "serve",
                "--profile",
                "ci",
                "--allow-write",
                "--allow-destructive",
                "--toolsets",
                "repos,issues",
                "--allow-secret-values"
            ]
        );
    }

    #[test]
    fn server_spec_requires_absolute_path() {
        let spec = ServerSpec::new(
            Path::new("sourcecraft-mcp"),
            "x",
            &ProfileSettings::default(),
        );
        assert!(spec.is_err());
    }

    #[test]
    fn shell_join_quotes_unsafe_args() {
        assert_eq!(
            shell_join(["/My Apps/bin", "it's", "plain-arg"]),
            r"'/My Apps/bin' 'it'\''s' plain-arg"
        );
    }
}
