//! Git Smart HTTP transport: clone/fetch via git-upload-pack and push via
//! git-receive-pack (ADR 0003 push transaction: admission, per-repository
//! serialization, journal replay, hooks, bounded streaming).
use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AppState, config::Config, git_admission, pushes};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(_config: &Config) -> Router<AppState> {
    Router::new()
        .route("/{org}/{repo}/info/refs", axum::routing::get(info_refs))
        .route(
            "/{org}/{repo}/git-upload-pack",
            axum::routing::post(upload_pack),
        )
        .route(
            "/{org}/{repo}/git-receive-pack",
            axum::routing::post(receive_pack),
        )
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn strip_dot_git(raw: &str) -> Option<&str> {
    raw.strip_suffix(".git").filter(|s| !s.is_empty())
}

#[derive(serde::Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

// ---------------------------------------------------------------------------
// GET /{org}/{repo}/info/refs
// ---------------------------------------------------------------------------

async fn info_refs(
    State(state): State<AppState>,
    Path((org, repo)): Path<(String, String)>,
    Query(query): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Result<Response, GitHttpError> {
    let repo_name = strip_dot_git(&repo).ok_or(GitHttpError::NotFound)?;

    let service = query.service.as_deref().unwrap_or("");
    let (want, svc, content_type) = match service {
        "git-upload-pack" => (
            git_admission::Want::Fetch,
            "git-upload-pack",
            "application/x-git-upload-pack-advertisement",
        ),
        "git-receive-pack" => (
            git_admission::Want::Push,
            "git-receive-pack",
            "application/x-git-receive-pack-advertisement",
        ),
        _ => {
            return Err(GitHttpError::BadRequest(
                "dumb HTTP protocol is not supported",
            ));
        }
    };

    let admitted = git_admission::admit(&state, &headers, &org, repo_name, want)
        .await
        .map_err(|e| GitHttpError::Auth(e.0, e.1))?;

    let permit = acquire_git_permit(&state).await?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(state.config.git_timeout_seconds);

    let git_protocol = extract_git_protocol(&headers);

    let cmd = if want == git_admission::Want::Fetch {
        state
            .storage
            .upload_pack(admitted.repository.id, true, git_protocol.as_deref())
    } else {
        let hooks_dir = state.config.storage_root.join("hooks");
        state.storage.receive_pack(
            admitted.repository.id,
            true,
            git_protocol.as_deref(),
            &hooks_dir,
        )
    }
    .map_err(|e| {
        tracing::error!(
            category = "git",
            repository_id = %admitted.repository.id,
            "storage error: {e}"
        );
        GitHttpError::ServerError
    })?;

    let mut child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            tracing::error!(
                category = "git",
                repository_id = %admitted.repository.id,
                "cannot spawn git: {e}"
            );
            GitHttpError::ServerError
        })?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let repo_id = admitted.repository.id;

    let use_v2 = is_protocol_v2(git_protocol.as_deref());
    let prefix: Vec<u8> = if use_v2 {
        Vec::new()
    } else {
        let svc_line = format!("# service={svc}\n");
        let len = svc_line.len() + 4;
        let mut buf = Vec::with_capacity(len + 4);
        buf.extend_from_slice(format!("{len:04x}").as_bytes());
        buf.extend_from_slice(svc_line.as_bytes());
        buf.extend_from_slice(b"0000");
        buf
    };

    let token = CancellationToken::new();
    let body = Body::from_stream(stream_git_output(
        child, stdout, stderr, prefix, repo_id, deadline, permit, None, None, token, None, true,
    ));

    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert("Content-Type", content_type.parse().unwrap());
    response
        .headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());
    Ok(response)
}

// ---------------------------------------------------------------------------
// POST /{org}/{repo}/git-upload-pack
// ---------------------------------------------------------------------------

