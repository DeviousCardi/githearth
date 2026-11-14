# ADR 0011: PostgreSQL high availability

Date: 2026-11-14; rewritten 2026-11-15 after the second plan review.

Status: **design accepted for M5; settings named, not yet measured.** Nothing
here is implemented. This narrows the provisional selection in
[ADR 0001](0001-metadata-database.md) for the M5 topology; it does not close
the Gate C production qualification. The settings below are the decision; Gate
C is the evidence that they deliver the stated behaviour.

## Context

Roadmap M5 requires synchronous durable acknowledgement, a qualified failover
manager, safe promotion, fencing, WAL/backup retention and connection
recovery. If safe promotion cannot be established, writes stop.

The earlier text claimed that "one node can be lost without losing an
acknowledged write" and, in the same paragraph, that "with fewer than two
healthy nodes, writes are unavailable". With one synchronous standby those two
claims are inconsistent: if the acking standby is the node that dies, the
surviving standby may be behind and the acknowledged commit is lost. A plan
that documents the doubt instead of choosing is not a decision.

Options considered: Patroni with etcd; pg_auto_failover; CloudNativePG
(Kubernetes only); a failover manager built into GitHearth on the Raft library
from [ADR 0010](0010-git-replication.md); and requiring an operator-provided
highly available PostgreSQL.

## Decision

One supported topology, with the trade-off stated rather than hidden.

### Configuration

Three PostgreSQL nodes on independent failure domains, with Patroni and etcd
on the same three machines. The patched settings:

```yaml
# patroni.yml — identical on all three nodes except name/connect_address
bootstrap:
  dcs:
    ttl: 30
    loop_wait: 10
    retry_timeout: 10
    maximum_lag_on_failover: 1048576
    synchronous_mode: true
    synchronous_mode_strict: true
    synchronous_node_count: 2
    postgresql:
      use_pg_rewind: true
      use_slots: true
      parameters:
        synchronous_commit: "on"
        wal_level: "replica"
        wal_log_hints: "on"
        max_wal_senders: 10
        hot_standby: "on"
watchdog:
  mode: required
  device: /dev/watchdog
```

- `synchronous_mode: true` makes Patroni manage
  `synchronous_standby_names` from the `/sync` key. `synchronous_node_count: 2`
  with three nodes means **both standbys must acknowledge**. With one
  synchronous standby the acking node can die and take the only copy with it;
  that configuration is not supported.
- `synchronous_mode_strict: true` stops writes rather than silently falling
  back to asynchronous commit when the sync set cannot be formed. Plain
  `synchronous_mode` falls back, and a fallback is exactly the acknowledged
  write this topology must not lose.
- `synchronous_commit: "on"` waits for the standby to flush to durable
  storage. `remote_write` acknowledges before the standby flushes and is not
  sufficient for this claim.
- The watchdog is **required**. An isolated primary that can still reach a
  standby but not etcd can otherwise keep committing while another primary is
  elected. `mode: required` makes Patroni refuse to run without a watchdog
  rather than degrade silently. A host without a watchdog is not a supported
  M5 topology.

### What the configuration guarantees, and what it costs

- **Durability:** every acknowledged commit is on the primary and both
  standbys. Losing any one node leaves at least one node with every
  acknowledged commit, and Patroni promotes a member of the `/sync` set, so no
  acknowledged commit is lost.
- **Availability:** losing one node costs write availability until two
  standbys are in sync again, because the primary cannot acknowledge without
  both. Reads continue on the primary; a standby can serve read-only queries
  when the deployment routes them there. This is a deliberate choice of
  durability over availability, and the roadmap states it.
- **Fencing window:** Patroni's watchdog is reset while the member holds its
  lease. If it cannot renew within the TTL, the watchdog expires and the
  postmaster is terminated, so an isolated primary stops writing rather than
  continuing. The window is bounded by the TTL and measured in Gate C; it is
  the only window in which two writers can exist.
- **Quorum:** losing two of the three etcd members stops elections. Writes stop
  when the primary can no longer confirm its role; they do not continue
  unsafely.

