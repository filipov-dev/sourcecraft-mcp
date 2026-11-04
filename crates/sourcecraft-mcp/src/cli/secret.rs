//! `sourcecraft-mcp secret` — использование секретов репозитория в командах оболочки без
//! попадания значений в контекст агента.
//!
//! - `exec` передаёт секреты команде в переменных окружения и маскирует их в её выводе;
//! - `write` пишет значение в новый файл с правами 0600 (для `scp`, `ssh -i` и т. п.);
//! - `list` показывает только ключи;
//! - `get --reveal` печатает значение — явное и осознанное раскрытие.
//!
//! `get` не требует `allow_secret_values` профиля: эта настройка ограничивает ответы
//! MCP-сервера, а CLI запускают явно. Против агента с доступом к оболочке настройка всё равно
//! не защитила бы (`exec` с преобразованием значения обходит маскирование), поэтому граница —
//! явный флаг `--reveal` и предупреждение, а не ложное ощущение запрета.
//!
//! Токен берётся только из системного хранилища по профилю, как у `serve`.

mod exec;
mod fetch;
mod mask;
mod write;

use std::ffi::OsString;
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail};
use clap::{Args, Subcommand};
use sourcecraft_api::executor::{ApiClient, ClientConfig};

use crate::base_url::{BASE_URL_ENV, resolve_base_url};
use crate::profiles::load_profile_explained;
use fetch::{RepoTarget, Secrets, fetch_secrets};

/// Переменная окружения с профилем по умолчанию для команд `secret` (имя профиля, не токен).
pub const PROFILE_ENV: &str = "SOURCECRAFT_MCP_PROFILE";

/// Таймаут запроса к API.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Лимит ответа со списком секретов.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Run a command with secrets in its environment; their values are masked in its output.
    #[command(after_help = EXEC_HELP)]
    Exec(ExecArgs),
    /// Write a secret value to a new file (mode 0600) without printing it.
    #[command(after_help = WRITE_HELP)]
    Write(WriteArgs),
    /// List secret keys of a repository (never values).
    List(ListArgs),
    /// Print a secret value. Requires --reveal.
    #[command(after_help = GET_HELP)]
    Get(GetArgs),
}

const EXEC_HELP: &str = "\
The command runs directly, without a shell; its stdin is inherited. Values of the injected \
secrets (and their base64 forms) are replaced with *** in its stdout and stderr; values shorter \
than 4 bytes are not masked. The exit code of the command is returned.

To use $NAME in a command line, run a shell yourself, in single quotes, so that the child shell \
expands it:
  sourcecraft-mcp secret exec --repo acme/web --env KEY=DEPLOY_KEY -- \\
    sh -c 'printf %s \"$KEY\" | ssh deploy@host \"cat > ~/.config/app/key\"'

Never echo or print secret variables. Masking is a safety net against accidental output, not a \
security boundary: a value that the command transforms (hex, URL encoding, split) is not \
recognized.";

const WRITE_HELP: &str = "\
The file is created new (an existing file or symlink is not overwritten without --force) with \
mode 0600. Only the path and the number of bytes are printed. Delete the file when done:
  sourcecraft-mcp secret write --repo acme/web DEPLOY_KEY --to /tmp/deploy_key && \\
    scp -i /tmp/deploy_key build.tar deploy@host: ; rm -f /tmp/deploy_key";

const GET_HELP: &str = "\
Without --reveal the command fails: prefer `secret exec` or `secret write`, which use the value \
without showing it. With --reveal the value is printed to stdout as is; when stdout is not a \
terminal, a reminder is printed to stderr that the value is now in the caller's output.";

/// Профиль, репозиторий и адрес API — общие аргументы всех команд `secret`.
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// Token profile stored in the system keychain.
    #[arg(long, env = PROFILE_ENV, default_value = "default")]
    pub profile: String,
    /// Repository as <org>/<repo>.
    #[arg(long, value_name = "ORG/REPO", required_unless_present = "repo_id")]
    pub repo: Option<String>,
    /// Repository ID, instead of --repo.
    #[arg(long, value_name = "ID", conflicts_with = "repo")]
    pub repo_id: Option<String>,
    /// Secret group [default: the API default group].
    #[arg(long, value_name = "NAME")]
    pub group: Option<String>,
    /// SourceCraft API URL [default: $SOURCECRAFT_BASE_URL or https://api.sourcecraft.tech].
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Set variable NAME to secret KEY (`NAME=KEY`), or variable <prefix>KEY to secret KEY
    /// (`KEY`). Repeatable.
    #[arg(long = "env", short = 'e', value_name = "NAME=KEY")]
    pub env: Vec<String>,
    /// Inject every secret of the repository under its own key (plus --prefix).
    #[arg(long)]
    pub all: bool,
    /// Prefix for variable names derived from keys (--all and `--env KEY`).
    #[arg(long, value_name = "PREFIX", default_value = "")]
    pub prefix: String,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct WriteArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Secret key.
    pub key: String,
    /// File to create.
    #[arg(long, value_name = "PATH")]
    pub to: PathBuf,
    /// File mode, octal.
    #[arg(long, value_name = "OCTAL", default_value = "600", value_parser = write::parse_mode)]
    pub mode: u32,
    /// Replace an existing file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub target: TargetArgs,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Secret key.
    pub key: String,
    /// Explicitly allow printing the value.
    #[arg(long)]
    pub reveal: bool,
}

