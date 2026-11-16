pub mod account_admin;
mod account_admin_ui;
pub mod audit;
pub mod audit_retention;
pub mod auto_merge;
pub mod backup;
pub mod branch_protection_ui;
pub mod browse;
mod browse_ui;
pub mod client_ip;
pub mod codeowners;
pub mod config;
pub mod data_keys;
pub mod dns;
pub mod doctor;
pub mod email_verification;
pub mod export;
pub mod facts;
pub mod fairness;
pub mod forks;
pub mod git_admission;
pub mod git_hooks;
pub mod git_http;
pub mod housekeeping;
pub mod idempotency;
pub mod identity;
pub mod invitations;
mod invitations_ui;
pub mod issues;
mod issues_ui;
pub mod jobs;
pub mod limits;
mod login_throttle;
pub mod mail;
pub mod maintenance;
pub mod membership;
mod membership_ui;
pub mod merge;
pub mod merge_intents;
pub mod merge_worker;
mod mergeability;
pub mod metrics;
pub mod notification_email;
pub mod notifications;
mod notifications_ui;
pub mod outbound;
pub mod outbox;
pub mod pins;
pub mod protection;
pub mod pulls;
mod pulls_ui;
pub mod pushes;
pub mod reconcile;
pub mod repositories;
pub mod repository_api;
pub mod repository_restore;
pub mod secrets;
mod settings_ui;
pub mod setup;
pub mod ssh;
pub mod ssh_keys;
pub mod statuses;
pub mod tag_protection;
mod tag_protection_ui;
pub mod teams;
mod teams_ui;
mod testseam;
mod tokens;
pub mod transfer;
mod transfer_ui;
pub mod ui;

use anyhow::Context;
use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use client_ip::{ClientIp, TrustedProxy, client_ip, parse_trusted_proxy};
use config::Config;
use githearth_authz::{Engine, Resource};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    ConnectOptions, PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::{limit::RequestBodyLimitLayer, timeout::TimeoutLayer};
use uuid::Uuid;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opens `<storage_root>/objects` with the configured deadline and quota.
async fn open_object_store(
    storage_root: &std::path::Path,
    config: &Config,
) -> anyhow::Result<Arc<dyn githearth_git::object_store::ObjectStore + Send + Sync>> {
    let root = storage_root.join("objects");
    let deadline = std::time::Duration::from_secs(config.object_upload_deadline_seconds);
    let quota = config.organization_quota_bytes;
    let store = tokio::task::spawn_blocking(move || {
        githearth_git::object_store::FsObjectStore::open_with(
            root,
            githearth_git::object_store::ObjectStoreOptions {
                upload_deadline: deadline,
                quota_bytes: quota,
                ..githearth_git::object_store::ObjectStoreOptions::default()
            },
        )
    })
    .await??;
    Ok(Arc::new(store))
}

tokio::task_local! {
    static ADMITTED_TOKEN: (Vec<u8>, Account, u64);
    // Work spawned onto other tasks (tokio::spawn, job runner, push replay)
    // runs outside this scope, so its audit rows record client_ip NULL.
    static CLIENT_IP: std::net::IpAddr;
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub(crate) config: Config,
    // pub so the password-reset saturation test can hold the only permit.
    pub auth_slots: Arc<Semaphore>,
    // pub so the concurrency-exhaustion integration test can hold a permit.
    // The share scheduler takes its permits from this same semaphore, so the
    // gauge and an externally held permit keep their meaning.
    pub git_slots: Arc<Semaphore>,
    /// The per-organization Git share scheduler (ADR 0014 §5). Every Git
    /// transport acquires through it; `git_slots` is its global capacity.
    pub git_scheduler: Arc<fairness::GitScheduler>,
    /// The live request, creation, email and sign-up budgets.
    pub limits: Arc<limits::Limits>,
    /// Bounds simultaneous housekeeping runs (`housekeeping_concurrency`),
    /// including direct enqueues that bypass the scheduler's allowance.
    pub(crate) housekeeping_slots: Arc<Semaphore>,
    dummy_hash: Arc<String>,
    pub authz: Arc<Engine>,
    pub storage: Arc<githearth_git::layout::Storage>,
    /// The S3-shaped object store every large-object feature uses. Its root is
    /// `<storage_root>/objects`, so the storage backup carries it.
    pub objects: Arc<dyn githearth_git::object_store::ObjectStore + Send + Sync>,
    // pub so the disconnect integration test can assert the token is released.
    pub hook_tokens: Arc<std::sync::Mutex<HashMap<String, git_admission::Admitted>>>,
    pub(crate) hook_socket_path: std::path::PathBuf,
    pub(crate) trusted_proxies: Vec<TrustedProxy>,
    /// Outbound mail. `DisabledTransport` unless SMTP is configured; tests
    /// replace it with an in-memory transport so no socket is opened.
    pub(crate) mailer: Arc<dyn mail::MailTransport>,
    /// The one client every server-initiated HTTP request uses (ADR 0014 §7,
    /// case U30c). Every later feature takes it from here; no feature opens its
    /// own outbound connection.
    pub outbound: Arc<outbound::OutboundClient>,
    /// In-memory counters and gauges the `/metrics` endpoint reads. `None` only
    /// for `AppState`s a unit test builds by hand; the middleware then counts
    /// nothing, and the endpoint still answers (with static gauges only).
    pub(crate) recorder: Option<Arc<metrics::Recorder>>,
    /// The job kinds `serve` registered, so `githearth_jobs_by_kind` publishes
    /// a series per registered kind instead of per kind in the database. `None`
    /// when no runner was started; the metric then publishes `other` only.
    pub(crate) job_kinds: Option<Arc<[&'static str]>>,
    /// The write fence `backup` proves before it snapshots (M4.1 U13).
    pub maintenance: maintenance::Maintenance,
    /// The on-host bearer token for the maintenance control endpoint. It is
    /// written to `<storage_root>/run/maintenance.token` with mode 0600.
    pub(crate) maintenance_token: Arc<str>,
    /// The operator's installation key set, when one is configured. It unwraps
    /// a secret at the point of use; no handler returns it.
    pub installation_keys: Option<Arc<secrets::InstallationKeySet>>,
}
impl AppState {
    /// Records that `added` bytes of a fork head were copied into `target`,
    /// charged to the target repository's organization: the objects live in
    /// the target's `objects/`, which is what the push quota measures, and the
    /// ledger in `repository_fork_fetches` is the audit trail for the cap.
    /// Best effort: the read that triggered the copy does not depend on it.
    pub(crate) async fn metrics_fork_fetched(&self, target: Uuid, source: Uuid, added: u64) {
        let result: Result<(), sqlx::Error> = async {
            let mut conn = self.pool.acquire().await?;
            sqlx::query(
                "INSERT INTO repository_fork_fetches \
                 (repository_id, source_repository_id, bytes, fetches) \
                 VALUES ($1, $2, $3, 1) \
                 ON CONFLICT (repository_id, source_repository_id) DO UPDATE SET \
                   bytes = repository_fork_fetches.bytes + EXCLUDED.bytes, \
                   fetches = repository_fork_fetches.fetches + 1, updated_at = now()",
            )
            .bind(target)
            .bind(source)
            .bind(i64::try_from(added).unwrap_or(i64::MAX))
            .execute(&mut *conn)
            .await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%target, %source, error = %error, "fork fetch charge not recorded");
        }
    }

    pub async fn new(pool: PgPool, config: Config) -> anyhow::Result<Self> {
        Self::build(pool, config, true).await
    }

    /// Builds the state for a one-off command (`housekeeping`) that must not
    /// touch the running server's control token. The token is only meaningful
    /// to the process that answers the control endpoint, and rewriting it here
    /// used to make `maintenance start|status|stop` and `backup` fail with
    /// `answered 401` until the server restarted.
    pub async fn for_command(pool: PgPool, config: Config) -> anyhow::Result<Self> {
        Self::build(pool, config, false).await
    }

    async fn build(pool: PgPool, config: Config, write_token: bool) -> anyhow::Result<Self> {
        config.validate()?;
        // Pin the operator's legacy deprecation date onto every legacy token.
        // The stored instant is the first instant of the UTC day after that
        // date, so the date the operator reads (config, `doctor`, the docs) is
        // the date enforced and both lookups refuse at exactly the next UTC
        // midnight. `LEAST` makes the pass monotone: shortening the configured
        // date shortens existing rows, while raising it back can never
        // re-extend a token that was already narrowed. A database that is not
        // migrated yet (a one-off unit test with a lazy pool) simply skips
        // this; `ready` reports the real schema state.
        let _ = sqlx::query(
            "UPDATE access_tokens SET legacy_deadline = LEAST(legacy_deadline, \
             ($1::date + 1)::timestamp AT TIME ZONE 'UTC') \
             WHERE legacy AND subject_kind = 'account'",
        )
        .bind(&config.legacy_token_deprecation_date)
        .execute(&pool)
        .await;
        let dummy_hash =
            tokio::task::spawn_blocking(|| hash_password("dummy-password-not-an-account"))
                .await??;
        let storage = tokio::task::spawn_blocking({
            let root = config.storage_root.clone();
            let hooks_path = root.join("hooks");
            let max_input_size = config.git_push_limit_bytes;
            move || {
                githearth_git::layout::Storage::open_with(
                    &root,
                    githearth_git::layout::StorageOptions {
                        hooks_path,
                        max_input_size,
                    },
                )
            }
        })
        .await??;

        let objects = open_object_store(&config.storage_root, &config).await?;

        // The hook scripts exec the server binary so the hooks talk to this
        // process's admission socket; tests point this at the real binary.
        let hook_program = if config.hook_program.as_os_str().is_empty() {
            std::env::current_exe().context("cannot resolve the server executable for hooks")?
        } else {
            config.hook_program.clone()
        };
        let hooks_path = config.storage_root.join("hooks");
        tokio::task::spawn_blocking(move || write_hooks(&hooks_path, &hook_program)).await??;

        // One admission socket per AppState.  A unique, short name lets tests
        // create several states on one storage root without bind conflicts or
        // exceeding the Unix socket path limit.
        let run_dir = config.storage_root.join("run");
        tokio::task::spawn_blocking(move || {
            create_run_dir(&run_dir)?;
            clean_stale_sockets(&run_dir)
        })
        .await??;
        let mut bytes = [0u8; 4];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
        let socket_path = config
            .storage_root
            .join("run")
            .join(format!("hook-{}.sock", hex::encode(bytes)));
        let listener = tokio::net::UnixListener::bind(&socket_path)
            .with_context(|| format!("cannot bind hook socket {}", socket_path.display()))?;

        let trusted_proxies = config
            .trusted_proxies
            .iter()
            .map(|e| parse_trusted_proxy(e))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow::anyhow!("trusted_proxies: invalid entry"))?;
        let mailer = mail::transport_from_config(&config)?;
        let outbound = Arc::new(outbound::OutboundClient::from_config(&config)?);
        // The control token is regenerated on every start; the state file is
        // only an observation, never the fence `backup` trusts. A command that
        // is not `serve` must not rewrite the token of a running server.
        let maintenance_token = if write_token {
            maintenance::write_control_token(
                &config
                    .storage_root
                    .join("run")
                    .join(maintenance::CONTROL_TOKEN_FILE),
            )
            .context("cannot write the maintenance control token")?
        } else {
            String::new()
        };
        let maintenance = maintenance::Maintenance::new(
            config
                .storage_root
                .join("run")
                .join(maintenance::STATE_FILE),
        );
        let installation_keys = secrets::InstallationKeySet::from_config(&config)?.map(Arc::new);
        let git_slots = Arc::new(Semaphore::new(config.git_concurrency));
        let git_scheduler = fairness::GitScheduler::new(
            git_slots.clone(),
            config.git_concurrency,
            config.git_organization_floor,
            config.git_organization_ceiling,
        );
        let request_limits = Arc::new(limits::Limits::from_config(&config));
        let state = Self {
            pool,
            auth_slots: Arc::new(Semaphore::new(config.auth_concurrency)),
            git_slots,
            git_scheduler,
            limits: request_limits,
            housekeeping_slots: Arc::new(Semaphore::new(config.housekeeping_concurrency)),
            config,
            dummy_hash: Arc::new(dummy_hash),
            authz: Arc::new(Engine::new()?),
            storage: Arc::new(storage),
            objects,
            hook_tokens: Arc::new(std::sync::Mutex::new(HashMap::new())),
            hook_socket_path: socket_path,
            trusted_proxies,
            mailer,
            outbound,
            recorder: Some(Arc::new(metrics::Recorder::new())),
            job_kinds: None,
            maintenance,
            maintenance_token: Arc::from(maintenance_token),
            installation_keys,
        };
        tokio::spawn(crate::git_http::hook_accept_loop(listener, state.clone()));
        Ok(state)
    }

    /// Records the job kinds the runner registered, for
    /// `githearth_jobs_by_kind`. `serve` calls this after building the registry.
    pub fn record_job_kinds(&mut self, kinds: &[&'static str]) {
        self.job_kinds = Some(Arc::from(kinds));
    }

    /// Replaces the outbound transport. Tests use this with an in-memory
    /// implementation so no SMTP socket is ever opened.
    pub fn with_mailer(mut self, mailer: Arc<dyn mail::MailTransport>) -> Self {
        self.mailer = mailer;
        self
    }

    /// Replaces the one outbound client. Used by a test that builds one from a
    /// configuration, and by a future feature that needs a different policy; the
    /// transport and resolver seam itself stays crate-private.
    pub fn with_outbound(mut self, outbound: Arc<outbound::OutboundClient>) -> Self {
        self.outbound = outbound;
        self
    }

    /// The in-memory counters behind `/metrics`.
    pub fn recorder(&self) -> Option<&Arc<metrics::Recorder>> {
        self.recorder.as_ref()
    }

    /// The sudo-mode window the authorization engine applies to sensitive
    /// actions. Callers compare against this rather than a copied constant, so
    /// a configured engine and the account API/page cannot disagree.
    pub(crate) fn fresh_auth(&self) -> std::time::Duration {
        self.authz.fresh_auth()
    }
}

