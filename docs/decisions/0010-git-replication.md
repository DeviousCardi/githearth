# ADR 0010: Replicated Git journal

Date: 2026-11-14; rewritten 2026-11-15 after the second plan review.

Status: **design accepted for M5.** This records the protocol, not an
implementation. The open items are implementation choices; the rules in
"Decision" are not optional. Nothing here is implemented.

## Context

Roadmap M5 requires that one supported node loss loses no acknowledged write,
that partitions cannot produce conflicting writers, and that a missing quorum
stops writes. Git log commitment and object durability are one protocol: an
acknowledged ref keeps an object recovery path.

Today (ADR 0003, ADR 0004) a push or merge writes objects and one journal entry
on a single disk, then commits metadata to PostgreSQL. The recovery rule treats
"prepared journal record, refs still at the old value" as aborted, and the
`reference-transaction` hook decides by returning success or failure to Git.
Both are single-node rules and neither survives replication:

- A two-valued hook cannot express "the log has not decided yet", and mapping
  that state to failure lets a client be told `ng` for a ref that later
  commits.
- A prepared record that is really a Raft entry cannot be "aborted locally":
  the log decides, and the local filesystem must follow.

The forge also hosts many organizations on one installation, so the failure of
one node or of the metadata database must not decide Git failover for every
repository.

Three approaches were considered:

- **A. Consensus log per repository group.** Objects are shipped to the
  followers first; the ref update commits through a Raft log once a majority
  holds the objects durably.
- **B. Primary with synchronous followers, coordinated through PostgreSQL.**
  Simpler at first, but acknowledging at two of three nodes requires
  reconciliation and safe-promotion rules, which amounts to a hand-written
  consensus protocol, and every Git failover depends on PostgreSQL.
- **C. Shared replicated storage under a single writer.** Least code, but it
  hands durability and fencing to the operator's storage layer.

## Decision

Approach A, with the commit point below.

### Terms

- **Group:** three storage nodes on independent failure domains running one
  Raft log. A group holds many repositories; a repository does not get its own
  Raft instance.
- **Entry:** one committed record in the group log. Kinds: `ref-update`,
  `create`, `adopt` (fork), `delete`, `prune`, `barrier`, `membership`.
- **Manifest:** the set of objects an entry names, identified by a digest and
  a list of pack-level content hashes. Objects are content-addressed by Git's
  own object ID, so the manifest is verifiable without trusting the sender.
- **`repo_seq`:** a per-repository sequence carried inside entries. It is
  assigned at apply time from the previous applied seq, preserved across a
  move, and never replaced by the group log index. Metadata keys keep using
  `(repository_id, repo_seq)`.
- **Commit index:** the highest log index a majority has appended durably.
  **Apply index:** the highest index a member has applied. **Object
  watermark:** the highest `repo_seq` for which the member holds every object
  in that entry's manifest.

### The commit point

A ref is acknowledged only after the log entry that carries it is committed.
The sequence for a push:

1. The gateway authenticates the client, resolves placement, and forwards the
   request to the group leader. The leader runs `receive-pack`, which writes
   the pack into quarantine and fsyncs it.
2. The leader ships the pack to the other members. A member acknowledges only
   after it has written the pack durably and verified it with
   `index-pack --verify`. A majority, including the leader, must hold it.
3. The leader proposes one entry carrying the operation ID, `repo_seq`,
   expected old values, new values, actor, the policy position, and the
   manifest digest.
4. The entry commits when a majority appends and fsyncs it.
5. Only after commit does the leader let Git's `reference-transaction` hook
   return success, so Git writes the ref locally and `receive-pack` reports
   `ok` to the client.
6. All members apply committed entries in log order. The leader's local write
   in step 5 is an idempotent roll-forward of an already committed decision; if
   the leader dies before it runs, the new leader applies from the log.

The hook's contract is therefore:

- It returns success only for a committed entry.
- It returns failure only for a decision that is final before any proposal:
  authorization denial, policy denial, expected-old mismatch, a sealed
  repository, or a malformed request. Those are safe rejections.
