# Metadata workload and consistency contract

Status: proposed product model; the experiment implements the explicitly identified subset. Updated 2026-10-27.

## Confirmed scope

Serve personal installations, teams of 1–100 users, and eventually organizations of approximately 100,000 users. Support one installer with separate dependency processes, single-node operation, and distributed operation within a region. Multi-region writes, AI, and remote development are later decisions.

User count is a dataset dimension. Request rate, active users, concurrency, repository size, issue volume, and CI job rate are separate dimensions. The benchmark does not convert account counts into an unsupported production capacity claim.

This follows the distinction in the incumbents' deployment guidance: GitLab sizes reference architectures primarily by request rate and workload composition, using user counts as an alternative estimate. GitHub Enterprise Server describes independently scalable service roles and recommends independent hardware for redundant nodes. Neither guide makes account count alone sufficient for capacity planning. [GitLab reference architectures](https://docs.gitlab.com/administration/reference_architectures/), [GitHub cluster nodes](https://docs.github.com/en/enterprise-server%403.20/admin/monitoring-and-managing-your-instance/configuring-clustering/about-cluster-nodes).

GitHearth will eventually publish its own measured installation profiles. This first experiment measures metadata operations only; an API request can cause multiple such operations, while a clone can stress Git CPU, disks, and bandwidth with comparatively little metadata work. The current 4-CPU/3-GiB database budget is a lab constraint, not a minimum specification for the complete forge.

## Domain ownership

| Domain | Authoritative records | Atomic boundary |
| --- | --- | --- |
| Identity | Account, verified identity, session, credential reference | Account and session changes with audit event |
| Organization | Tenant, membership, team, invitation | Membership grant/revoke and authorization generation |
| Repository | Stable repository ID, namespace, visibility, placement, policy generation | Name uniqueness and metadata lifecycle |
| Git storage | Objects, authoritative refs, repository journal | Reference transaction with durable object prerequisites; outside the metadata DB transaction |
| Collaboration | Issue, comment, pull request, review, reaction, attachment reference | Local edit, idempotency record, counters, and outbox event |
| Merge coordination | Merge intent, expected source/target OIDs, policy/check snapshot, result | Intent state transitions; Git journal commits the actual branch change |
| Automation | Workflow version, run, job, attempt, lease, runner identity | Claim/renew/finish with fencing token and durable events |
| Packages | Package/version manifests, object references, retention | Version publication after object durability |
| Administration | Audit record, quota, usage reservation, lifecycle operation | Tenant-local policy mutation and audit event |

Use immutable internal IDs. Human-readable slugs can change without moving identity. Tenant IDs accompany tenant-owned records and index prefixes. Foreign ownership checks must exist in both the relational and key-value designs. Benchmark integer IDs are compact synthetic keys, not a final production ID scheme.

## Transaction requirements

1. An issue comment, its counter increment, and its event either all commit or none do.
2. A repeated operation ID with the same payload returns the previous outcome. The same ID with another payload is rejected.
3. An operation must not claim a job already owned by another current attempt. Production leases additionally require expiration, renewal, monotonically advancing fences, and rejection of late completion.
4. Permission evaluation reads a consistent membership/repository state. Production revocation has an explicit point after which new requests cannot be admitted; already admitted operations and caches need bounded behavior.
5. Pagination uses a stable cursor within a defined order, never an unbounded offset scan.
6. Namespace uniqueness, membership edits, and branch-policy changes have explicit conflict behavior.
7. Repository writes and metadata are separate transactions. Use a durable intent, operation ID, Git result, and reconciler; do not claim a cross-system atomic commit without implementing one.
8. Events are delivered at least once. An outbox prevents a committed domain change from losing its event record; it does not guarantee that a webhook receiver sees a single delivery.
9. A timeout may leave an unknown commit outcome. Retrying the same operation ID resolves uncertainty safely. Never infer rollback from a network timeout alone.

The first experiment uses PostgreSQL SERIALIZABLE transactions with fsync, synchronous commit, and full-page writes enabled. FoundationDB uses its ordinary transactional API and durable SSD storage in single redundancy mode. Both profiles tolerate process restart with the host disk intact; neither is an HA configuration. PostgreSQL requires serialization-failure retries, and FoundationDB provides transaction retry handling through its client API. [PostgreSQL isolation](https://www.postgresql.org/docs/current/transaction-iso.html), [FoundationDB Python API](https://apple.github.io/foundationdb/api-python.html).

## First implemented workload

| Operation | Mixed share | Behavior in both adapters |
| --- | --- | --- |
| Permission check | 30% | Membership and repository lookup; missing identities or resources denied |
| Issue page | 25% | Permission check; open issues before cursor; at most 20 full records |
| Add comment | 25% | Write permission, issue existence, operation-ID deduplication, increment, comment, event |
| Enqueue job | 10% | Operation-ID deduplication, queued job, event |
| Claim job | 10% | One of 16 queue buckets; claim ownership and event in one transaction |

Queue operations are internal trusted-worker operations in this experiment. Their production authorization is not implemented. A separate hot scenario directs every comment to one issue, exposing conflict costs. It is intentionally more contentious than the mixed workload.

FoundationDB stores explicit secondary index keys for issue pagination, repository-name lookup, ready jobs, and claim deduplication. The issue listing index stores a pointer to the authoritative issue; full records are fetched through parallel futures. PostgreSQL uses relational indexes and foreign keys. This measures two application designs, not only database storage engines.

## Dataset profiles

| Profile | Accounts | Tenants | Repositories | Issues | Purpose |
| --- | ---: | ---: | ---: | ---: | --- |
| solo | 1 | 1 | 10 | 1,000 | Personal installation |
| small | 100 | 1 | 50 | 5,000 | Primary self-hosted target |
| growth | 10,000 | 10 | 1,000 | 100,000 | Multi-tenant growth and isolation checks |
| enterprise | 100,000 | 1 | 10,000 | 1,000,000 | Large single-organization dataset |

These repository and issue counts are provisional synthetic fixtures, not claims about a typical organization. Seeded text is deliberately simple and therefore compressible. The working set may fit in RAM; a later test must include incompressible varied payloads, skewed repository sizes, and datasets larger than available cache.

The current permission fixture has one membership per account and two resource lookups. Team nesting, direct grants, deny rules, cross-organization collaboration, and revocation races remain selection-gate workloads. Simple membership throughput must not be presented as full authorization performance.

## Additional workloads before a production commitment

- Concurrent permission revocation and admission; nested teams and direct grants.
- Merge admission with policy changes, duplicate workers, stale checks, and changed branch tips.
- Issue search/filter combinations, custom fields, labels, reactions, review threads, and cross-repository dashboards.
- Lease expiry, fenced completion, runner loss, cancellations, and recurring schedules.
- Repository create/rename/delete, tenant transfer, namespace contention, and quota reservation.
- Outbox draining, consumer deduplication, backlog recovery, and retention cleanup.
- Online index/schema changes, tenant export/import, backup/restore, and large-tenant migration.
- Three-node durability with equal guarantees, failover, partition behavior, and loss of quorum on independent machines.

PostgreSQL is provisionally selected for the next implementation milestone in [ADR 0001](../decisions/0001-metadata-database.md). Production qualification remains pending until enough of this contract is validated to justify the first production topology.