async fn upload_pack(
    State(state): State<AppState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Response, GitHttpError> {
    let repo_name = strip_dot_git(&repo).ok_or(GitHttpError::NotFound)?;

    let admitted = git_admission::admit(
        &state,
        &headers,
        &org,
        repo_name,
        git_admission::Want::Fetch,
    )
    .await
    .map_err(|e| GitHttpError::Auth(e.0, e.1))?;

    let permit = acquire_git_permit(&state).await?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(state.config.git_timeout_seconds);
    let limit = state.config.git_request_limit_bytes;

    // Content-Length check — reject before spawning git.
    let content_encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase());
    let is_identity = matches!(content_encoding.as_deref(), None | Some("identity"));
    if is_identity
        && let Some(cl) = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        && cl > limit
    {
        return Err(GitHttpError::PayloadTooLarge);
    }

    // Refuse an unknown encoding before spawning git.
    let is_gzip = matches!(content_encoding.as_deref(), Some("gzip" | "x-gzip"));
    if !is_identity && !is_gzip {
        return Err(GitHttpError::UnsupportedContentEncoding);
    }

    let git_protocol = extract_git_protocol(&headers);

    let cmd = state
        .storage
        .upload_pack(admitted.repository.id, false, git_protocol.as_deref())
        .map_err(|e| {
            tracing::error!(
                category = "git",
                repository_id = %admitted.repository.id,
                "storage error: {e}"
            );
            GitHttpError::ServerError
        })?;

    let mut child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            tracing::error!(
                category = "git",
                repository_id = %admitted.repository.id,
                "cannot spawn git: {e}"
            );
            GitHttpError::ServerError
        })?;

    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let repo_id = admitted.repository.id;

    // Kill channel: stdin feeder sends when the decoded body exceeds the limit.
    let (kill_tx, kill_rx) = oneshot::channel::<()>();

    // CancellationToken: one per request.  The supervisor cancels it after git
    // exits so the feeder stops promptly.  The response body holds a drop-guard
    // so client disconnect also cancels.
    let token = CancellationToken::new();
    let feeder_token = token.clone();

    // Spawn stdin feeder, storing its JoinHandle.
    let feeder_handle: tokio::task::JoinHandle<()> = if is_gzip {
        tokio::spawn(stream_gzip_body_to_stdin(
            repo_id,
            body,
            stdin,
            limit,
            kill_tx,
            feeder_token,
        ))
    } else {
        tokio::spawn(stream_body_to_stdin(
            repo_id,
            body,
            stdin,
            limit,
            kill_tx,
            feeder_token,
        ))
    };

    let body = Body::from_stream(stream_git_output(
        child,
        stdout,
        stderr,
        Vec::new(),
        repo_id,
        deadline,
        permit,
        Some(kill_rx),
        Some(feeder_handle),
        token,
        None,
        true,
    ));

    let mut response = Response::new(body);
    response.headers_mut().insert(
        "Content-Type",
        "application/x-git-upload-pack-result".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());
    Ok(response)
}

// ---------------------------------------------------------------------------
// POST /{org}/{repo}/git-receive-pack
// ---------------------------------------------------------------------------

