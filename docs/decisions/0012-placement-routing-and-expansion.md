# ADR 0012: Placement, routing, follower reads and expansion

Date: 2026-11-14; rewritten 2026-11-15 after the second plan review.

Status: **design accepted for M5.** Depends on [ADR 0010](0010-git-replication.md)
and [ADR 0011](0011-postgresql-ha.md). Nothing here is implemented; the open
items are implementation choices.

## Context

With repositories held by three-node groups, a gateway must find the group for
a repository, reads should use all three copies, repositories must be movable
between groups, and an existing single-node installation must become a cluster
without changing IDs, URLs or features.

The earlier correction fixed the seal but left the move without a commit point
at the target, without an abort path, and with two competing fences (a Raft
seal and an "epoch" that no one owned). It also left the read rule as a
sentence rather than a per-endpoint contract. This ADR replaces both.

## Decision

### Placement table

PostgreSQL records the mapping, and nothing else, is authoritative for it:

- `repository_groups(id, generation)` — one row per group.
- `group_members(group_id, node_id, state)` — a cache of Raft membership, never
  the authority for it.
- `repository_placement(repository_id, group_id, generation)` — where a
  request is sent.
- `repository_moves(id, repository_id, source_group, target_group, position,
  state, updated_by, updated_at)` — one row per move, and the single decision
  record for it.

Consistent hashing and gossip were rejected: hashing makes moving one large
repository and rebalancing hard, and gossip makes a single write authority
during a move harder to guarantee and to audit.

A gateway caches placement with its generation. A storage node that receives a
request with an older generation, or for a repository it does not hold,
answers "stale route" and the caller refreshes. The generation is a **cache
invalidation token, not a fence**: a stale gateway cannot acquire write
authority by presenting a generation, because only the group leader commits.

### Read consistency, per endpoint

The rule is stated per endpoint because the cost and the acceptable staleness
differ:

| Endpoint | Rule |
| --- | --- |
| API reads that follow a write by the same client (page after merge, detail after push) | Read-index at the leader; the response carries the position |
| Git `ls-refs` + `fetch` from an authenticated user | Read-index; the position is fixed for the whole request, so refs and pack agree |
| Merge worker reading a candidate, CI checkout after a push event | Read-index against the event's position |
| Anonymous clone/fetch of a public repository | A member that has applied within 2 log positions and 5 s may serve; the configured maximum staleness is published |
| Anonymous page/API read of a public resource | The same bounded-staleness rule; private resources never use it |

Read-index is one leader round trip; batching covers `ls-refs` and `fetch`.
Without a quorum a read that requires a position fails rather than serving
stale. "Applied the position" and "holds the objects" are separate conditions,
and a member must satisfy both (ADR 0010).

### Moving a repository

A move is a state machine with exactly one decision point for each transition.
The record is `repository_moves`; every transition is a compare-and-swap on
`state` and increments `updated_at`, so two coordinators cannot both proceed.

1. **`prepared`** — the move row is created with the source group, target
   group and the source position `P`. Nothing is sealed.
2. **`copying`** — objects and refs at `P` are copied to the target group's
   members and verified against the manifest. A failure here returns to
   `prepared` or to `aborted`; the source still serves writes.
3. **`sealed`** — a `seal` entry commits in the **source** log at position
   `F`. From `F` the source refuses new writes for the repository; it still
   serves reads at `F`.
4. **`imported`** — entries `P+1..F` are replayed to the target and an `import`
   entry carrying the repository ID, per-repository sequence `F` and the object
   manifest commits on a **target majority**. The target holds the repository
   in a non-serving `importing` state. Until this commits, the move can still
   be aborted.
5. **`switching`** — the move row is compare-and-swapped from `imported` to
   `switching`, which also increments the placement generation. Exactly one
   coordinator wins this CAS. The target begins refusing requests with "not
   serving yet".
6. **`released`** — a `release` entry commits in the **source** log. From it
   the source refuses **reads and writes** for the repository and answers a
   stale route with the new generation. The placement row is updated to the
   target with the new generation, and the target starts serving.
7. **Cleanup** — the source's copy is retained for its normal trash retention
   and then removed.

Order matters: the source is released before the target serves, so a gateway
holding a cached route to the source cannot read the old copy after the
switch. The window in which neither serves is bounded and returns
`503`/`stale route`, never a stale read.

**Abort paths.** Before `imported`, the move can be aborted: a `unseal` entry
commits on the source and the target's copy is discarded. Between `imported` and
`switching`, abort requires a `discard` on the target and an `unseal` on the
source, in that order. After `switching`, there is no abort: the only route
back is a new reverse move, because the placement decision is already durable.
A target that dies after `imported` therefore leaves the repository readable (at
the source, sealed) and unwritable until the target recovers or a reverse move
runs; that is stated, and it is why abort is a documented operator action
rather than an automatic rollback.

