use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use githearth_server::{
    AppState, MIGRATOR, bootstrap_admin, config::Config, connect, doctor, reconcile, router,
};
use std::{
    io::{self, Read},
    path::PathBuf,
};

#[derive(Parser)]
#[command(about = "Experimental GitHearth foundation server")]
struct Args {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve,
    Migrate,
    /// Run read-only preflight checks against configuration, storage, database, and Git.
    Doctor,
    /// Create the first admin; password is read from stdin (never command arguments).
    BootstrapAdmin {
        #[arg(long)]
        username: String,
    },
    /// Internal: dispatched by the Git hooks before any configuration is loaded.
    #[command(hide = true, name = "git-hook")]
    GitHook {
        name: String,
        args: Vec<String>,
    },
}
#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Command::GitHook { name, args } = &args.command {
        std::process::exit(githearth_server::git_hooks::run_process(name, args));
    }
    tracing_subscriber::fmt()
        .json()
        .with_env_filter("githearth_server=info")
        .init();
    if let Err(error) = run(args).await {
        // Only configuration and explicitly sanitized application errors are printed here.
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run(args: Args) -> Result<()> {
    let config = Config::load(args.config.as_deref())?;
    if let Command::Doctor = args.command {
        let exit_code = doctor::run(&config).await;
        if exit_code != 0 {
            anyhow::bail!("doctor reported failures; see output above");
        }
        return Ok(());
    }
    let pool = connect(&config).await?;
    match args.command {
        Command::Doctor => unreachable!("handled above"),
        Command::Migrate => {
            MIGRATOR.run(&pool).await.map_err(|_| {
                anyhow::anyhow!(
                    "migration failed; inspect database availability and migration history"
                )
            })?;
            tracing::info!("schema migrated");
        }
        Command::BootstrapAdmin { username } => {
            let mut password = String::new();
            io::stdin()
                .take(1026)
                .read_to_string(&mut password)
                .context("cannot read password from stdin")?;
            if password.len() > 1025 {
                bail!("password input exceeds limit");
            }
            let password = password.trim_end_matches(['\r', '\n']).to_owned();
            // Preserve actionable duplicate errors without displaying SQL driver data.
            match bootstrap_admin(&pool, &username, password).await {
                Ok(id) => tracing::info!(%id, "administrator created"),
                Err(error) => {
                    let message = error.to_string();
                    if message.starts_with("an administrator already exists")
                        || message.starts_with("username must")
                        || message.starts_with("password must")
                    {
                        bail!("{message}");
                    }
                    bail!("bootstrap failed; check schema, database, and username availability");
                }
            }
        }
        Command::Serve => {
            let state = AppState::new(pool, config.clone())
                .await
                .context("cannot initialize authentication")?;
            let _reconciler = reconcile::spawn(state.clone());
            let _merge_worker = githearth_server::merge_worker::spawn(state.clone());
            let listener = tokio::net::TcpListener::bind(config.bind)
                .await
                .context("cannot bind HTTP listener")?;
            tracing::info!(bind = %config.bind, "HTTP server listening; use loopback or a trusted TLS proxy");
            axum::serve(listener, router(state))
                .with_graceful_shutdown(shutdown())
                .await
                .context("HTTP server failed")?;
        }
        // Handled in `main` before any configuration is loaded.
        Command::GitHook { .. } => unreachable!("git-hook is dispatched in main"),
    }
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
