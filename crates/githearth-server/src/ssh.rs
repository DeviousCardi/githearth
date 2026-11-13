//! Embedded SSH Git transport (ADR 0005). Runs an SSH server inside
//! githearth-server, reusing the same admission, limits, and hook token flow
//! as the HTTP transport.
use crate::{
    AppState,
    git_admission::{self, Want},
    pushes,
};
use anyhow::Context;
use futures_util::future::BoxFuture;
use githearth_git::layout;
use std::{
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

// ------------------------------------------------------------------
// Types (ADR 0005 boundary between Person A and Person B)
// ------------------------------------------------------------------

#[derive(Clone)]
pub struct SshIdentity {
    pub account: Uuid,
    pub key_id: Uuid,
    pub read_only: bool,
}

pub type Authenticate = Arc<
    dyn Fn(String) -> BoxFuture<'static, Result<Option<SshIdentity>, sqlx::Error>> + Send + Sync,
>;

// ------------------------------------------------------------------
// Host key generation
// ------------------------------------------------------------------

pub fn generate_host_key(path: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let key = russh::keys::PrivateKey::random(
        &mut russh::keys::key::safe_rng(),
        russh::keys::Algorithm::Ed25519,
    )
    .context("cannot generate Ed25519 host key")?;
    let mut pem = Vec::new();
    key.write_openssh(&mut pem, russh::keys::ssh_key::LineEnding::LF)
        .context("cannot serialize host key")?;
    // create_new makes "never overwrite" atomic; the mode applies at creation,
    // so the private key is never readable by others, even briefly.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Idempotent: the Compose one-shot service runs this on every
            // `docker compose up`, and a persisted volume already has a key.
            tracing::info!(path = %path.display(), "SSH host key already exists; keeping it");
            return Ok(());
        }
        other => other.context("cannot write host key")?,
    };
    file.write_all(&pem).context("cannot write host key")?;
    tracing::info!(path = %path.display(), "SSH host key generated");
    Ok(())
}

// ------------------------------------------------------------------
// Command parsing
// ------------------------------------------------------------------

pub fn parse_command(cmd: &str) -> Option<(Want, String, String)> {
    let (want, rest) = if let Some(rest) = cmd.strip_prefix("git-upload-pack ") {
        (Want::Fetch, rest)
    } else {
        let rest = cmd.strip_prefix("git-receive-pack ")?;
        (Want::Push, rest)
    };
    let rest = rest.trim();
    if !rest.starts_with('\'') || !rest.ends_with('\'') {
        return None;
    }
    let inner = &rest[1..rest.len() - 1];
    let inner = inner.strip_prefix('/').unwrap_or(inner);
    let repo_name = crate::git_http::strip_dot_git(inner)?;
    let (org, repo) = repo_name.split_once('/')?;
    if org.is_empty() || repo.is_empty() || org.contains('/') || repo.contains('/') {
        return None;
    }
    Some((want, org.to_owned(), repo.to_owned()))
}

// ------------------------------------------------------------------
// Shared state for each connection
// ------------------------------------------------------------------

struct ConnectionState {
    identity: Mutex<Option<SshIdentity>>,
    git_protocol: Mutex<Option<String>>,
    unauthenticated_permit: Mutex<Option<tokio::sync::OwnedSemaphorePermit>>,
    /// Direct handle to git's stdin, used from the data() callback.
    git_stdin: Mutex<Option<tokio::process::ChildStdin>>,
    /// Git uses one session channel and one exec per connection; a second
    /// would share git_stdin and could block the session on the repository lock.
    session_opened: std::sync::atomic::AtomicBool,
    exec_started: std::sync::atomic::AtomicBool,
}

// ------------------------------------------------------------------
// SSH server handler
// ------------------------------------------------------------------

#[derive(Clone)]
struct SshHandler {
    state: AppState,
    authenticate: Authenticate,
    conn: Arc<ConnectionState>,
}