async fn receive_pack(
    State(state): State<AppState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Response, GitHttpError> {
    let repo_name = strip_dot_git(&repo).ok_or(GitHttpError::NotFound)?;

    let admitted =
        git_admission::admit(&state, &headers, &org, repo_name, git_admission::Want::Push)
            .await
            .map_err(|e| GitHttpError::Auth(e.0, e.1))?;

    let permit = acquire_git_permit(&state).await?;
    // Per-repository lock BEFORE any pool connection (see reconcile.rs lock
    // ordering).  Held until the response stream finishes so the journal
    // records of one ref transaction stay adjacent.
    let repo_guard = state
        .storage
        .repository_lock(admitted.repository.id)
        .lock_owned()
        .await;
    let repo_id = admitted.repository.id;

    // Remove lock files left by a receive-pack killed mid-update (we hold the
    // per-repository lock, so nothing else can be writing refs).
    if let Err(e) = state.storage.remove_stale_locks(repo_id) {
        tracing::warn!(repository_id = %repo_id, error = %e, "cannot remove stale ref locks");
    }

    // Stage 6 pre-flight: apply any journaled-but-unapplied pushes first.
    if let Err(e) = pushes::replay(&state, repo_id).await {
        return Err(match e {
            pushes::ReplayError::Diverged { .. } => GitHttpError::Diverged,
            other => {
                tracing::error!(repository_id = %repo_id, error = %other, "pre-push journal replay failed");
                GitHttpError::ServerError
            }
        });
    }

    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(state.config.git_timeout_seconds);
    let limit = state.config.git_push_limit_bytes;

    // Content-Length check — reject before spawning git.
    let content_encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase());
    let is_identity = matches!(content_encoding.as_deref(), None | Some("identity"));
    if is_identity
        && let Some(cl) = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        && cl > limit
    {
        return Err(GitHttpError::PayloadTooLarge);
    }

    let is_gzip = matches!(content_encoding.as_deref(), Some("gzip" | "x-gzip"));
    if !is_identity && !is_gzip {
        return Err(GitHttpError::UnsupportedContentEncoding);
    }

    let git_protocol = extract_git_protocol(&headers);
    let hooks_dir = state.config.storage_root.join("hooks");

    let cmd = state
        .storage
        .receive_pack(repo_id, false, git_protocol.as_deref(), &hooks_dir)
        .map_err(|e| {
            tracing::error!(
                category = "git",
                repository_id = %repo_id,
                "storage error: {e}"
            );
            GitHttpError::ServerError
        })?;

    let hook_token = new_hook_token();
    let operation = Uuid::new_v4();
    state
        .hook_tokens
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(hook_token.clone(), admitted.clone());
    // RAII: a cancelled request or an aborted supervisor still releases the
    // one-use token.
    let token_guard = HookTokenGuard::new(state.hook_tokens.clone(), hook_token.clone());

    let mut hook_cmd = tokio::process::Command::from(cmd);
    hook_cmd
        .env("GITHEARTH_HOOK_SOCKET", &state.hook_socket_path)
        .env("GITHEARTH_HOOK_TOKEN", &hook_token)
        .env("GITHEARTH_OPERATION", operation.to_string())
        .env("GITHEARTH_ACTOR", admitted.account.to_string());
    // coverage: let the hook subprocess (the same instrumented binary) record
    // its own profile under the test harness.
    if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
        hook_cmd.env("LLVM_PROFILE_FILE", profile);
    }
    // Once the pack is fully fed, a client that went away must not kill
    // receive-pack mid-ref-update (that would leave stale lock files); the
    // deadline still bounds it.  So: no kill on drop, no kill on disconnect.
    let mut child = match hook_cmd
        .kill_on_drop(false)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            drop(token_guard);
            tracing::error!(
                category = "git",
                repository_id = %repo_id,
                "cannot spawn git: {e}"
            );
            return Err(GitHttpError::ServerError);
        }
    };

    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Kill channel: stdin feeder sends when the decoded body exceeds the limit.
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let token = CancellationToken::new();
    let feeder_token = token.clone();

    let feeder_handle: tokio::task::JoinHandle<()> = if is_gzip {
        tokio::spawn(stream_gzip_body_to_stdin(
            repo_id,
            body,
            stdin,
            limit,
            kill_tx,
            feeder_token,
        ))
    } else {
        tokio::spawn(stream_body_to_stdin(
            repo_id,
            body,
            stdin,
            limit,
            kill_tx,
            feeder_token,
        ))
    };

    // Stage 7: after git exits, apply the committed push to metadata.  The
    // repository lock and the one-use hook token are released by their guards
    // when this future drops (RAII), so a client that went away cannot skip
    // the replay or leak the token.
    let after_exit: futures_util::future::BoxFuture<'static, ()> = {
        let state = state.clone();
        Box::pin(async move {
            // Guards live inside the future: they drop when it completes or is
            // itself dropped, never before the hook has had a chance to run.
            let _guard = repo_guard;
            let _token_guard = token_guard;
            if let Err(e) = pushes::replay(&state, repo_id).await {
                // A metadata failure after git succeeded never changes the
                // client's result; the reconciler applies it later.
                tracing::warn!(
                    repository_id = %repo_id,
                    error = %e,
                    "post-push journal replay failed; reconciler will retry"
                );
            }
        })
    };

    let body = Body::from_stream(stream_git_output(
        child,
        stdout,
        stderr,
        Vec::new(),
        repo_id,
        deadline,
        permit,
        Some(kill_rx),
        Some(feeder_handle),
        token,
        Some(after_exit),
        false,
    ));

    let mut response = Response::new(body);
    response.headers_mut().insert(
        "Content-Type",
        "application/x-git-receive-pack-result".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());
    Ok(response)
}