pub async fn run(command: SecretCommand) -> anyhow::Result<ExitCode> {
    match command {
        SecretCommand::Exec(args) => run_exec(args).await,
        SecretCommand::Write(args) => {
            let (client, target) = connect(&args.target)?;
            let secrets = fetch_secrets(
                &client,
                &target,
                args.target.group.as_deref(),
                std::slice::from_ref(&args.key),
            )
            .await?;
            let value = require(&secrets, &args.key)?;
            if value.is_empty() {
                warn(format!("secret `{}` is empty", args.key));
            }
            if args.mode & 0o077 != 0 {
                warn(format!(
                    "mode {:o} lets other users read the secret file",
                    args.mode
                ));
            }
            write::write_secret_file(&args.to, value, args.mode, args.force)?;
            println!(
                "Wrote {} bytes to {} (mode {:o})",
                value.len(),
                args.to.display(),
                args.mode
            );
            Ok(ExitCode::SUCCESS)
        }
        SecretCommand::List(args) => {
            let (client, target) = connect(&args.target)?;
            let secrets =
                fetch_secrets(&client, &target, args.target.group.as_deref(), &[]).await?;
            if secrets.is_empty() {
                eprintln!("No secrets.");
            }
            for key in secrets.keys() {
                println!("{key}");
            }
            Ok(ExitCode::SUCCESS)
        }
        SecretCommand::Get(args) => {
            // Проверяем до обращения к keychain и API.
            check_reveal(&args)?;
            let (client, target) = connect(&args.target)?;
            let secrets = fetch_secrets(
                &client,
                &target,
                args.target.group.as_deref(),
                std::slice::from_ref(&args.key),
            )
            .await?;
            let value = require(&secrets, &args.key)?;
            let mut stdout = std::io::stdout().lock();
            let terminal = stdout.is_terminal();
            stdout.write_all(value)?;
            if terminal && !value.ends_with(b"\n") {
                stdout.write_all(b"\n")?;
            }
            stdout.flush()?;
            if !terminal {
                eprintln!(
                    "sourcecraft-mcp: note: the value of secret `{}` was written to stdout and is \
                     now in the output of the calling program (and in an AI agent's context, if \
                     an agent ran this command)",
                    args.key
                );
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn check_reveal(args: &GetArgs) -> anyhow::Result<()> {
    if args.reveal {
        return Ok(());
    }
    bail!(
        "`secret get` prints the value of `{key}` only with --reveal. To use the secret without \
         seeing it, run a command with it in the environment: `sourcecraft-mcp secret exec \
         --repo <org>/<repo> --env NAME={key} -- <command>`, or write it to a file: \
         `sourcecraft-mcp secret write --repo <org>/<repo> {key} --to <path>`",
        key = args.key
    )
}

async fn run_exec(args: ExecArgs) -> anyhow::Result<ExitCode> {
    if args.env.is_empty() && !args.all {
        bail!("nothing to inject: pass --env NAME=KEY (repeatable) or --all");
    }
    let explicit = args
        .env
        .iter()
        .map(|arg| exec::parse_env_arg(arg, &args.prefix))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (client, target) = connect(&args.target)?;
    // Без `--all` запрашиваем только нужные ключи.
    let keys: Vec<String> = if args.all {
        Vec::new()
    } else {
        explicit.iter().map(|b| b.key.clone()).collect()
    };
    let secrets = fetch_secrets(&client, &target, args.target.group.as_deref(), &keys).await?;
    let bindings = exec::resolve_bindings(&explicit, args.all, &args.prefix, &secrets, &mut warn)?;
    if bindings.is_empty() {
        warn("no secrets to inject: the repository has none".to_owned());
    }
    let exec::PreparedEnv { env, needles } = exec::prepare_env(&bindings, &secrets, &mut warn)?;
    drop(secrets);

    // Ctrl-C получает и дочерний процесс (общая группа); сами не завершаемся, чтобы дочитать
    // и замаскировать его вывод и вернуть его код.
    tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });
    let code = exec::run_masked(
        &args.command,
        &env,
        &needles,
        tokio::io::stdout(),
        tokio::io::stderr(),
    )
    .await?;
    Ok(ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)))
}