impl russh::server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<russh::server::Auth, Self::Error> {
        Ok(russh::server::Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        Ok(russh::server::Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        _offer: russh::server::PublicKeyOffer<'_>,
    ) -> Result<russh::server::Auth, Self::Error> {
        Ok(russh::server::Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<russh::server::Auth, Self::Error> {
        let fp = key.public_key.fingerprint(russh::keys::HashAlg::Sha256);
        let result = (self.authenticate)(fp.to_string()).await?;
        match result {
            Some(identity) => {
                *self.conn.identity.lock().unwrap() = Some(identity);
                Ok(russh::server::Auth::Accept)
            }
            None => Ok(russh::server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }),
        }
    }

    async fn auth_succeeded(
        &mut self,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let identity = self.conn.identity.lock().unwrap().clone();
        if let Some(id) = &identity {
            tracing::info!(account_id = %id.account, key_id = %id.key_id, "ssh authentication succeeded");
        }
        drop(self.conn.unauthenticated_permit.lock().unwrap().take());
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.conn.session_opened.swap(true, SeqCst) {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        reply.accept().await;
        // I/O goes through data() and the Handle; drop the Channel, or russh queues
        // every data packet into its bounded buffer and the session stalls at ~100.
        drop(channel);
        Ok(())
    }

    fn adjust_window(&mut self, _channel: russh::ChannelId, current: u32) -> u32 {
        current + 1048576
    }

    async fn env_request(
        &mut self,
        channel: russh::ChannelId,
        name: &str,
        value: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" && layout::validate_git_protocol(value).is_ok() {
            *self.conn.git_protocol.lock().unwrap() = Some(value.to_owned());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        if self
            .conn
            .exec_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return refuse(session, channel, b"githearth: one command per connection\n");
        }
        let command = String::from_utf8_lossy(data).to_string();

        let identity = self.conn.identity.lock().unwrap().clone();
        let git_protocol = self.conn.git_protocol.lock().unwrap().clone();

        let identity = match identity {
            Some(id) => id,
            None => return refuse(session, channel, b"githearth: repository not found\n"),
        };

        // Parse, admit, and spawn git synchronously so that when we return,
        // git_stdin is stored and data() callbacks can feed it.
        let state = self.state.clone();
        let mut handle = session.handle();
        let conn = self.conn.clone();

        // Do admission and git spawn here, in the handler task
        let (want, org, repo) = match parse_command(&command) {
            Some(v) => v,
            None => {
                return refuse(
                    session,
                    channel,
                    "githearth: repository not found\n".as_bytes(),
                );
            }
        };

        let admitted = match git_admission::admit_account(
            &state,
            identity.account,
            identity.read_only,
            git_admission::CredentialRef::SshKey(identity.key_id),
            &org,
            &repo,
            want,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                let msg = match e.0 {
                    axum::http::StatusCode::NOT_FOUND => "githearth: repository not found\n",
                    _ => "githearth: not permitted\n",
                };
                return refuse(session, channel, msg.as_bytes());
            }
        };

        let permit = match state.git_slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                return refuse(
                    session,
                    channel,
                    "githearth: service unavailable\n".as_bytes(),
                );
            }
        };

        let repo_id = admitted.repository.id;

        let repo_guard = if want == Want::Push {
            let guard = state.storage.repository_lock(repo_id).lock_owned().await;
            if let Err(e) = state.storage.remove_stale_locks(repo_id) {
                tracing::warn!(repository_id = %repo_id, error = %e, "cannot remove stale ref locks");
            }
            if let Err(_e) = pushes::replay(&state, repo_id).await {
                return refuse(
                    session,
                    channel,
                    "githearth: repository not found\n".as_bytes(),
                );
            }
            Some(guard)
        } else {
            None
        };

        let cmd = match build_git_cmd(&state, repo_id, want, git_protocol.as_deref()) {
            Ok(c) => c,
            Err(msg) => {
                return refuse(session, channel, format!("{msg}\n").as_bytes());
            }
        };

