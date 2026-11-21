//! `sourcecraft-mcp` — MCP-сервер и CLI для SourceCraft REST API.

mod base_url;
mod browser;
mod cli;
mod clients;
mod profiles;
mod ui;
mod verify;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "sourcecraft-mcp",
    version,
    about = "MCP server and CLI for the SourceCraft REST API"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the MCP server over stdio with the access of the profile (read-only by default),
    /// or over HTTP with --http.
    Serve(cli::serve::ServeArgs),
    /// Call a tool of the server from the console, with the access of the profile; without
    /// a tool, list the tools.
    Call(cli::call::CallCommand),
    /// Register the server in an MCP client with its own token profile.
    Connect(cli::connect::ConnectArgs),
    /// Manage stored tokens.
    #[command(subcommand)]
    Auth(cli::auth::AuthCommand),
    /// Use repository secrets in shell commands without exposing their values.
    #[command(subcommand)]
    Secret(cli::secret::SecretCommand),
    /// List toolsets that can be passed to `--toolsets`.
    Toolsets,
    /// Custom tools defined in tools.toml.
    #[command(subcommand)]
    Tools(cli::tools::ToolsCommand),
    /// Interactive view of the profiles: settings, access and its approval, registrations
    /// in MCP clients, what the token can see, and the tools.
    Ui(cli::tui::UiArgs),
    /// Internal: evaluate one `jq` filter for `serve` (JSON on stdin and stdout).
    #[command(name = "jq-worker", hide = true)]
    JqWorker,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let command = Cli::parse().command;
    // stdout занят протоколом MCP, поэтому все логи идут только в stderr. В `ui` логов нет:
    // терминал занят интерфейсом, и строка в stderr испортила бы экран.
    if !matches!(command, Command::Ui(_)) {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                EnvFilter::try_from_env("SOURCECRAFT_MCP_LOG")
                    .unwrap_or_else(|_| EnvFilter::new("warn")),
            )
            .init();
    }

    // `secret exec` возвращает код завершения дочерней команды, `call` — 1, если инструмент
    // вернул ошибку; остальные — успех или ошибку.
    match command {
        Command::Serve(args) => cli::serve::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::Call(args) => cli::call::run(args).await,
        Command::Connect(args) => cli::connect::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::Auth(command) => cli::auth::run(command).await.map(|()| ExitCode::SUCCESS),
        Command::Secret(command) => cli::secret::run(command).await,
        Command::Toolsets => cli::toolsets::run().map(|()| ExitCode::SUCCESS),
        Command::Tools(command) => cli::tools::run(command).map(|()| ExitCode::SUCCESS),
        Command::Ui(args) => cli::tui::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::JqWorker => cli::jq_worker::run().map(|()| ExitCode::SUCCESS),
    }
}
