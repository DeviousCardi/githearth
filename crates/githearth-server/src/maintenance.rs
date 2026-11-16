//! The write fence `backup` proves before it takes a snapshot (M4.1 U13).
//!
//! Maintenance mode is a mechanism, not a sentence: a write request must take a
//! [`WritePermit`] *before* it can reach a handler, and taking one is atomic
//! with the fence check.  Fencing the server therefore either refuses the
//! request (`503`) or counts it as in flight.  Background work that writes the
//! storage tree — the merge worker, the reconciler's mutating pass and the
//! housekeeping job handler — takes the same permit for the whole unit of work,
//! so the drain counts it too.  The state is drained only when the last permit
//! is dropped, which for a Git push happens after receive-pack has exited and
//! the journal has been replayed into PostgreSQL — not when the response
//! headers are written.
//!
//! The state is observable in two ways: `GET /api/v1/maintenance/status` on
//! the server's bind address (authenticated with the on-host control token and
//! reachable only from a loopback client), and
//! `<storage_root>/run/maintenance.json`, which records the last observed
//! phase.  `backup` uses the authenticated endpoint, not the file, so a stale
//! file can never satisfy the fence.

use crate::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

/// The control token lives beside the hook sockets and the maintenance state.
pub const CONTROL_TOKEN_FILE: &str = "maintenance.token";
pub const STATE_FILE: &str = "maintenance.json";
pub const STATUS_PATH: &str = "/api/v1/maintenance/status";
pub const START_PATH: &str = "/api/v1/maintenance/start";
pub const STOP_PATH: &str = "/api/v1/maintenance/stop";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Active,
    Draining,
    Drained,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub phase: Phase,
    pub fenced: bool,
    pub in_flight: usize,
}

struct Inner {
    state: Mutex<FenceState>,
    changed: Notify,
    state_path: PathBuf,
}

#[derive(Default)]
struct FenceState {
    fenced: bool,
    in_flight: usize,
}

/// The process-wide fence. Cloning shares the same state.
#[derive(Clone)]
pub struct Maintenance {
    inner: Arc<Inner>,
}

impl Maintenance {
    /// `state_path` is `<storage_root>/run/maintenance.json`; the file is
    /// rewritten on every phase transition so an operator can observe the
    /// fence without polling the server.
    pub fn new(state_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(FenceState::default()),
                changed: Notify::new(),
                state_path,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FenceState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn phase(&self) -> Phase {
        let state = self.lock();
        Phase::from_state(state.fenced, state.in_flight)
    }

    pub fn status(&self) -> Status {
        let state = self.lock();
        Status {
            phase: Phase::from_state(state.fenced, state.in_flight),
            fenced: state.fenced,
            in_flight: state.in_flight,
        }
    }

    /// Fences new writes.  In-flight writes are left to finish; the phase
    /// becomes `draining` until each permit is dropped.
    pub fn start(&self) -> Status {
        {
            let mut state = self.lock();
            state.fenced = true;
        }
        let status = self.status();
        self.record(&status);
        self.inner.changed.notify_waiters();
        status
    }

    /// Lifts the fence.  A caller that is mid-backup can still be holding a
    /// permit; that permit remains counted, but it cannot prevent `stop`.
    pub fn stop(&self) -> Status {
        {
            let mut state = self.lock();
            state.fenced = false;
        }
        let status = self.status();
        self.record(&status);
        self.inner.changed.notify_waiters();
        status
    }

    /// Atomically checks the fence and counts one in-flight write.  `None`
    /// means the write must be refused and must not reach a handler.
    pub fn admit_write(&self) -> Option<WritePermit> {
        let mut state = self.lock();
        if state.fenced {
            return None;
        }
        state.in_flight += 1;
        Some(WritePermit {
            inner: self.inner.clone(),
        })
    }

    /// Counts a write that was already admitted through [`admit_write`] but
    /// whose lifetime is longer than the request that started it (a Git push
    /// keeps committing after the response headers are written).  This never
    /// checks the fence: the push is already in flight and must be drained.
    pub fn track_write(&self) -> WritePermit {
        {
            let mut state = self.lock();
            state.in_flight += 1;
        }
        WritePermit {
            inner: self.inner.clone(),
        }
    }

    /// Waits until every admitted permit has been dropped.  Returns the final
    /// status, or `None` on timeout while still draining.
    pub async fn wait_drained(&self, timeout: Duration) -> Option<Status> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let status = self.status();
            match status.phase {
                Phase::Drained => return Some(status),
                Phase::Active => return None,
                Phase::Draining => {}
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let _ = tokio::time::timeout(remaining, self.inner.changed.notified()).await;
        }
    }