        let hook_token = if want == Want::Push {
            let token = crate::git_http::new_hook_token();
            state
                .hook_tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(token.clone(), admitted.clone());
            Some(token)
        } else {
            None
        };
        let operation = if want == Want::Push {
            Some(Uuid::new_v4())
        } else {
            None
        };

        let token_guard = hook_token
            .clone()
            .map(|token| crate::git_http::HookTokenGuard::new(state.hook_tokens.clone(), token));

        let mut hook_cmd = tokio::process::Command::from(cmd);
        if let (Some(token), Some(op_id)) = (&hook_token, operation) {
            hook_cmd
                .env("GITHEARTH_HOOK_SOCKET", &state.hook_socket_path)
                .env("GITHEARTH_HOOK_TOKEN", token)
                .env("GITHEARTH_OPERATION", op_id.to_string())
                .env("GITHEARTH_ACTOR", admitted.account.to_string())
                .env(
                    "GITHEARTH_QUOTA_BYTES",
                    state.config.repository_quota_bytes.to_string(),
                );
        }
        if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
            hook_cmd.env("LLVM_PROFILE_FILE", profile);
        }
        if let Some(ref proto) = git_protocol {
            hook_cmd.env("GIT_PROTOCOL", proto);
        }

        let kill_on_drop = want != Want::Push;

        if let Some(op_id) = operation {
            let (kind, cred_id) = match &admitted.credential {
                crate::git_admission::CredentialRef::Password => ("password", None),
                crate::git_admission::CredentialRef::Token(id) => ("token", Some(*id)),
                crate::git_admission::CredentialRef::SshKey(id) => ("ssh_key", Some(*id)),
            };
            if let Err(e) = sqlx::query(
                "INSERT INTO push_credentials (operation_id, credential_kind, credential_id) VALUES ($1, $2, $3)",
            )
            .bind(op_id)
            .bind(kind)
            .bind(cred_id)
            .execute(&state.pool)
            .await
            {
                tracing::error!(category = "database", operation_id = %op_id, error = %e, "cannot insert push credential");
                return refuse(
                    session,
                    channel,
                    "githearth: repository not found\n".as_bytes(),
                );
            }
        }

        let mut child = match hook_cmd
            .kill_on_drop(kill_on_drop)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                // The credential was recorded before spawning; a push that
                // never starts must not leave it dangling (issue #143).
                if let Some(op_id) = operation {
                    let _ = sqlx::query("DELETE FROM push_credentials WHERE operation_id = $1")
                        .bind(op_id)
                        .execute(&state.pool)
                        .await;
                }
                tracing::error!(category = "git", repository_id = %repo_id, "cannot spawn git: {e}");
                return refuse(
                    session,
                    channel,
                    "githearth: repository not found\n".as_bytes(),
                );
            }
        };

        let stdin = child.stdin.take().expect("stdin piped");
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        // Store stdin so data() callbacks can write directly to it.
        *conn.git_stdin.lock().unwrap() = Some(stdin);

        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(state.config.git_timeout_seconds);

        // OpenSSH sends exec with want-reply and waits for it before sending stdin;
        // upload-pack speaks first, so stream git's output from now on, not after EOF.
        if let Err(e) = session.channel_success(channel) {
            // The credential was recorded before spawning; no push can start.
            if let Some(op_id) = operation {
                let _ = sqlx::query("DELETE FROM push_credentials WHERE operation_id = $1")
                    .bind(op_id)
                    .execute(&state.pool)
                    .await;
            }
            return Err(e.into());
        }

        {
            tokio::spawn(async move {
                // The hook token must outlive git, which runs the hooks.
                let _token_guard = token_guard;
                let exit_code = stream_git_io(
                    channel,
                    &mut handle,
                    &mut child,
                    &mut stdout,
                    &mut stderr,
                    deadline,
                )
                .await;

                if want == Want::Push {
                    drop(repo_guard);
                    // Only a replayed journal proves the push was accepted (and
                    // then a `repository_pushes` row exists). On a replay error
                    // the reconciler will apply it later, so keeping the
                    // credential preserves provenance; removing it here would
                    // strip the credential from a push that did land.
                    match pushes::replay(&state, repo_id).await {
                        Ok(_) => {
                            // A refused push leaves no `repository_pushes` row;
                            // remove the credential recorded before spawning so
                            // it does not overstate push activity (issue #143).
                            if let Some(op_id) = operation
                                && let Err(e) = sqlx::query(
                                    "DELETE FROM push_credentials \
                                     WHERE operation_id = $1 AND NOT EXISTS (\
                                       SELECT 1 FROM repository_pushes p WHERE p.operation_id = $1)",
                                )
                                .bind(op_id)
                                .execute(&state.pool)
                                .await
                            {
                                tracing::warn!(
                                    repository_id = %repo_id,
                                    error = %e,
                                    "cannot remove the credential of a refused push"
                                );
                            }
                        }
                        Err(e) => tracing::warn!(
                            repository_id = %repo_id,
                            error = %e,
                            "post-push journal replay failed; reconciler will retry"
                        ),
                    }
                }

                // exit-status, then EOF, then close: OpenSSH waits for the close.
                let _ = handle.exit_status_request(channel, exit_code).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
                drop(permit);
            });
        }

        Ok(())
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        data: &[u8],
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let mut taken = self.conn.git_stdin.lock().unwrap().take();
        if let Some(ref mut stdin) = taken {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(data).await;
        }
        *self.conn.git_stdin.lock().unwrap() = taken;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: russh::ChannelId,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        drop(self.conn.git_stdin.lock().unwrap().take());
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // No shell: same refusal as an unknown command (ADR 0005).
        refuse(session, channel, b"githearth: repository not found\n")
    }

    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        _name: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn x11_request(
        &mut self,
        channel: russh::ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
}

