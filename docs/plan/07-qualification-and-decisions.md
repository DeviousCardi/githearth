# Qualification, decisions and release gates

Baseline: 2026-10-27. This document governs claims made by the [roadmap](04-forge-roadmap.md). Proposed targets become supported guarantees only after recorded tests pass on the named topology/version.

## Workload dimensions

| Profile | Accounts | Initial synthetic repos/issues | Primary goal |
| --- | ---: | --- | --- |
| Personal | 1 | 10 / 1,000 | Low idle cost, installation and recovery |
| Team | 100 | 50 / 5,000 | Small self-hosted responsiveness |
| Growth | 10,000 | 1,000 / 100,000 | Concurrent workflows, multi-tenant isolation |
| Enterprise | 100,000 | 10,000 / 1,000,000 | Large single-org permissions and sustained scale |

These inherited fixtures are provisional, not representative of every company. Each product benchmark adds real varied text, small/medium/large repositories, monorepos, binary/LFS data, nested teams, realistic retention, and a working set exceeding cache. User count is separate from active concurrency, arrivals, Git bytes, automation fan-out, CI job rate and storage growth.

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
| Git receive | Interrupt quarantine, object flush, journal, ref publish, response | Acknowledged refs recover with objects; no invalid partial publication |
| Merge | Changed tips/policy, revoked access, duplicate worker, lost response | Exact candidate/check binding; expected target; one outcome/reconciliation |
| Sessions/policy | Logout, expiry, SCIM revoke, cache lag, admitted requests | Stated new-admission cutoff; bounded in-flight behavior |
| HA | Primary loss, partition, lagging standby, fencing failure, quorum loss | No conflicting writers; no acknowledged loss inside declared model |
| Placement | Interrupted move/catch-up, stale route, membership change | One write authority; IDs/URLs unchanged; resumable repair |
| Objects/GC | Disk full, checksum failure, abandoned upload, concurrent GC | No live object deletion; integrity checked; bounded cleanup |
| Jobs/CI | Lease expiry, runner loss, duplicate events, stale completion | Fencing; no stale winning result; secrets/cleanup/quotas preserved |
| Search | Index lag, rebuild, revoked permission, missing shard | No hidden snippets/counts; documented incomplete results |
| Backup | Interrupted copy, missing key/object, inconsistent watermark | Detected unusable backup; fresh restore verifies cross-store relationships |
| Upgrade | Old/new binary overlap, failed migration, format mismatch | Declared skew only; no unsafe downgrade assumption; tested recovery |

Local process kill tests establish only local process recovery. HA tests require independent failure domains and controlled network faults. Disk/power/region-loss tests have separate claims; three containers on one laptop are not three independent nodes.

## Supported deployment profiles

Single-node: one host is one failure domain, PostgreSQL is separate, objects local, CI isolated or remote. Durable local commit does not tolerate host/disk loss. Recovery uses external backup.

Single-region distributed: redundant gateways/workers; qualified PostgreSQL synchronous/fenced HA; replicated Git journal and durable objects; supported object-store topology; independent failure domains; managed credentials/certificates and external backup. The target is no acknowledged-write loss for the stated one-node failure model. Quorum failure may make writes unavailable. Region loss/correlated disks/operator error require separate recovery guarantees.

Expansion gate: version/storage preflight, current backup, establish replicas, verify catch-up/integrity, add limited traffic, migrate objects and repository placement through fenced resumable records, exercise failure, then publish profile status. Never copy a live data directory and call it supported migration.

## Decision register

| Decision | Current position | Required evidence / deadline |
| --- | --- | --- |
| Product name/visibility | GitHearth, private repo during development | Branding/domain checks before public launch |
| License | MIT OR Apache-2.0 proposed, not adopted | Final files and dependency notices before external distribution |
| Metadata | PostgreSQL provisional | Rust workload, restore, HA and sustained load before production |
| Application | Rust, axum/Tokio, SQLx for M1 | Pinned compatible versions and tests; alternatives need evidence |
| Authorization | Rust membership core; richer versioned policy | Revocation/role inheritance contract before protected writes/team features |
| Git write protocol | Native Git + owned journal/intents proposed | ADR before M2 public push; crash tests before release |
| Git consensus | Library/protocol not selected | Object/log durability, membership/snapshot/GC proof sketch and faults before M5 |
| Metadata HA | Synchronous replication + qualified failover/fencing | Select manager/topology in M5 ADR; failover tests required |
| Sharding | Tenant ownership now; defer physical shards until measured | Global catalog/oversized tenant/migration design before claiming write scale |
| Object store | Local first; S3-compatible distributed backend | License, checksums, GC, restore and failure evidence before M5/M7 |
| Search | Evaluate Tantivy/trigrams | Corpus/permissions/update-churn benchmark before M9 selection |
| Queue/events | PostgreSQL outbox/jobs first; optional JetStream | Backlog/recovery evidence before separate transport |
| UI | Server rendering plus focused TypeScript | Framework choice, escaping/accessibility/CSP before M2 UI |
| Runner/scheduler | MicroVM boundary proposed; remote option | Hardware/kernel support, untrusted-run contract and churn benchmark before M6 |
| Actions compatibility | Explicit subset proposed | Pinned fixtures and differences before compatibility claim |
| Scanners/feeds | Open/self-hostable integrations | License/freshness/privacy evidence before M10 |
| Audit analytics | PostgreSQL initially; ClickHouse only if needed | Retention/query/cost measurement and restore ownership before adoption |
| Multi-region/federation | Deferred | Separate design and acceptance, after single-region foundation |
| AI/workspaces | Optional | Customer backend/hosting/isolation decisions later |

No need to resolve every future dependency before M1. Resolve each before its irreversible data/protocol boundary. Record alternatives, measured reason, known costs, migration path and conditions for reopening in an ADR.

## Release evidence bundle

Every milestone records criterion IDs, source revision or reviewed working-tree state, tool/dependency versions, exact commands, exit status, results and limitations. Raw logs must exclude passwords/tokens/config secrets. A reviewer checks important behavior beyond the implementing agent's summary.

A production release additionally requires license/SBOM/notices, build provenance/signature, tested fresh install, supported upgrade and rollback-or-restore, backup restoration, vulnerability response process, operator runbooks, accessibility/task walkthroughs and documented capacity/durability profiles. Missing required checks keep the gate open; a schedule estimate cannot waive correctness.

## Maintenance obligations

After release: dependency/security updates, regression and compatibility fixtures, restore drills, capacity trending, key/certificate rotation, retention verification, incident reviews and supported-version lifecycle. A stable forge is an ongoing maintained service, not merely a completed feature checklist.
