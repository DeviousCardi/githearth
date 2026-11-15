# Outbox delivery and dead letters

A relay in `githearth-server` reads committed `outbox_events` rows in
audit-chain order and enqueues one durable job per registered consumer. The
job's retry, lease and dead-letter state lives in `jobs`, not on the audit log;
the design and its limits are in [ADR 0006](../decisions/0006-durable-jobs.md)
and its "Revision for M4.1". This page is the operator's view.

## What a delivery is

Each `(event, consumer)` pair has one row in `outbox_deliveries`. The row is the
record that a consumer was owed the event; it is created in the same transaction
that enqueues the consumer's job. The event's `delivered_at` means the fan-out
committed for every consumer due the event, **not** that every handler
succeeded. The job's own state remains the authority on whether the handler
finished.

The built-in `notification.outbox` consumer takes the pull-request events that
produce in-app notifications. `codeowners.request_reviews` is not an outbox
consumer: it is still enqueued directly by the code that creates or refreshes a
pull request.

## Delivery states

| State | Meaning | What to do |
| --- | --- | --- |
| `pending` | Waiting for its next attempt. `next_attempt_at` is when the job becomes due. | None; the runner claims it when due. |
| `delivering` | Its job is running now. | None; a crashed runner is recovered by the job lease. |
| `delivered` | The handler finished successfully. | None. The job is later removed by job retention. |
| `dead` | The attempt budget is exhausted (or the handler returned a permanent failure). | Fix the cause, then `jobs requeue`. `doctor` warns until it is recovered. |

A `pending` delivery that has never run is normal for a new consumer: its
registration position is the head of each chain, so it has no old backlog. A
consumer always has a handler: `outbox::register` refuses a consumer with no job
kind (ADR 0006, revised for M4.1), and the schema admits NULL only so a later
release can add an audit-only consumer deliberately. The `outbox` check reports
`N consumer(s) without a handler; deliveries stay pending` if a row with a NULL
`job_kind` ever exists with undelivered deliveries (an upgraded or hand-edited
database): `doctor` says so by name, and nothing in this release creates one.

## Retry budget

The delivery row carries the budget and the job is enqueued with it: 30 attempts,
base 1 second, doubling, capped at 10 minutes. That is 29 retry gaps:
`1+2+4+8+16+32+64+128+256+512` seconds, then 19 gaps at the 600-second cap =
**12,423 seconds (3 h 27 min)**. An ordinary 30-minute mail outage is absorbed
well inside the budget. Attempts, `next_attempt_at` and the last error are on the
delivery row; the job carries the same cap so a retry does not silently fall back
to the generic five-attempt default.

A delivery's cap (and its attempt budget) is **fixed at enqueue time**: the relay
copies the consumer's `max_attempts` onto the delivery row and onto its job, and
its `backoff_cap_seconds` onto the job. Raising or lowering a consumer's cap
later changes the deliveries created afterwards only; a delivery already queued
keeps the values it was created with. So a cap lowered to shorten a stuck
backlog does not shorten the deliveries already in it.

`jobs requeue` is the exception: it reenqueues with the row's `max_attempts` but
with the consumer's **current** `backoff_cap_seconds`, so a requeued delivery
picks up a cap that was changed after it was first created.

One consumer's failures never delay another's: each `(event, consumer)` is its
own job, so a failing handler retries on its own schedule while other consumers
receive the same event normally.

## Dead letters and `jobs requeue`

A delivery goes `dead` when its budget is exhausted or its handler reports a
permanent failure. The row keeps the attempt count and `last_error`; the event
itself and every other consumer's delivery are untouched. `doctor` reports the
count in its `outbox` check and names the command.

List the dead rows:

```sh
githearth-server --config /etc/githearth/githearth.toml jobs list
```

Re-enqueue them:

```sh
githearth-server --config /etc/githearth/githearth.toml jobs requeue
githearth-server --config /etc/githearth/githearth.toml jobs requeue --consumer notification.outbox
githearth-server --config /etc/githearth/githearth.toml jobs requeue --job-key outbox-deliver:<event-uuid>:<consumer-uuid>
githearth-server --config /etc/githearth/githearth.toml jobs requeue --dry-run
```

`--consumer` and `--job-key` narrow the selection; without either, every dead
delivery is requeued. `--dry-run` prints what would happen and writes nothing.
The command is safe to run while the server is up: it takes a row lock per
delivery, so a concurrent redelivery or a second requeue cannot double it.

A requeue uses a **new** deterministic key, `<old key>:r1` (then `:r2`, and so
on), and gives the job the delivery row's own attempt budget again. The old key's
finished job cannot absorb the new one. The delivery goes back to `pending` with
its attempt count reset. If a requeued job for the row is still pending or
running, the row is skipped rather than doubled.

## What `doctor` reports

- `outbox` warns when any delivery is `dead`, with the count and the requeue
  command. It also warns when a registered consumer has no handler and holds
  undelivered rows. Otherwise it reports the pending, delivering and delivered
  counts and passes.
- `jobs` still warns about dead jobs and late pending jobs, but outbox delivery
  jobs are excluded from the "late pending" test: a new consumer's backlog and a
  delivery waiting out its backoff are late by design. Dead outbox deliveries are
  the `outbox` check's business, not `jobs`.
- Neither check writes.

## Upgrades and the cutover watermark

The built-in consumer's notifications are keyed on the source rows their events
name (the merge intent's `operation_id`, the `review_id`, the `request_id`), not
on the audit event id. Rows a pre-`0050` release wrote keep the audit event id in
`notifications.event_id`: there is no backfill, so the uniqueness of those rows
is still per event. A replay of a journal entry that happened before the upgrade
can therefore still notify twice; one after it cannot. See ADR 0006, "Revision
for M4.1".

Migration `0050_outbox_fanout.sql` writes one position per audit chain for the
built-in consumer, at that chain's head at migration time. A consumer receives
only events with `seq` greater than its position, so nothing from before the
upgrade is delivered to anybody. A consumer registered later starts at the head
of each existing chain; a chain created after it registered has no position and
is delivered from its own first event. See
[upgrade.md](upgrade.md) for the general migration procedure.