- On any uncertainty — leadership lost, no quorum, the decision not yet known,
  a timeout, a node that cannot reach the log — it **returns neither**. The
  server aborts the request by killing `receive-pack` and closing the
  connection, so the client receives no verdict. A timeout is never converted
  into `ng`.
- An operation ID is persisted with the proposed entry, so a retry discovers
  the committed result instead of re-executing. An entry is unique on
  `(repository_id, op_id)`.

Two implementation constraints follow, and neither is optional:

- The reference-transaction channel must not reuse the pre-receive socket's
  30-second read timeout, and a transport error on it must abort the request,
  not become a rejection. The existing channel turns every error into `DENIED`;
  that behaviour is correct for `pre-receive` and wrong here.
- The single-node classifier that treats "prepared record, refs still at the
  old value" as aborted is removed. Without a decision from the log, that state
  is **Unknown**: the reconciler waits for or retries the proposal, and never
  rolls the ref back on the strength of a missing marker.

This is what makes "no acknowledged loss" and "no false rejection" compatible:
`ok` implies committed, and the only alternative to `ok` is silence.

### Object durability for every ref writer

"Objects before log" applies to every writer, not only to a pushed pack:

- A merge candidate is built on the leader outside any pack. Its objects reach
  a majority with a manifest before the entry that moves the ref.
- A fork (`adopt`, below) carries the copy's refs and manifest; the objects
  reach the target group's majority before the entry is proposed.
- A fork-pull fetch and an applied suggestion get a manifest before their
  entry. The fetch itself writes no journal entry, and it does not need one:
  the pull request's pin (M4.1's `pinned_commits`, carried into the replicated
  keep-set) makes the fetched object graph live from the moment the pull
  request commits, and the fetch runs under the repository's maintenance lock.
  The requirement is that the pin is committed before any prune can run, not
  that the fetch is journaled.
- A prune entry, which deletes rather than creates, carries the keep-set it
  was computed from (see Housekeeping).

### Repository lifecycle is in the log

Repository create, fork and delete are state-machine commands, not local
filesystem work:

- **`create`** carries the repository ID, its object format (SHA-1 or
  SHA-256, fixed at creation) and its empty initial state. It is committed by
  the group before the HTTP `201`; every member creates the bare repository on
  apply. A repository created on one member therefore exists on all of them,
  and a leader change after creation leaves it available.
- **`adopt`** is a fork. The new repository's objects are copied to the target
  group's members and verified against the manifest, the entry carries the
  initial refs and the manifest, and it commits before the `201`. The fork's
  refs are journaled by construction, so ADR 0008's "own journal starting
  empty" no longer hides unreplicated state.
- **`delete`** carries the retention interval. Members move the repository to
  trash at the same log position, so a member that missed the delete cannot
  keep serving it, and a restore from that member's backup is reconciled
  against the log.

A member that missed any of these recovers it from the log or a snapshot.

### Object completeness gates leadership, voting and membership

Objects-before-log guarantees a majority at commit time. Keeping that true
afterwards requires the property to be visible to Raft:

- Each member reports its object watermark and the manifests it holds.
- **A member appends or acknowledges a ref entry only when it already holds
  that entry's manifest.** Before then it takes no part in that entry's
  quorum, and the leader does not count it. This is what ties the object
  majority to the commit majority: the set that can commit is a subset of the
  set that holds the objects, so any majority that commits an entry also holds
  it. Without this rule the two majorities can differ, and a leader election
  can hand the repository to a member that never received the pack.
- The rule has a cost that is stated, not hidden. A Raft log is contiguous, so
  a member that cannot append one entry cannot append any later entry of the
  group: while it waits for one repository's pack it falls behind on every
  repository in the group. The leader proposes only after a majority holds the
  pack, so the group keeps committing; the third member lags and catches up.
  With one member already down, a slow transfer to the remaining follower
  stalls the whole group's writes. Whether the chosen library lets a follower
  defer an append this way is part of the library spike (open item 1).
