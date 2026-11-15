use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use githearth_server::audit_retention::{self, DEFAULT_BATCH_SIZE};
use githearth_server::{
    AppState, MIGRATOR, bootstrap_admin, config::Config, connect, doctor, jobs, reconcile,
    reset_admin, router, setup, ssh,
};
use std::{
    io::{self, BufRead, IsTerminal, Read},
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
    /// Break-glass: restore an existing account as an enabled site administrator.
    /// The password is read from stdin (never command arguments).
    ResetAdmin {
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
    /// Install from a clean host: configuration, directories, host key, schema,
    /// first administrator, then the doctor checks. Safe to run again.
    /// The database URL comes from GITHEARTH_DATABASE_URL or stdin; the
    /// administrator password from stdin (never from command arguments).
    Setup {
        /// Configuration file to write; when absent, the global --config is
        /// used, and otherwise /etc/githearth/config.toml. The two flags are
        /// mutually exclusive, so the tool never silently prefers one.
        #[arg(long, conflicts_with = "config")]
        config_path: Option<PathBuf>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        http_port: Option<u16>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        ssh_port: Option<u16>,
        #[arg(long)]
        no_ssh: bool,
        #[arg(long)]
        storage_root: Option<PathBuf>,
        #[arg(long)]
        ssh_host_key_path: Option<PathBuf>,
    },
    /// Repository lifecycle commands.
    Repository {
        #[command(subcommand)]
        action: RepositoryCommand,
    },
    /// Audit log IP retention management.
    Audit {
        #[command(subcommand)]
        action: AuditCommand,
    },
    /// Repack one repository and prune unreachable objects now (ADR 0013).
    Housekeeping {
        #[arg(long)]
        repository: uuid::Uuid,
    },
    /// Durable job operations (ADR 0006).
    Jobs {
        #[command(subcommand)]
        action: JobsCommand,
    },
}

#[derive(Subcommand)]
enum JobsCommand {
    /// Re-enqueue outbox deliveries that exhausted their budget (state
    /// `dead`) under a new job key, using the delivery row's own attempt
    /// budget. Without filters, every dead delivery is requeued.
    Requeue {
        /// Only this consumer (for example `notification.outbox`).
        #[arg(long)]
        consumer: Option<String>,
        /// Only this delivery's job key, as `doctor` or `jobs list` shows it.
        #[arg(long)]
        job_key: Option<String>,
        /// Report what would be requeued and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// List dead deliveries, which `jobs requeue` recovers.
    List,
}

#[derive(Subcommand)]
enum RepositoryCommand {
    /// Restore a deleted repository from trash as the named account. The
    /// account must be an organization owner or a site administrator: the
    /// command runs the same operation and authorization as the API and the
    /// browser, with no host-operator exception.
    Restore {
        #[arg(long)]
        organization: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        actor: String,
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
    if let Command::Setup {
        config_path,
        http_port,
        ssh_port,
        no_ssh,
        storage_root,
        ssh_host_key_path,
    } = args.command
    {
        return run_setup(
            args.config,
            config_path,
            http_port,
            ssh_port,
            no_ssh,
            storage_root,
            ssh_host_key_path,
        )
        .await;
    }
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
        Command::Setup { .. } => unreachable!("handled above"),
        Command::Migrate => {
            MIGRATOR.run(&pool).await.map_err(|_| {
                anyhow::anyhow!(
                    "migration failed; inspect database availability and migration history"
                )
            })?;
            tracing::info!("schema migrated");
        }
        Command::BootstrapAdmin { username } => {
            let password = read_password_from_stdin()?;
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
        Command::ResetAdmin { username } => {
            let password = read_password_from_stdin()?;
            match reset_admin(&pool, &username, password).await {
                Ok(id) => tracing::info!(%id, "administrator reset"),
                Err(error) => {
                    let message = error.to_string();
                    if message.starts_with("no account named")
                        || message.starts_with("username must")
                        || message.starts_with("password must")
                    {
                        bail!("{message}");
                    }
                    bail!("administrator reset failed; check schema and database");
                }
            }
        }
        Command::SshKeygen => unreachable!("handled above"),
        Command::Serve => {
            if let Err(message) = doctor::schema_check(&pool).await {
                bail!("database schema does not match this binary: {message}");
            }
            let mut state = AppState::new(pool, config.clone())
                .await
                .context("cannot initialize authentication")?;
            // The one pin 0046 cannot write (a base is not stored in
            // PostgreSQL) is backfilled before any housekeeping run can be
            // scheduled, so no prune ever races it.
            if let Err(error) = githearth_server::housekeeping::backfill_bases(&state).await {
                tracing::warn!(error = %error, "pull-request base pin backfill failed; will retry on the next start");
            }
            let _reconciler = reconcile::spawn(state.clone());
            let _merge_worker = githearth_server::merge_worker::spawn(state.clone());
            // The outbox consumers are registered before the relay starts, so
            // every event the relay sees has a registration position.
            if let Err(error) = githearth_server::outbox::register_builtin(&state).await {
                tracing::warn!(error = %error, "outbox consumer registration failed; will retry on the next start");
            }
            // Handlers are registered by their owners (ADR 0006).
            let jobs_shutdown = tokio_util::sync::CancellationToken::new();
            let mut registry = jobs::Registry::new();
            githearth_server::notifications::register(&mut registry);
            registry.mark_outbox_delivery(githearth_server::notifications::OUTBOX_JOB_KIND);
            githearth_server::codeowners::register(&mut registry);
            githearth_server::housekeeping::register(&mut registry, &config);
            // `/metrics` labels a job only by a kind this runner registered, so
            // the endpoint never grows a series for a row left by an older
            // binary.
            state.record_job_kinds(&registry.kinds());
            let _housekeeping = githearth_server::housekeeping::spawn(state.clone());
            let _outbox = githearth_server::outbox::spawn(state.clone(), jobs_shutdown.clone());
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
            if config.metrics_enabled {
                tracing::info!(
                    non_loopback = config.metrics_allow_non_loopback,
                    "metrics endpoint enabled"
                );
            }
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
        Command::Housekeeping { repository } => {
            // The operator command runs the same code the job runs. It takes
            // the same in-process lock, which a separate process does NOT
            // share with a running server: stop the server first (see
            // docs/operations/housekeeping.md). It also runs even when
            // `housekeeping_enabled = false`, because it is explicit.
            let state = AppState::new(pool, config.clone())
                .await
                .context("cannot initialize repository storage")?;
            match githearth_server::housekeeping::run_repository_now(&state, repository).await? {
                githearth_server::housekeeping::Outcome::Completed(report) => println!(
                    "housekeeping completed for {repository}: {} -> {} bytes, {} expired object(s), {}, {}",
                    report.bytes_before,
                    report.bytes_after,
                    report.expired,
                    if report.repacked {
                        "repacked"
                    } else {
                        "not repacked"
                    },
                    if report.truncated {
                        "scan/deletion stopped at its deadline; the next run continues"
                    } else {
                        "scan/deletion complete"
                    }
                ),
                githearth_server::housekeeping::Outcome::Skipped(reason) => {
                    println!("housekeeping skipped for {repository}: {reason}")
                }
                githearth_server::housekeeping::Outcome::Deferred(deferred) => {
                    println!("housekeeping deferred for {repository}: {deferred}")
                }
            }
        }
        Command::Repository { action } => match action {
            RepositoryCommand::Restore {
                organization,
                name,
                actor,
            } => {
                let state = AppState::new(pool, config.clone())
                    .await
                    .context("cannot initialize repository storage")?;
                match githearth_server::repository_restore::restore_by_username(
                    &state,
                    &organization,
                    &name,
                    &actor,
                )
                .await
                {
                    Ok(row) => println!(
                        "restored {}/{} ({}) as {}",
                        organization, row.name, row.id, actor
                    ),
                    Err(error) => anyhow::bail!(
                        "restore refused: {} ({})",
                        error.message(),
                        error.status_code()
                    ),
                }
            }
        },
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
        Command::Jobs { action } => match action {
            JobsCommand::Requeue {
                consumer,
                job_key,
                dry_run,
            } => {
                let results = githearth_server::outbox::requeue(
                    &pool,
                    consumer.as_deref(),
                    job_key.as_deref(),
                    dry_run,
                )
                .await?;
                let prefix = if dry_run {
                    "DRY RUN: would requeue"
                } else {
                    "requeued"
                };
                let mut done = 0;
                for (delivery_id, event_id, outcome) in &results {
                    match outcome {
                        githearth_server::outbox::RequeueOutcome::Requeued { job_key, .. } => {
                            done += 1;
                            println!(
                                "{prefix} delivery {delivery_id} (event {event_id}) as job key {job_key}"
                            );
                        }
                        githearth_server::outbox::RequeueOutcome::Skipped(reason) => {
                            println!("skipped delivery {delivery_id} (event {event_id}): {reason}");
                        }
                    }
                }
                if dry_run {
                    println!("would requeue {done} dead delivery(ies)");
                } else {
                    println!("requeued {done} dead delivery(ies)");
                }
            }
            JobsCommand::List => {
                let rows: Vec<(uuid::Uuid, String, String, Option<String>, i32, i32)> =
                    sqlx::query_as(
                        "SELECT d.id, c.name, d.job_key, d.last_error, d.attempts, d.max_attempts \
                         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
                         WHERE d.state = 'dead' ORDER BY d.chain, d.seq, c.name",
                    )
                    .fetch_all(&pool)
                    .await?;
                if rows.is_empty() {
                    println!("no dead deliveries");
                }
                for (id, consumer, key, error, attempts, max) in rows {
                    println!(
                        "{id}  {consumer}  key={key}  attempts={attempts}/{max}  error={}",
                        error.as_deref().unwrap_or("-")
                    );
                }
            }
        },
    }
    Ok(())
}

/// `githearth-server setup`: resolves the explicit settings, reads the
/// database URL from the environment or stdin, then installs. The URL is never
/// accepted as an argument, and it is never printed.
async fn run_setup(
    global_config: Option<PathBuf>,
    config: Option<PathBuf>,
    http_port: Option<u16>,
    ssh_port: Option<u16>,
    no_ssh: bool,
    storage_root: Option<PathBuf>,
    ssh_host_key_path: Option<PathBuf>,
) -> Result<()> {
    // clap only rejects the two flags when the global `--config` is parsed
    // inside the subcommand; written before `setup` it is a separate scope. Both
    // values are visible here, so the ambiguity is refused for either order.
    let path = match (global_config, config) {
        (Some(_), Some(_)) => {
            // The same usage error clap reports for the other ordering, so a
            // script sees exit 2 whichever flag is written first.
            eprintln!("--config and --config-path were both given; pass only one of them");
            std::process::exit(2);
        }
        (Some(path), None) | (None, Some(path)) => path,
        (None, None) => PathBuf::from("/etc/githearth/config.toml"),
    };
    // Resolved against the working directory at the moment setup runs, before
    // the file is inspected or its directory is created: `std::path::absolute`
    // does not follow symlinks, so the recorded settings are CWD-independent
    // without changing which file is used.
    let path = setup::absolute_path(&path)?;
    // The file is inspected and validated before anything is read from stdin:
    // a resumed run uses its recorded URL and never waits for input, and a
    // loose or symlinked file is refused before any prompt could appear.
    let recorded = setup::recorded_database_url(&path)?;
    let mut options = setup::Options::new(path, database_url_from_environment_or_stdin(recorded)?)?;
    // Named in the report when an existing configuration overrides them, so a
    // supplied value is never ignored without a word.
    let mut supplied_flags = Vec::new();
    if let Some(port) = http_port {
        supplied_flags.push("--http-port");
        options.bind = SocketAddr::from(([127, 0, 0, 1], port));
    }
    if let Some(port) = ssh_port {
        supplied_flags.push("--ssh-port");
        options.ssh_bind = SocketAddr::from(([127, 0, 0, 1], port));
    }
    options.ssh_enabled = !no_ssh;
    if no_ssh {
        supplied_flags.push("--no-ssh");
    }
    if let Some(root) = storage_root {
        supplied_flags.push("--storage-root");
        options.storage_root = setup::absolute_path(&root)?;
    }
    if let Some(key) = ssh_host_key_path {
        supplied_flags.push("--ssh-host-key-path");
        options.ssh_host_key_path = setup::absolute_path(&key)?;
    }
    options.supplied_flags = supplied_flags;
    setup::run(&options, &mut setup::StdinPassword).await?;
    println!(
        "setup complete: start the server with `githearth-server serve --config {}`",
        options.config.display()
    );
    Ok(())
}

/// GITHEARTH_DATABASE_URL wins, as it does for every other command. With no
/// environment URL, a URL recorded in an existing configuration is used, so a
/// resumed run neither re-reads nor prompts for one; stdin carries one line
/// only on a first install. Either way the URL is never an argument and never
/// printed. Stdin must be a pipe: a terminal would echo what is typed, so no
/// interactive prompt is offered.
fn database_url_from_environment_or_stdin(recorded: Option<String>) -> Result<String> {
    if let Ok(url) = std::env::var("GITHEARTH_DATABASE_URL")
        && !url.trim().is_empty()
    {
        return Ok(url.trim().to_owned());
    }
    if let Some(url) = recorded {
        return Ok(url);
    }
    if io::stdin().is_terminal() {
        bail!(
            "GITHEARTH_DATABASE_URL is not set and stdin is a terminal, \
             so the connection URL would be echoed; set the variable or pipe \
             the URL to setup on stdin"
        );
    }
    // One line only: the password reader takes the rest. `Stdin` buffers
    // internally and keeps it across reads, so nothing is lost here.
    let mut line = String::new();
    io::stdin()
        .lock()
        .take(1025)
        .read_line(&mut line)
        .context("cannot read the database URL from stdin")?;
    if !line.ends_with('\n') && line.len() >= 1024 {
        bail!("database URL input exceeds limit");
    }
    Ok(line.trim().to_owned())
}

/// Reads the operator-supplied password from stdin, never from argv, and
/// strips only the trailing newline.
fn read_password_from_stdin() -> Result<String> {
    let mut password = String::new();
    io::stdin()
        .take(1026)
        .read_to_string(&mut password)
        .context("cannot read password from stdin")?;
    if password.len() > 1025 {
        bail!("password input exceeds limit");
    }
    Ok(password.trim_end_matches(['\r', '\n']).to_owned())
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
