# Qualification, decisions and release gates

Baseline: 2026-10-27; updated 2026-11-15. This document governs claims made by the [roadmap](04-forge-roadmap.md). Proposed targets become supported guarantees only after recorded tests pass on the named topology/version.

## Workload dimensions

| Profile | Accounts | Organizations | Repositories | Primary goal |
| --- | ---: | ---: | ---: | --- |
| Personal | 1 | 1 | 10 | Low idle cost, installation and recovery |
| Team | 100 | 1 | 50 | Small self-hosted responsiveness |
| Growth | 10,000 | 20 | 1,000 | Concurrent workflows, multi-tenant isolation |
| Enterprise | 100,000 | 1 | 10,000 | Large single-org permissions and sustained scale |
| Hosting | 100,000 | 8,000 | 100,000 | Fairness between organizations, noisy-neighbor bounds, tenant isolation under load |

The Hosting profile is the second product goal stated on 2026-11-15: an operator serving many independent organizations, with skewed sizes, 20% of repositories in fork networks, 5% of accounts active daily, and one organization holding a quarter of the load. Its numbers are initial targets, not claims; M5 and M13 measure them. The other fixtures are provisional, not representative of every company.

Each product benchmark adds real varied text, small/medium/large repositories, monorepos, binary/LFS data, nested teams, realistic retention, and a working set exceeding cache. User count is separate from active concurrency, arrivals, Git bytes, automation fan-out, CI job rate and storage growth.

At each profile measure idle/peak CPU, resident/cache memory, disk bytes and IOPS, network bytes, database pool pressure, outbox/job lag, index freshness, p50/p95/p99, throughput, timeout/conflict/retry rates, acknowledged durability, recovery time and total machine/dependency resources. Report each endpoint/operation mix; one combined average hides slow Git and permissions paths.

## Benchmark protocol

1. Record source revision, OS/kernel, CPU/RAM/disk/network, versions, image digests, config and resource limits. Redact credentials without hiding durability settings.
2. Use equal end-user operations and durability goals, not artificially equal internal SQL counts. Include permission checks, rendering and follow-up work where the scenario requires them.
3. Seed fixtures, verify invariants and establish controlled warm/cache/cold states. Track background indexing/checkpoint/GC activity.
4. Use increasing open-loop arrival rates plus bounded closed-loop diagnostic tests. Distinguish queue wait from service time. Include warmup and enough steady-state duration to observe background work; no capacity claim from the six-second screening runs.
5. Repeat and report distributions, ranges and failures. Failed requests remain part of the outcome; successful p99 is labeled as such. Include client saturation and coordinated-omission limitations.
6. Run contention, large objects, many small repos, one large repo, nested authorization, revocation, outbox backlog, job churn and mixed read/write scenarios.
7. Establish an SLO before a qualification run. Proposed initial interactive target: p95 <=250ms and p99 <=1s for named metadata-backed actions at the declared load, excluding explicitly asynchronous completion and Git bytes. This is a proposal, not a current guarantee. Publish endpoint-specific bounds and error budget after measurement; do not weaken them silently to pass.
8. Compare self-hosted GitLab/Gitea/Forgejo on controlled local fixtures when available. Publish feature differences and total resource cost. GitHub Enterprise benchmarking requires an available licensed installation; GitHub.com is not an equivalent controlled environment.
9. Store machine-readable raw records, runner source, configuration and interpretation. Reproduce key findings independently before marketing a speed/cost ratio.

## Correctness and fault matrix

| Boundary | Required faults | Invariants/evidence |
| --- | --- | --- |
| Metadata | Crash before/after commit, timeout, serialization retry, duplicate operation | Atomic domain/audit/outbox; stable retry outcome; unknown is not rollback |
| Git receive | Interrupt quarantine, object flush, journal, ref publish, response, leadership loss before and after commit | Acknowledged refs recover with objects; no ref is written before its entry commits; uncertainty produces no verdict, never a rejection |
| Git lifecycle | Loss of the serving node after create, adopt (fork) and delete | The group log carries the command; the repository exists, or does not, at the same position for every member |
| Merge | Changed tips/policy, revoked access, duplicate worker, lost response | Exact candidate/check binding; expected target; one outcome/reconciliation |
| Sessions/policy | Logout, expiry, SCIM revoke, cache lag, admitted requests, stale re-authentication | Stated new-admission cutoff; bounded in-flight behavior; a sensitive action fails closed |
| HA | Primary loss, partition, lagging standby, fencing failure, quorum loss, watchdog absent | No conflicting writers; no acknowledged loss inside declared model; writes stop rather than weaken |
| Placement | Interrupted move, stale route, membership change, abort after seal, target death after adoption | One write authority; IDs/URLs unchanged; no stale read after release; resumable repair |
| Objects/GC | Disk full, checksum failure, abandoned upload, concurrent GC, quarantine racing prune | No live object deletion; integrity checked; bounded cleanup |
| Tokens | Expiry, revocation, resource-set escape, legacy token, run end | Denied at the next admission on every transport |
| Jobs/CI | Lease expiry, runner loss, duplicate events, stale completion | Fencing; no stale winning result; secrets/cleanup/quotas preserved |
| Search | Index lag, rebuild, revoked permission, missing shard | No hidden snippets/counts; documented incomplete results |
| Backup | Interrupted copy, missing key, wrong key, inconsistent watermark, write racing the backup, metadata newer than Git and Git newer than metadata, repository mid-move | Detected unusable backup; fresh restore verifies cross-store relationships; an acknowledged write is in the restore or was refused |
| Upgrade | Old/new binary overlap, failed migration, format mismatch, mixed-version voter | Declared skew only; no unsafe downgrade assumption; old voter applies and does not stall quorum; tested recovery |