### Application contract

- GitHearth connects to the current read-write primary, through an HAProxy (or
  equivalent) that checks Patroni's `/primary` endpoint. Driver-side multi-host
  connection is not the default until SQLx support is verified; a proxy is the
  supported route.
- The pool drops a connection that receives a read-only error (SQLSTATE
  25006) and retries on the new primary. A connection to a demoted node must
  never serve a write decision.
- A commit that times out while waiting for the standby is **unknown**, not
  rolled back: the transaction may have committed locally and become visible
  when the standby catches up. It is reported as unknown and resolved by the
  operation ID, exactly as ADR 0003 requires.
- **Authorization facts for a Git write are read from the current primary at a
  recorded position.** The read and the write decision happen on a node that
  can commit; a read-only replica, or a node that cannot confirm it is
  authoritative, refuses the write. The position is recorded in the log entry
  (ADR 0010), so a replicated write can be tied to the metadata state it was
  authorized against. Without this rule a partitioned former primary could
  authorize a Git write from facts that are about to disappear, while the Git
  group independently commits it.
- **Schema compatibility for a rolling upgrade is M5's, not M11's.** `serve`
  refuses to start when the database schema does not match the binary, so an
  N/N+1 cluster is possible only when the migration is expand-only for the
  releases M5 supports: additive columns and tables, no destructive change, and
  a compatibility check that accepts the N and the N+1 schema for the upgrade
  window. The roadmap's M5 exit and case U50 carry this; the general
  expand/contract discipline for later releases stays M11's.

Deferred, in this order of likelihood:

| Item | When | Note |
| --- | --- | --- |
| Operator-provided HA PostgreSQL (managed services, CloudNativePG) | After M5 | Needs the same required-settings list and a `doctor` verification command; the application contract already allows it |
| Kubernetes packaging for GitHearth's own roles | M11 | Uses the operator-provided database path |
| Built-in failover manager on the product's Raft library | After the Git Raft layer passes its fault tests | Would remove Patroni and etcd; adopted only with its own ADR and fault evidence |

## Consequences

- A clustered installation gains Patroni and etcd to install, upgrade and
  monitor, and a watchdog device on every database host. The `setup` command
  grows to manage them in M5.
- One node loss stops metadata writes until the node is replaced. This is
  visible and must be published; it is not a defect to be tuned away without
  changing the durability claim.
- Qualifying one topology keeps the M5 fault matrix bounded. Other topologies
  are unsupported until separately qualified.
- Because the application sees only an endpoint, replacing the manager later
  changes no application code.

## Gate C evidence this decision requires

1. Kill the primary; every acknowledged commit is present on the promoted
   node; clients resolve unknown outcomes by operation ID.
2. Kill the acking standby; no acknowledged commit is lost, and writes stop
   until a second standby is in sync.
3. Partition the primary from etcd but not from the standbys; the watchdog
   stops it before a new primary accepts a write, and the window is measured.
4. Kill two etcd members; writes stop, and no split brain appears.
5. Cancel a client during commit; the commit is either absent or present, and
   the reported outcome is unknown, never a false rollback.
6. Measure commit latency and throughput for the strict two-standby setting on
   the reference hardware; publish the cost.
7. Restore from an external backup and from a WAL archive; verify the promoted
   primary's position.
8. Fail over under load and verify the pool drops and reconnects; verify a
   read-only error never becomes a write decision.
9. Roll one node to N+1 while an N binary still serves: the N binary starts
   against the N+1 schema, serves reads and writes, and a destructive migration
   is refused.

## Open items

1. The exact HAProxy health configuration and the pool's retry parameters.
2. etcd data directory placement, backup and restore, and the interaction of a
   restored etcd revision with a promoted primary.
3. Watchdog device choice and the measured timeout on the reference hardware.
4. WAL retention values and the archive destination.
5. The `doctor` checks that verify each setting above on an installed cluster.
6. The HA row of
   [the fault matrix](../plan/07-qualification-and-decisions.md#correctness-and-fault-matrix)
   as executable tests on independent machines.