fn write_hooks(hooks_dir: &std::path::Path, program: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(hooks_dir)?;
    let program = program.display().to_string().replace('\'', "'\\''");
    for name in ["pre-receive", "reference-transaction"] {
        let path = hooks_dir.join(name);
        let script = format!("#!/bin/sh\nexec '{program}' git-hook {name} \"$@\"\n");
        std::fs::write(&path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

fn create_run_dir(run_dir: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::create_dir(run_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Remove hook sockets left by a previous process.  A socket that still
/// accepts a connection belongs to a live AppState and is kept.
fn clean_stale_sockets(run_dir: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        for entry in std::fs::read_dir(run_dir)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("hook-")
                && name.ends_with(".sock")
                && UnixStream::connect(entry.path()).is_err()
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

pub async fn connect(config: &Config) -> anyhow::Result<PgPool> {
    config.validate()?;
    let options = PgConnectOptions::from_str(&config.database_url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection URL"))?
        .disable_statement_logging();
    PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .acquire_timeout(Duration::from_secs(config.database_timeout_seconds))
        .after_connect(|conn, _| {
            Box::pin(async move {
                // Bound SQL work, including locks, independently of HTTP cancellation.
                sqlx::query("SET statement_timeout = '5s'")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET lock_timeout = '3s'")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET idle_in_transaction_session_timeout = '10s'")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "cannot connect to PostgreSQL; check database availability and credentials"
            )
        })
}

pub fn router(state: AppState) -> Router {
    let json = Router::new()
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(ready))
        // The team and invitation API and pages. They are merged deliberately,
        // not appended as empty nested routers: axum 0.8 routes by path shape
        // and method, so each route below is registered like every other one.
        .merge(teams::router())
        .merge(transfer::router())
        .merge(transfer_ui::router())
        .merge(invitations::router())
        .merge(teams_ui::router())
        .merge(invitations_ui::router())
        .merge(invitations_ui::accept_router())
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/me", get(me))
        .route(
            "/api/v1/organizations",
            post(create_organization).get(list_organizations),
        )
        .route("/api/v1/organizations/{id}", get(read_organization))
        .merge(issues::router())
        .merge(forks::router())
        .merge(pulls::router())
        .merge(merge_intents::router())
        .merge(auto_merge::router())
        .merge(membership::router())
        .merge(protection::router())
        .merge(tag_protection::router())
        .merge(statuses::router())
        .merge(audit::router())
        .merge(ui::router())
        .merge(browse::router())
        .merge(tokens::router())
        .merge(account_admin::router())
        .merge(email_verification::router())
        .merge(identity::router())
        .merge(notification_email::router())
        .merge(ssh_keys::router())
        .merge(notifications::router())
        .route(
            "/api/v1/organizations/{org}/repositories",
            post(repository_api::create).get(repository_api::list),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{name}",
            get(repository_api::read)
                .patch(repository_api::update)
                .delete(repository_api::delete),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{name}/archive",
            post(repository_api::archive),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{name}/unarchive",
            post(repository_api::unarchive),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{name}/restore",
            post(repository_api::restore),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{name}/access",
            get(repository_api::list_access).post(repository_api::grant_access),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{name}/access/{grant_id}",
            delete(repository_api::revoke_access),
        )
        .layer(RequestBodyLimitLayer::new(state.config.body_limit_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(state.config.request_timeout_seconds),
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            enforce_read_only_boundary,
        ));

    let git = git_http::router(&state.config);
    // Its own router, not part of `json`: a scrape is a plain `GET` with no
    // body, and the JSON API's read-only boundary, body limit and JSON
    // rejection rewriting have nothing to say about it. The handler refuses a
    // non-loopback client itself, per request, using the resolved address.
    let operator = Router::new()
        .route(metrics::PATH, get(metrics::metrics))
        // The fence routes are mounted here, outside the JSON API's read-only
        // layer and outside the fence itself: `stop` must remain reachable even
        // while writes are refused. They authenticate with the on-host token.
        .merge(maintenance::router());

    // The last layer runs first: resolve the client IP before observe logs it.
    // `operator` is merged last because `ui::router()` installs a `fallback`,
    // and a fallback answers only a path no route claims — merging order cannot
    // shadow `/metrics`. An explicit route added by any merged router would,
    // so `/metrics` must stay claimed only here.
    json.merge(git)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            maintenance::fence,
        ))
        .merge(operator)
        .layer(axum::middleware::from_fn(observe))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            client_ip_middleware,
        ))
        .layer(axum::middleware::from_fn(json_rejections))
        .with_state(state)
}

/// Fail-closed read-only boundary for the JSON API. Read-only enforcement must
/// not depend on every handler remembering to call `is_read_only()`; this layer
/// refuses any non-GET/HEAD request whose Bearer credential is a personal
/// access token without `repo:write`, so new handlers are safe by default.
/// Session credentials and anonymous requests pass through untouched. The Git
/// routes are not under `/api/v1` and keep their own transport check.
async fn enforce_read_only_boundary(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method().clone();
    let is_api = request.uri().path().starts_with("/api/v1");

    // Resolve the credential on the API (for the published org/account budget)
    // and on every non-GET/HEAD request (for the read-only boundary). An
    // access token is a `gth_` bearer; everything else either authenticates
    // or falls through to the handler as before.
    let mut admitted = None;
    if is_api || (method != Method::GET && method != Method::HEAD) {
        match bearer_credential(request.headers()) {
            Ok(Credential::AccessToken { token_hash }) => {
                match authenticate_credential(
                    &state.pool,
                    Credential::AccessToken {
                        token_hash: token_hash.clone(),
                    },
                )
                .await
                {
                    Ok((account, age)) => admitted = Some((token_hash, account, age)),
                    // An invalid or expired token: the handler refuses it the
                    // same way.
                    Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {}
                    // Any other lookup failure (pool timeout, dropped
                    // database) must fail closed: never run the handler, or a
                    // transient error would let a read-only token's write
                    // through.
                    Err(e) => return e.into_response(),
                }
            }
            Ok(credential @ Credential::Session { .. }) => {
                if let Ok((account, age)) = authenticate_credential(&state.pool, credential).await {
                    admitted = Some((Vec::new(), account, age));
                }
            }
            Err(_) => {}
        }
    }

    if is_api {
        // The middleware is the single place that charges the published
        // organization and account budgets, so a new route cannot forget them.
        let account = admitted.as_ref().map(|(_, account, _)| account.id);
        let organization = organization_in_path(&state, request.uri().path(), account).await;
        if !state.limits.allow_api(organization, account) {
            return rate_limited().into_response();
        }
    }

    if method == Method::GET || method == Method::HEAD {
        return next.run(request).await;
    }
    match admitted {
        Some((_, account, _)) if account.is_read_only() => read_only_denied().into_response(),
        Some((token_hash, account, age)) => {
            ADMITTED_TOKEN
                .scope((token_hash, account, age), next.run(request))
                .await
        }
        None => next.run(request).await,
    }
}

/// The organization named by `/api/v1/organizations/{slug}/...`, when the
/// path has one, it exists, and the caller is a member. The membership join
/// matters: a stranger must not be able to spend a victim organization's
/// budget by spraying requests at its paths. A missing organization or a
/// non-member is not charged to that organization — the handler answers
/// `403`/`404`, and the caller still spends its own account budget.
async fn organization_in_path(
    state: &AppState,
    path: &str,
    account: Option<uuid::Uuid>,
) -> Option<uuid::Uuid> {
    let account = account?;
    let rest = path.strip_prefix("/api/v1/organizations/")?;
    let slug = rest.split('/').next()?;
    if slug.is_empty() {
        return None;
    }
    let by_id = slug.parse::<uuid::Uuid>().ok();
    let found = match by_id {
        Some(id) => {
            sqlx::query_scalar(
                "SELECT o.id FROM organizations o \
                 JOIN memberships m ON m.organization_id = o.id AND m.account_id = $2 \
                 WHERE o.id = $1",
            )
            .bind(id)
            .bind(account)
            .fetch_optional(&state.pool)
            .await
        }
        None => {
            sqlx::query_scalar(
                "SELECT o.id FROM organizations o \
                 JOIN memberships m ON m.organization_id = o.id AND m.account_id = $2 \
                 WHERE o.slug = $1",
            )
            .bind(slug)
            .bind(account)
            .fetch_optional(&state.pool)
            .await
        }
    };
    found.ok().flatten()
}

async fn client_ip_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let keys = state.installation_keys.clone();
    // `observe` runs outside this layer and cannot extract `State`; hand it the
    // recorder through the request.
    request
        .extensions_mut()
        .insert(metrics::ObservedRecorder(state.recorder.clone()));
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied()
        .map(|c| c.0);
    if let Some(addr) = peer {
        let fwd: Vec<String> = request
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_owned())
            .collect();
        let ip = client_ip(addr, &fwd, &state.trusted_proxies);
        request.extensions_mut().insert(ClientIp(ip));
        return secrets::scope(keys, CLIENT_IP.scope(ip, next.run(request))).await;
    }
    secrets::scope(keys, next.run(request)).await
}