    /// Rewrites the observability file on a phase change.  It is best effort:
    /// the authoritative answer is the endpoint, and a read-only or missing
    /// directory must not make a write fail.
    fn record(&self, status: &Status) {
        let Ok(text) = serde_json::to_vec_pretty(status) else {
            return;
        };
        let temporary = self.inner.state_path.with_extension("json.tmp");
        if let Some(parent) = temporary.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&temporary, text).is_ok() {
            let _ = fs::rename(&temporary, &self.inner.state_path);
        }
    }
}

impl Phase {
    fn from_state(fenced: bool, in_flight: usize) -> Self {
        match (fenced, in_flight) {
            (false, _) => Phase::Active,
            (true, 0) => Phase::Drained,
            (true, _) => Phase::Draining,
        }
    }
}

/// One admitted write.  Dropping it removes the write from the drain count and
/// wakes a waiter when the count reaches zero.
pub struct WritePermit {
    inner: Arc<Inner>,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        let (status, became_drained) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.in_flight = state.in_flight.saturating_sub(1);
            let status = Status {
                phase: Phase::from_state(state.fenced, state.in_flight),
                fenced: state.fenced,
                in_flight: state.in_flight,
            };
            (status, state.fenced && state.in_flight == 0)
        };
        self.inner.changed.notify_waiters();
        if became_drained {
            // Reuse the same best-effort writer as start/stop.
            Maintenance {
                inner: self.inner.clone(),
            }
            .record(&status);
        }
    }
}

/// The routes that observe and change the fence.  They are mounted outside the
/// JSON API's read-only layer so a 64-hex control token is not mistaken for a
/// personal access token, and outside the fence middleware so `stop` is always
/// reachable.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(STATUS_PATH, get(status))
        .route(START_PATH, post(start))
        .route(STOP_PATH, post(stop))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: crate::client_ip::ResolvedIp,
) -> Response {
    if !from_loopback(&client) {
        return not_found();
    }
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    Json(state.maintenance.status()).into_response()
}

async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: crate::client_ip::ResolvedIp,
) -> Response {
    if !from_loopback(&client) {
        return not_found();
    }
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    Json(state.maintenance.start()).into_response()
}

async fn stop(
    State(state): State<AppState>,
    headers: HeaderMap,
    client: crate::client_ip::ResolvedIp,
) -> Response {
    if !from_loopback(&client) {
        return not_found();
    }
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    Json(state.maintenance.stop()).into_response()
}

/// The control endpoint is on-host only. Like `/metrics`, it answers a
/// non-loopback client with a bare `404`, so a cleartext endpoint carrying a
/// bearer token is never exposed off-host even when `bind` is not loopback.
fn from_loopback(client: &crate::client_ip::ResolvedIp) -> bool {
    client
        .0
        .map(|client| crate::metrics::is_loopback(client.addr()))
        .unwrap_or(false)
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "invalid maintenance control token",
    )
        .into_response()
}