// ---------------------------------------------------------------------------
// Hook admission socket (one per AppState)
// ---------------------------------------------------------------------------

const HOOK_REQUEST_LIMIT: usize = 1024 * 1024;
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

fn new_hook_token() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    hex::encode(bytes)
}

/// Removes a one-use hook token when dropped, so a cancelled request or an
/// aborted supervisor cannot leak the admitted principal.
struct HookTokenGuard {
    tokens: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, git_admission::Admitted>>,
    >,
    token: String,
}

impl HookTokenGuard {
    fn new(
        tokens: std::sync::Arc<
            std::sync::Mutex<std::collections::HashMap<String, git_admission::Admitted>>,
        >,
        token: String,
    ) -> Self {
        Self { tokens, token }
    }
}

impl Drop for HookTokenGuard {
    fn drop(&mut self) {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.token);
    }
}

pub(crate) async fn hook_accept_loop(listener: tokio::net::UnixListener, state: AppState) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(handle_hook_connection(stream, state));
            }
            Err(e) => {
                tracing::warn!(error = %e, "hook admission socket accept failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_hook_connection(mut stream: tokio::net::UnixStream, state: AppState) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let decision = tokio::time::timeout(HOOK_TIMEOUT, async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > HOOK_REQUEST_LIMIT {
                        return Err("request too large".to_owned());
                    }
                }
                Err(_) => return Err("cannot read the hook request".to_owned()),
            }
        }

        let text = String::from_utf8_lossy(&buf);
        let (token, rest) = text.split_once('\n').unwrap_or((text.as_ref(), ""));
        // One use: removed here even if the hook never runs again.
        let admitted = state
            .hook_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(token.trim_end())
            .ok_or_else(|| "invalid or expired hook token".to_owned())?;

        let updates = parse_hook_updates(rest)?;

        let fresh = git_admission::reload(&state, &admitted)
            .await
            .map_err(|e| match e.0 {
                StatusCode::NOT_FOUND => "repository is no longer available".to_owned(),
                _ => "authentication unavailable".to_owned(),
            })?;
        for (update, fast_forward) in &updates {
            if let Err(reason) = git_admission::decide_ref(&state, &fresh, update, *fast_forward) {
                return Err(reason.to_owned());
            }
        }
        Ok(())
    })
    .await;

    let response = match decision {
        Ok(Ok(())) => "OK\n".to_owned(),
        Ok(Err(reason)) => format!("DENIED {reason}\n"),
        Err(_) => "ERROR hook service timed out\n".to_owned(),
    };
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Parse the ref-update lines after the leading token line.
fn parse_hook_updates(
    rest: &str,
) -> Result<Vec<(githearth_git::journal::Update, bool)>, &'static str> {
    let mut updates = Vec::new();
    for line in rest.lines() {
        let mut parts = line.split(' ');
        let (Some(old), Some(new), Some(reference), Some(ff)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("malformed ref update");
        };
        if parts.next().is_some() {
            return Err("malformed ref update");
        }
        updates.push((
            githearth_git::journal::Update {
                old: old.to_owned(),
                new: new.to_owned(),
                reference: reference.to_owned(),
            },
            ff == "1",
        ));
    }
    if updates.is_empty() {
        return Err("no ref updates in the hook request");
    }
    Ok(updates)
}

// ---------------------------------------------------------------------------
// Concurrency permit
// ---------------------------------------------------------------------------

async fn acquire_git_permit(state: &AppState) -> Result<OwnedSemaphorePermit, GitHttpError> {
    state
        .git_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| GitHttpError::ConcurrencyExhausted)
}

// ---------------------------------------------------------------------------
// Git-Protocol header
// ---------------------------------------------------------------------------

fn extract_git_protocol(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("git-protocol")?.to_str().ok()?;
    githearth_git::layout::validate_git_protocol(value).ok()?;
    Some(value.to_owned())
}

