# M1: executable application foundation

Status: verified experimental. Implementation owner: GPT-6.1 Sol, medium reasoning. Supervisor: primary agent. Baseline: 2026-10-27.

## Outcome

Start GitHearth with a separate PostgreSQL process, apply migrations, bootstrap the first administrator, log in, create an organization, and verify membership isolation through HTTP. Report health, reject invalid configuration and bound resource consumption.

M2 adds repository lifecycle, authenticated Git transport, browsing, issues and comments. M1 is an experimental API foundation. Distributed storage, CI, external identity and stable release remain separate gates.

## Deliverables and acceptance

| ID | Requirement | Required evidence |
| --- | --- | --- |
| C01 | Rust server integrates without breaking existing prototype | Format check, workspace tests, strict all-target clippy; toolchain compatibility checked |
| C02 | Embedded versioned PostgreSQL migrations | Empty real database migrates; repeat safe; failed/unknown/checksum-mismatched history not ready |
| C03 | Validated file/environment config | Documented precedence, rejected unknown fields/invalid bounds, sanitized configuration errors |
| C04 | Protected first-admin bootstrap | Password input avoids CLI arguments; no default password; concurrent/repeated commands cannot replace admin |
| C05 | Secure credential/session persistence | Reviewed Argon2id hash; random bearer token stored only as hash; valid/invalid login and current-account tests |
| C06 | Session admission and revocation | Missing/malformed/expired/revoked tokens denied; logout prevents new admission |
| C07 | Stable UUID identities and tenant boundaries | Two-account fixture proves private org read/list isolation; duplicate slug/invalid input handled |
| C08 | Atomic organization creation | Organization, owner membership, audit and outbox either all commit or all roll back under forced event-insert failure |
| C09 | Useful bounded probes | Live independent of DB; ready checks DB/schema; actual DB outage and recovery observed |
| C10 | Bounded resource consumption | Pool/acquisition/SQL/request/body limits; bounded expensive password work; overload/body rejection exercised |
| C11 | Runnable development deployment | Container build; Compose app + separate PostgreSQL + migration; executable bootstrap/auth/org/logout smoke; state survives restart |
| C12 | Operational handoff | Exact runbook, test fixture instructions, cleanup preserving data, HTTP/TLS boundary, explicit missing features and evidence report |

**C01–C12 passed:** see the linked verification report. No criterion passes merely because code exists. Record outcomes in [M1 verification](../verification/m1-foundation.md). An ignored database test must be explicitly executed against the documented disposable fixture.

## HTTP contract

| Endpoint | Access | Behavior |
| --- | --- | --- |
| GET /health/live | Public | Process alive without sensitive dependency details |
| GET /health/ready | Public | Success only with reachable DB and compatible schema; bounded failure |
| POST /api/v1/login | Public, bounded expensive work | Token and expiry for valid credentials; uniform denial for invalid credentials |
| POST /api/v1/logout | Authenticated | Revoke presented session |
| GET /api/v1/me | Authenticated | Identity excluding credential material |
| POST /api/v1/organizations | Authenticated | Unique validated slug/name, atomic owner/audit/outbox |
| GET /api/v1/organizations | Authenticated | Membership-filtered bounded cursor page |
| GET /api/v1/organizations/{id} | Member | Inaccessible and nonexistent org give no private detail |

Bootstrap is operator-only. The test fixture may provision another user through trusted setup. Public registration, account invitations/reset, browser cookie/CSRF policy, MFA, full token scopes and account administration come before their relevant UI/stable-release gates.

## Implementation order

1. Config, migrations, executable and probes.
2. Protected bootstrap, password/session admission and logout.
3. Tenant transaction and isolated list/read.
4. Real PostgreSQL tests: migration, authentication, isolation, rollback and outage.
5. Container/Compose executable smoke and restart persistence.
6. Supervisor review, fixes, affected-check reruns and recorded evidence.
7. Mark M1 verified only after the required criteria pass; start M2 using its separate gate.

## Review boundaries

Log routes/status/duration without raw URLs, headers, credentials or request bodies. SQL logging is disabled or sanitized. Tokens expire and are checked against authoritative state; already-admitted requests may finish after logout. Full distributed revocation semantics remain an explicit later design.

Password work must remain bounded even when HTTP requests time out; the permit lives with the actual blocking work. Session cleanup and distributed login throttling are required operational follow-up before broad exposure. Development Compose publishes loopback ports; a trusted TLS ingress is required beyond local HTTP development.

Unknown write outcomes require eventual operation-ID support. M1 organization creation does not promise replay-safe retries: after a lost response, query the unique slug before retrying. M2 implements durable operation IDs before labeling mutation APIs retry-safe. Outbox persistence in M1 does not mean event delivery exists.

## M2 first task breakdown

1. [ADR 0003](../decisions/0003-git-push-and-repository-lifecycle.md) for the Git receive journal/acknowledgement boundary and repository lifecycle reconciliation (accepted).
2. Harden native Git process limits, environment, object formats and storage ownership.
3. Repository metadata, team/grant/protection and operation-ID tables/migrations; every check through the [ADR 0002](../decisions/0002-authorization.md) engine.
4. Private repository API and Git smart HTTP adapters with durable write handling.
5. Files/commits/diffs browsing and basic accessible login/navigation UI.
6. Issues/comments with atomic idempotency/outbox and cursor pagination.
7. End-to-end negative-access, crash-recovery and fresh restore scenario.
8. Whole-workflow benchmark and single-node resource report.
