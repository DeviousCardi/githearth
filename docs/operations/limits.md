# Per-organization limits and fairness

M4.2 replaces the single global Git semaphore with a share scheduler and gives
every tenant its own request, creation, job, email and storage budgets. The
defaults below are the values this build enforces; ADR 0014 §5 and open item 6
record the two that measurement revised. The exit bound is relative, not an
absolute latency.

## Published defaults (what is enforced)

| Limit | Default | Configuration | Charged at |
| --- | --- | --- | --- |
| Global Git concurrency | 8 slots | `git_concurrency` | the scheduler's capacity |
| Per-organization Git floor | 1 slot | `git_organization_floor` | a queued organization is served first |
| Per-organization Git ceiling | 4 slots | `git_organization_ceiling` | every scheduler grant; clamped to `git_concurrency - 1` |
| Git wait before `Retry-After` | 1 s | `git_wait_millis` | `info/refs`, `upload-pack`, `receive-pack`, SSH |
| API requests | 300/min per organization, 60/min per account | `api_requests_per_organization`, `api_requests_per_account` | one `/api/v1` middleware, every route |
| Authenticated Git requests | 600/min per organization, 120/min per account | `git_requests_per_organization`, `git_requests_per_account` | Git HTTP (all three endpoints) and SSH, after admission |
| Anonymous clone/fetch | 120/min per address, 2,000/min installation | `anonymous_fetch_per_address`, `anonymous_fetch_global` | `info/refs` and `upload-pack` for a request with no credential, after the repository is known to exist |
| Anonymous login | 20/min per address, 300/min installation | `anonymous_login_per_address`, `anonymous_login_global` | every sign-in path before hashing |
| Organization creation | 3 per account per 24 h, 50 per installation per day | `organization_creation_per_account`, `organization_creation_per_installation` | `POST /api/v1/organizations` |
| Repository storage | 5 GiB per repository; 50 GiB per organization for each of Git storage and the object store | `repository_quota_bytes`, `organization_quota_bytes` | the push hook, a server-side merge, and the object store |
| Job worker share | 2 of 4 workers per organization | `job_organization_share` | the claim query |
| Email | 200/hour per organization, 5/hour per address for security mail | `email_per_organization`, `email_security_per_address` | the notification and verification consumers |
| Sign-up velocity | 3 per address per hour (M11a endpoint) | `signup_velocity_per_address`, `signup_velocity_window_seconds` | the M11a sign-up endpoint |

The API row is charged in one middleware layer on `/api/v1`, not in each
handler: the organization comes from the path when there is one, the account
from the bearer credential. A route added later is covered without remembering
to call the limiter. The Git row is separate, as ADR 0014 §5 asks, and is
charged only after admission so a request for a repository that does not exist
cannot spend a tenant's budget. A clone is at least two requests (`info/refs`
and `upload-pack`), which is why the Git budget is twice the API budget.

Storage quota counts objects, merge-created objects, fork copies and stored
large objects; `0` disables a quota, and the repository quota may not exceed
the organization quota. The organization figure covers Git repository storage:
every push is measured against the sum of every active repository's `objects/`
in the organization plus the incoming quarantine, and a server-side merge is
measured after the candidate is written and before the ref moves, so the merge
commit, a squash's tree and a cross-repository fork fetch are all charged. The
same configured number bounds the object store's ledger separately (ADR 0014
§1): it is 50 GiB of Git repository storage and 50 GiB of object-store bytes,
not 50 GiB of their sum, because the two ledgers are kept apart so a delete or
a reconcile in one cannot erase the other's accounting. The
walk happens on the blocking pool while the push or merge holds the
repository lock, once per write, so a tenant with many repositories or many
loose objects pays a delay proportional to its object count before
`receive-pack` starts or the merge lands; a cached counter was rejected because
it drifts after a crash, a restore or a hand-run `gc`, and the quota is a
refusal rather than an estimate. `repository_quota_bytes` defaults to 5 GiB
since M4.2; an installation that upgraded and has larger repositories can set
it to `0` to restore the previous unlimited behaviour.

**Search is not routed by this build, but its budget is real.** The row used to
read "login/search". M9 owns search and no search endpoint exists here, so the
table publishes only the surface a request can reach today: sign-in.
`Limits::allow_anonymous_search` already spends the same
`anonymous_login_per_address` and `anonymous_login_global` counters, and
`tests/limits.rs::the_anonymous_search_limiter_refuses_above_the_published_budget`
exercises both refusals (the 21st search from one address, the 301st request
installation-wide). M9's route calls that limiter, so "20/minute for login and
search" will be true by construction rather than a second number to keep in
step.

## Window semantics

