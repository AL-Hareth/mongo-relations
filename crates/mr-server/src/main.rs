use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use mr_engine::Engine;
use mr_server::config::Config;
use mr_server::{AppState, Server};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "mongo-relations",
    version,
    about = "SQL-style relations on top of MongoDB"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "MR_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default).
    Serve,
    /// Print the SHA-256 hash of an API key for the config file.
    HashKey { key: String },
    /// Validate the configuration and exit.
    CheckConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::HashKey { key } => {
            println!("{}", mr_server::auth::hash_key(&key));
            Ok(())
        }
        Command::CheckConfig => {
            Config::load(cli.config.as_deref())?;
            println!("configuration OK");
            Ok(())
        }
        Command::Serve => serve(cli.config).await,
    }
}

async fn serve(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Config::load(config_path.as_deref())?;
    if config.auth.disabled {
        tracing::warn!("authentication is disabled; every request is treated as admin");
    }
    let engine = Engine::connect(&config.mongodb.uri, config.engine_config())
        .await
        .context("connecting to MongoDB")?;
    let topology = engine.topology();
    tracing::info!(
        mongodb = %topology.server_version,
        transactions = topology.supports_transactions,
        schema_version = engine.catalog().version(),
        "connected"
    );
    let background = engine.spawn_background();
    let server = Server::bind(
        AppState::new(engine, &config),
        config.server.grpc_addr,
        config.server.http_addr,
    )
    .await?;
    tracing::info!(grpc = %server.grpc_addr(), http = %server.http_addr(), "listening");
    server
        .serve(async {
            shutdown_signal().await;
            tracing::info!("shutting down");
        })
        .await?;
    background.shutdown().await;
    Ok(())
}

/// Resolve on Ctrl-C or SIGTERM (what container runtimes send).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