- A member may serve as leader for writes, or serve reads, only if it holds
  every manifest up to the commit index for the repositories it serves. A
  member that does not steps down from write service and requests catch-up;
  it does not serve a ref it cannot back with objects.
- A learner is promoted to voter only when its apply index and its object
  watermark are caught up to the commit index.
- A membership change that would leave any committed manifest with fewer than
  two holders is refused. A three-node group therefore cannot lose the last
  two holders of a committed object set through a planned change.

### Apply and read ordering

The group log is one total order, but applying it is not one queue for every
repository:

- Entries carry `repo_id`; apply runs concurrently per repository, and a
  per-repository barrier stops a read from overtaking its own repository's
  apply. On a member that has appended the entries, a slow apply on one
  repository does not stall another repository's apply or reads. Append is
  different: see the cost stated under object completeness.
- A read serves only a member that has both applied the required position and
  holds the objects. Those are separate conditions (ADR 0012).
- The read-index default is defined per endpoint in ADR 0012; an anonymous
  public clone may serve bounded-stale, an authenticated or post-write read may
  not.

### Metadata interaction

- The ref entry commits first; the metadata commit (ref rows, audit, outbox)
  follows in PostgreSQL. If metadata is unavailable, the push has still
  succeeded and the metadata commit is retried by the reconciler from the
  entry's operation ID. This is the existing ADR 0003 order with a replicated
  first step.
- Policy is evaluated at the gateway before proposal, using metadata read from
  the current primary at a known position; that position is recorded in the
  entry. Apply re-checks only deterministic conditions — expected old values,
  seal state, object presence — so two members applying the same entry decide
  identically.
- The in-process `repository_lock` (ADR 0004) stops being the durability
  authority. The leader is the only writer for a repository, and the lock
  remains a local optimisation around apply. A gateway that is not the leader
  forwards; it never writes a ref.

### Housekeeping

- A `prune` entry carries the keep-set computed at a `repo_seq` (from ADR
  0004 and M4.1): every ref, every object reachable from an object ID
  PostgreSQL references, and every in-flight intent. It is a statement of the
  object set, not a command to run `git prune` identically everywhere.
- Each member deletes unreachable objects that are not in the keep-set,
  **except** objects it holds for a sequence not yet committed, which are
  protected by a local in-flight lease: its own quarantine packs and any
  objects it has received for an uncommitted entry. Prune never deletes
  quarantine or incoming transfer state.
- Repack is local and deterministic in the object set, not in pack bytes; pack
  boundaries and deltas legitimately differ between members.

### Compaction, snapshots and retention

- A snapshot is a per-group capture of each repository's applied `repo_seq`,
  refs, and the manifest of live objects. A new or rebuilt member is seeded
  from a snapshot plus objects from any member that holds them, then catches
  up from the log.
- Compaction may discard entries below the minimum of the applied position and
  the oldest retained backup's position, **and** is bounded by a retention size
  with an alert. A stalled backup must not fill the disk; the log is trimmed by
  size before it is trimmed by the backup's position.
- **An entry whose metadata commit has not been confirmed in PostgreSQL is
  never trimmed.** The reconciler's only source for that commit is the entry.
  When unreconciled entries reach the retention size during a PostgreSQL
  outage, the group stops accepting ref writes with a stated error instead of
  discarding them; reads continue. The size bound therefore limits how long Git
  writes outlive a metadata outage, and that limit is published.
- A backup carries the entries it needs to bring a restored snapshot forward,
  so restore does not depend on a log that has already been compacted.

### Storage interface and trust

- The interface is an in-process trait first (record, object put/get, snapshot,
  apply) and a remote one behind it for gateways and workers. Merge, browse
  and fork code take filesystem paths today; the trait replaces those paths,
  not the callers' logic.