/// Extractor rejections (a malformed query string or path id) answer
/// `text/plain`; the JSON API must keep its envelope, so rewrite those 400s.
async fn json_rejections(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::body::Body;
    use axum::http::{HeaderValue, header};
    let path = request.uri().path();
    let is_api = path == "/api" || path.starts_with("/api/");
    let response = next.run(request).await;
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/plain"));
    let status = response.status();
    if is_api && (status == StatusCode::METHOD_NOT_ALLOWED || (status.is_client_error() && plain)) {
        let (mut parts, _body) = response.into_parts();
        let body = if status == StatusCode::METHOD_NOT_ALLOWED {
            r#"{"error":"method not allowed"}"#
        } else {
            r#"{"error":"invalid input"}"#
        };
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        parts
            .headers
            .insert("x-frame-options", HeaderValue::from_static("DENY"));
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        return Response::from_parts(parts, Body::from(body));
    }
    response
}

async fn observe(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let start = std::time::Instant::now();
    let method = request.method().clone();
    // The counters are in memory: one atomic increment and one duration sum per
    // request, never a database write. The recorder lives on the request's
    // state extensions so the middleware needs no extractor of its own.
    let recorder = request
        .extensions()
        .get::<metrics::ObservedRecorder>()
        .and_then(|r| r.0.clone());
    let service = metrics::git_service(request.uri().path());
    // Counted from the start, not the finish: the scrape reads the gauge while
    // its own request is running, so a request must be in flight while it is
    // answered, or `in_flight` would always be one short and a lone `curl`
    // scrape would always read 0.
    if let Some(recorder) = &recorder {
        recorder.request_started();
    }
    // Never log raw URIs, query strings, headers, bodies, database errors or configuration.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".into());
    let client_ip = request
        .extensions()
        .get::<ClientIp>()
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "unknown".into());
    let mut response = next.run(request).await;
    if let Some(recorder) = &recorder {
        let status = response.status().as_u16();
        recorder.request(method.as_str(), status, start.elapsed());
        if let Some(service) = service {
            recorder.git_request(service, status);
        }
        recorder.request_finished();
    }
    let headers = response.headers_mut();
    headers.insert("cache-control", "no-store".parse().unwrap());
    // Every response: no MIME sniffing (raw file views serve user content), and
    // no URL paths leaked to other sites in Referer (#87).
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "same-origin".parse().unwrap());
    tracing::info!(%method, route, status = response.status().as_u16(), elapsed_ms = start.elapsed().as_millis() as u64, client_ip, "http request");
    response
}

