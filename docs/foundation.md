# M1: experimental server foundation

The `githearth-server` binary runs independently of the existing local `githearth` Git prototype. M1 provides PostgreSQL migrations, one-time administrator bootstrap, password login, expiring/revocable bearer sessions, and membership-scoped organizations. It has no public account registration, Git HTTP/SSH transport, issue/review workflow, CI, UI, or replicated storage.

## Local Docker Compose

Requirements: Docker Engine with Compose, Python 3, and a terminal. Ports 8080 and 54329 must be available. Run from the repository root. The database is a separate process with a persistent Docker volume. These commands generate a unique local password; the ignored `.env` file is a secret.

```bash
umask 077
python3 - <<'PY'
import pathlib, secrets
with pathlib.Path('.env').open('x') as f:
    f.write('POSTGRES_PASSWORD=' + secrets.token_hex(32) + '\n')
PY
docker compose up --build --wait
curl --fail http://127.0.0.1:8080/health/live
curl --fail http://127.0.0.1:8080/health/ready
read -r -s -p 'New administrator password (12+ bytes): ' ADMIN_PASSWORD; echo
printf '%s' "$ADMIN_PASSWORD" | docker compose exec -T app githearth-server bootstrap-admin --username admin
unset ADMIN_PASSWORD
```

Bootstrap succeeds once. Repeating it fails with an explicit message and never updates an existing administrator or password. It takes a transaction-scoped database lock, creates the account and audit/outbox records atomically, and requires schema migration first. There is no password reset or account provisioning command in M1. Bootstrap is an operator command, never an HTTP route. Passwords are accepted only on stdin, never as CLI options.

Compose starts PostgreSQL, runs a one-shot migration service, then starts the app. A failed migration prevents app startup. Application serving itself does not apply migrations, keeping schema changes an explicit operator action. The image runs as an unprivileged user and handles SIGINT/SIGTERM. Shutdown drains admitted requests; HTTP and SQL work have separate deadlines.

Plain HTTP is for loopback development only. Compose publishes both ports on `127.0.0.1`; the app binds all interfaces inside its container. Before remote use, install a trusted TLS reverse proxy, restrict the backend network to that proxy, and use TLS for any nonlocal database connection (`sslmode=verify-full` and an appropriate CA). M1 is experimental, without a production deployment or availability claim.

Stop services while keeping data with `docker compose down`. **`docker compose down --volumes` permanently removes this installation's database.** Never reuse the generated development secret in another environment. Changing `.env` does not rotate credentials in an existing PostgreSQL volume; use PostgreSQL's supported password rotation and update application configuration together.

## Run the Rust binary directly

The checked-in sample omits the secret connection URL. Config files are TOML with unknown fields rejected. Environment values override file values, then all fields are validated. Parse errors do not print configuration contents. Protect any config containing a database URL with filesystem permissions.

```bash
docker compose up --wait postgres
set -a
source .env
set +a
export GITHEARTH_DATABASE_URL="postgres://githearth:${POSTGRES_PASSWORD}@127.0.0.1:54329/githearth"
cargo run -p githearth-server -- --config githearth.example.toml migrate
read -r -s -p 'New administrator password (12+ bytes): ' ADMIN_PASSWORD; echo
printf '%s' "$ADMIN_PASSWORD" | cargo run -p githearth-server -- bootstrap-admin --username admin
unset ADMIN_PASSWORD
cargo run -p githearth-server -- --config githearth.example.toml serve
```

Skip bootstrap when the Compose installation already has its administrator. Use the same database and identities in either execution mode.

