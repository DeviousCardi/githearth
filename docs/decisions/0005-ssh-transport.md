# ADR 0005: SSH Git transport

Date: 2026-11-07.

Status: **accepted** for phase 5, after Person A's review on 2026-11-07 (PR #57). It sets the boundary between the two owners:

- **Person A:** SSH key storage, the key API and UI, revocation, audit.
- **Person B:** the SSH listener, command parsing, admission, process limits and routing into the existing Git pipeline.

The phase 5 gate requires HTTP and SSH to make identical authorization decisions. So SSH changes only how the caller is identified; everything after that is the code HTTP already runs.

## Constraints

From [ADR 0003](0003-git-push-and-repository-lifecycle.md) ("SSH transport reuses stages 2–7 with a different admission step"), [ADR 0002](0002-authorization.md), the [deployment contract](../plan/03-deployment-contract.md) and [roadmap](../plan/08-two-person-roadmap.md) phase 5:

- **Shared authorization.** Every decision goes through `githearth_authz::Engine::authorize`. SSH calls the functions HTTP calls, not copies of them.
- **Shared limits.** SSH honours the same process budget (`git_slots`), per-repository lock, `git_timeout_seconds`, `git_push_limit_bytes`, and hook admission token and operation ID flow. Push stages 2–7 (quarantine, `pre-receive` policy, journal, ref commit, acknowledgement, metadata) are unchanged.
- **Same Git admission today.** In `git_admission::admit` and `decide_ref`, every Git credential counts as freshly authenticated: they pass `Some(Duration::from_secs(0))` as the authentication age. There is no anonymous Git access.

## Decision: embedded SSH server

Run an SSH server inside `githearth-server`, using the `russh` crate. Its license and dependencies are checked by `cargo deny` when it is added; if it fails, this decision is revisited. Do not use OpenSSH with `AuthorizedKeysCommand` and a forced command.

1. **One process and one config.** There's no second daemon to configure, upgrade and keep consistent with GitHearth's keys, port and admission. This fits the single-host target in the deployment contract.
2. **Keys stay in PostgreSQL.** `AuthorizedKeysCommand` would run a helper per connection that needs database credentials, and puts a second program between a revocation and its enforcement.
3. **Restrictions are enforced in code.** No shell, PTY, forwarding or subsystems: the server simply implements none of them. There's no `sshd_config` to get wrong.

**Consequences.**
- B owns the listener, the handshake and the host key.
- The deployment exposes one more port.
- Rejected: OpenSSH was the less code option, but it doubles the hardening and upgrade surface and needs the helper above.

## Ownership split

| Person A — application | Person B — Git and infrastructure |
| --- | --- |
| `ssh_keys` migration, key API/UI, fingerprint uniqueness, expiry | SSH listener, handshake, host key lifecycle |
| Revocation and audit of keys | Command parsing, admission, process limits |
| `ssh_keys::authenticate` (below) | Routing into the existing fetch/push pipeline |

## Narrow interface

B calls one A function, which does authentication only:

```rust
ssh_keys::authenticate(conn: &mut PgConnection, fingerprint: &str)
    -> Result<Option<SshIdentity>, sqlx::Error>

struct SshIdentity { account: Uuid, key_id: Uuid, read_only: bool }
```

**What `authenticate` does.**
- `fingerprint` is the SHA-256 over the public key blob the client proved it holds during the handshake (see Settled with Person A).
- It returns `None` for an unknown, revoked or expired key, or a suspended account.
- `read_only` is `false` until A adds read-only keys. The field exists now so that adding them later changes no B code.

