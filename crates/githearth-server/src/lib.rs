pub mod audit;
pub mod audit_retention;
pub mod auto_merge;
pub mod branch_protection_ui;
pub mod browse;
mod browse_ui;
pub mod client_ip;
pub mod codeowners;
pub mod config;
pub mod doctor;
pub mod facts;
pub mod forks;
pub mod git_admission;
pub mod git_hooks;
pub mod git_http;
pub mod idempotency;
pub mod issues;
mod issues_ui;
pub mod jobs;
pub mod merge;
pub mod merge_intents;
pub mod merge_worker;
mod mergeability;
pub mod notifications;
mod notifications_ui;
pub mod protection;
pub mod pulls;
mod pulls_ui;
pub mod pushes;
pub mod reconcile;
pub mod repositories;
pub mod repository_api;
mod settings_ui;
pub mod ssh;
pub mod ssh_keys;
pub mod statuses;
pub mod tag_protection;
mod tag_protection_ui;
mod tokens;
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
    pub(crate) auth_slots: Arc<Semaphore>,
    // pub so the concurrency-exhaustion integration test can hold a permit.
    pub git_slots: Arc<Semaphore>,
    dummy_hash: Arc<String>,
    pub authz: Arc<Engine>,
    pub storage: Arc<githearth_git::layout::Storage>,
    // pub so the disconnect integration test can assert the token is released.
    pub hook_tokens: Arc<std::sync::Mutex<HashMap<String, git_admission::Admitted>>>,
    pub(crate) hook_socket_path: std::path::PathBuf,
    pub(crate) trusted_proxies: Vec<TrustedProxy>,
}
impl AppState {
    pub async fn new(pool: PgPool, config: Config) -> anyhow::Result<Self> {
        config.validate()?;
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
        let state = Self {
            pool,
            auth_slots: Arc::new(Semaphore::new(config.auth_concurrency)),
            git_slots: Arc::new(Semaphore::new(config.git_concurrency)),
            config,
            dummy_hash: Arc::new(dummy_hash),
            authz: Arc::new(Engine::new()?),
            storage: Arc::new(storage),
            hook_tokens: Arc::new(std::sync::Mutex::new(HashMap::new())),
            hook_socket_path: socket_path,
            trusted_proxies,
        };
        tokio::spawn(crate::git_http::hook_accept_loop(listener, state.clone()));
        Ok(state)
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
        .merge(protection::router())
        .merge(tag_protection::router())
        .merge(statuses::router())
        .merge(audit::router())
        .merge(ui::router())
        .merge(browse::router())
        .merge(tokens::router())
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

    // The last layer runs first: resolve the client IP before observe logs it.
    json.merge(git)
        .layer(axum::middleware::from_fn(observe))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            client_ip_middleware,
        ))
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
    if method == Method::GET || method == Method::HEAD {
        return next.run(request).await;
    }
    // Only a valid `gth_` access token is intercepted; anything else (no
    // credential, a session token, or a malformed token) falls through to the
    // handler, which authenticates as it always has.
    let token_hash = match bearer_credential(request.headers()) {
        Ok(Credential::AccessToken { token_hash }) => token_hash,
        _ => return next.run(request).await,
    };
    match authenticate_credential(
        &state.pool,
        Credential::AccessToken {
            token_hash: token_hash.clone(),
        },
    )
    .await
    {
        Ok((account, _)) if account.is_read_only() => read_only_denied().into_response(),
        Ok((account, age)) => {
            ADMITTED_TOKEN
                .scope((token_hash, account, age), next.run(request))
                .await
        }
        // An invalid or expired token: the handler refuses it the same way.
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => next.run(request).await,
        // Any other lookup failure (pool timeout, dropped database, a failed
        // `last_used_at` refresh) must fail closed: never run the handler,
        // or a transient error would let a read-only token's write through.
        Err(e) => e.into_response(),
    }
}

async fn client_ip_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
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
        return CLIENT_IP.scope(ip, next.run(request)).await;
    }
    next.run(request).await
}

async fn observe(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let start = std::time::Instant::now();
    let method = request.method().clone();
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
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
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
pub(crate) fn invalid() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "invalid input")
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

pub(crate) async fn verify_password(
    state: &AppState,
    username: &str,
    password: &str,
) -> ApiResult<Option<Uuid>> {
    if !valid_username(username) || password.len() > 1024 {
        return Ok(None);
    }
    // A short queue absorbs bursts such as a clone's back-to-back Git requests; only a
    // sustained overload is refused. Waiting holds no connection.
    let permit = tokio::time::timeout(AUTH_QUEUE_WAIT, state.auth_slots.clone().acquire_owned())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "authentication busy",
        ))?;
    let row = sqlx::query("SELECT id, password_hash, suspended FROM accounts WHERE username = $1")
        .bind(username)
        .fetch_optional(&state.pool)
        .await?;
    // A suspended account still pays for the hash, so timing does not reveal the suspension.
    let id = row
        .as_ref()
        .filter(|r| !r.get::<bool, _>("suspended"))
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