| TOML key | Environment override | Default / accepted bounds |
| --- | --- | --- |
| `database_url` | `GITHEARTH_DATABASE_URL` | Required PostgreSQL URL |
| `bind` | `GITHEARTH_BIND` | `127.0.0.1:8080`, numeric socket address |
| `database_max_connections` | `GITHEARTH_DATABASE_MAX_CONNECTIONS` | 10 / 1–100 |
| `database_timeout_seconds` | `GITHEARTH_DATABASE_TIMEOUT_SECONDS` | 3 / 1–30 seconds, pool acquisition and readiness |
| `request_timeout_seconds` | `GITHEARTH_REQUEST_TIMEOUT_SECONDS` | 15 / 1–120 seconds |
| `token_ttl_seconds` | `GITHEARTH_TOKEN_TTL_SECONDS` | 3600 / 1–86400 seconds |
| `body_limit_bytes` | `GITHEARTH_BODY_LIMIT_BYTES` | 524288 / 1024–1048576 bytes |
| `auth_concurrency` | `GITHEARTH_AUTH_CONCURRENCY` | 2 / 1–16 concurrent password verifications |

Every connection sets a 5-second statement timeout, 3-second lock timeout, and 10-second idle-transaction timeout. Password verification uses blocking workers with a bounded permit held until hashing finishes, even when its HTTP caller times out. Busy authentication returns 429. The pool is bounded; HTTP bodies and total request duration are bounded. This is resource protection, not a distributed brute-force prevention service; per-account/IP throttling is a later production requirement.

## API exercise

The following script prompts for the password without placing it in shell history, logs in, reads the current user, creates an organization, lists memberships, and logs out. The bearer token remains only in the script's memory.

```bash
python3 - <<'PY'
import getpass, json, urllib.request
base = 'http://127.0.0.1:8080'
def call(method, path, payload=None, token=None):
    headers = {'Content-Type': 'application/json'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    data = None if payload is None else json.dumps(payload).encode()
    with urllib.request.urlopen(urllib.request.Request(base + path, data=data, headers=headers, method=method)) as r:
        body = r.read()
        return json.loads(body) if body else None
session = call('POST', '/api/v1/login', {'username': 'admin', 'password': getpass.getpass()})
token = session['token']
print(call('GET', '/api/v1/me', token=token))
print(call('POST', '/api/v1/organizations', {'slug': 'my-team', 'name': 'My Team'}, token))
print(call('GET', '/api/v1/organizations?limit=50', token=token))
call('POST', '/api/v1/logout', token=token)
PY
```

| Endpoint | Behavior |
| --- | --- |
| `GET /health/live` | 200 while the process serves requests; no DB requirement |
| `GET /health/ready` | 200 only when DB responds, exact embedded migration history/checksums match, and required schema relations/columns exist; otherwise 503 |
| `POST /api/v1/login` | JSON `username`, `password`; returns `token`, `token_type: Bearer`, `expires_in` in seconds |
| `POST /api/v1/logout` | Requires valid bearer; revokes that session; 204 |
| `GET /api/v1/me` | Requires valid bearer; returns UUID, username, admin flag |
| `POST /api/v1/organizations` | Requires valid bearer; JSON `slug`, `name`; 201 with UUID, slug, name, owner role; 409 for duplicate slug |
| `GET /api/v1/organizations` | Requires valid bearer; membership-filtered, UUID ordered, maximum 100 rows; `limit` defaults to 50; pass last returned UUID as `after` |
| `GET /api/v1/organizations/{uuid}` | Requires membership; 404 for missing or inaccessible org, including when caller is a platform admin |

Usernames are lowercase ASCII letters/digits/underscore/hyphen, 3–64 bytes; organization slugs are lowercase ASCII letters/digits/hyphen, 1–64 bytes. Names are nonblank and at most 200 Unicode characters. All internal identities are stable UUIDs. An organization, its owner membership, its audit record, and its outbox record commit in one database transaction. Outbox delivery is not implemented; events remain durable for a later consumer.

Passwords use salted Argon2id version 19 with explicitly pinned memory 19,456 KiB (19 MiB), 2 iterations, 1 lane, and a 32-byte output. Session tokens contain 256 random bits and only their SHA-256 digests are stored. Tokens expire according to the PostgreSQL clock. New authorization checks after a committed logout reject that token; a request already admitted may finish within configured deadlines. Organization reads load the organization and then authorize `organization.read` through the Cedar engine ([ADR 0002](decisions/0002-authorization.md)); a denial returns the same 404 as a missing organization. Organization lists filter by membership in SQL. M1 has no tenant-admin bypass. Responses use `Cache-Control: no-store`. JSON logs include matched route, method, status, and duration, never credentials, request bodies, raw URIs/queries, SQL bind values, or driver error text. Access logs on a future proxy must follow the same policy.