fn is_protocol_v2(protocol: Option<&str>) -> bool {
    protocol.is_some_and(|p| p.split(':').any(|part| part == "version=2"))
}

// ---------------------------------------------------------------------------
// Stdin feeders — cancellable via CancellationToken
// ---------------------------------------------------------------------------

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

async fn stream_body_to_stdin(
    repo_id: Uuid,
    body: axum::body::Body,
    mut stdin: tokio::process::ChildStdin,
    limit: usize,
    kill_tx: oneshot::Sender<()>,
    token: CancellationToken,
) {
    let mut stream = body.into_data_stream();
    let mut total: usize = 0;
    loop {
        tokio::select! {
            chunk = stream.next() => {
                let Some(chunk) = chunk else { break };
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(_) => break,
                };
                total += chunk.len();
                if total > limit {
                    tracing::warn!(
                        repository_id = %repo_id, total, limit,
                        "git request body exceeds limit; signalling kill"
                    );
                    let _ = kill_tx.send(());
                    return;
                }
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            _ = token.cancelled() => {
                return;
            }
        }
    }
    drop(stdin);
}

async fn stream_gzip_body_to_stdin(
    repo_id: Uuid,
    body: axum::body::Body,
    mut stdin: tokio::process::ChildStdin,
    limit: usize,
    kill_tx: oneshot::Sender<()>,
    token: CancellationToken,
) {
    use async_compression::tokio::bufread::GzipDecoder;
    use tokio::io::{AsyncReadExt, BufReader};
    use tokio_util::io::StreamReader;

    let body_stream = body
        .into_data_stream()
        .map(|r| r.map_err(std::io::Error::other));
    let reader = StreamReader::new(body_stream);
    let mut decoder = GzipDecoder::new(BufReader::new(reader));
    let mut total: usize = 0;
    let mut buf = [0u8; 8192];
    loop {
        tokio::select! {
            result = decoder.read(&mut buf) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        if total > limit {
                            tracing::warn!(
                                repository_id = %repo_id, total, limit,
                                "git decoded request body exceeds limit; signalling kill"
                            );
                            let _ = kill_tx.send(());
                            return;
                        }
                        if stdin.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(repository_id = %repo_id, "gzip decode failed: {e}; signalling kill");
                        let _ = kill_tx.send(());
                        return;
                    }
                }
            }
            _ = token.cancelled() => {
                return;
            }
        }
    }
    drop(stdin);
}

