//! Регистрация MCP-сервера в клиентах (Claude Code, Codex).

pub mod claude;
pub mod codex;

use std::path::Path;

use anyhow::{Context, bail};
use clap::ValueEnum;

use crate::profiles::ProfileSettings;

/// Имя бинарника без расширения: по нему `mcp-kit` узнаёт свои регистрации в клиентах и
/// называет сервис одобренных прав в Keychain (`sourcecraft-mcp.access`).
pub const EXE_STEM: &str = "sourcecraft-mcp";

/// Абсолютный путь к бинарнику для тестов. В Windows путь без буквы диска не абсолютный,
/// поэтому `/usr/local/bin/...` там не годится.
#[cfg(test)]
pub(crate) const TEST_EXE: &str = if cfg!(windows) {
    r"C:\tools\sourcecraft-mcp.exe"
} else {
    "/usr/local/bin/sourcecraft-mcp"
};

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
/// из системного хранилища по профилю. Флагов доступа тоже нет: права — настройка профиля,
/// флаги `serve` их только сужают.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub command: String,
    pub args: Vec<String>,
}

impl ServerSpec {
    /// `exe` — абсолютный путь к бинарнику. Путь не канонизируется: при установке через
    /// Homebrew symlink в `bin/` переживает обновление версии, а путь в `Cellar` — нет.
    pub fn new(exe: &Path, profile: &str, settings: &ProfileSettings) -> anyhow::Result<Self> {
        let command = command_string(exe)?;

        let mut args = vec![
            "serve".to_owned(),
            "--profile".to_owned(),
            profile.to_owned(),
        ];
        if !settings.toolsets.is_empty() {
            args.push("--toolsets".to_owned());
            args.push(settings.toolsets.join(","));
        }
        if settings.allow_secret_values {
            args.push("--allow-secret-values".to_owned());
        }
        if settings.dynamic_toolsets {
            args.push("--dynamic-toolsets".to_owned());
        }
        Ok(Self { command, args })
    }

    /// Та же регистрация без флагов доступа (`--allow-write`, `--allow-destructive`,
    /// `--allow-path`): остальные аргументы сохраняются как есть. Для повторной регистрации
    /// из `ui`.
    pub fn without_access_flags(command: &Path, args: &[String]) -> anyhow::Result<Self> {
        let command = command_string(command)?;
        let mut kept = Vec::with_capacity(args.len());
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--allow-write" | "--allow-destructive" => {}
                "--allow-path" => {
                    iter.next();
                }
                other if other.starts_with("--allow-path=") => {}
                _ => kept.push(arg.clone()),
            }
        }
        Ok(Self {
            command,
            args: kept,
        })
    }

    /// Команда целиком для показа пользователю.
    pub fn display(&self) -> String {
        shell_join(
            std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)),
        )
    }
}

/// Абсолютный путь к бинарнику строкой (UTF-8).
fn command_string(exe: &Path) -> anyhow::Result<String> {
    if !exe.is_absolute() {
        bail!("executable path `{}` is not absolute", exe.display());
    }
    Ok(exe
        .to_str()
        .with_context(|| format!("executable path `{}` is not valid UTF-8", exe.display()))?
        .to_owned())
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
        let exe = PathBuf::from(TEST_EXE);
        let spec = ServerSpec::new(&exe, "codex", &ProfileSettings::default()).unwrap();
        assert_eq!(spec.command, TEST_EXE);
        assert_eq!(spec.args, ["serve", "--profile", "codex"]);

        let settings = ProfileSettings {
            client: None,
            allow_write: true,
            allow_destructive: true,
            allow_path: vec![PathBuf::from(TEST_EXE)],
            toolsets: vec!["repos".into(), "issues".into()],
            allow_secret_values: true,
            dynamic_toolsets: true,
        };
        let spec = ServerSpec::new(&exe, "ci", &settings).unwrap();
        // Права профиля в регистрацию не попадают.
        assert_eq!(
            spec.args,
            [
                "serve",
                "--profile",
                "ci",
                "--toolsets",
                "repos,issues",
                "--allow-secret-values",
                "--dynamic-toolsets"
            ]
        );
    }

    #[test]
    fn access_flags_are_dropped_from_old_registrations() {
        let exe = PathBuf::from(TEST_EXE);
        let old: Vec<String> = [
            "serve",
            "--profile",
            "ci",
            "--allow-write",
            "--allow-destructive",
            "--allow-path",
            "/srv/a",
            "--allow-path=/srv/b",
            "--toolsets",
            "issues",
            "--allow-secret-values",
        ]
        .map(str::to_owned)
        .to_vec();
        let spec = ServerSpec::without_access_flags(&exe, &old).unwrap();
        assert_eq!(spec.command, TEST_EXE);
        assert_eq!(
            spec.args,
            [
                "serve",
                "--profile",
                "ci",
                "--toolsets",
                "issues",
                "--allow-secret-values"
            ]
        );
        assert!(ServerSpec::without_access_flags(Path::new("x"), &old).is_err());
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