**Epoch.** There is no separate epoch check on writes. The seal is the fence;
the routing generation only tells a caller to refresh. Two fences were the
earlier defect.

### Expansion in place

A single-node installation is a one-member group.

1. Add **both** new members as non-voting learners.
2. Seed them from a snapshot and objects (ADR 0010) and catch up their apply
   index and object watermark.
3. Verify both are caught up, then promote both. Promote both only after both
   are ready: promoting one at a time creates a two-voter window in which
   losing either node stops writes, and the original node is then the only
   remaining member.
4. PostgreSQL is expanded by its own procedure: add two nodes, let them catch
   up, raise `synchronous_node_count` to 2 and set strict mode only when both
   are in sync. Introducing strict mode earlier would stop writes; later would
   keep the weaker guarantee.

Restoring a backup into a new cluster remains available as the fallback.

### Conversion of an existing installation

Conversion is a maintenance-window procedure, not an online migration:

1. Enter maintenance mode (no new writes, drain).
2. Take a verified backup (M4.2 online path or the maintenance-window path).
3. Migrate each repository's journal into the replicated log, one repository
   at a time, with a checkpoint; a failure returns to the backup.
4. Add the two PostgreSQL nodes, catch up, then enable strict two-standby
   commit.
5. Add the two Git learners, catch up, promote both.
6. Run the re-qualification drills, then leave maintenance mode.
7. **Rollback** is a restore of the taken backup plus journal replay; the loss
   window is the conversion itself and is stated to the operator. There is no
   in-place downgrade.

### Rolling upgrade

- Upgrade the **followers first**, one at a time, then the leader last.
- A new-format log entry is proposed only when the minimum voter version can
  apply it; an older voter **applies and ignores** an unknown entry kind and
  still advances its apply index. Ignoring is not the same as refusing: a
  follower that refuses to apply stalls replication and can cost quorum.
- GitHearth refuses to start on a schema newer than the binary, so a rolling
  upgrade requires N/N+1 schema compatibility for the metadata database too.
  That schema work is M11's; M5 owns the Git log format and the mixed-version
  test for it.

### Backup interaction

- A backup records the move state for every repository. A repository in
  `copying`, `sealed` or `imported` is recorded as mid-move; a restore of such a
  backup **refuses** to serve that repository until an operator resolves the
  move, rather than guessing which copy is authoritative.
- A backup cannot be taken while a move is in `switching`; the backup retries
  after the switch or after the abort.
- A restore whose placement generation is older than the storage nodes'
  generation is refused; a generation is never rolled back by a restore.

### Capacity model

- A repository group is three members and holds up to 1,000 repositories in
  the initial model.
- A storage node hosts at most three group members and is sized at 8 TiB of
  usable disk and 5,000 sustained IOPS for the initial profile.
- A repository larger than the per-repository quota is refused at creation or
  push; a single repository is not sharded in M5.
- The minimum qualified topology is six machines: three metadata nodes (with
  etcd) and three storage nodes holding one group. Co-locating a metadata node
  and a storage member on one machine is unsupported, because one machine loss
  would then remove both a database node and a group member.
- Groups per node, repositories per group and the many-organization profile are
  measured in M5's capacity item and published with the numbers above revised
  from measurement, not asserted here.

### Fork placement

A fork is placed in its parent's group by default, so the copy is local and its
`adopt` entry does not cross a group boundary. A cross-group fork is allowed, at the
cost of a full object transfer, and is recorded as such.

## Consequences

- Routing lookups depend on PostgreSQL only on a cache miss or a stale route;
  the common path is a cached lookup.
- Follower reads do not raise read capacity towards three times one node, as
  the earlier text claimed: reads that need a position pay one leader round
  trip, and only anonymous public reads use a follower without one. The gain
  is measured per endpoint.
- Moves take longer and can end in an operator-visible dead end after
  `switching`; the alternative is a switch that can be undone and therefore
  two authoritative copies.
- Conversion is a maintenance window with a stated loss window, not an online
  migration.
- The expansion procedure is a controlled sequence, never a copy of a live
  data directory, as the deployment contract requires.

## Open items

1. Schema and migration numbers for groups, members, placement and moves.
2. The exact read-index request and its timeout, and the batching for
   `ls-refs` + `fetch`.
3. Rebalancing policy: what triggers a move, and the limit on concurrent
   moves.
4. The operator runbook for a move that reaches `switching` and then fails.
5. The placement row of
   [the fault matrix](../plan/07-qualification-and-decisions.md#correctness-and-fault-matrix)
   as executable tests: interrupted move, stale route, membership change.
6. Tests for a push racing each step of a move, a partitioned source group, a
   target death after import, an abort after sealing, a stale gateway read
   after release, and an interrupted move at each boundary.
7. Restore with a placement generation older than the storage nodes', and a
   repository that was mid-move when the backup was taken.
8. The mixed-version test harness and the exact entry-version negotiation.