Every budget in the table is an in-process fixed window. The window starts at
the first request and admits at most its limit until it expires, so a caller
that straddles a boundary can send up to twice the published number in one
wall-clock minute. The counters live in the server process: they reset on
restart and are not shared between instances (the cluster profile is M5's).
An address that cannot be resolved is counted under one shared key rather than
bypassing its budget.

The per-address budgets need `trusted_proxies` when the server is behind a
reverse proxy, or every request appears to come from the proxy and shares one
bucket. `doctor` reports this: with `trusted_proxies` empty and a public
`public_url` it warns that a proxy would collapse the address budgets; a
loopback `public_url` is the local profile and reads the connection peer
directly.

## Git share scheduler

Every Git invocation takes its slot from the scheduler, including the web
tree/blob/commit/compare pages, the pull-request diff and the background
mergeability/CODEOWNERS reads. `git_slots` is only the scheduler's capacity;
nothing acquires it directly, so one organization can no longer hold every
slot from a browser loop.

A request belongs to the organization in its path (the repository's
organization). The scheduler caps each organization at:

```
demand = organizations that hold a slot OR have a request queued
cap    = min(ceiling, max(floor, ceil(global / demand)))
```

Counting queued organizations is what re-derives the cap at arrival: a
newcomer shrinks the incumbent's share as soon as it queues, and a grant to an
organization already above its floor refuses capacity reserved for a queued
peer's floor. A release serves a queued organization below its floor before
any organization merely under its fair share, so a busy tenant cannot starve
an idle one at its reserved slot.

No organization may hold the whole global capacity, even when it is the only
tenant: the ceiling is clamped to `git_concurrency - 1`, and the default
ceiling is 4 rather than 6 so the first mover cannot hold 75% of the
installation before a peer arrives. A request over its share waits up to
`git_wait_millis`; after that the transport answers `503`/`Retry-After` (HTTP)
or `service unavailable` (SSH). The scheduler lock is taken only around
in-memory counters, never across an await, a pool connection or the
per-repository lock; the transport order stays Git permit, then repository
lock, then database.

A handoff sends a fully built permit through the waiter's channel. The
counters are incremented before the send and the permit's `Drop` is the only
decrement path, so a waiter cancelled after the handoff (a transport timeout,
a dropped HTTP client) still returns the slot. A cancelled handoff cannot
wedge the scheduler with free permits and a full counter.

## The measured bound

The exit test is relative: with the configured shares, an organization at its
share sees a p95 within twice its measured single-organization p95 at the same
total load. The measurement is in
`crates/githearth-server/src/fairness.rs`
(`an_organization_at_its_share_stays_within_twice_the_single_organization_p95`)
and runs with a fixed simulated service time, so it measures the scheduler
rather than Git or PostgreSQL. It presents sustained load at the measured
organization's share (48 requests, four in flight), alone and then while a
second organization offers 480 requests with 32 in flight at its own share.
The same harness is repeated with the cap removed (`floor = ceiling = global`)
as a negative control; that control must breach the bound, otherwise the test
would pass with the mechanism deleted.

Measured on the reference machine (macOS, `cargo test`, release-independent
scheduler simulation), three consecutive runs: single-organization p95
**273–275 ms**, shared p95 **290–295 ms**, ratio **1.06–1.07** (worst 1.07),
with the no-cap control at **4.55–4.64×**. The runs are recorded in the fix
report
(`/Users/suniljadav/Aarav/Github/githearth-m41-reports/m42b5-fix-report.md`).
The defaults revised from this measurement (ADR 0014 open item 6):

- `git_organization_ceiling` 6 → **4** (half of `git_concurrency`). At 6 the
  first mover filled 6 of 8 slots before a peer arrived, and the peer's first
  requests were refused after the 1 s wait; at 4 the first mover can never
  hold more than half, and two tenants at their share fill the installation
  without queueing.
- `git_organization_floor` stays **1**: the floor is what serves a queued
  newcomer before an incumbent's next request.
- The cap is clamped to `git_concurrency - 1` for installations that lower
  concurrency below the ceiling default.

## Enforcement points

- **API** — one middleware on `/api/v1` charges the organization (from the
  path) and the account (from the credential) on every route; organization
  creation additionally charges the account-per-day and installation-per-day
  caps.
- **Git** — anonymous `info/refs` and `upload-pack` charge the per-address and
  installation budgets after the repository is known to exist; authenticated
  Git HTTP and SSH charge the organization and account Git budgets after
  admission; both transports use the share scheduler, as do the web reads.
  A push also measures the organization's total repository bytes and refuses
  before `receive-pack` when the tenant quota would be crossed.
- **Merge** — after building the candidate and before moving the ref, a merge
  measures the repository and the organization and refuses with the same two
  messages a push uses; the candidate's objects stay unreachable until
  housekeeping reclaims them. This is what makes "merge-created objects" in
  the storage paragraph true.
- **Sign-in** — every login path (JSON, browser, Git basic) charges the
  anonymous per-address and per-installation login budget before hashing.
- **Jobs** — the claim query refuses to run more than `job_organization_share`
  jobs for one organization at once, so its backlog cannot occupy every
  worker.
- **Email** — notification and verification sends are charged before delivery;
  an over-budget intent remains queued and retries.
- **Housekeeping** — the job runner's hourly retention tick also prunes the
  fixed-window maps, so an address seen once is not remembered forever.