M1 does not implement operation-ID replay for organization creation. A deadline or connection loss can leave a commit outcome unknown; inspect the member-visible organization list and slug before retrying. Reusing a committed slug returns 409. Durable command replay, membership administration, session cleanup/quotas, password reset, outbox delivery, backups/restore automation, and production abuse controls remain later work. No HA or capacity claim is made.

## Verification

The supervisor's reproducible isolated check runs the real PostgreSQL contract plus the actual HTTP executable, including database pause/restart, application restart, persisted sessions/organizations, logout and credential log checks:

```bash
python3 scripts/verify-foundation.py
```

It creates a uniquely named disposable PostgreSQL container, uses generated secrets outside the repository, and removes only its own resources. It requires Docker, Python 3 and the Rust toolchain. TCP readiness avoids PostgreSQL's temporary initialization-only Unix socket. The database host port is explicit because Docker can reassign automatically published ports on container restart; the test asserts that the address stays unchanged.


```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Unit tests additionally verify authentication saturation returns 429 before DB access, cancellation retains the blocking worker permit until completion, explicit Argon2 resource parameters, unknown TOML fields, and secret-safe configuration errors.

The PostgreSQL contract test is explicitly ignored by the ordinary suite. It refuses a nonempty public schema. Use an isolated test database; the test writes schema and fixtures and leaves them available for inspection. For a repeat run, recreate only this disposable test database.

```bash
docker compose up --wait postgres
docker compose exec -T postgres createdb -U githearth githearth_test
set -a
source .env
set +a
GITHEARTH_TEST_DATABASE_URL="postgres://githearth:${POSTGRES_PASSWORD}@127.0.0.1:54329/githearth_test" \
  cargo test -p githearth-server --test postgres -- --ignored --nocapture
docker compose exec -T postgres dropdb -U githearth githearth_test
```

The contract covers an uninitialized schema, fresh migration, repeated migration, readiness migration/checksum mismatch, unknown migration and missing relation, concurrent and duplicate bootstrap preserving the original credentials, Argon2id storage, correct/wrong/missing-user login, hashed token storage, authenticated identity, absent/malformed bearer credentials, invalid organization input, cursor pagination without duplicates/omissions, two-user organization isolation (including platform-admin nonmembership), pagination bounds, duplicate slug, atomic rollback after a forced final outbox failure, logout, token expiry, oversized bodies, absence of public registration, and dependency outage via closing the application pool. Actual PostgreSQL process stop/restart is also testable with `docker compose stop postgres` (live stays 200, ready becomes 503) and `docker compose start postgres` (ready recovers after reconnection). These tests do not establish power-loss durability, HA, load capacity, or backup recoverability.

Migration files are embedded and versioned through SQLx. Never edit an applied migration; add the next version and test upgrading from the previous schema. Back up the database before migrations. M1 has no automatic down migration or backup/restore tooling. Use a dedicated migration owner and a least-privilege application role before production deployment; the local Compose fixture deliberately uses one development database owner.

## M2 verification

The M2 workflow (Git hosting, browsing, issues and recovery) is verified by the executable acceptance harness in [`scripts/verify-m2.py`](../scripts/verify-m2.py), which builds the release binary, drives real `git` clients against a disposable `postgres:17-bookworm` container, and covers push/clone, browsing, issues/comments idempotency and the browser issue pages, access isolation, crash recovery, restart and the ADR 0003 fresh-restore procedure. Run it with

```bash
python3 scripts/verify-m2.py --report /tmp/m2-report.json
```

It prints PASS/FAIL per scenario, exits non-zero on any failure, records whole-workflow resource measurements as JSON, and cleans up its container and temp dirs. Recorded results are in [`docs/verification/m2-workflow.md`](verification/m2-workflow.md).


## Browser login (M2)

The server-rendered UI is available at `http://127.0.0.1:8080/`. Sign in with the bootstrapped administrator; session cookies are `HttpOnly`, `SameSite=Lax`, and `Secure` by default. The local Compose profile sets `GITHEARTH_SECURE_COOKIES=false` because it serves plain HTTP on loopback; production deployments should leave Secure enabled (or set `secure_cookies = true`) and terminate TLS at a trusted ingress.