#[derive(Debug, Clone)]
pub struct ApiError(StatusCode, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Sensitive actions carry a machine-readable reason so a client can
        // prompt for re-authentication instead of parsing the sentence.
        let body = if self.1 == "reauthentication required" {
            serde_json::json!({"error": self.1, "reason": "sensitive"})
        } else {
            serde_json::json!({"error": self.1})
        };
        (self.0, Json(body)).into_response()
    }
}
impl ApiError {
    pub fn status_code(&self) -> StatusCode {
        self.0
    }
    pub fn message(&self) -> &'static str {
        self.1
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Protocol(message) = &error
            && let Some(cause) = message.strip_prefix(INSTALLATION_KEY_ERROR)
        {
            // A configuration/key problem, not a database outage. The cause
            // names key ids and paths only; never key material.
            tracing::error!(
                category = "installation key",
                cause = %cause,
                "installation key unavailable"
            );
            return Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "installation key unavailable",
            );
        }
        if error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation())
        {
            Self(StatusCode::CONFLICT, "already exists")
        } else {
            // The driver can include SQL values; log only a fixed category.
            tracing::error!(category = "database", "database operation failed");
            Self(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}
type ApiResult<T> = Result<T, ApiError>;
fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "invalid or expired credentials")
}
/// Too many failed sign-ins from this username or client address. The message
/// is the same for a known and an unknown username, so throttling reveals
/// nothing about which accounts exist.
pub(crate) fn too_many_attempts() -> ApiError {
    ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "too many sign-in attempts; try again later",
    )
}
/// A sensitive action needs a fresh password entry (`POST
/// /api/v1/me/reauthenticate`). The response body adds `"reason":"sensitive"`.
pub(crate) fn reauthentication_required() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "reauthentication required")
}
pub(crate) fn invalid() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "invalid input")
}
/// A request over its published per-organization or per-account share. The
/// message is fixed; callers do not reveal which budget was exhausted.
pub(crate) fn rate_limited() -> ApiError {
    ApiError(StatusCode::TOO_MANY_REQUESTS, "request rate limit exceeded")
}

/// PostgreSQL text columns cannot store U+0000. Reject it as invalid input
/// before a value reaches a query so it never surfaces as a 503 database
/// error (issue #152).
pub(crate) fn has_nul(value: &str) -> bool {
    value.contains('\0')
}

/// Validate a raw string path parameter before it is compared against a text
/// column. Path extractors decode percent escapes, so `%00` arrives as NUL.
pub(crate) fn valid_path_param(value: &str) -> bool {
    !has_nul(value)
}

/// A read-only scoped access token is refused any write with 403.
pub(crate) fn read_only_denied() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "read-only token")
}

async fn ready(State(state): State<AppState>) -> StatusCode {
    let check = async {
        // Validate the exact embedded history, including checksum and absence of unknown migrations.
        let rows =
            sqlx::query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&state.pool)
                .await?;
        let migrations: Vec<_> = MIGRATOR.iter().collect();
        let valid = rows.len() == migrations.len()
            && rows.iter().zip(migrations).all(|(row, migration)| {
                row.get::<i64, _>("version") == migration.version
                    && row.get::<bool, _>("success")
                    && row.get::<Vec<u8>, _>("checksum") == migration.checksum.as_ref()
            });
        // Detect missing required relations/columns even if history was left untouched.
        sqlx::query("SELECT a.id,a.username,a.password_hash,a.is_admin,s.token_hash,s.expires_at,o.id,o.slug,o.name,m.role,e.kind,x.payload FROM accounts a, sessions s, organizations o, memberships m, audit_events e, outbox_events x LIMIT 0")
            .execute(&state.pool).await?;
        Ok::<_, sqlx::Error>(valid)
    };
    match tokio::time::timeout(
        Duration::from_secs(state.config.database_timeout_seconds),
        check,
    )
    .await
    {
        Ok(Ok(true)) => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub fn valid_username(value: &str) -> bool {
    (3..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
fn password_hasher() -> Argon2<'static> {
    // Explicit resource contract: Argon2id v19, 19 MiB, 2 iterations, 1 lane, 32-byte output.
    let params = Params::new(19_456, 2, 1, Some(32)).expect("fixed Argon2 parameters are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

async fn password_work<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Ok(password_hasher()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))?
        .to_string())
}

/// One-time operator command. The transaction lock serializes concurrent invocations.
/// A repeated bootstrap never changes an existing account or password.
pub async fn bootstrap_admin(
    pool: &PgPool,
    username: &str,
    password: String,
) -> anyhow::Result<Uuid> {
    anyhow::ensure!(
        valid_username(username),
        "username must contain 3..64 lowercase ASCII letters, digits, underscores or hyphens"
    );
    anyhow::ensure!(
        (12..=1024).contains(&password.len()),
        "password must contain 12..1024 bytes"
    );
    let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await??;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(706918231)")
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE is_admin)")
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        !exists,
        "an administrator already exists; bootstrap refused without changing credentials"
    );
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin) VALUES ($1,$2,$3,true)",
    )
    .bind(id)
    .bind(username)
    .bind(hash)
    .execute(&mut *tx)
    .await?;
    event(&mut tx, None, id, "administrator.bootstrapped").await?;
    tx.commit().await?;
    Ok(id)
}

/// The single-use credential the host `reset-admin` command prints once.
/// The plaintext code is never stored, and redemption uses the same table and
/// endpoint as the administrator-reset API.
pub struct ResetCredential {
    pub id: Uuid,
    pub code: String,
    pub expires_at: String,
}