/// Constant-time comparison: the token is short-lived and on-host, but a
/// timing side channel is still not worth writing.
fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    // A command that has no control token (anything but `serve`) cannot
    // authenticate anyone, not even with an empty `Bearer ` value.
    if state.maintenance_token.is_empty() {
        return false;
    }
    let Some(value) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    constant_time_eq(value.as_bytes(), state.maintenance_token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Axum middleware for the JSON API and Git transports.  Reads pass; a write
/// must take a permit or is refused with `503`.  A Git push is counted twice on
/// purpose: this request-scoped permit covers ordinary API and UI handlers, and
/// `receive_pack` moves a second permit into the task that owns the committed
/// push, so the drain cannot finish before the journal replay does.
pub async fn fence(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method();
    let path = request.uri().path();
    let read = method == axum::http::Method::GET
        || method == axum::http::Method::HEAD
        || method == axum::http::Method::OPTIONS;
    // Smart HTTP uses POST for fetches too; only receive-pack is a write.
    let is_fetch = path.ends_with("/git-upload-pack");
    if read || is_fetch {
        return next.run(request).await;
    }
    match state.maintenance.admit_write() {
        Some(permit) => {
            let response = next.run(request).await;
            drop(permit);
            response
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "5")],
            "maintenance mode: writes are refused while a backup is in progress\n",
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// On-host client used by `backup`, `restore` and `maintenance`
// ---------------------------------------------------------------------------

/// What `backup` proved before it took a snapshot.  It is written into the
/// manifest and printed by the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fence {
    /// Both listeners refused a connection, so no writer could acknowledge a
    /// commit through this installation.
    NotRunning { detail: String },
    /// The server reported that it is fenced and that no admitted write is
    /// still in flight.
    Maintenance { detail: String },
    /// The online path briefly and automatically fenced the running server for
    /// the metadata position and the copy-on-write snapshot, then released it
    /// before the dump and archive. It is not an operator maintenance window.
    OnlineSnapshot { detail: String },
}

impl Fence {
    pub fn kind(&self) -> &'static str {
        match self {
            Fence::NotRunning { .. } => "not-running",
            Fence::Maintenance { .. } => "maintenance",
            Fence::OnlineSnapshot { .. } => "online-snapshot",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Fence::NotRunning { detail }
            | Fence::Maintenance { detail }
            | Fence::OnlineSnapshot { detail } => detail,
        }
    }
}

enum Listener {
    Refused,
    Open,
    Unknown(String),
}

fn probe(address: SocketAddr) -> Listener {
    let address = reachable_address(address);
    // outbound-exempt: on-host liveness probe of a configured bind address, rewritten to loopback
    match TcpStream::connect_timeout(&address, Duration::from_millis(750)) {
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Listener::Refused,
        Err(error) => Listener::Unknown(error.to_string()),
        Ok(_) => Listener::Open,
    }
}

/// A wildcard bind address answers on loopback; connecting to `0.0.0.0` or
/// `::` is not portable, and the question is only whether a local writer could
/// connect.
fn reachable_address(address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        if address.is_ipv4() {
            SocketAddr::from(([127, 0, 0, 1], address.port()))
        } else {
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], address.port()))
        }
    } else {
        address
    }
}