Local process kill tests establish only local process recovery. HA tests require independent failure domains and controlled network faults. Disk/power/region-loss tests have separate claims; three containers on one laptop are not three independent nodes.

## Supported deployment profiles

Single-node: one host is one failure domain, PostgreSQL is separate, objects local, CI isolated or remote. Durable local commit does not tolerate host/disk loss. Recovery uses external backup. Backup is a maintenance window until M4.2's snapshot path, and online after it.

Single-region distributed: redundant gateways/workers; the Patroni topology and exact settings of [ADR 0011](../decisions/0011-postgresql-ha.md); replicated Git journal and durable objects per [ADR 0010](../decisions/0010-git-replication.md); the shared S3-compatible object store chosen in M5; independent failure domains; managed credentials/certificates and external backup.

The qualified failure model is stated, not implied: one machine loss, not simultaneous with another fault, with no operator action in the first 60 seconds; minimum six machines (three metadata with etcd, three storage); co-locating a metadata node and a storage member is unsupported. The target is no acknowledged-write loss for that model, with metadata writes unavailable rather than lossy while fewer than two standbys are synchronised. Quorum failure may make writes unavailable. Region loss, correlated disks and operator error require separate recovery guarantees.

Expansion gate: version/storage preflight, current backup, establish replicas, verify catch-up and object completeness, add limited traffic, migrate objects and repository placement through fenced resumable records, exercise failure, then publish profile status. Both new Git members are added as learners and promoted only after both catch up. Never copy a live data directory and call it supported migration.

## Decision register