/// Break-glass recovery from the host: restores an existing account as an
/// enabled site administrator by issuing the same single-use, expiring reset
/// code the administrator-reset API issues. No password is read, chosen or
/// stored: the account is in `must_change_password` until it redeems the code
/// and sets its own password, so nothing (API, browser, Git HTTP, Git SSH)
/// works before that. Every existing credential (sessions, access tokens, SSH
/// keys, reset codes) is revoked in the same transaction, and the action is
/// audited without the code. It never creates an account: the operator
/// bootstrap does that.
pub async fn reset_admin(pool: &PgPool, username: &str) -> anyhow::Result<ResetCredential> {
    anyhow::ensure!(
        valid_username(username),
        "username must contain 3..64 lowercase ASCII letters, digits, underscores or hyphens"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(706918231)")
        .execute(&mut *tx)
        .await?;
    let id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 FOR UPDATE")
            .bind(username)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(id) = id else {
        anyhow::bail!("no account named `{username}`; create one with bootstrap-admin first");
    };
    // No password hash is written: the account cannot sign in with any
    // password until it redeems the code below and stores its own.
    sqlx::query(
        "UPDATE accounts SET is_admin = true, suspended = false, \
         must_change_password = true WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE account_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE access_tokens SET revoked_at = clock_timestamp() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE ssh_keys SET revoked_at = clock_timestamp() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    // A forgotten password is often accompanied by failed guesses; clear the
    // counter so the account can redeem its credential without waiting out the
    // window.
    sqlx::query("DELETE FROM login_attempts WHERE scope = 'username' AND subject = $1")
        .bind(login_throttle::subject_username(username))
        .execute(&mut *tx)
        .await?;
    // The same issuance the administrator-reset API uses; it also retires any
    // earlier unredeemed code, so a previously printed credential is dead.
    let (code, expires_at) = account_admin::issue_password_reset_code(&mut tx, id).await?;
    event_with(
        &mut tx,
        None,
        id,
        "administrator.reset",
        serde_json::json!({"account_id": id, "reset_expires_at": expires_at}),
    )
    .await?;
    tx.commit().await?;
    Ok(ResetCredential {
        id,
        code,
        expires_at,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    username: String,
    password: String,
}
#[derive(Serialize)]
struct Token {
    token: String,
    token_type: &'static str,
    expires_in: u64,
}
const AUTH_QUEUE_WAIT: Duration = Duration::from_secs(2);

/// Waits briefly for an authentication slot. A short queue absorbs bursts such
/// as a clone's back-to-back Git requests; only a sustained overload is
/// refused. Callers hold the permit across the database lookup and the hash so
/// the concurrency bound covers both, and waiting itself holds no connection.
pub(crate) async fn acquire_auth_slot(state: &AppState) -> ApiResult<OwnedSemaphorePermit> {
    tokio::time::timeout(AUTH_QUEUE_WAIT, state.auth_slots.clone().acquire_owned())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "authentication busy",
        ))
}

pub(crate) async fn verify_password(
    state: &AppState,
    username: &str,
    password: &str,
) -> ApiResult<Option<Uuid>> {
    let permit = acquire_auth_slot(state).await?;
    verify_password_with(state, username, password, permit).await
}

/// Verifies a password while holding a caller-supplied authentication slot.
/// `sign_in` acquires the permit before its throttle query so a saturated queue
/// answers `429` without touching PostgreSQL.
async fn verify_password_with(
    state: &AppState,
    username: &str,
    password: &str,
    permit: OwnedSemaphorePermit,
) -> ApiResult<Option<Uuid>> {
    if !valid_username(username) || password.len() > 1024 {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT id, password_hash, suspended, must_change_password FROM accounts WHERE username = $1",
    )
    .bind(username)
    .fetch_optional(&state.pool)
    .await?;
    // A suspended account, or one that must change its password after an
    // administrator reset, still pays for the hash so timing does not reveal
    // the account state.
    let id = row
        .as_ref()
        .filter(|r| !r.get::<bool, _>("suspended") && !r.get::<bool, _>("must_change_password"))
        .map(|r| r.get::<Uuid, _>("id"));
    let hash = row
        .as_ref()
        .map(|r| r.get::<String, _>("password_hash"))
        .unwrap_or_else(|| state.dummy_hash.to_string());
    let password_owned = password.to_owned();
    let valid = password_work(permit, move || {
        PasswordHash::new(&hash).is_ok_and(|parsed| {
            password_hasher()
                .verify_password(password_owned.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "authentication failed"))?;
    Ok(id.filter(|_| valid))
}

/// The throttled sign-in decision (M4.1 U03): shared by the JSON login, the
/// browser sign-in form and Git password authentication so all three count
/// towards the same bounds. `Ok(None)` is a wrong password or a disabled
/// account, which are deliberately indistinguishable to the caller.
pub(crate) async fn sign_in(
    state: &AppState,
    username: &str,
    password: &str,
    request: login_throttle::Request,
) -> ApiResult<Option<Uuid>> {
    // The anonymous login/search ceiling (ADR 0014 §5): before the failure
    // throttle and before Argon2. It is per client address, so a public login
    // form cannot be used to saturate the hashing pool.
    if !state.limits.allow_anonymous_login(request.client_ip()) {
        return Err(too_many_attempts());
    }
    // A saturated authentication queue is refused without reading PostgreSQL.
    let mut permit = Some(acquire_auth_slot(state).await?);
    // The same throttle decision as the browser form and Git HTTP basic. The
    // exponential delay (U18) is charged before any Argon2 work, but not while
    // holding a hashing slot: a client that keeps knocking must not make the
    // authentication queue unavailable to everyone else.
    let attempt = login_throttle::examine(&state.pool, &state.config, request, username).await?;
    if !attempt.delay.is_zero() {
        permit = None;
        tokio::time::sleep(attempt.delay).await;
    }
    if attempt.blocked {
        return Err(too_many_attempts());
    }
    let permit = match permit {
        Some(permit) => permit,
        None => acquire_auth_slot(state).await?,
    };
    match verify_password_with(state, username, password, permit).await {
        Ok(Some(id)) => {
            login_throttle::succeeded(&state.pool, username).await?;
            Ok(Some(id))
        }
        Ok(None) => {
            login_throttle::failed(&state.pool, request, username).await?;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

async fn login(State(state): State<AppState>, Json(input): Json<Login>) -> ApiResult<Json<Token>> {
    let request = login_throttle::Request::from_current();
    let id = sign_in(&state, &input.username, &input.password, request)
        .await?
        .ok_or_else(unauthorized)?;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let token_hash = Sha256::digest(token.as_bytes()).to_vec();
    let mut tx = state.pool.begin().await?;
    sqlx::query("INSERT INTO sessions (token_hash, account_id, expires_at) VALUES ($1,$2,clock_timestamp() + $3 * interval '1 second')")
        .bind(token_hash).bind(id).bind(state.config.token_ttl_seconds as f64).execute(&mut *tx).await?;
    event(&mut tx, None, id, "session.created").await?;
    tx.commit().await?;
    Ok(Json(Token {
        token,
        token_type: "Bearer",
        expires_in: state.config.token_ttl_seconds,
    }))
}
/// A Bearer credential is either a 64-hex session token or a personal access
/// token (`gth_` followed by 64 hex). Both resolve to their SHA-256 for a
/// database lookup; the kind only changes which table the hash is matched in.
pub(crate) enum Credential {
    Session { token_hash: Vec<u8> },
    AccessToken { token_hash: Vec<u8> },
}

impl Credential {
    fn hash(&self) -> Vec<u8> {
        match self {
            Credential::Session { token_hash } | Credential::AccessToken { token_hash } => {
                token_hash.clone()
            }
        }
    }
}

pub(crate) fn bearer_credential(headers: &HeaderMap) -> ApiResult<Credential> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    if let Some(rest) = token.strip_prefix("gth_") {
        if rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Credential::AccessToken {
                token_hash: Sha256::digest(token.as_bytes()).to_vec(),
            });
        }
        return Err(unauthorized());
    }
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Credential::Session {
            token_hash: Sha256::digest(token.as_bytes()).to_vec(),
        });
    }
    Err(unauthorized())
}

pub(crate) async fn authenticate_token_hash(
    pool: &PgPool,
    token_hash: Vec<u8>,
) -> ApiResult<(Account, u64)> {
    let row = sqlx::query_as::<_, (Uuid, String, bool, i64)>(
        "SELECT a.id, a.username, a.is_admin, EXTRACT(EPOCH FROM clock_timestamp() - s.authenticated_at)::bigint AS auth_age FROM accounts a JOIN sessions s ON s.account_id=a.id WHERE s.token_hash=$1 AND s.expires_at > clock_timestamp() AND NOT a.suspended AND NOT a.must_change_password",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or_else(unauthorized)?;
    Ok((
        Account {
            id: row.0,
            username: row.1,
            is_admin: row.2,
            read_only: false,
            confinement: None,
        },
        row.3.max(0) as u64,
    ))
}
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Account {
    pub id: Uuid,
    pub username: String,
    pub is_admin: bool,
    #[serde(skip)]
    pub read_only: bool,
    /// The token's resource-set/scope/audience narrowing, when this account
    /// authenticated with a scoped token rather than a session or legacy token.
    #[serde(skip)]
    pub confinement: Option<githearth_authz::Confinement>,
}
impl Account {
    /// `repo:read`-only access tokens may never perform writes.
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// Who an authenticated access token acts for. Sessions and legacy tokens
/// resolve to `Account`; service-account and run tokens keep their own
/// principal so the audit actor and the engine see the non-human identity.
#[derive(Clone, Debug)]
pub enum Actor {
    Account {
        account: Account,
        confinement: Option<githearth_authz::Confinement>,
    },
    ServiceAccount {
        id: Uuid,
        organization: Uuid,
        confinement: Option<githearth_authz::Confinement>,
    },
    Run {
        id: Uuid,
        account: Uuid,
        organization: Uuid,
        confinement: Option<githearth_authz::Confinement>,
    },
}

impl Actor {
    /// The account a decision acts for: the account itself, or a run's
    /// triggering account. A service account has none.
    pub fn account_id(&self) -> Option<Uuid> {
        match self {
            Actor::Account { account, .. } => Some(account.id),
            Actor::ServiceAccount { .. } => None,
            Actor::Run { account, .. } => Some(*account),
        }
    }

    pub fn audit_actor(&self) -> AuditActor {
        match self {
            Actor::Account { account, .. } => AuditActor::account(account.id),
            Actor::ServiceAccount { id, .. } => AuditActor::service_account(*id),
            Actor::Run { id, account, .. } => AuditActor::run(*id, *account),
        }
    }
}

/// Authenticates a Bearer credential. A session token yields its real
/// `auth_age`; a personal access token can never satisfy a recent-auth (sudo)
/// check, so its `auth_age` is set to a value beyond any freshness window.
pub(crate) async fn authenticate(pool: &PgPool, headers: &HeaderMap) -> ApiResult<(Account, u64)> {
    authenticate_credential(pool, bearer_credential(headers)?).await
}

pub(crate) async fn authenticate_credential(
    pool: &PgPool,
    credential: Credential,
) -> ApiResult<(Account, u64)> {
    match credential {
        Credential::Session { token_hash } => authenticate_token_hash(pool, token_hash).await,
        Credential::AccessToken { token_hash } => {
            // The read-only boundary middleware already authenticated this exact
            // token for this request, so the handler reuses that single lookup.
            if let Ok(Some(admitted)) = ADMITTED_TOKEN
                .try_with(|(h, account, age)| (h == &token_hash).then(|| (account.clone(), *age)))
            {
                return Ok(admitted);
            }
            authenticate_access_token(pool, token_hash)
                .await
                .map(|(result, _token_id)| result)
        }
    }
}

/// The transport a credential is presented on. A token's audience must contain
/// it; legacy tokens carry both `api` and `git`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Api,
    Git,
    Registry,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::Api => "api",
            Transport::Git => "git",
            Transport::Registry => "registry",
        }
    }
}

/// Resolves a legacy personal access token: reject revoked, expired and
/// suspended-account tokens, and refuse the token after its deprecation
/// deadline (U27). Only legacy account tokens authenticate here; scoped and
/// non-account tokens go through [`authenticate_access_token_actor`].
pub async fn authenticate_access_token(
    pool: &PgPool,
    token_hash: Vec<u8>,
) -> ApiResult<((Account, u64), Uuid)> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, bool, Vec<String>, Option<bool>)>(
        "SELECT a.id, t.id, a.username, a.is_admin, t.scopes, \
         ((t.expires_at IS NOT NULL AND t.expires_at <= clock_timestamp()) \
           OR (t.legacy_deadline IS NOT NULL AND t.legacy_deadline <= clock_timestamp())) AS expired \
         FROM accounts a JOIN access_tokens t ON t.account_id = a.id \
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL AND NOT a.suspended \
         AND NOT a.must_change_password AND t.subject_kind = 'account' AND t.legacy",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or_else(unauthorized)?;
    if row.5.unwrap_or(false) {
        return Err(unauthorized());
    }
    let (account_id, token_id, username, is_admin, scopes, _) = row;
    // shortcut: scopes collapse to read-only vs read-write here. `repo:write`
    // (which implies repo:read) is the only capability beyond reading, so a
    // token without it is treated as read-only for every scope check.
    let read_only = !scopes.iter().any(|s| s == "repo:write");
    touch_token(pool, &token_hash).await;
    Ok((
        (
            Account {
                id: account_id,
                username,
                is_admin,
                read_only,
                confinement: None,
            },
            u64::MAX,
        ),
        token_id,
    ))
}

/// Resolves any access-token subject on one transport. The token's audience,
/// expiry, revocation, legacy deadline and (for a run) the run's live status
/// are checked here; the resource set and scopes are enforced by the policy
/// engine from the returned [`Actor`].
pub async fn authenticate_access_token_actor(
    pool: &PgPool,
    token_hash: Vec<u8>,
    transport: Transport,
) -> ApiResult<(Actor, Uuid)> {
    let row = sqlx::query(
        "SELECT t.id, t.subject_kind, t.account_id, t.service_account_id, t.run_id, \
                t.scopes, t.resource_set, t.audience, t.legacy, \
                ((t.expires_at IS NOT NULL AND t.expires_at <= clock_timestamp()) \
                  OR (t.legacy_deadline IS NOT NULL AND t.legacy_deadline <= clock_timestamp())) AS expired, \
                a.id AS a_id, a.username, a.is_admin, a.suspended AS a_suspended, \
                a.must_change_password AS a_mcp, \
                sa.organization_id AS sa_org, (sa.disabled_at IS NOT NULL) AS sa_disabled, \
                r.organization_id AS run_org, r.triggered_by, r.status AS run_status, \
                (r.ended_at IS NOT NULL) AS run_ended, \
                ar.suspended AS ar_suspended, ar.must_change_password AS ar_mcp \
         FROM access_tokens t \
         LEFT JOIN accounts a ON a.id = t.account_id \
         LEFT JOIN service_accounts sa ON sa.id = t.service_account_id \
         LEFT JOIN ci_runs r ON r.id = t.run_id \
         LEFT JOIN accounts ar ON ar.id = r.triggered_by \
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or_else(unauthorized)?;
    if row.get::<Option<bool>, _>("expired").unwrap_or(false) {
        return Err(unauthorized());
    }
    let audience: Vec<String> = row.get("audience");
    if !audience.iter().any(|a| a == transport.name()) {
        return Err(unauthorized());
    }
    let scopes: Vec<String> = row.get("scopes");
    let read_only = !scopes.iter().any(|s| s == "repo:write");
    let legacy: bool = row.get("legacy");
    let confinement = if legacy {
        None
    } else {
        Some(confinement(&row)?)
    };
    let token_id: Uuid = row.get("id");
    let actor = match row.get::<String, _>("subject_kind").as_str() {
        "account" => {
            if row.get::<Option<bool>, _>("a_suspended").unwrap_or(true)
                || row.get::<Option<bool>, _>("a_mcp").unwrap_or(true)
            {
                return Err(unauthorized());
            }
            Actor::Account {
                account: Account {
                    id: row.get("a_id"),
                    username: row.get("username"),
                    is_admin: row.get("is_admin"),
                    read_only,
                    confinement: confinement.clone(),
                },
                confinement,
            }
        }
        "service_account" => {
            if row.get::<Option<bool>, _>("sa_disabled").unwrap_or(true) {
                return Err(unauthorized());
            }
            Actor::ServiceAccount {
                id: row.get("service_account_id"),
                organization: row.get("sa_org"),
                confinement,
            }
        }
        "run" => {
            if row.get::<Option<bool>, _>("run_ended").unwrap_or(true)
                || row.get::<Option<String>, _>("run_status").as_deref() != Some("running")
                || row.get::<Option<bool>, _>("ar_suspended").unwrap_or(true)
                || row.get::<Option<bool>, _>("ar_mcp").unwrap_or(true)
            {
                return Err(unauthorized());
            }
            Actor::Run {
                id: row.get("run_id"),
                account: row.get("triggered_by"),
                organization: row.get("run_org"),
                confinement,
            }
        }
        _ => return Err(unauthorized()),
    };
    touch_token(pool, &token_hash).await;
    Ok((actor, token_id))
}

/// Builds the engine's confinement from a token row. A malformed stored set is
/// a 500-class refusal, not a token with no limit.
fn confinement(row: &sqlx::postgres::PgRow) -> ApiResult<githearth_authz::Confinement> {
    let scopes: Vec<String> = row.get("scopes");
    let audience: Vec<String> = row.get("audience");
    let value: serde_json::Value = row.get("resource_set");
    let resources = parse_resource_set(&value).ok_or(ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "token resource set is invalid",
    ))?;
    Ok(githearth_authz::Confinement {
        resources,
        scopes,
        audience,
    })
}

/// Parses the canonical resource-set grammar (ADR 0014 section 2):
/// `[{"type":"repository","id":...}, {"type":"organization","id":...},
///   {"type":"package","organization":...,"name":...}]`.
pub(crate) fn parse_resource_set(
    value: &serde_json::Value,
) -> Option<Vec<githearth_authz::ResourceSelector>> {
    use githearth_authz::ResourceSelector;
    let items = value.as_array()?;
    if items.is_empty() {
        return None;
    }
    let mut selectors = Vec::with_capacity(items.len());
    for item in items {
        let kind = item.get("type")?.as_str()?;
        match kind {
            "repository" => selectors.push(ResourceSelector::Repository(
                item.get("id")?.as_str()?.parse().ok()?,
            )),
            "organization" => selectors.push(ResourceSelector::Organization(
                item.get("id")?.as_str()?.parse().ok()?,
            )),
            "package" => selectors.push(ResourceSelector::Package {
                organization: item.get("organization")?.as_str()?.parse().ok()?,
                name: item.get("name")?.as_str()?.to_owned(),
            }),
            _ => return None,
        }
    }
    Some(selectors)
}

async fn touch_token(pool: &PgPool, token_hash: &[u8]) {
    // Throttle the last_used_at refresh: touch only when stale by >60s.
    let _ = sqlx::query(
        "UPDATE access_tokens SET last_used_at = clock_timestamp() \
         WHERE token_hash = $1 AND (last_used_at IS NULL OR last_used_at < clock_timestamp() - interval '60 seconds')",
    )
    .bind(token_hash)
    .execute(pool)
    .await;
}

/// Resolves an access token from its plaintext `gth_` string, for transports
/// that receive the token directly (e.g. the Git Basic-auth path).
pub(crate) async fn authenticate_access_token_plaintext(
    pool: &PgPool,
    token: &str,
    transport: Transport,
) -> ApiResult<(Actor, Uuid)> {
    let rest = token.strip_prefix("gth_").ok_or_else(unauthorized)?;
    if rest.len() != 64 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unauthorized());
    }
    authenticate_access_token_actor(pool, Sha256::digest(token.as_bytes()).to_vec(), transport)
        .await
}