// ------------------------------------------------------------------
// Stream git I/O between child process and SSH channel
// ------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn stream_git_io(
    channel: russh::ChannelId,
    handle: &mut russh::server::Handle,
    child: &mut tokio::process::Child,
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    deadline: tokio::time::Instant,
) -> u32 {
    use tokio::io::AsyncReadExt;

    let mut stdout_buf = [0u8; 32768];
    let mut stderr_buf = [0u8; 16384];
    let mut stderr_open = true;

    loop {
        tokio::select! {
            result = stdout.read(&mut stdout_buf) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = handle.data(channel, bytes::Bytes::copy_from_slice(&stdout_buf[..n])).await;
                    }
                    Err(_) => break,
                }
            }
            result = stderr.read(&mut stderr_buf), if stderr_open => {
                match result {
                    Ok(n) if n > 0 => {
                        let _ = handle.extended_data(channel, 1, bytes::Bytes::copy_from_slice(&stderr_buf[..n])).await;
                    }
                    // Closed or failed: stop polling it, or the loop spins on Ok(0).
                    _ => stderr_open = false,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.start_kill();
                break;
            }
        }
    }

    let status = child.wait().await;
    status.ok().and_then(|s| s.code()).unwrap_or(128) as u32
}

fn build_git_cmd(
    state: &AppState,
    repo_id: Uuid,
    want: Want,
    git_protocol: Option<&str>,
) -> Result<std::process::Command, String> {
    let session = githearth_git::layout::PackMode::Session;
    let built = if want == Want::Fetch {
        state.storage.upload_pack(repo_id, session, git_protocol)
    } else {
        let hooks_dir = state.config.storage_root.join("hooks");
        state
            .storage
            .receive_pack(repo_id, session, git_protocol, &hooks_dir)
    };
    built.map_err(|e| {
        tracing::error!(category = "git", repository_id = %repo_id, "storage error: {e}");
        "githearth: repository not found".to_owned()
    })
}

// ------------------------------------------------------------------
// Spawn the SSH listener
// ------------------------------------------------------------------