/// Proves the fence.  Either every listener refuses a connection, or the
/// running server reports `drained` under the authenticated control endpoint.
/// Anything else is refused with the reason, because a sentence is not proof.
pub async fn prove(config: &crate::config::Config) -> anyhow::Result<Fence> {
    let http = probe(config.bind);
    let ssh = if config.ssh_enabled {
        Some((config.ssh_bind, probe(config.ssh_bind)))
    } else {
        None
    };

    match http {
        Listener::Refused => {
            if let Some((address, Listener::Open)) = ssh {
                anyhow::bail!(
                    "the HTTP listener on {} refuses connections, but the SSH listener on {} \
                     still accepts them; a push could still be acknowledged. Stop the whole \
                     server or enter maintenance mode",
                    config.bind,
                    address
                );
            }
            if let Some((address, Listener::Unknown(reason))) = ssh {
                anyhow::bail!("cannot prove the SSH listener on {address} is stopped: {reason}");
            }
            Ok(Fence::NotRunning {
                detail: match config.ssh_enabled {
                    true => format!(
                        "HTTP {} and SSH {} both refused connections",
                        config.bind, config.ssh_bind
                    ),
                    false => format!("HTTP {} refused connections", config.bind),
                },
            })
        }
        Listener::Open => {
            let token = control_token(config)?;
            let status = request_status(config.bind, &token).await.map_err(|error| {
                anyhow::anyhow!(
                    "the server at {} is running, but the maintenance state could not be \
                         read ({error}); stop it or start maintenance mode",
                    config.bind
                )
            })?;
            match status.phase {
                Phase::Drained => Ok(Fence::Maintenance {
                    detail: format!(
                        "server at {} reported drained with 0 writes in flight (HTTP control endpoint)",
                        config.bind
                    ),
                }),
                Phase::Draining => anyhow::bail!(
                    "maintenance mode is draining: {} write(s) still in flight at {}; wait for \
                     `maintenance status` to report drained",
                    status.in_flight,
                    config.bind
                ),
                Phase::Active => anyhow::bail!(
                    "the server at {} is running and accepting writes; stop it or run \
                     `githearth-server maintenance start` and wait for drained",
                    config.bind
                ),
            }
        }
        Listener::Unknown(reason) => anyhow::bail!(
            "cannot prove the server at {} is stopped: {reason}",
            config.bind
        ),
    }
}

/// A brief automatic gate used only by the online backup path. It reuses the
/// proved maintenance fence so a write that starts while the snapshot is taken
/// is refused before acknowledgement, but it drains before the metadata
/// position and is released before the dump, tar and encryption, so the
/// operator never enters maintenance mode.
pub struct OnlineGate {
    address: SocketAddr,
    token: String,
    started_here: bool,
}

impl OnlineGate {
    /// Releases the gate. Idempotent from the caller's side: a gate the
    /// operator had already entered is left alone.
    pub async fn release(mut self) -> anyhow::Result<()> {
        if self.started_here {
            request_stop(self.address, &self.token).await?;
            // The stop was acknowledged; `Drop` must not send it again.
            self.started_here = false;
        }
        Ok(())
    }
}

impl Drop for OnlineGate {
    /// The last chance to release a gate this process started. Error paths
    /// release explicitly and a normal exit never drops an unreleased gate;
    /// this covers a panic, an unwound error and a caller that forgot. The
    /// request is synchronous and bounded by the same timeouts the control
    /// endpoint uses, so unwinding cannot hang.
    fn drop(&mut self) {
        if self.started_here {
            let _ = request_blocking(self.address, &self.token, "POST", STOP_PATH);
            self.started_here = false;
        }
    }
}