- Gateways and storage nodes authenticate with mutual TLS. The gateway decides
  authorization and sends a signed, tenant-scoped request; a storage node
  enforces that scope and refuses a request for another tenant. Storage nodes
  do not evaluate Cedar, because they cannot reliably reach metadata; they
  enforce the scope they were given and audit what they did.
- Audit records both the gateway decision and the storage execution, so a
  stolen gateway credential cannot act outside its tenant silently.

## Failure cases

| Failure | Required outcome |
| --- | --- |
| Leader dies after objects, before proposal | No entry, no `ok`; client retries and the operation is executed once |
| Leader dies after commit, before `ok` | Client sees no verdict; retry with the operation ID returns the committed result; new leader applies the ref |
| Local ref write fails after commit | The entry is committed, so the reconciler completes the write and a retry converges. The client may have been told `ng` by `receive-pack` in that narrow window: a false rejection with no loss, requiring the local disk to fail between the hook and Git's write. It is recorded, not hidden |
| Leader is deposed mid-request | Hook returns no verdict; connection is closed; no local ref moves before apply |
| Leader cannot reach a quorum | Writes stop; hook returns no verdict; reads that require a read-index fail rather than serve stale |
| Follower dies during object transfer | Transfer resumes or the proposal does not happen; no partial manifest is committed |
| New leader lacks the last pack | It cannot take write service or the read until its object watermark catches up |
| Membership change removes the object holder | Refused if a committed manifest would fall below two holders |
| Membership change during a push | The push either completes under the old membership or is retried; no two writers |
| Snapshot restore from an older position | Restore replays the entries it carries; a gap fails the restore rather than serving a missing ref |

## Consequences

- Every replicated write adds a network round trip and an object transfer
  before acknowledgement. Acknowledging at a majority bounds the effect of one
  slow member; the cost is measured, not assumed.
- A client can receive no answer. That is deliberate: an unknown outcome plus
  an operation ID is recoverable; a wrong `ok` or `ng` is not.
- The hook and `receive-pack` need a way to abort without a verdict, which is
  a change from today's hook that only returns a status.
- The lifecycle commands make create, fork and delete slower than a local
  `git init`/copy and make them survivable.
- The single-node path is not a compatibility layer: M5 changes the hook, the
  journal classification and the lifecycle commands in the one code path both
  modes share. The M2–M4.1 crash and recovery drills are its regression suite,
  and the rewrite is budgeted as part of the commit-point work, not as an
  addition on top of it.
- M5 is the largest milestone in the roadmap. The design here is smaller than
  the open implementation work, not larger.

## Open items

1. Library comparison: `openraft` and alternatives on license, maintenance,
   membership change, snapshot support and a fault-injection run. `openraft`
   is pre-1.0; whether it manages many groups itself must be answered with a
   spike.
2. Object transfer: pack shipping format, resumption, verification depth, and
   what a lagging member fetches during catch-up.
3. Snapshot format and the seeding procedure for a rebuilt member.
4. Membership change protocol and the proof sketch that a removed or
   partitioned member cannot publish. Expansion from one voter to three in one
   change (ADR 0012) requires joint consensus; whether replacing one member
   uses single-step changes is the remaining choice.
5. Group sizing and split policy.
6. The exact hook abort mechanism (signal, socket close, process kill) and how
   the client's retry carries the operation ID.
7. The storage RPC authentication scheme and the signed request format.
8. The fault matrix rows for Git receive, HA and placement in
   [the qualification register](../plan/07-qualification-and-decisions.md#correctness-and-fault-matrix),
   written as executable tests.
9. The local ref write after commit. Either prove that a committed entry's ref
   can always be written on the leader before `receive-pack` replies (pre-
   allocated ref storage, a filesystem that cannot fail the write independently
   of the log), or move the client-facing answer to the server so Git's ref
   update is a cache. Until one is chosen, the failure table's false-rejection
   window stands.

The statement that another forge moved from a database-coordinated design to
per-repository consensus influenced the original choice. It was recalled, not
checked; verify it against that project's published documentation before citing
it.
