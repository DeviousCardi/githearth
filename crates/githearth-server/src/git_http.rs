//! Git Smart HTTP transport: clone/fetch via git-upload-pack.
//! Push (receive-pack) is explicitly refused until a later task.
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

use crate::{AppState, config::Config, git_admission};

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
            axum::routing::post(receive_pack_denied),
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
    match service {
        "git-upload-pack" => {}
        "git-receive-pack" => {
            return Err(GitHttpError::PushNotEnabled);
        }
        _ => {
            return Err(GitHttpError::BadRequest(
                "dumb HTTP protocol is not supported",
            ));
        }
    }

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

    let git_protocol = extract_git_protocol(&headers);

    let cmd = state
        .storage
        .upload_pack(admitted.repository.id, true, git_protocol.as_deref())
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
        let svc = "# service=git-upload-pack\n";
        let len = svc.len() + 4;
        let mut buf = Vec::with_capacity(len + 4);
        buf.extend_from_slice(format!("{len:04x}").as_bytes());
        buf.extend_from_slice(svc.as_bytes());
        buf.extend_from_slice(b"0000");
        buf
    };

    let token = CancellationToken::new();
    let body = Body::from_stream(stream_git_output(
        child, stdout, stderr, prefix, repo_id, deadline, permit, None, None, token,
    ));

    let mut response = Response::new(body);
    response.headers_mut().insert(
        "Content-Type",
        "application/x-git-upload-pack-advertisement"
            .parse()
            .unwrap(),
    );
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
    let feeder_handle: tokio::task::JoinHandle<()> = match content_encoding.as_deref() {
        None | Some("identity") => tokio::spawn(stream_body_to_stdin(
            repo_id,
            body,
            stdin,
            limit,
            kill_tx,
            feeder_token,
        )),
        Some("gzip") | Some("x-gzip") => tokio::spawn(stream_gzip_body_to_stdin(
            repo_id,
            body,
            stdin,
            limit,
            kill_tx,
            feeder_token,
        )),
        Some(_) => {
            child.kill().await.ok();
            return Err(GitHttpError::UnsupportedContentEncoding);
        }
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
// POST /{org}/{repo}/git-receive-pack  (explicitly denied)
// ---------------------------------------------------------------------------

async fn receive_pack_denied() -> Result<Response, GitHttpError> {
    Err(GitHttpError::PushNotEnabled)
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
            _ = supervisor_token.cancelled() => {
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

        drop(permit);
    });

    // Build the response body stream from stdout, wrapped with a deadline
    // check and a drop-guard so client disconnect cancels the token.
    let stdout_stream = ReaderStream::new(stdout);
    let guard = token.clone().drop_guard();
    let deadline_stream = DeadlineStream {
        inner: stdout_stream,
        deadline,
        token,
        guard: Some(guard),
        finished: false,
    };

    if prefix.is_empty() {
        stream::StreamExt::boxed(deadline_stream)
    } else {
        let prefix_item = stream::once(async move { Ok(bytes::Bytes::from(prefix)) });
        stream::StreamExt::boxed(prefix_item.chain(deadline_stream))
    }
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
    deadline: tokio::time::Instant,
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
        if tokio::time::Instant::now() >= self.deadline {
            self.finished = true;
            self.token.cancel();
            self.guard.take();
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "git deadline exceeded",
            ))));
        }
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
    PushNotEnabled,
    Auth(StatusCode, &'static str),
    ConcurrencyExhausted,
    PayloadTooLarge,
    ServerError,
    UnsupportedContentEncoding,
}

impl IntoResponse for GitHttpError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            Self::PushNotEnabled => {
                (StatusCode::FORBIDDEN, "push is not enabled yet").into_response()
            }
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
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