**What B changes.** B splits `git_admission::admit` (B's module) into two parts:
- The credential step reads HTTP Basic credentials and checks a password or an access token. It is HTTP-only.
- `admit_account(state, account, read_only, org_slug, repo_name, want)` loads facts with `load_scoped`, looks up the repository, refuses inactive repositories, and runs the `RepositoryRead`/`RepositoryClone` decisions and the protection rules.

HTTP calls the credential step, then `admit_account`. SSH calls `ssh_keys::authenticate`, then `admit_account`. Ref updates on both transports go through the same `git_admission::reload` and `decide_ref`. There is no SSH-specific policy path, so agreement holds by construction, and the tests below confirm it.

## Command handling

**Accepted commands.** An exec request is accepted only when it is exactly `git-upload-pack <path>` or `git-receive-pack <path>`. `git-upload-archive` is not supported in this phase; HTTP doesn't offer it either.

**Path forms.** Git sends the path single-quoted, in either of two forms:
- `'/org/repo.git'` from an `ssh://host/org/repo.git` URL;
- `'org/repo.git'` from the `host:org/repo.git` form.

Both are accepted. The `.git` suffix is required, as over HTTP (`git_http::strip_dot_git`). The parser accepts exactly one quoted argument with two path segments that are valid slugs and names, and rejects anything else. It never invokes a shell.

**Session restrictions.**
- **Environment:** the only variable accepted is `GIT_PROTOCOL`, and only with the values HTTP passes through, so protocol v2 works. Every other env request is refused.
- **Refused outright:** PTY, shell, subsystems (SFTP), and port, X11 and agent forwarding.

**Errors.**
- **Not found:** an unknown command, an unknown repository, an invisible repository and an inactive repository all produce the same message, `githearth: repository not found`, with exit status 128. This matches HTTP's identical 404 for all of these.
- **Not permitted:** a visible repository where the caller may not clone or push gets `githearth: not permitted`, matching HTTP's 403.

## Admission and limits

**Before authentication**, nothing else bounds a connection, because `git_slots` is taken only before Git is spawned. So:
- **Timeouts.** The server sets a handshake and authentication timeout (`russh` `Config::inactivity_timeout`, 30 s) and an `auth_rejection_time` (1 s).
- **Attempts.** A connection may make at most 6 authentication attempts; after that it is closed.
- **Unauthenticated connections.** A semaphore caps concurrent unauthenticated connections at 64; further connections are closed immediately. A connection's permit is released when it authenticates or closes.
- **Lookup only after proof.** `authenticate` is called only from `auth_publickey`, after the client's signature has been verified. `auth_publickey_offered` accepts every well-formed key without a database lookup, so an offered key can't be used to test whether a key is registered.
- **Uniform rejections.** Every rejection waits the same `auth_rejection_time`, whether or not the key is registered.

**After authentication:**

SSH acquires a `git_slots` permit before spawning Git, applies the `git_timeout_seconds` deadline and the `git_push_limit_bytes` limit on receive-pack input, and uses the same hook token flow (`new_hook_token`, `HookTokenGuard`), so the hooks admit SSH pushes the same way they admit HTTP pushes.

**Freshness.** Git admission already treats every credential as fresh (see Constraints), so an SSH key gets the same authentication age, 0. Force-push and ref deletion are therefore decided exactly as over HTTP: by the role and the branch protection rules. Freshness only becomes meaningful if HTTP Git admission changes, and then both transports change together.

**Revocation.** A revoked key or removed membership affects SSH the way a revoked token or removed membership affects HTTP:
- **New connections** fail at `authenticate` or `admit_account`.
- **A push in progress** is re-checked under the lock by `reload` and `decide_ref`, the ADR 0003 boundary.
- **A fetch already streaming** is not interrupted.

## Agreement tests (phase 5 gate)

One table-driven integration test starts one server with both transports. It runs every row over HTTP, with a password or token for the actor, and over SSH, with a key for the same actor, then asserts the same outcome class: allowed, not found, or not permitted.

| Actor | Repository | Operation | Expected |
| --- | --- | --- | --- |
| Owner | private, active | clone, push, force-push to unprotected | allowed |
| Owner | private, branch protection forbids force-push | force-push to protected | not permitted (ref refused) |
| Org member, `read` grant | private | clone / push | allowed / not permitted |
| Org member, no grant | private | clone | not found |
| Non-member | private | clone | not found |
| Non-member | public | clone / push | allowed / not permitted |
| Owner | archived | clone / push | allowed / not permitted |
| Owner | deleting or deleted | clone | not found |
| Any | nonexistent path | clone | not found |
| Read-only token over HTTP vs `read_only` key over SSH | private | push | not permitted (row added when A adds read-only keys) |
| Revoked token vs revoked key | private | clone | authentication fails |
| Member removed from the organization between connect and push (holds a write grant at connect) | private | push | not permitted (refused by `reload` under the lock) |
| Suspended account | private | clone | authentication fails |

The SSH side generates a key pair per actor and uses the system `ssh` client through `GIT_SSH_COMMAND`, so the test covers the same client real users run.

## Rollout

| Config key | Default | Meaning |
| --- | --- | --- |
| `ssh_enabled` | `false` | The listener starts only when this is true |
| `ssh_bind` | `127.0.0.1:2222` | Listen address |
| `ssh_host_key_path` | none; required when enabled | Ed25519 host key, mode 0600 |

**Host key.**
- `githearth-server ssh-keygen --config …` creates the key if it is missing. `serve` refuses to start with SSH enabled and no readable key. It never generates one silently, because a regenerated key looks like an attack to every client.
- The key belongs with configuration and secrets, not under `storage_root`. The backup runbook already says to back those up separately.
- `doctor` gains an `ssh` check: skipped when disabled; otherwise it checks that the host key exists, parses, and is not readable by other users, and that `ssh_bind` parses. It does not try to bind: on a live host `serve` already holds the port, and real bind failures are reported by `serve` at startup.

## Consequences

- SSH is one more listener in the same process, sharing the pool, semaphores, storage, hooks and authorization engine.
- The split of `admit` is the only change to existing code. Its HTTP behaviour is pinned by the existing `git_http` tests.
- Read-only keys, audit of the credential type per push, and per-key last-used timestamps are A's follow-ups. They need no change to this boundary.

## Settled with Person A

- **Fingerprint:** SHA-256 over the public key blob, unique across all accounts via a unique index on the fingerprint. One key maps to exactly one account. Adding a key that another account already holds returns 409.
- **Audit:** push audit events record `credential_kind` (`password`, `token` or `ssh_key`) plus the key ID or token ID. That's A's migration. B passes `key_id` through from `SshIdentity`.
