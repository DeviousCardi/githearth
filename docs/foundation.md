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
| `body_limit_bytes` | `GITHEARTH_BODY_LIMIT_BYTES` | 16384 / 1024–1048576 bytes |
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


## Browser login (M2)

The server-rendered UI is available at `http://127.0.0.1:8080/`. Sign in with the bootstrapped administrator; session cookies are `HttpOnly`, `SameSite=Lax`, and `Secure` by default. The local Compose profile sets `GITHEARTH_SECURE_COOKIES=false` because it serves plain HTTP on loopback; production deployments should leave Secure enabled (or set `secure_cookies = true`) and terminate TLS at a trusted ingress.
