//! Claude Code: регистрация через `claude mcp add`.
//!
//! Конфигурацию Claude Code (`~/.claude.json`, `.mcp.json`) напрямую не правим — только через
//! его CLI, чтобы не зависеть от внутреннего формата файлов.

use std::io;
use std::process::Command;

use anyhow::{Context, bail};

use super::{SERVER_NAME, Scope, ServerSpec, shell_join};
use crate::ui::Ui;

/// Имя исполняемого файла Claude Code CLI.
pub const CLAUDE_PROGRAM: &str = "claude";

/// Результат внешней команды.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Запуск внешних команд; в тестах подменяется записью вызовов.
pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

/// Запуск через `std::process::Command`.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Аргументы `claude mcp add --scope <scope> sourcecraft -- <exe> serve --profile <p> ...`.
pub fn add_args(scope: Scope, spec: &ServerSpec) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "--scope", scope.as_str(), SERVER_NAME, "--"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    args.push(spec.command.clone());
    args.extend(spec.args.iter().cloned());
    args
}

/// Аргументы `claude mcp remove --scope <scope> sourcecraft`.
pub fn remove_args(scope: Scope) -> Vec<String> {
    ["mcp", "remove", "--scope", scope.as_str(), SERVER_NAME]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn display(args: &[String]) -> String {
    shell_join(std::iter::once(CLAUDE_PROGRAM).chain(args.iter().map(String::as_str)))
}

/// Проверяет, что Claude Code CLI доступен, до того как пользователь введёт токен.
pub fn ensure_available(runner: &mut dyn CommandRunner) -> anyhow::Result<()> {
    match runner.run(CLAUDE_PROGRAM, &["--version".to_owned()]) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => bail!(
            "`{CLAUDE_PROGRAM} --version` failed: {}",
            output.stderr.trim()
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(
            "Claude Code CLI `{CLAUDE_PROGRAM}` was not found in PATH; install Claude Code first"
        ),
        Err(err) => Err(err).context("failed to run Claude Code CLI"),
    }
}

/// Сообщение Claude Code о том, что сервер с таким именем уже есть.
fn already_exists(output: &CommandOutput) -> bool {
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("already exists")
}

fn run_checked(runner: &mut dyn CommandRunner, args: &[String]) -> anyhow::Result<CommandOutput> {
    runner
        .run(CLAUDE_PROGRAM, args)
        .with_context(|| format!("failed to run `{}`", display(args)))
}

/// Регистрирует сервер в Claude Code. Возвращает `false`, если пользователь отказался.
pub fn register(
    runner: &mut dyn CommandRunner,
    ui: &mut dyn Ui,
    scope: Scope,
    spec: &ServerSpec,
    yes: bool,
) -> anyhow::Result<bool> {
    let add = add_args(scope, spec);
    ui.say(&format!(
        "Claude Code ({} scope) will get MCP server `{SERVER_NAME}`:\n  {}",
        scope.as_str(),
        display(&add)
    ));
    if !yes && !ui.confirm("Register the server in Claude Code?")? {
        return Ok(false);
    }

    let output = run_checked(runner, &add)?;
    if output.success {
        return Ok(true);
    }
    if !already_exists(&output) {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }

    let remove = remove_args(scope);
    ui.say(&format!(
        "Claude Code already has an MCP server named `{SERVER_NAME}` in {} scope. \
         It will be replaced:\n  {}\n  {}",
        scope.as_str(),
        display(&remove),
        display(&add)
    ));
    if !yes && !ui.confirm("Replace the existing entry?")? {
        return Ok(false);
    }
    let output = run_checked(runner, &remove)?;
    if !output.success {
        bail!("`{}` failed: {}", display(&remove), output.stderr.trim());
    }
    let output = run_checked(runner, &add)?;
    if !output.success {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }
    Ok(true)
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Записывает вызовы и возвращает заранее заданные результаты; `claude` не запускается.
    #[derive(Debug, Default)]
    pub struct FakeRunner {
        pub calls: Vec<Vec<String>>,
        pub results: VecDeque<CommandOutput>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
            assert_eq!(program, CLAUDE_PROGRAM);
            self.calls.push(args.to_vec());
            Ok(self.results.pop_front().unwrap_or(CommandOutput {
                success: true,
                ..Default::default()
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeRunner;
    use super::*;
    use crate::ui::testing::ScriptedUi;

    fn spec() -> ServerSpec {
        ServerSpec {
            command: "/usr/local/bin/sourcecraft-mcp".into(),
            args: vec!["serve".into(), "--profile".into(), "claude-code".into()],
        }
    }

    #[test]
    fn builds_add_and_remove_argv() {
        assert_eq!(
            add_args(Scope::User, &spec()),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "sourcecraft",
                "--",
                "/usr/local/bin/sourcecraft-mcp",
                "serve",
                "--profile",
                "claude-code"
            ]
        );
        assert_eq!(
            remove_args(Scope::Project),
            ["mcp", "remove", "--scope", "project", "sourcecraft"]
        );
    }

    #[test]
    fn register_with_confirmation() {
        let mut runner = FakeRunner::default();
        let mut ui = ScriptedUi {
            answers: [true].into(),
            ..Default::default()
        };
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
        assert_eq!(runner.calls, [add_args(Scope::User, &spec())]);
        assert!(
            ui.text()
                .contains("claude mcp add --scope user sourcecraft --")
        );
    }

    #[test]
    fn declined_registration_runs_nothing() {
        let mut runner = FakeRunner::default();
        let mut ui = ScriptedUi {
            answers: [false].into(),
            ..Default::default()
        };
        assert!(!register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn replaces_existing_entry() {
        let mut runner = FakeRunner {
            results: [CommandOutput {
                success: false,
                stderr: "MCP server sourcecraft already exists in user config".into(),
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap());
        assert_eq!(
            runner.calls,
            [
                add_args(Scope::User, &spec()),
                remove_args(Scope::User),
                add_args(Scope::User, &spec())
            ]
        );
        assert!(ui.questions.is_empty());
    }

    #[test]
    fn other_failures_are_errors() {
        let mut runner = FakeRunner {
            results: [CommandOutput {
                success: false,
                stderr: "boom".into(),
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        let err = register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(runner.calls.len(), 1);
    }
}
