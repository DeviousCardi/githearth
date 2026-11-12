use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use githearth_server::audit_retention::{self, DEFAULT_BATCH_SIZE};
use githearth_server::{
    AppState, MIGRATOR, bootstrap_admin, config::Config, connect, doctor, jobs, reconcile, router,
    ssh,
};
use std::{
    io::{self, Read},
    net::SocketAddr,
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
    /// Generate an Ed25519 host key for the embedded SSH server.
    SshKeygen,
    /// Audit log IP retention management.
    Audit {
        #[command(subcommand)]
        action: AuditCommand,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// Set client_ip=NULL on old rows that carry a digest.
    PruneIps {
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        older_than: i64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Insert a new per-row HMAC key.
    RotateIpKey {
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete keys older than DAYS (skips newest and keys still in use).
    DropIpKeys {
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        older_than: i64,
        #[arg(long)]
        dry_run: bool,
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
    // Needs no database: an operator creates the key before the first serve.
    if let Command::SshKeygen = args.command {
        // Its errors already name the path and the reason.
        return ssh::generate_host_key(&config.ssh_host_key_path);
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
        Command::SshKeygen => unreachable!("handled above"),
        Command::Serve => {
            if let Err(message) = doctor::schema_check(&pool).await {
                bail!("database schema does not match this binary: {message}");
            }
            let state = AppState::new(pool, config.clone())
                .await
                .context("cannot initialize authentication")?;
            let _reconciler = reconcile::spawn(state.clone());
            let _merge_worker = githearth_server::merge_worker::spawn(state.clone());
            // Handlers are registered by their owners (ADR 0006).
            let jobs_shutdown = tokio_util::sync::CancellationToken::new();
            let mut registry = jobs::Registry::new();
            githearth_server::notifications::register(&mut registry);
            githearth_server::codeowners::register(&mut registry);
            let jobs = jobs::spawn(state.clone(), registry, jobs_shutdown.clone());
            let _ssh = if config.ssh_enabled {
                ssh::load_host_key(&config.ssh_host_key_path)?;
                let authenticate = githearth_server::ssh_keys::authenticator(state.pool.clone());
                let (handle, _port_rx) = ssh::spawn(state.clone(), authenticate);
                Some(handle)
            } else {
                None
            };
            let listener = tokio::net::TcpListener::bind(config.bind)
                .await
                .context("cannot bind HTTP listener")?;
            tracing::info!(bind = %config.bind, "HTTP server listening; use loopback or a trusted TLS proxy");
            axum::serve(
                listener,
                router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown())
            .await
            .context("HTTP server failed")?;
            // Let in-flight jobs finish and release their leases (ADR 0006).
            jobs_shutdown.cancel();
            let _ = jobs.await;
        }
        // Handled in `main` before any configuration is loaded.
        Command::GitHook { .. } => unreachable!("git-hook is dispatched in main"),
        Command::Audit { action } => match action {
            AuditCommand::PruneIps {
                older_than,
                dry_run,
            } => {
                let result =
                    audit_retention::prune_ips(&pool, older_than, dry_run, DEFAULT_BATCH_SIZE)
                        .await?;
                if dry_run {
                    println!(
                        "DRY RUN: would clear {} row(s), {} legacy row(s) still have raw IPs without digests",
                        result.cleared, result.legacy_remaining
                    );
                } else {
                    println!(
                        "cleared {} row(s), {} legacy row(s) still have raw IPs without digests",
                        result.cleared, result.legacy_remaining
                    );
                }
            }
            AuditCommand::RotateIpKey { dry_run } => {
                let id = audit_retention::rotate_ip_key(&pool, dry_run).await?;
                if dry_run {
                    println!("DRY RUN: would create new key (id: {id})");
                } else {
                    println!("created new key (id: {id})");
                }
            }
            AuditCommand::DropIpKeys {
                older_than,
                dry_run,
            } => {
                let result = audit_retention::drop_ip_keys(&pool, older_than, dry_run).await?;
                if dry_run {
                    println!("DRY RUN: would drop:");
                } else {
                    println!("dropped:");
                }
                for id in &result.dropped {
                    println!("  {id}");
                }
                for id in &result.skipped_in_use {
                    println!("skipped key still in use (id: {id})");
                }
            }
        },
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