/// Authenticates any subject for an API request. The historical [`authenticate`]
/// keeps its account/session/legacy-token contract; handlers that accept the
/// non-account subjects call this one.
pub(crate) async fn authenticate_actor(
    pool: &PgPool,
    headers: &HeaderMap,
    transport: Transport,
) -> ApiResult<(Actor, u64)> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => {
            let (account, age) = authenticate_token_hash(pool, token_hash).await?;
            Ok((
                Actor::Account {
                    account,
                    confinement: None,
                },
                age,
            ))
        }
        Credential::AccessToken { token_hash } => {
            let (actor, _) = authenticate_access_token_actor(pool, token_hash, transport).await?;
            Ok((actor, u64::MAX))
        }
    }
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Account>> {
    Ok(Json(authenticate(&state.pool, &headers).await?.0))
}
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let credential = bearer_credential(&headers)?;
    // A personal access token has no session to revoke; reporting 204 while
    // writing a `session.revoked` event would be misleading (issue #139).
    if matches!(&credential, Credential::AccessToken { .. }) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot log out",
        ));
    }
    let token_hash = credential.hash();
    let (account, _) = authenticate_credential(&state.pool, credential).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
        .bind(token_hash)
        .execute(&mut *tx)
        .await?;
    event(&mut tx, None, account.id, "session.revoked").await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewOrganization {
    pub(crate) slug: String,
    pub(crate) name: String,
}
#[derive(Serialize, sqlx::FromRow)]
pub(crate) struct Organization {
    pub(crate) id: Uuid,
    pub(crate) slug: String,
    pub(crate) name: String,
    pub(crate) role: String,
}
pub(crate) async fn create_organization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewOrganization>,
) -> ApiResult<(StatusCode, Json<Organization>)> {
    if input.slug.is_empty()
        || input.slug.len() > 64
        || input.slug.starts_with('-')
        || input.slug.ends_with('-')
        || input.slug == "new"
        || input.slug.contains("--")
        || !input
            .slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || input.name.trim().is_empty()
        || input.name.chars().count() > 200
        || has_nul(&input.name)
    {
        return Err(invalid());
    }
    let (account, _) = authenticate(&state.pool, &headers).await?;
    // The per-account API budget is charged by the `/api/v1` middleware. The
    // organization-creation cap stays here because every new organization
    // earns a fresh Git, job and email share; without it a single account
    // could multiply its share.
    if !state.limits.allow_organization_creation(account.id) {
        return Err(rate_limited());
    }
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&input.slug)
        .bind(&input.name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(id)
        .bind(account.id)
        .execute(&mut *tx)
        .await?;
    // Authorization reads only grants; the membership role stays for display.
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4()).bind(account.id).bind(id)
        .execute(&mut *tx)
        .await?;
    event(&mut tx, Some(id), account.id, "organization.created").await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Organization {
            id,
            slug: input.slug,
            name: input.name,
            role: "owner".into(),
        }),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub after: Option<Uuid>,
    pub limit: Option<i64>,
}
async fn list_organizations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<Organization>>> {
    let (account, _) = authenticate(&state.pool, &headers).await?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    Ok(Json(sqlx::query_as::<_, Organization>("SELECT o.id,o.slug,o.name,m.role FROM organizations o JOIN memberships m ON m.organization_id=o.id WHERE m.account_id=$1 AND NOT o.suspended AND ($2::uuid IS NULL OR o.id > $2) ORDER BY o.id LIMIT $3")
        .bind(account.id).bind(page.after).bind(limit).fetch_all(&state.pool).await?))
}
async fn read_organization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Organization>> {
    let (account, auth_age) = authenticate(&state.pool, &headers).await?;
    let not_found = || ApiError(StatusCode::NOT_FOUND, "not found");
    // Load, then authorize: a denial and a missing organization look the same.
    let mut conn = state.pool.acquire().await?;
    let Some(facts) = facts::load(&mut conn, account.id, id).await? else {
        return Err(not_found());
    };
    let decision = state.authz.authorize(&facts.request(
        githearth_authz::Action::OrganizationRead,
        Resource::Organization,
        Some(Duration::from_secs(auth_age)),
    ));
    if !decision.is_allowed() {
        return Err(not_found());
    }
    let row = sqlx::query_as::<_, Organization>("SELECT o.id,o.slug,o.name,m.role FROM organizations o JOIN memberships m ON m.organization_id=o.id AND m.account_id=$1 WHERE o.id=$2")
        .bind(account.id).bind(id).fetch_optional(&mut *conn).await?.ok_or_else(not_found)?;
    Ok(Json(row))
}
pub(crate) async fn event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization: Option<Uuid>,
    actor: Uuid,
    kind: &str,
) -> Result<(), sqlx::Error> {
    event_with(tx, organization, actor, kind, serde_json::json!({})).await
}