/// Starts the online snapshot gate, or proves the server is already stopped.
/// The returned gate is `None` when no server is listening. Unlike [`prove`],
/// a running server is fenced automatically here; that is the point of the
/// online path, and it is not an operator maintenance window.
pub async fn begin_online(
    config: &crate::config::Config,
) -> anyhow::Result<(Fence, Option<OnlineGate>)> {
    let http = probe(config.bind);
    let ssh = if config.ssh_enabled {
        Some((config.ssh_bind, probe(config.ssh_bind)))
    } else {
        None
    };
    match http {
        Listener::Refused => {
            if let Some((address, Listener::Open)) = ssh {
                anyhow::bail!(
                    "the HTTP listener on {} refuses connections, but the SSH listener on {} \
                     still accepts them and the online snapshot gate cannot reach the server; \
                     stop the whole server, or use backup_snapshot_mode = \"fence\" with \
                     maintenance mode",
                    config.bind,
                    address
                );
            }
            if let Some((address, Listener::Unknown(reason))) = ssh {
                anyhow::bail!("cannot prove the SSH listener on {address} is stopped: {reason}");
            }
            Ok((
                Fence::NotRunning {
                    detail: match config.ssh_enabled {
                        true => format!(
                            "HTTP {} and SSH {} both refused connections; no writers to gate",
                            config.bind, config.ssh_bind
                        ),
                        false => format!(
                            "HTTP {} refused connections; no writers to gate",
                            config.bind
                        ),
                    },
                },
                None,
            ))
        }
        Listener::Open => {
            let token = control_token(config)?;
            let status = request_status(config.bind, &token).await.map_err(|error| {
                anyhow::anyhow!(
                    "the server at {} is running, but the online snapshot gate could not read \
                     its maintenance state ({error}); stop it or use the fence path",
                    config.bind
                )
            })?;
            let started_here = match status.phase {
                Phase::Active => {
                    let started = match request_start(config.bind, &token).await {
                        Ok(started) => started,
                        Err(error) => {
                            // It may have started before the response was lost.
                            return Err(release_failed_gate(config, &token, error)
                                .await
                                .context("the online snapshot gate could not be started"));
                        }
                    };
                    if started.phase == Phase::Active {
                        return Err(release_failed_gate(
                            config,
                            &token,
                            anyhow::anyhow!(
                                "the server at {} did not enter the snapshot gate",
                                config.bind
                            ),
                        )
                        .await);
                    }
                    true
                }
                Phase::Draining | Phase::Drained => false,
            };
            match wait_gate_drained(config, &token).await {
                Ok(()) => Ok((
                    Fence::OnlineSnapshot {
                        detail: format!(
                            "server at {} drained with 0 writes in flight and was gated only for \
                             the recorded position and the copy-on-write snapshot (HTTP control \
                             endpoint)",
                            config.bind
                        ),
                    },
                    Some(OnlineGate {
                        address: reachable_address(config.bind),
                        token,
                        started_here,
                    }),
                )),
                Err(error) => {
                    if !started_here {
                        // The operator's own maintenance window is not ours to end.
                        return Err(error);
                    }
                    Err(release_failed_gate(config, &token, error).await)
                }
            }
        }
        Listener::Unknown(reason) => anyhow::bail!(
            "cannot prove the server at {} is stopped or gate it: {reason}",
            config.bind
        ),
    }
}

/// Stops a gate this command started after it could not be used, and reports
/// whether it was actually released. A failure here is stated as a failure,
/// with the command that recovers it: the server must never be left fenced
/// because a backup failed, and an operator must not be told it was released
/// when it was not.
async fn release_failed_gate(
    config: &crate::config::Config,
    token: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match request_stop(config.bind, token).await {
        Ok(_) => error.context(format!(
            "the online snapshot gate was released; the server at {} accepts writes again",
            config.bind
        )),
        Err(stop) => anyhow::anyhow!(
            "{error:#}\n\nthe online snapshot gate could not be released ({stop}); the server at \
             {} may still be refusing writes. Run `githearth-server maintenance stop`",
            config.bind
        ),
    }
}

/// Waits for the gate started by [`begin_online`] to report `drained`, bounded
/// by `maintenance_drain_timeout_seconds`.
async fn wait_gate_drained(config: &crate::config::Config, token: &str) -> anyhow::Result<()> {
    let deadline =
        std::time::Instant::now() + Duration::from_secs(config.maintenance_drain_timeout_seconds);
    loop {
        let status = request_status(config.bind, token).await?;
        match status.phase {
            Phase::Drained => return Ok(()),
            Phase::Draining => {}
            Phase::Active => anyhow::bail!(
                "the server at {} left the snapshot gate before draining; refusing to take an \
                 online snapshot",
                config.bind
            ),
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the online snapshot gate did not drain within {}s: {} write(s) still in flight at \
                 {}; increase maintenance_drain_timeout_seconds or stop the server",
                config.maintenance_drain_timeout_seconds,
                status.in_flight,
                config.bind
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn control_token(config: &crate::config::Config) -> anyhow::Result<String> {
    let path = control_token_path(config);
    let token = fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "cannot read the maintenance control token {}: {error}; the server may not have \
             started on this storage root",
            path.display()
        )
    })?;
    Ok(token.trim().to_owned())
}

