# ADR 0006: Durable job execution

Date: 2026-11-07.

Status: **accepted** for phase 5, after Person A's review on 2026-11-07 (PR #63). The [two-person roadmap](../plan/08-two-person-roadmap.md) phase 5 splits the work:

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
| `organization_id` | Scope, nullable; `REFERENCES organizations ON DELETE CASCADE`, so deleting an organization leaves no orphan jobs |
| `kind` | Handler key, e.g. `notification.merged` |
| `payload` | `jsonb` handler input: IDs only, never content (see the handler contract) |
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
| `jobs::enqueue` calls in domain transactions | `jobs` migration (columns and indexes above) and `jobs::enqueue` |
| Real handlers per `kind`, idempotent (below) | Runner: claim loop, leases, fencing, recovery, backoff, dead jobs |
| Choosing which domain events get jobs | Handler registry, concurrency, per-job timeout, shutdown |
| | `doctor` `jobs` check; retention sweep of `done`; test handlers |

B owns every statement against `jobs`, so B ships the table with them. A's handler PRs then depend only on B's merged runner.

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

**Poison pills:** the claim increments `attempt`, so a job that kills its process reaches `attempt = max_attempts` and then has its lease expire. Recovery therefore sends it to `dead` with `last_error = 'lease expired'` when `attempt >= max_attempts`. A crash loop ends after `max_attempts` claims instead of claiming forever.

## Retry and dead jobs

| Event | Result |
| --- | --- |
| `Done` | `state = 'done'` |
| `Retry`, panic or timeout, with `attempt < max_attempts` | `state = 'pending'`, `not_before = now() + delay` |
| The same, with `attempt = max_attempts`, or `Failed` | `state = 'dead'`, `last_error` kept |
| Lease expired (process died), `attempt < max_attempts` | Back to `pending` |
| Lease expired, `attempt >= max_attempts` | `state = 'dead'`, `last_error = 'lease expired'` |

- **Delay:** `max(after, base × 2^(attempt−1))`, with ±25% jitter, capped at 10 minutes. `base` is 1 s.
- **Dead jobs:** kept until an operator deletes them, and never claimed again.
- **Dedupe after a job ends:** `done` and `dead` jobs are outside the dedupe index, so the same key can be enqueued again later.

**Handler contract.**
- **Idempotent, because delivery is at-least-once.** A job can run more than once: after a crash, after a timeout whose handler had already done the work, or after a retry. A second run with the same payload must have the same effect as one run.
- **Stable keys.** Any side effect is keyed on a stable ID in the payload, such as the outbox event ID or the merge's `journal_sequence`.
- **IDs, not content.** Payloads carry identifiers, for example the outbox event ID and the pull request ID, never content. `dead` jobs are kept indefinitely, so they mustn't retain private data. The handler loads current data when it runs.

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
- **Poison pill:** a job that crashes its runner every time ends `dead` with `last_error = 'lease expired'` after `max_attempts` claims.
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

## Settled with Person A

1. **First jobs:** `pull_request.merged` and review-requested notifications. A adds an in-app `notifications` table as the handler's idempotent target, keyed on (event ID, recipient).
2. **`max_attempts`:** 5 for every kind for now. A per-`enqueue` parameter is added when a kind needs one.
3. **Merge worker:** stays on its own loop for phase 5.