/// Reads the host key; `serve` calls this before starting so a missing or
/// invalid key fails startup instead of only the SSH task (ADR 0005).
pub fn load_host_key(path: &Path) -> anyhow::Result<russh::keys::PrivateKey> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read SSH host key at {}", path.display()))?;
    russh::keys::PrivateKey::from_openssh(&data).context("invalid SSH host key")
}

/// Connections that have not authenticated by then are closed (ADR 0005).
pub const AUTH_DEADLINE: Duration = Duration::from_secs(30);

pub fn spawn(
    state: AppState,
    authenticate: Authenticate,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<SocketAddr>,
) {
    spawn_with(state, authenticate, AUTH_DEADLINE)
}

/// [`spawn`] with a custom authentication deadline (tests use a short one).
pub fn spawn_with(
    state: AppState,
    authenticate: Authenticate,
    auth_deadline: Duration,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<SocketAddr>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_server(state, authenticate, auth_deadline, tx).await {
            tracing::error!(error = %e, "SSH server exited");
        }
    });
    (handle, rx)
}

async fn run_server(
    state: AppState,
    authenticate: Authenticate,
    auth_deadline: Duration,
    port_tx: tokio::sync::oneshot::Sender<SocketAddr>,
) -> anyhow::Result<()> {
    let bind = state.config.ssh_bind;
    let key_path = state.config.ssh_host_key_path.clone();
    let key = load_host_key(&key_path)?;

    let unauthenticated = Arc::new(tokio::sync::Semaphore::new(64));

    let russh_config = russh::server::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        auth_rejection_time: Duration::from_secs(1),
        // russh disconnects after this many failed attempts (ADR 0005).
        max_auth_attempts: 6,
        keys: vec![key],
        ..russh::server::Config::default()
    };

    let russh_config = Arc::new(russh_config);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind SSH listener on {bind}"))?;

    let bound_addr = listener.local_addr()?;
    let _ = port_tx.send(bound_addr);

    tracing::info!(bind = %bound_addr, "SSH server listening");

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "SSH accept failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        tracing::info!(client_ip = %peer_addr.ip(), "ssh connection accepted");

        let state = state.clone();
        let authenticate = authenticate.clone();
        let unauthenticated = unauthenticated.clone();

        let permit = match unauthenticated.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                drop(stream);
                continue;
            }
        };

        let russh_config = Arc::clone(&russh_config);
        tokio::spawn(async move {
            let conn = Arc::new(ConnectionState {
                identity: Mutex::new(None),
                git_protocol: Mutex::new(None),
                unauthenticated_permit: Mutex::new(Some(permit)),
                git_stdin: Mutex::new(None),
                session_opened: Default::default(),
                exec_started: Default::default(),
            });

            let handler = SshHandler {
                state,
                authenticate,
                conn: conn.clone(),
            };

            // run_stream returns once the session is set up and the session then runs
            // on its own task, so await it; inactivity_timeout resets on every byte, so
            // a separate deadline bounds the time to authenticate.
            let handle = Mutex::new(None);
            let run = async {
                if let Ok(session) = russh::server::run_stream(russh_config, stream, handler).await
                {
                    *handle.lock().unwrap() = Some(session.handle());
                    let _ = session.await;
                }
            };
            let deadline = async {
                tokio::time::sleep(auth_deadline).await;
                // The permit is released on authentication.
                if conn.unauthenticated_permit.lock().unwrap().is_none() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = run => {}
                _ = deadline => {
                    let handle = handle.lock().unwrap().take();
                    if let Some(handle) = handle {
                        let _ = handle
                            .disconnect(
                                russh::Disconnect::ByApplication,
                                "authentication timeout".into(),
                                String::new(),
                            )
                            .await;
                    }
                }
            }
            drop(conn.unauthenticated_permit.lock().unwrap().take());
        });
    }
}

// ------------------------------------------------------------------
// Unit tests
// ------------------------------------------------------------------