async fn login(State(state): State<AppState>, Json(input): Json<Login>) -> ApiResult<Json<Token>> {
    let id = verify_password(&state, &input.username, &input.password)
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
        "SELECT a.id, a.username, a.is_admin, EXTRACT(EPOCH FROM clock_timestamp() - s.authenticated_at)::bigint AS auth_age FROM accounts a JOIN sessions s ON s.account_id=a.id WHERE s.token_hash=$1 AND s.expires_at > clock_timestamp() AND NOT a.suspended",
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
        },
        row.3.max(0) as u64,
    ))
}
#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct Account {
    pub id: Uuid,
    pub username: String,
    pub is_admin: bool,
    #[serde(skip)]
    pub read_only: bool,
}
impl Account {
    /// `repo:read`-only access tokens may never perform writes.
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
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

/// Resolves a personal access token: reject revoked, expired and
/// suspended-account tokens, return the account with `read_only` set whenever
/// the token lacks `repo:write`, and `auth_age` large enough that sensitive
/// actions keep requiring a fresh session. `last_used_at` is refreshed, at a
/// throttled rate so a burst of Git requests does not write per-request.
pub async fn authenticate_access_token(
    pool: &PgPool,
    token_hash: Vec<u8>,
) -> ApiResult<((Account, u64), Uuid)> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, bool, Vec<String>, Option<bool>)>(
        "SELECT a.id, t.id, a.username, a.is_admin, t.scopes, (t.expires_at IS NOT NULL AND t.expires_at <= clock_timestamp()) AS expired \
         FROM accounts a JOIN access_tokens t ON t.account_id = a.id \
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL AND NOT a.suspended",
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
    // Throttle the last_used_at refresh: touch only when stale by >60s.
    let _ = sqlx::query(
        "UPDATE access_tokens SET last_used_at = clock_timestamp() \
         WHERE token_hash = $1 AND (last_used_at IS NULL OR last_used_at < clock_timestamp() - interval '60 seconds')",
    )
    .bind(&token_hash)
    .execute(pool)
    .await?;
    Ok((
        (
            Account {
                id: account_id,
                username,
                is_admin,
                read_only,
            },
            u64::MAX,
        ),
        token_id,
    ))
}
/// Resolves an access token from its plaintext `gth_` string, for transports
/// that receive the token directly (e.g. the Git Basic-auth path).
pub(crate) async fn authenticate_access_token_plaintext(
    pool: &PgPool,
    token: &str,
) -> ApiResult<((Account, u64), Uuid)> {
    let rest = token.strip_prefix("gth_").ok_or_else(unauthorized)?;
    if rest.len() != 64 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unauthorized());
    }
    authenticate_access_token(pool, Sha256::digest(token.as_bytes()).to_vec()).await
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
struct NewOrganization {
    slug: String,
    name: String,
}
#[derive(Serialize, sqlx::FromRow)]
struct Organization {
    id: Uuid,
    slug: String,
    name: String,
    role: String,
}
async fn create_organization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewOrganization>,
) -> ApiResult<(StatusCode, Json<Organization>)> {
    let (account, _) = authenticate(&state.pool, &headers).await?;
    if input.slug.is_empty()
        || input.slug.len() > 64
        || input.slug.starts_with('-')
        || input.slug.ends_with('-')
        || input.slug.contains("--")
        || !input
            .slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || input.name.trim().is_empty()
        || input.name.chars().count() > 200
    {
        return Err(invalid());
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
    let not_found = || ApiError(StatusCode::NOT_FOUND, "organization not found");
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

/// `event` with `extra` fields merged into the outbox payload (IDs only, never
/// secrets); the audit row keeps its fixed columns and shares the event id.
pub(crate) async fn event_with(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization: Option<Uuid>,
    actor: Uuid,
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
        "INSERT INTO audit_events (id,organization_id,actor_id,kind,client_ip,client_ip_key_id,client_ip_digest) VALUES ($1,$2,$3,$4,$5::inet,$6,$7)",
    )
    .bind(id)
    .bind(organization)
    .bind(actor)
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
            payload["actor_id"] = serde_json::json!(actor);
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
async fn current_or_new_ip_key(
    conn: &mut sqlx::PgConnection,
) -> Result<(Uuid, Vec<u8>), sqlx::Error> {
    if let Some(key) = sqlx::query_as::<_, (Uuid, Vec<u8>)>(
        "SELECT id, key FROM audit_ip_keys ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?
    {
        return Ok(key);
    }
    let id = Uuid::new_v4();
    let key = audit_retention::new_ip_key_bytes();
    sqlx::query(
        "INSERT INTO audit_ip_keys (id, key) \
         SELECT $1, $2 WHERE NOT EXISTS (SELECT 1 FROM audit_ip_keys)",
    )
    .bind(id)
    .bind(&key)
    .execute(&mut *conn)
    .await?;
    sqlx::query_as::<_, (Uuid, Vec<u8>)>(
        "SELECT id, key FROM audit_ip_keys ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .fetch_one(&mut *conn)
    .await
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
