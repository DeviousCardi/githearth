# ADR 0006: Durable job execution

Date: 2026-11-07.

Status: **proposed for phase 5**, pending Person A review. The [two-person roadmap](../plan/08-two-person-roadmap.md) phase 5 splits the work:

- **Person A:** transactional job records and domain handlers.
- **Person B:** claiming, retry and recovery execution.

The roadmap requires the interface between them to be agreed before parallel implementation, and this ADR does that.

## Constraints

- **Single binary.** Jobs run inside `githearth-server`, on its PostgreSQL pool. This is for in-process consumers (notifications and search indexing first, webhooks later), not CI runners.
- **The outbox is never delivered.** `outbox_events` rows (migration `0001_foundation.sql`) are written in the domain transaction by `crate::event`, `pushes.rs` (`repository.pushed`) and `merge_intents.rs` (`pull_request.merged`). Nothing reads them and nothing sets `delivered_at`.
- **Fencing.** The [qualification matrix](../plan/07-qualification-and-decisions.md) Jobs row requires fencing and "no stale winning result". `merge_worker.rs` already has the pattern this ADR generalizes:
  - an atomic `FOR UPDATE SKIP LOCKED` claim;
  - a per-process lease holder;
  - fenced updates on (holder, attempt);
  - an attempt cap.

  Issue #56 is the lesson that a claim query needs its own index.

## Decision: a `jobs` table

The claim loop polls a new `jobs` table, not `outbox_events`.

**Why not claim outbox rows directly:**
- The outbox is the audit-linked integration log. One domain event can need zero, one or several jobs.
- Retries, scheduling and dead jobs need columns the log shouldn't carry.

`outbox_events` is unchanged. Whether a future external relay sets `delivered_at` is out of scope.

| Column | Meaning |
| --- | --- |
| `id` | Primary key |
| `organization_id` | Scope, nullable |
| `kind` | Handler key, e.g. `notification.merged` |
| `payload` | `jsonb` handler input |
| `dedupe_key` | Unique per `kind` while `pending` or `running` |
| `state` | `pending`, `running`, `done` or `dead` |
| `not_before` | Earliest time the job may run |
| `attempt`, `max_attempts` | Attempts so far; the limit (default 5) |
| `lease_holder`, `lease_expires_at` | The current claim |
| `last_error` | Last failure reason, kept on `dead` |
| `created_at`, `updated_at` | Timestamps |

The indexes:
- **Claim:** `jobs_claimable` on `(not_before) WHERE state = 'pending'`.
- **Recovery:** `jobs_running_expiry` on `(lease_expires_at) WHERE state = 'running'`.
- **Dedupe:** a unique index on `(kind, dedupe_key) WHERE state IN ('pending','running')`.

## Ownership split

| Person A — application | Person B — infrastructure |
| --- | --- |
| `jobs` migration (columns and indexes above) | Claim loop, leases, fencing, recovery |
| `jobs::enqueue` calls in domain transactions | Backoff, attempt cap, dead jobs |
| Handlers per `kind`, idempotent (below) | Concurrency, per-job timeout, shutdown |
| Choosing which domain events get jobs | `doctor` `jobs` check; retention sweep of `done` |

## Narrow interface

A writes jobs only inside the domain transaction, so a job exists exactly when its domain change committed:

```rust
jobs::enqueue(tx: &mut PgConnection, kind: &str, payload: serde_json::Value,
              dedupe_key: &str, delay: Option<Duration>) -> Result<(), sqlx::Error>
```

- **Duplicates:** a duplicate `(kind, dedupe_key)` that is still active is a no-op (`ON CONFLICT DO NOTHING`), not an error.
- **Ownership:** B implements `enqueue`, because it owns the table's write semantics. A calls it.
- **Scheduling:** `delay` sets `not_before = now() + delay`; `None` means now. The server has no time crate, so delays stay intervals in SQL.

A registers one handler per `kind` at startup:

```rust
type Handler = fn(&AppState, &Job) -> BoxFuture<'_, JobOutcome>;
enum JobOutcome { Done, Retry { after: Duration, reason: String }, Failed { reason: String } }
```

**What each outcome does (B):**
- **`Done`:** the job is finished.
- **`Retry`:** the job runs again after at least `after`.
- **`Failed`:** a permanent failure. The job goes straight to `dead`, without more attempts.

**Errors without an outcome:** B treats a handler panic or a per-job timeout as `Retry` with B's backoff and a recorded reason. There are no other error types.

## Claim, lease and fencing