/// Reports `message` on stderr, exit status 128, and closes the channel.
fn refuse(
    session: &mut russh::server::Session,
    channel: russh::ChannelId,
    message: &[u8],
) -> Result<(), anyhow::Error> {
    session.channel_success(channel)?;
    session.extended_data(channel, 1, bytes::Bytes::copy_from_slice(message))?;
    session.exit_status_request(channel, 128)?;
    session.eof(channel)?;
    session.close(channel)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_upload_pack_ssh_url() {
        let (want, org, repo) = parse_command("git-upload-pack '/org/repo.git'").unwrap();
        assert_eq!(want, Want::Fetch);
        assert_eq!(org, "org");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_upload_pack_scp_style() {
        let (want, org, repo) = parse_command("git-upload-pack 'org/repo.git'").unwrap();
        assert_eq!(want, Want::Fetch);
        assert_eq!(org, "org");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_receive_pack() {
        let (want, org, repo) = parse_command("git-receive-pack '/my-org/my-repo.git'").unwrap();
        assert_eq!(want, Want::Push);
        assert_eq!(org, "my-org");
        assert_eq!(repo, "my-repo");
    }

    #[test]
    fn parse_reject_missing_dot_git() {
        assert!(parse_command("git-upload-pack '/org/repo'").is_none());
    }

    #[test]
    fn parse_reject_unknown_command() {
        assert!(parse_command("git-upload-archive '/org/repo.git'").is_none());
    }

    #[test]
    fn parse_reject_unquoted() {
        assert!(parse_command("git-upload-pack /org/repo.git").is_none());
    }

    #[test]
    fn parse_reject_extra_args() {
        assert!(parse_command("git-upload-pack '/org/repo.git' extra").is_none());
    }

    #[test]
    fn parse_reject_empty() {
        assert!(parse_command("").is_none());
    }

    #[test]
    fn parse_reject_no_path_segments() {
        assert!(parse_command("git-upload-pack 'repo.git'").is_none());
    }

    #[test]
    fn parse_reject_three_segments() {
        assert!(parse_command("git-upload-pack '/a/b/c.git'").is_none());
    }

    #[test]
    fn parse_reject_empty_org() {
        assert!(parse_command("git-upload-pack '//repo.git'").is_none());
    }

    #[test]
    fn parse_reject_empty_repo() {
        assert!(parse_command("git-upload-pack '/org/.git'").is_none());
    }

    #[test]
    fn parse_reject_trailing_content() {
        assert!(parse_command("git-upload-pack '/org/repo.git/'").is_none());
    }

    #[test]
    fn git_protocol_validation_accepts_valid() {
        assert!(layout::validate_git_protocol("version=2").is_ok());
        assert!(layout::validate_git_protocol("version=1:version=2").is_ok());
        assert!(layout::validate_git_protocol("version=0").is_ok());
    }

    #[test]
    fn git_protocol_validation_rejects_invalid() {
        assert!(layout::validate_git_protocol("invalid").is_err());
        assert!(layout::validate_git_protocol("").is_err());
    }

    #[test]
    fn generate_key_keeps_an_existing_key() {
        let dir = std::env::temp_dir().join(format!("githearth-ssh-key-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("host_key");
        std::fs::write(&key_path, "existing").unwrap();
        generate_host_key(&key_path).expect("an existing key is not an error");
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "existing",
            "must not overwrite"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn generate_key_creates_valid_key() {
        let dir = std::env::temp_dir().join(format!("githearth-ssh-key-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("host_key");
        let _ = std::fs::remove_file(&key_path);
        generate_host_key(&key_path).unwrap();
        assert!(key_path.exists(), "key path must exist");
        let content = std::fs::read_to_string(&key_path).unwrap();
        assert!(
            content.contains("OPENSSH PRIVATE KEY"),
            "key must be OpenSSH format"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&key_path).unwrap();
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "host key must be mode 0600"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