// ---------------------------------------------------------------------------
// Git output stream — supervisor + deadline-aware response body
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn stream_git_output(
    mut child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    prefix: Vec<u8>,
    repo_id: Uuid,
    deadline: tokio::time::Instant,
    permit: OwnedSemaphorePermit,
    kill_rx: Option<oneshot::Receiver<()>>,
    feeder_handle: Option<tokio::task::JoinHandle<()>>,
    token: CancellationToken,
    after_exit: Option<futures_util::future::BoxFuture<'static, ()>>,
    kill_on_disconnect: bool,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::stream;
    use tokio_util::io::ReaderStream;

    // Supervisor task: drains stderr concurrently, waits for child exit /
    // deadline / kill signal / cancel, then stops the feeder and drops permit.
    let supervisor_token = token.clone();
    tokio::spawn(async move {
        // Drain stderr concurrently (capped at 64 KiB).
        let stderr_task = tokio::spawn(drain_stderr(stderr));

        // A future that resolves only on an explicit kill request.
        let kill_requested = async {
            match kill_rx {
                Some(rx) => {
                    if rx.await.is_ok() {
                        return;
                    }
                    std::future::pending::<()>().await;
                }
                None => {
                    std::future::pending::<()>().await;
                }
            }
        };

        let status = tokio::select! {
            s = child.wait() => s,
            _ = tokio::time::sleep_until(deadline) => {
                tracing::warn!(repository_id = %repo_id, "git deadline exceeded; killing");
                let _ = child.start_kill();
                child.wait().await
            }
            _ = kill_requested => {
                let _ = child.start_kill();
                child.wait().await
            }
            _ = supervisor_token.cancelled(), if kill_on_disconnect => {
                // Client disconnected (response body dropped its guard).
                let _ = child.start_kill();
                child.wait().await
            }
        };

        // After child exits (for any reason), cancel the token so the feeder
        // stops promptly.
        supervisor_token.cancel();

        // Stop the feeder: abort its handle and await it.
        if let Some(handle) = feeder_handle {
            handle.abort();
            let _ = handle.await;
        }

        // Join stderr with a short bound.
        let stderr_bytes = tokio::time::timeout(Duration::from_secs(5), stderr_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        if let Ok(s) = status
            && !s.success()
        {
            let stderr_str = String::from_utf8_lossy(&stderr_bytes);
            tracing::warn!(
                repository_id = %repo_id,
                exit_code = s.code(),
                stderr = %stderr_str.trim(),
                "git exited non-zero"
            );
        }

        // Post-push metadata replay (stage 7), still holding the repository
        // lock and git permit.
        if let Some(after_exit) = after_exit {
            after_exit.await;
        }

        drop(permit);
    });

    // Build the full output stream first (prefix + stdout if needed), then
    // wrap it in DeadlineStream so the prefix is also governed by the deadline.
    let stdout_stream = ReaderStream::new(stdout);
    let full_stream: Box<
        dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin + Send,
    > = if prefix.is_empty() {
        Box::new(stdout_stream)
    } else {
        use futures_util::{future, stream};
        let prefix_item = stream::once(future::ready(Ok(bytes::Bytes::from(prefix))));
        Box::new(prefix_item.chain(stdout_stream))
    };

    let guard = kill_on_disconnect.then(|| token.clone().drop_guard());
    let sleep = Box::pin(tokio::time::sleep_until(deadline));
    let deadline_stream = DeadlineStream {
        inner: full_stream,
        sleep,
        token,
        guard,
        finished: false,
    };

    stream::StreamExt::boxed(deadline_stream)
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut reader = tokio::io::BufReader::new(stderr);
    let mut buf = Vec::new();
    let mut cap = [0u8; 4096];
    loop {
        match reader.read(&mut cap).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < 65536 {
                    let remaining: usize = 65536usize.saturating_sub(buf.len());
                    buf.extend_from_slice(&cap[..n.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

// ---------------------------------------------------------------------------
// DeadlineStream — wraps a stdout stream, enforcing the deadline on every poll.
// Holds a CancellationToken drop-guard so client disconnect cancels the token.
// ---------------------------------------------------------------------------

struct DeadlineStream<S> {
    inner: S,
    /// A sleep that fires at the deadline.  Polled first on every poll so the
    /// deadline wakes a pending inner read.
    sleep: Pin<Box<tokio::time::Sleep>>,
    token: CancellationToken,
    /// Disarmed when the stream ends normally (EOF).  If the stream is dropped
    /// before reaching EOF (client disconnect, deadline error), the guard
    /// cancels the token so the supervisor kills git.
    guard: Option<tokio_util::sync::DropGuard>,
    /// Set after the deadline error so the stream ends instead of repeating it.
    finished: bool,
}

impl<S, E> futures_util::Stream for DeadlineStream<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        // Poll the deadline sleep first.  If it fires, end the stream with
        // TimedOut — even if the inner stream still has data pending.
        match self.sleep.as_mut().poll(cx) {
            Poll::Ready(()) => {
                self.finished = true;
                self.token.cancel();
                self.guard.take();
                return Poll::Ready(Some(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "git deadline exceeded",
                ))));
            }
            Poll::Pending => {}
        }
        // Deadline hasn't fired yet — poll the inner stream.
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(std::io::Error::other(e)))),
            Poll::Ready(None) => {
                // Normal EOF — disarm guard so token is NOT cancelled.
                if let Some(g) = self.guard.take() {
                    g.disarm();
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: Unpin> Unpin for DeadlineStream<S> {}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

enum GitHttpError {
    NotFound,
    BadRequest(&'static str),
    Auth(StatusCode, &'static str),
    ConcurrencyExhausted,
    PayloadTooLarge,
    ServerError,
    UnsupportedContentEncoding,
    Diverged,
}

impl IntoResponse for GitHttpError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            Self::Auth(status, msg) => git_admission::AuthError(status, msg).into_response(),
            Self::ConcurrencyExhausted => {
                let mut response =
                    (StatusCode::SERVICE_UNAVAILABLE, "git requests busy").into_response();
                response
                    .headers_mut()
                    .insert("Retry-After", "5".parse().unwrap());
                response
            }
            Self::PayloadTooLarge => {
                (StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit").into_response()
            }
            Self::ServerError => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
            Self::UnsupportedContentEncoding => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported Content-Encoding",
            )
                .into_response(),
            Self::Diverged => (StatusCode::CONFLICT, "repository needs repair").into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn hook_request_parsing_accepts_and_rejects() {
        let ok = parse_hook_updates(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/main 1\n",
        )
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert!(ok[0].1, "ff flag parsed as true");

        let no_ff = parse_hook_updates(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/main 0\n",
        )
        .unwrap();
        assert!(!no_ff[0].1);

        assert_eq!(
            parse_hook_updates("short line\n").unwrap_err(),
            "malformed ref update"
        );
        assert_eq!(
            parse_hook_updates("a a refs/heads/main 1 extra\n").unwrap_err(),
            "malformed ref update"
        );
        assert_eq!(
            parse_hook_updates("").unwrap_err(),
            "no ref updates in the hook request"
        );
    }

    #[test]
    fn protocol_v2_detection_by_component() {
        assert!(is_protocol_v2(Some("version=2")));
        assert!(is_protocol_v2(Some("version=1:version=2")));
        assert!(is_protocol_v2(Some("version=2:version=1")));
        assert!(!is_protocol_v2(Some("version=1")));
        assert!(!is_protocol_v2(Some("version=0")));
        assert!(!is_protocol_v2(None));
    }

    #[tokio::test]
    async fn git_permit_exhausted_when_semaphore_full() {
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let _held = sem.clone().try_acquire_owned().unwrap();
        assert!(sem.clone().try_acquire_owned().is_err());
        drop(_held);
        assert!(sem.clone().try_acquire_owned().is_ok());
    }

    /// DeadlineStream over a never-yielding inner stream: the sleep must fire
    /// and produce a TimedOut error, then None on the next poll.
    #[tokio::test]
    async fn deadline_stream_timed_out() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let token = CancellationToken::new();
        let guard = token.clone().drop_guard();
        let inner: Box<
            dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin + Send,
        > = Box::new(futures_util::stream::pending());
        let sleep = Box::pin(tokio::time::sleep_until(deadline));
        let mut stream = DeadlineStream {
            inner,
            sleep,
            token,
            guard: Some(guard),
            finished: false,
        };

        let start = tokio::time::Instant::now();

        // First poll must yield TimedOut around 200 ms.
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("stream hung")
            .expect("stream ended without item");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed <= Duration::from_secs(1),
            "TimedOut arrived at {:.0} ms, expected ~200 ms",
            elapsed.as_millis()
        );
        assert_eq!(item.unwrap_err().kind(), std::io::ErrorKind::TimedOut);

        // Next poll must return None.
        let next = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .unwrap_or(None);
        assert!(next.is_none(), "second poll must return None, got {next:?}");
    }

    /// When the deadline is already in the past, the sleep fires immediately and
    /// the inner stream's ready item is never yielded.
    #[tokio::test]
    async fn deadline_stream_past_deadline_skips_inner() {
        let deadline = tokio::time::Instant::now() - Duration::from_secs(1);
        let token = CancellationToken::new();
        let guard = token.clone().drop_guard();
        let inner: Box<
            dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin + Send,
        > = Box::new(futures_util::stream::once(futures_util::future::ready(Ok(
            bytes::Bytes::from("should not appear"),
        ))));
        let sleep = Box::pin(tokio::time::sleep_until(deadline));
        let mut stream = DeadlineStream {
            inner,
            sleep,
            token,
            guard: Some(guard),
            finished: false,
        };

        let item = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("stream hung")
            .expect("stream ended without item");
        assert_eq!(item.unwrap_err().kind(), std::io::ErrorKind::TimedOut);

        let next = stream.next().await;
        assert!(next.is_none(), "second poll must return None, got {next:?}");
    }
}