| Decision | Current position | Required evidence / deadline |
| --- | --- | --- |
| Product name/visibility | GitHearth, private repo during development | Branding/domain checks before public launch |
| License | Business Source License 1.1, no Additional Use Grant, changing to Apache-2.0 after four years (replaces the MIT OR Apache-2.0 proposal) | Dependency notices before distributing binaries or images |
| Metadata | PostgreSQL provisional | Rust workload, restore, HA and sustained load before production |
| Application | Rust, axum/Tokio, SQLx for M1 | Pinned compatible versions and tests; alternatives need evidence |
| Authorization | Cedar PBAC engine ([ADR 0002](../decisions/0002-authorization.md)) | Grant/team/protection tables, anonymous principal and list-filter tests before M2 public access; principal kinds for service accounts and run tokens in M4.2; revocation/cache contract before any decision cache or M5 local checks |
| Git write protocol | Decided: native Git with owned journal and intents ([ADR 0003](../decisions/0003-git-push-and-repository-lifecycle.md), [ADR 0004](../decisions/0004-merge-execution.md), [ADR 0005](../decisions/0005-ssh-transport.md)) | Single-node crash tests recorded in M2–M4 verification; replicated form is the Git consensus row |
| Git consensus | Raft log per repository group; a committed entry is the only decision; every ref writer ships an object manifest to a quorum first; object completeness gates leadership, voting and membership; create, adopt and delete are log commands ([ADR 0010](../decisions/0010-git-replication.md)) | Close ADR 0010 open items (library spike, transfer, snapshots, membership proof sketch, hook abort) before M5 implementation |
| Metadata HA | Patroni with etcd on three nodes, `synchronous_mode: true`, `synchronous_mode_strict: true`, `synchronous_node_count: 2`, `synchronous_commit: on`, required watchdog ([ADR 0011](../decisions/0011-postgresql-ha.md)) | Gate C tests on independent machines, including the watchdog window and the availability cost of two sync standbys |
| Placement/routing | Placement table with routing generation as a cache invalidation token; per-endpoint read consistency; a move is prepared → copying → sealed → adopted → switching → released, with one PostgreSQL decision record ([ADR 0012](../decisions/0012-placement-routing-and-expansion.md)) | Interrupted-move, abort, stale-route and target-death tests |
| M5 test environment | Docker Compose on one host for development and functional tests; independent machines (available) for qualification | Compose results never support an availability or durability claim; exit evidence names the machines and network faults |
| M5 failure model | One machine loss, minimum six machines, no co-location of metadata and storage | Published machine counts, network and resource costs from measurement |
| Fresh authentication on Git | Undecided: Git admission treats tokens and SSH keys as fresh, contradicting ADRs 0002/0003 for `ref.force_push` and `ref.delete` | Decide and record in M4.1 with an agreement test on both transports; re-authentication endpoint ships in M4.1 |
| Secrets at rest | Envelope encryption under an operator-supplied installation key that never enters the database or a backup; key id only in the manifest; the audit IP key is in the wrapped set ([ADR 0014](../decisions/0014-platform-slice.md)) | M4.2 ADR items; dump and backup grep for known secrets; rotation and wrong-key restore |
| Tokens and principals | User, service-account and run-token subjects; non-account principals are types in authorization and audit; resource sets, audience and short expiry ([ADR 0014](../decisions/0014-platform-slice.md)) | M4.2 tests per subject on API and Git HTTP, service-account SSH, legacy deprecation |
| Backups | Encrypted archives; a write fence for the maintenance window; single-node online backup by recorded positions plus a filesystem snapshot; cluster backup in M5 | M4.2 wrong-key, missing-key and write-racing-backup tests; M5 cross-store restore |
| Fairness | Per-organization and per-account shares, per-address limits for anonymous traffic, organization-creation cap, per-organization job and email shares ([ADR 0014](../decisions/0014-platform-slice.md)) | M4.2 measured bound; M5 states how shares are enforced across gateways |
| Identity and sign-up | OIDC configured per installation with per-organization policy; TOTP and passkeys; sign-up off by default; deletion by tombstone and crypto-shredding (identity slice) | M11a cases; IdP outage and deletion-in-backup evidence |
| Namespaces | Organizations are the only namespace; no personal namespaces in the baseline | Reopen only with a separate decision |
| Repository names | Reuse never redirects an old path to a new owner; the old path returns 410 | Case in the repository management rows |
| Server-initiated requests | One outbound client with an egress allowlist, no cross-host redirects, no link-local or metadata addresses | M4.2 abuse baseline; each feature names its egress policy |
| Sharding | Tenant ownership now; defer physical shards until measured | Global catalog/oversized tenant/migration design before claiming write scale |
| Object store | One S3-shaped interface; M4.2 builds the filesystem backend, M5 the shared S3-compatible store, M7 consumes it for LFS and packages ([ADR 0014](../decisions/0014-platform-slice.md)) | M4.2 integrity and restore; M5 reference backend chosen by failure tests |
| Search | Evaluate Tantivy/trigrams; index location decided in its own ADR before M9 (separate index filesystem as the starting option) | Corpus/permissions/update-churn benchmark before M9 selection |
| Queue/events | PostgreSQL jobs decided ([ADR 0006](../decisions/0006-durable-jobs.md)); the outbox gains `(chain, seq)`, consumers with start positions and per-consumer delivery in M4.1 | ADR 0006 revision in M4.1; backlog and failing-consumer evidence |
| UI | Server-rendered pages, no client framework so far | Decide the TypeScript/interaction approach before the first feature that needs it (boards, editors) |
| Runner/scheduler | Firecracker microVMs chosen; without KVM, organization-dedicated runners only; a runner belongs to one organization | Hardware/kernel support, untrusted-run contract and churn benchmark before M6 |
| Actions compatibility | Installation-hosted actions by default; external sources by administrator allowlist with full commit-SHA pins | Pinned fixtures and differences before compatibility claim |
| Scanners/feeds | Open/self-hostable integrations | License/freshness/privacy evidence before M10 |
| Audit analytics | PostgreSQL initially; ClickHouse only if needed | Retention/query/cost measurement and restore ownership before adoption |
| Multi-region/federation | Deferred | Separate design and acceptance, after single-region foundation |
| AI/workspaces | Optional | Customer backend/hosting/isolation decisions later |

Directions chosen on 2026-11-15 for the remaining rows are listed in [milestone direction decisions](09-milestone-decisions.md); the evidence in the right-hand column is still required before each becomes a supported claim.

No need to resolve every future dependency before M1. Resolve each before its irreversible data/protocol boundary. Record alternatives, measured reason, known costs, migration path and conditions for reopening in an ADR.

## Release evidence bundle

Every milestone records criterion IDs, source revision or reviewed working-tree state, tool/dependency versions, exact commands, exit status, results and limitations. Raw logs must exclude passwords/tokens/config secrets. A reviewer checks important behavior beyond the implementing agent's summary.

A production release additionally requires license/SBOM/notices, build provenance/signature, tested fresh install, supported upgrade and rollback-or-restore, backup restoration, vulnerability response process, operator runbooks, accessibility/task walkthroughs and documented capacity/durability profiles. Missing required checks keep the gate open; a schedule estimate cannot waive correctness.

## Maintenance obligations

After release: dependency/security updates, regression and compatibility fixtures, restore drills, capacity trending, key/certificate rotation, retention verification, incident reviews and supported-version lifecycle. A stable forge is an ongoing maintained service, not merely a completed feature checklist.