## Issue pages (M2)

Signed-in browsers can list a repository's issues (open, closed or all, 30 per page, newest first), open an issue with its comments (30 per page, oldest first), create an issue and add a comment at `/organizations/{org}/repositories/{repo}/issues`. The pages use the browser session cookie; every form carries the session's CSRF token (a bad token gets 403) and a fresh idempotency key, so a double submit or a resent POST replays the first result instead of creating a duplicate. Blank titles and comments are refused by both the pages and the API. They call the same functions as the JSON issue API, so authorization, validation, idempotency storage and audit and outbox events are identical. Closing, reopening and editing issues are API-only.

## Pull requests (M3 step 1)

Pull requests live between two branches of one repository and store branch names, not commits; their head and base are resolved from Git on every read. They share the issue number sequence, so an issue and a pull request never have the same number, and only one open pull request may exist per source/target branch pair. The JSON API at `/api/v1/organizations/{org}/repositories/{repo}/pulls` supports create, list, read (with live `head_id`/`base_id`), and PATCH of title/body/state; comments (general or anchored to a commit/path/line at the head, with replies) and reviews are nested resources. Reviews are append-only and each one names the head commit it was given on: approvals and change requests count only at the current head, so pushing a new commit makes earlier approvals stale, and only the latest verdict per reviewer at the head counts. Approving one's own pull request is forbidden. Merging is recorded as a merge intent via `POST /api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge` and the merge form on the pull request page; the same validation and authorization run in both. A background worker polls for queued intents and executes them, and each intent reaches a terminal state: `merged` (the ref moved and the pull request is marked merged), `conflicted`, `stale`, `failed` or `cancelled`.

Signed-in browsers can list a repository's pull requests (open, closed or all, 30 per page, newest first), view one with its head/base ids, approvals, reviews and comments, create a pull request and add comments and reviews at `/organizations/{org}/repositories/{repo}/pulls`. The pages use the browser session cookie and CSRF-token forms with idempotency keys, exactly like the issue pages, and call the same functions as the JSON API.

## Branch protection (M3)

Maintainers protect branches with rules under `/api/v1/organizations/{org}/repositories/{repo}/protection`. Each rule has a `pattern` — a concrete branch name or a glob with one trailing `*` prefix wildcard (`main`, `release/*`) — and the gates `allow_force_push`, `allow_deletion`, `require_pull_request`, `required_approvals` (0–10) and `require_checks`. `GET` lists the rules with the repository's current protection `generation`; `POST` creates, `PATCH /{id}` edits and `DELETE /{id}` removes. Mutations are gated on `repository.update` (maintain), reads on `repository.read`, and they accept `Idempotency-Key` headers and emit audit and outbox events like the issue API. A rule or its deletion bumps a per-repository `policy_generation` counter in the same transaction, so any cached ruleset can detect staleness.

At push admission the server loads the repository's rules once and passes them through the [ADR 0002](decisions/0002-authorization.md) ref decision. Matching maps `refs/heads/<branch>` to the most specific rule: an exact branch name beats any glob, and among globs the longest prefix wins (`release/v1` picks `release/v1/*` over `release/*`). On a protected branch the engine's built-in forbids (`protected-no-force-push`, `protected-no-deletion`, `protected-requires-pull-request`) refuse a force push, deletion or direct push that the rule does not allow; the push error reports `protected branch: ...`. Unprotected branches are unchanged, and `ref.force_push` and `ref.delete` remain in the `write` group wherever no rule matches.