```sql
UPDATE jobs SET state = 'running', attempt = attempt + 1, lease_holder = $1,
       lease_expires_at = now() + $2 * interval '1 second', updated_at = now()
WHERE id = (SELECT id FROM jobs WHERE state = 'pending' AND not_before <= now()
            ORDER BY not_before FOR UPDATE SKIP LOCKED LIMIT 1)
RETURNING *
```

**Lease length:** the lease is `2 × job_timeout_seconds`. A live worker always finishes, or abandons the job at the timeout, before its lease expires. So a job is claimed again only when its process has died.

**Fenced writes:** every write after the claim is fenced on `id`, `state = 'running'`, `lease_holder` and `attempt`. A worker whose lease expired and whose job was claimed again changes nothing: there's no stale winning result.

**Lease holder:** one UUID per process, as in `merge_worker.rs`.

**Recovery:** a `running` job whose lease expired goes back to `pending`, keeping its `attempt` count. That happens in a fenced update in the same poll, using `jobs_running_expiry`.

## Retry and dead jobs

| Event | Result |
| --- | --- |
| `Done` | `state = 'done'` |
| `Retry`, panic or timeout, with `attempt < max_attempts` | `state = 'pending'`, `not_before = now() + delay` |
| The same, with `attempt = max_attempts`, or `Failed` | `state = 'dead'`, `last_error` kept |
| Lease expired (process died) | Back to `pending`, then as above |

- **Delay:** `max(after, base × 2^(attempt−1))`, with ±25% jitter, capped at 10 minutes. `base` is 1 s.
- **Dead jobs:** kept until an operator deletes them, and never claimed again.
- **Dedupe after a job ends:** `done` and `dead` jobs are outside the dedupe index, so the same key can be enqueued again later.

**At-least-once delivery.** A job can run more than once: after a crash, after a timeout whose handler had already done the work, or after a retry. **A's handlers must be idempotent**:
- the effect of a second run with the same payload must equal the effect of one run;
- they key any external side effect on a stable ID in the payload, such as the outbox event ID or the merge's `journal_sequence`.

## Bounds and shutdown

- **Concurrency:** `job_concurrency` slots (default 4), separate from `git_slots`.
- **Timeout:** each handler runs under `job_timeout_seconds` (default 60).
- **Polling, not LISTEN/NOTIFY:** the loop polls every `job_poll_interval_ms` (default 500), and right away again while it keeps finding work. LISTEN/NOTIFY needs a dedicated connection and catch-up logic after a reconnect, and polling an indexed partial predicate is cheap at this scale. Revisit if measurement shows poll load.
- **Shutdown:** stop claiming and wait for in-flight handlers up to their timeout. Return any job still running to `pending` with a fenced update, so a restart doesn't wait for its lease.

## Merge worker

`merge_worker.rs` keeps its own loop for now. Its lease is tied to Git execution (`git_timeout_seconds`), its claim is scoped by repository, and `merged` is owned by `record_merge_result`. Moving it onto the generic runner would add abstraction for one consumer. Revisit when a second Git-backed job appears.

## Observability

`doctor` gains a read-only `jobs` check:
- **Warns** when any job is `dead`.
- **Warns** when the oldest due `pending` job is more than a minute late.
- **Fails** when a `running` job's lease expired more than `2 × job_timeout_seconds` ago. Recovery should have returned it to `pending`, so the runner isn't running.

**Retention:** B's runner deletes `done` jobs older than `job_retention_days` (default 7) in a periodic sweep. `doctor` never writes.

## Tests (the phase 5 gate)

`tests/jobs.rs` runs against a real database with test handlers.
- **Duplicate workers:** two concurrent loops run each of N jobs exactly once.
- **Crash mid-job:** a process dies with a job running; after the lease expires, another loop runs it.
- **Stale completion:** a completion with the old attempt or holder after a reclaim changes nothing.
- **Backoff:** `Retry` and timeouts advance `not_before` within the jitter bounds.
- **Attempt cap and `Failed`:** the job ends `dead` with `last_error`, and isn't claimed again.
- **Dedupe:** an active duplicate key is a no-op; after `done`, the same key enqueues.
- **Shutdown:** no new claims; a job still running goes back to `pending`.
- **Idempotent redelivery:** a handler that ran but timed out runs again, and its effect is applied once (the test handler's own key).

## Consequences

- Delivery never blocks a domain transaction beyond one `INSERT`.
- Handlers carry the idempotence burden; this ADR states it as their contract.
- Polling costs one indexed query per interval while idle.

## Open items for Person A

1. Which events get jobs first. Recommended: notifications for `pull_request.merged` and review requests.
2. Per-kind `max_attempts`. Default 5; A can set it per `enqueue` later if needed.
3. Confirm leaving `merge_worker.rs` on its own loop for phase 5.