/// Клиент API для профиля и репозиторий из аргументов.
fn connect(target: &TargetArgs) -> anyhow::Result<(ApiClient, RepoTarget)> {
    let repo = RepoTarget::from_args(target.repo.as_deref(), target.repo_id.as_deref())?;
    let from_env = std::env::var(BASE_URL_ENV).ok();
    let base_url = resolve_base_url(target.base_url.as_deref(), from_env.as_deref())?;
    let (_settings, token) = load_profile_explained(&target.profile)?;
    let client = ApiClient::new(ClientConfig {
        base_url,
        token,
        timeout: TIMEOUT,
        max_response_bytes: MAX_RESPONSE_BYTES,
    })
    .map_err(|err| anyhow!("cannot create the SourceCraft API client: {err}"))?;
    Ok((client, repo))
}

fn require<'a>(secrets: &'a Secrets, key: &str) -> anyhow::Result<&'a [u8]> {
    secrets.get(key).ok_or_else(|| {
        anyhow!(
            "secret `{key}` not found in the repository. See the keys with `sourcecraft-mcp \
             secret list`"
        )
    })
}

fn warn(message: String) {
    eprintln!("sourcecraft-mcp: warning: {message}");
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(subcommand)]
        secret: SecretCommand,
    }

    fn parse(args: &[&str]) -> Result<SecretCommand, clap::Error> {
        let mut argv = vec!["secret"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).map(|w| w.secret)
    }

    #[test]
    fn parses_exec() {
        let SecretCommand::Exec(args) = parse(&[
            "exec",
            "--repo",
            "acme/web",
            "-e",
            "A=KEY",
            "--env",
            "B",
            "--prefix",
            "P_",
            "--",
            "sh",
            "-c",
            "echo --env",
        ])
        .unwrap() else {
            panic!("not exec");
        };
        assert_eq!(args.target.repo.as_deref(), Some("acme/web"));
        assert_eq!(args.env, ["A=KEY", "B"]);
        assert_eq!(args.prefix, "P_");
        assert_eq!(args.command, ["sh", "-c", "echo --env"]);

        // Команда обязательна и идёт после `--`.
        assert!(parse(&["exec", "--repo", "a/b", "--all"]).is_err());
        // Репозиторий обязателен, и только один способ адресации.
        assert!(parse(&["exec", "--all", "--", "true"]).is_err());
        assert!(
            parse(&[
                "exec",
                "--repo",
                "a/b",
                "--repo-id",
                "1",
                "--all",
                "--",
                "true"
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_write_and_get() {
        let SecretCommand::Write(args) =
            parse(&["write", "--repo-id", "7", "KEY", "--to", "/tmp/x"]).unwrap()
        else {
            panic!("not write");
        };
        assert_eq!(args.mode, 0o600);
        assert!(!args.force);
        assert!(
            parse(&[
                "write", "--repo", "a/b", "KEY", "--to", "x", "--mode", "999"
            ])
            .is_err()
        );

        let SecretCommand::Get(args) = parse(&["get", "--repo", "a/b", "KEY"]).unwrap() else {
            panic!("not get");
        };
        assert!(!args.reveal);
        assert_eq!(args.target.profile, "default");
    }

    #[test]
    fn get_requires_reveal() {
        let SecretCommand::Get(args) =
            parse(&["get", "--profile", "codex", "--repo", "a/b", "API_TOKEN"]).unwrap()
        else {
            panic!("not get");
        };
        let err = check_reveal(&args).unwrap_err().to_string();
        assert!(err.contains("--reveal"), "{err}");
        assert!(err.contains("secret exec"), "{err}");
        assert!(err.contains("secret write"), "{err}");

        let SecretCommand::Get(args) =
            parse(&["get", "--repo", "a/b", "API_TOKEN", "--reveal"]).unwrap()
        else {
            panic!("not get");
        };
        check_reveal(&args).unwrap();
    }

    #[tokio::test]
    async fn get_without_reveal_fails_before_keychain() {
        let SecretCommand::Get(args) =
            parse(&["get", "--profile", "no-such-profile", "--repo", "a/b", "K"]).unwrap()
        else {
            panic!("not get");
        };
        let err = run(SecretCommand::Get(args)).await.unwrap_err().to_string();
        assert!(err.contains("only with --reveal"), "{err}");
    }

    #[tokio::test]
    async fn exec_needs_something_to_inject() {
        let SecretCommand::Exec(args) = parse(&["exec", "--repo", "a/b", "--", "true"]).unwrap()
        else {
            panic!("not exec");
        };
        let err = run_exec(args).await.unwrap_err().to_string();
        assert!(err.contains("nothing to inject"), "{err}");
    }
}