/// The audit actor for a row. An account acts as itself; a service account by
/// its row id with no account; a run by its run id plus its triggering account.
#[derive(Clone, Copy, Debug)]
pub struct AuditActor {
    pub kind: &'static str,
    pub id: Uuid,
    pub account: Option<Uuid>,
}

impl AuditActor {
    pub fn account(id: Uuid) -> Self {
        Self {
            kind: "account",
            id,
            account: Some(id),
        }
    }

    pub fn service_account(id: Uuid) -> Self {
        Self {
            kind: "service_account",
            id,
            account: None,
        }
    }

    pub fn run(id: Uuid, account: Uuid) -> Self {
        Self {
            kind: "run",
            id,
            account: Some(account),
        }
    }
}

/// `event` with `extra` fields merged into the outbox payload (IDs only, never
/// secrets); the audit row keeps its fixed columns and shares the event id.
pub(crate) async fn event_with(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization: Option<Uuid>,
    actor: Uuid,
    kind: &str,
    extra: serde_json::Value,
) -> Result<(), sqlx::Error> {
    event_with_actor(tx, organization, AuditActor::account(actor), kind, extra).await
}

/// The same event under a non-account principal's audit actor.
pub(crate) async fn event_with_actor(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization: Option<Uuid>,
    actor: AuditActor,
    kind: &str,
    extra: serde_json::Value,
) -> Result<(), sqlx::Error> {
    // At most one audit chain per transaction, or the site chain last (lock order).
    let id = Uuid::new_v4();
    let client_ip = CLIENT_IP.try_with(|ip| ip.to_string()).ok();
    // The digest only hides the IP once its key is deleted: IPv4 is
    // brute-forceable while the key exists, and any backup that still contains
    // a deleted key or the raw IP bounds how long the IP is retained.
    let (client_ip, client_ip_key_id, client_ip_digest) = match client_ip {
        Some(ip) => {
            let (key_id, key) = current_or_new_ip_key(tx).await?;
            let ip_text: String = sqlx::query_scalar("SELECT host($1::inet)")
                .bind(&ip)
                .fetch_one(&mut **tx)
                .await?;
            let digest = hmac_sha256(&key, ip_text.as_bytes());
            (Some(ip), Some(key_id), Some(digest))
        }
        None => (None, None, None),
    };
    sqlx::query(
        "INSERT INTO audit_events (id,organization_id,actor_id,actor_kind,actor_account_id,kind,client_ip,client_ip_key_id,client_ip_digest) \
         VALUES ($1,$2,$3,$4,$5,$6,$7::inet,$8,$9)",
    )
    .bind(id)
    .bind(organization)
    .bind(actor.id)
    .bind(actor.kind)
    .bind(actor.account)
    .bind(kind)
    .bind(client_ip)
    .bind(client_ip_key_id)
    .bind(client_ip_digest)
    .execute(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO outbox_events (id,organization_id,kind,payload) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(organization)
        .bind(kind)
        .bind({
            let mut payload = extra;
            payload["actor_id"] = serde_json::json!(actor.id);
            payload["actor_kind"] = serde_json::json!(actor.kind);
            payload["actor_account_id"] = serde_json::json!(actor.account);
            payload["organization_id"] = serde_json::json!(organization);
            payload
        })
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The most recent IP-key row, or a freshly created one when none exists. The
/// insert is guarded by `WHERE NOT EXISTS`, but under READ COMMITTED two
/// concurrent first writers may both see no rows and each insert a key. That is
/// harmless because each event stores its own key_id and digest, so both keys
/// remain equally valid.
///
/// The key material is stored as an envelope: a per-key data key wrapped by the
/// installation key, and the HMAC key encrypted under that data key. Only rows
/// written before 0056's backfill (or by a test that inserts the old column
/// directly) still carry plaintext, and start-up refuses to run until those are
/// encrypted.
#[derive(sqlx::FromRow)]
struct CurrentIpKey {
    id: Uuid,
    key: Option<Vec<u8>>,
    secret_id: Option<Uuid>,
    data_key_id: Option<Uuid>,
    nonce: Option<Vec<u8>>,
    ciphertext: Option<Vec<u8>>,
    kek_id: Option<String>,
    wrap_method: Option<String>,
    wrapped_key: Option<Vec<u8>>,
}

const CURRENT_IP_KEY: &str = "SELECT k.id, k.key, k.secret_id, s.data_key_id, s.nonce, \
     s.ciphertext, d.kek_id, d.wrap_method, d.wrapped_key \
     FROM audit_ip_keys k \
     LEFT JOIN stored_secrets s ON s.id = k.secret_id \
     LEFT JOIN secret_data_keys d ON d.id = s.data_key_id \
     ORDER BY k.created_at DESC, k.id DESC LIMIT 1";

fn no_installation_key() -> sqlx::Error {
    installation_key_error(
        "no installation key is configured, so a client IP digest cannot be stored",
    )
}

/// Marks an error as an installation-key problem rather than a database
/// outage. `From<sqlx::Error>` turns this into `503 installation key
/// unavailable` and logs the cause (which names key ids only, never key
/// material), instead of the misleading `503 database unavailable`.
const INSTALLATION_KEY_ERROR: &str = "installation-key: ";

fn installation_key_error(message: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::Protocol(format!("{INSTALLATION_KEY_ERROR}{message}"))
}

async fn current_or_new_ip_key(
    conn: &mut sqlx::PgConnection,
) -> Result<(Uuid, Vec<u8>), sqlx::Error> {
    if let Some(row) = sqlx::query_as::<_, CurrentIpKey>(CURRENT_IP_KEY)
        .fetch_optional(&mut *conn)
        .await?
    {
        return open_current_ip_key(row);
    }
    let keys = secrets::current_keys().ok_or_else(no_installation_key)?;
    let id = Uuid::new_v4();
    let key = audit_retention::new_ip_key_bytes();
    let secret_id = Uuid::new_v4();
    let sealed = secrets::seal(&keys, secret_id, &key).map_err(|error| {
        installation_key_error(format!("cannot encrypt the audit IP key: {error:#}"))
    })?;
    secrets::insert_sealed(&mut *conn, &format!("audit_ip_key/{id}"), &sealed)
        .await
        .map_err(|error| {
            installation_key_error(format!("cannot store the audit IP key: {error:#}"))
        })?;
    let inserted = sqlx::query(
        "INSERT INTO audit_ip_keys (id, secret_id) \
         SELECT $1, $2 WHERE NOT EXISTS (SELECT 1 FROM audit_ip_keys)",
    )
    .bind(id)
    .bind(secret_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok((id, key));
    }
    // Another writer won the first-key race. Its row is equally valid; use it.
    // Remove this call's two rows first, or they would inflate the secret and
    // data-key counts forever without a referencing audit key.
    sqlx::query("DELETE FROM stored_secrets WHERE id = $1")
        .bind(secret_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM secret_data_keys WHERE id = $1")
        .bind(sealed.data_key_id)
        .execute(&mut *conn)
        .await?;
    let row = sqlx::query_as::<_, CurrentIpKey>(CURRENT_IP_KEY)
        .fetch_one(&mut *conn)
        .await?;
    open_current_ip_key(row)
}

fn open_current_ip_key(row: CurrentIpKey) -> Result<(Uuid, Vec<u8>), sqlx::Error> {
    let (
        Some(secret_id),
        Some(data_key_id),
        Some(nonce),
        Some(ciphertext),
        Some(kek_id),
        Some(wrap_method),
        Some(wrapped_key),
    ) = (
        row.secret_id,
        row.data_key_id,
        row.nonce,
        row.ciphertext,
        row.kek_id,
        row.wrap_method,
        row.wrapped_key,
    )
    else {
        // A row written before the envelope, or by another writer that has
        // not run the 0056 backfill. Refuse it: silently using the plaintext
        // would both serve and preserve exactly the secret this slice exists
        // to remove. Backfill is an operator action, so the error names it.
        let id = row.id;
        return row.key.map_or_else(
            || Err(installation_key_error("audit IP key has no key material")),
            |_| {
                Err(installation_key_error(format!(
                    "audit IP key {id} is still plaintext; stop every other writer and run \
                     `githearth-server migrate` with the installation key"
                )))
            },
        );
    };
    let keys = secrets::current_keys().ok_or_else(no_installation_key)?;
    let data_key = secrets::WrappedDataKey {
        id: data_key_id,
        kek_id,
        wrap_method,
        wrapped_key,
        owner_key_id: None,
        owner_kind: None,
        owner_id: None,
    };
    let plaintext =
        secrets::open(&keys, secret_id, &data_key, &nonce, &ciphertext).map_err(|error| {
            installation_key_error(format!("cannot decrypt the audit IP key: {error:#}"))
        })?;
    Ok((row.id, plaintext))
}

pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    /// A unique storage root in the system temp dir so AppState setup never
    /// writes under the source tree.
    fn temp_storage_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "githearth-lib-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hashes_pin_argon2_resource_parameters() {
        let hash = hash_password("a sufficiently long password").unwrap();
        let parsed = PasswordHash::new(&hash).unwrap();
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        assert_eq!(parsed.version, Some(19));
        assert_eq!(parsed.params.get_decimal("m"), Some(19_456));
        assert_eq!(parsed.params.get_decimal("t"), Some(2));
        assert_eq!(parsed.params.get_decimal("p"), Some(1));
        assert_eq!(parsed.hash.unwrap().len(), 32);
        assert!(
            password_hasher()
                .verify_password(b"a sufficiently long password", &parsed)
                .is_ok()
        );
    }

    #[test]
    fn hook_script_escapes_single_quotes_in_the_program_path() {
        let root = temp_storage_root();
        write_hooks(&root.join("hooks"), &root.join("it's")).unwrap();
        let script = std::fs::read_to_string(root.join("hooks").join("pre-receive")).unwrap();
        assert!(script.contains("it'\\''s"), "{script}");
        assert!(script.contains("git-hook pre-receive \"$@\""), "{script}");
    }

    #[tokio::test]
    async fn auth_queues_briefly_instead_of_refusing_a_burst() {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://localhost:1/unused")
            .unwrap();
        let config = Config {
            database_url: "postgres://localhost:1/unused".into(),
            auth_concurrency: 1,
            storage_root: temp_storage_root(),
            ..Config::default()
        };
        let state = AppState::new(pool, config).await.unwrap();
        let permit = state.auth_slots.clone().acquire_owned().await.unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(permit);
        });
        // The slot frees within the queue wait, so the request proceeds to the
        // (unreachable) database instead of being refused as busy.
        let response = router(state)
            .oneshot(
                Request::post("/api/v1/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"username":"some-user","password":"some password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn saturated_auth_returns_429_before_database_access() {
        // The lazy pool never connects: saturation must be rejected before any query.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let config = Config {
            database_url: "postgres://localhost/unused".into(),
            auth_concurrency: 1,
            storage_root: temp_storage_root(),
            ..Config::default()
        };
        let state = AppState::new(pool, config).await.unwrap();
        let permit = state.auth_slots.clone().acquire_owned().await.unwrap();
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"username":"some-user","password":"some password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(state.auth_slots.available_permits(), 0);
        drop(permit);
        assert_eq!(state.auth_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn saturated_browser_login_is_busy_not_bad_credentials() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let config = Config {
            database_url: "postgres://localhost/unused".into(),
            auth_concurrency: 1,
            storage_root: temp_storage_root(),
            ..Config::default()
        };
        let state = AppState::new(pool, config).await.unwrap();
        let _permit = state.auth_slots.clone().acquire_owned().await.unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let response = router(state)
            .oneshot(
                Request::post("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", format!("githearth_csrf={token}"))
                    .body(Body::from(format!(
                        "username=some-user&password=some+password&csrf_token={token}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn cancelled_password_caller_retains_slot_until_worker_finishes() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let caller = tokio::spawn(password_work(permit, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.await.unwrap();
        // HTTP timeout cancels the future awaiting this same helper; it cannot stop its blocking worker.
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(slots.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(2), slots.clone().acquire_owned())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(slots.available_permits(), 1);
    }
}