Protection applies to a branch only once it exists. Creating a branch (old value all zeros) that matches a rule is an ordinary `ref.push`, so `require_pull_request` does not block it; the rule applies to every later push. As a result, anyone who can push can create a protected name from any commit, for example `main` in an empty repository, or recreate it after a delete the rule allows. GitHub behaves the same way by default.

## SSH keys

Signed-in sessions manage SSH public keys for Git transport over the embedded SSH server ([ADR 0005](decisions/0005-ssh-transport.md)). The API is session-authenticated only; a personal access token can never create, list or revoke keys. The server is built without RSA, so only Ed25519 and ECDSA (nistp256/384/521) keys are accepted — RSA, DSA, `sk-*` and certificate lines are refused with 400.

```bash
ssh-keygen -t ed25519 -f ~/.ssh/id_githearth -N '' -C 'you@example.com'
# POST /api/v1/user/ssh-keys  {"name": "laptop", "public_key": "...", "expires_in_days": 30}
git clone ssh://githearth-host:2222/org/repo.git
```

| Endpoint | Behavior |
| --- | --- |
| `POST /api/v1/user/ssh-keys` | Session bearer; JSON `name` (1–100 chars), `public_key` (OpenSSH line), optional `expires_in_days` (1–3650); 201 with metadata including the `SHA256:` fingerprint; 409 if the fingerprint is already registered to any account and not revoked; 400 for an unsupported or malformed key |
| `GET /api/v1/user/ssh-keys` | Session bearer; lists the caller's keys, including `revoked_at` |
| `DELETE /api/v1/user/ssh-keys/{id}` | Session bearer; revokes (sets `revoked_at`); 204; 404 for a missing or another account's key |

A key's SHA-256 fingerprint is unique among non-revoked keys across all accounts, so one key maps to exactly one account; after revocation the same key can be added again. Revoked, expired, and suspended-account keys fail authentication on new connections exactly like unknown keys. `git clone ssh://host:port/org/repo.git` and the `host:org/repo.git` form both work, with Ed25519 or ECDSA client keys.

Registration does not require proof that the caller holds the private key, as on most forges (issue #66). Someone who registers another person's public key first makes that person's own registration fail with 409 until the key is revoked. This fails safe: a connection with that key authenticates as the registrant, so it reaches only repositories the registrant can already access, never the key owner's. A key owner who gets an unexpected 409 should generate a new key; a signature-challenge step before activating a key would close the gap and is not implemented.

## Notifications

In-app notifications are the first real handlers on the [ADR 0006](decisions/0006-durable-jobs.md) job runner. A merge that a pull request author did not perform themselves, and a review submitted by someone else, each enqueue a job inside the domain transaction; the handler loads the current rows by ID, skips self-notifications, and inserts one `notifications` row keyed idempotently on `(event_id, account_id)`. Job payloads carry IDs only (the audit event id or review id and the pull request id), never titles or bodies, so retained `dead` jobs keep no private data.

| Endpoint | Behavior |
| --- | --- |
| `GET /api/v1/user/notifications` | Session bearer only (an access token gets 403); lists up to 50, newest first; optional `?unread=true`; each item has `id, kind, pull_request_id, actor_username (nullable), created_at, read_at`. No titles or repository names are returned — the client fetches those through the pull request API, which enforces read access, so a notification never leaks a repository the user lost access to |
| `POST /api/v1/user/notifications/{id}/read` | Session bearer only; sets `read_at` and returns 204; idempotent; 404 for an unknown id or another account's notification |

The two kinds are `pull_request.merged` and `pull_request.reviewed`. The recipient is always the pull request author, and a notification is skipped when the author is the one who merged or reviewed. The `notifications` table's `actor_id` set to null when the acting account is deleted, `pull_request_id` cascades on pull request deletion, and `account_id` cascades on account deletion.

## Settings

Signed-in sessions manage personal access tokens and SSH keys from the browser at `/settings` (create with a one-time plaintext-token display, list, and revoke), using the same CSRF-protected forms and shared API functions as the JSON endpoints.