pub fn control_token_path(config: &crate::config::Config) -> PathBuf {
    config.storage_root.join("run").join(CONTROL_TOKEN_FILE)
}

/// `GET /api/v1/maintenance/status` over the same listener the server uses.
pub async fn request_status(address: SocketAddr, token: &str) -> anyhow::Result<Status> {
    request(address, token, "GET", STATUS_PATH).await
}

pub async fn request_start(address: SocketAddr, token: &str) -> anyhow::Result<Status> {
    request(address, token, "POST", START_PATH).await
}

pub async fn request_stop(address: SocketAddr, token: &str) -> anyhow::Result<Status> {
    request(address, token, "POST", STOP_PATH).await
}

async fn request(
    address: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
) -> anyhow::Result<Status> {
    let address = reachable_address(address);
    let token = token.to_owned();
    let method = method.to_owned();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || request_blocking(address, &token, &method, &path))
        .await
        .map_err(|error| anyhow::anyhow!("maintenance request task failed: {error}"))?
}

fn request_blocking(
    address: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
) -> anyhow::Result<Status> {
    // outbound-exempt: on-host loopback control channel; the address is the local state file's, rewritten to loopback
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| anyhow::anyhow!("cannot connect to {address}: {error}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed response from {address}"))?;
    let status_line = head.lines().next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("malformed status line from {address}"))?;
    if code != 200 {
        anyhow::bail!("{address} answered {code}");
    }
    serde_json::from_str::<Status>(body)
        .map_err(|error| anyhow::anyhow!("invalid maintenance status from {address}: {error}"))
}

/// Writes a fresh control token with mode 0600.  It is regenerated at every
/// start, so an old token cannot control a new process.
pub fn write_control_token(path: &Path) -> anyhow::Result<String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let token = hex::encode(bytes);
    let temporary = path.with_extension("token.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, path)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maintenance() -> (Maintenance, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let maintenance = Maintenance::new(dir.path().join(STATE_FILE));
        (maintenance, dir)
    }

    #[test]
    fn a_fence_refuses_new_writes_and_drains_only_after_the_last_permit() {
        let (maintenance, _dir) = maintenance();
        let permit = maintenance.admit_write().expect("active admits writes");
        assert_eq!(maintenance.status().phase, Phase::Active);
        let status = maintenance.start();
        assert_eq!(status.phase, Phase::Draining);
        assert_eq!(status.in_flight, 1);
        assert!(
            maintenance.admit_write().is_none(),
            "a new write is refused while fenced"
        );
        drop(permit);
        assert_eq!(maintenance.status().phase, Phase::Drained);
        maintenance.stop();
        assert_eq!(maintenance.status().phase, Phase::Active);
        assert!(maintenance.admit_write().is_some());
    }

    #[tokio::test]
    async fn wait_drained_returns_only_when_in_flight_is_zero() {
        let (maintenance, _dir) = maintenance();
        let permit = maintenance.admit_write().unwrap();
        maintenance.start();
        assert!(
            maintenance
                .wait_drained(Duration::from_millis(20))
                .await
                .is_none(),
            "a held permit must keep the fence draining"
        );
        drop(permit);
        let status = maintenance
            .wait_drained(Duration::from_secs(1))
            .await
            .expect("drained after the permit drops");
        assert_eq!(status.in_flight, 0);
    }

    #[test]
    fn the_state_file_is_observable_and_says_drained_only_at_zero() {
        let (maintenance, dir) = maintenance();
        let permit = maintenance.admit_write().unwrap();
        maintenance.start();
        let text = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        let observed: Status = serde_json::from_str(&text).unwrap();
        assert_eq!(observed.phase, Phase::Draining);
        assert_eq!(observed.in_flight, 1);
        drop(permit);
        let text = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        let observed: Status = serde_json::from_str(&text).unwrap();
        assert_eq!(observed.phase, Phase::Drained);
        assert_eq!(observed.in_flight, 0);
    }

    #[test]
    fn a_token_comparison_is_length_checked_and_constant_time() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcdef"));
    }
}
