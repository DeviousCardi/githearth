# GitHearth master plan

Status: proposed design, revision 4; local screening complete and application foundation verified. Planning date: 2026-10-27.

## 1. Objective

Build a freely usable, self-hostable development platform with a path from a personal installation to a large organization. The intended scope covers source hosting, review, planning, automation, releases, packages, security, administration, and an integration ecosystem.

The founder has explicitly confirmed that **one product must support single-node and distributed deployment modes**. The distributed requirement refers to one logical GitHearth installation spread across machines. Federation between independent installations is a separate, later capability.

The initial experiments established a Git reader and metadata baseline. The next deliverable is an executable application foundation, followed by the complete Git-and-issues workflow. The full scope and gated sequence are in [the implementation roadmap](04-forge-roadmap.md).

## 2. Priority order

1. Preserve customer data and enforce access boundaries.
2. Preserve customer freedom: self-hosting, standard protocols, complete export, permissive product licensing, and no required vendor service.
3. Deliver high useful throughput under a stated durability model and bounded resource consumption.
4. Keep installation, upgrades, recovery, and expansion understandable.
5. Make everyday browsing, reviewing, and collaboration responsive and accessible.

Development speed is not the deciding criterion for technology selection. Neither is language purity. A replacement must justify itself through correctness, measured efficiency, operational behavior, or customer freedom. Eliminating a process boundary or garbage collector is not, by itself, evidence of a throughput improvement.

## 3. Meaning of one setup

One distribution, one configuration schema, one administrative CLI, one release train, and one feature set. An installer manages the required services or connects to operator-managed equivalents.

It does not mean every dependency must live inside one executable. PostgreSQL remains a separate service. Untrusted CI runs in an isolated execution boundary. A distributed installation additionally needs replicated databases, object storage, networking, and independent failure domains.

| Concern | Single machine | Distributed installation |
| --- | --- | --- |
| Product code | Shared Rust workspace and release | Same workspace and release |
| Application | Co-located gateway, domain, and worker roles | Independently replicated roles |
| Metadata | Chosen engine's local deployment | Same engine's qualified distributed deployment |
| Git | Local Git data; one-member journal group | Sharded repository groups with replicated journals and object durability |
| Large objects | Filesystem backend | Supported S3-compatible backend |
| Search | Local index workers | Partitioned indexes and query fan-out |
| Events | Transactional outbox and durable jobs in the chosen engine | Same outbox; optional JetStream transport |
| CI | Separate local microVM executor when supported, or remote runners | Dedicated runner pools |
| Feature availability | All implemented features, subject to resources | Same features with greater capacity and availability |
| Failure tolerance | No machine-failure tolerance | Explicit per-subsystem failure guarantees |

A cluster is not healthy merely because three application processes are running. Its database, Git replicas, object store, ingress, credentials, and recovery tooling must satisfy the advertised deployment profile.

## 4. Scope boundaries and parity

The intended product scope includes repositories, review and merge queues, issues and projects, discussions and wikis, search and navigation, notifications, CI/CD, packages and releases, static sites, application security, enterprise identity and policy, audit and retention, migration/export, APIs, and integrations. The [capability inventory](06-capability-inventory.md) allocates required domains to milestones; none is silently dropped from the long-term goal.

“GitHub/GitLab-style capability” means comparable user outcomes. It does not imply copying branding, replicating proprietary implementation details, providing every changing vendor API, funding a public compute fleet, or reproducing a commercial AI model. Exact import formats, workflow compatibility, and integration APIs require explicit matrices.

Managed hosting, payments, marketplace commerce, and an organization-operated public runner fleet are optional business operations. The forge must work without them. AI assistance and remote development are optional product modules, available through self-hosted or customer-selected backends.

## 5. Proposed platform direction

- Rust for application services, Git orchestration, search workers, scheduling logic, CLI, and runner agent.
- Tokio, axum, and tonic as the initial runtime, HTTP, and internal RPC stack, subject to pinned-version review.
- Use the provisional PostgreSQL decision from the completed PostgreSQL/FoundationDB screening for the first application milestone. Tenant ownership is explicit from the first schema. PostgreSQL read replicas alone do not provide horizontal write scaling; a FoundationDB design needs its own query and index layer. Add another candidate only for a concrete advantage not covered by these two.
- A Git service that owns every repository mutation. Native Git is the initial wire-protocol baseline; gitoxide is used for supported read operations after conformance testing.
- A durable repository journal in single-node mode that can become a replicated group without changing repository identity or client URLs.
- Local immutable object files for Git; filesystem or S3-compatible storage for LFS, uploads, packages, logs, and artifacts.
- Server-rendered pages with focused TypeScript interaction for review, boards, and editors. WASM is optional where measurements justify it.
- Permissions and policy evaluation in Rust backed by the selected metadata store, with a versioned authorization contract. Introducing another policy engine requires a benchmark and migration decision.
- Transactional outbox/jobs in the selected engine; optional JetStream for event transport at scale. Consumers remain idempotent regardless of transport.

The initial tradeoffs, acceptance gates, and order of work are in [decision gates](02-decision-gates.md) and the [metadata contract](01-metadata-contract.md).

The first local comparison is now complete. [ADR 0001](../decisions/0001-metadata-database.md) provisionally selects PostgreSQL for the first implementation and records the remaining production/distributed qualification requirements. This supersedes treating both engines as equally undecided for the next coding milestone.

## 6. Product promises

An acknowledged write has a documented durability boundary. Replication has a documented failure model. An interrupted request can return an unknown outcome; retries must discover or reuse the existing operation instead of blindly duplicating it.

A successful merge identifies the exact target revision, source revision, policy generation, and tested candidate. Background worker duplication must never produce a second branch update for the same merge intent.

Authorization is checked by the service that owns the resource. Search indexes, cache contents, package digests, and replica placement do not confer access. Critical revocation behavior is explicit, including in-flight work.

Export includes product metadata and large objects as well as Git history. Restore is tested independently of backup creation. Telemetry that reports to the project is disabled by default; local operational metrics remain available.

## 7. Release strategy

The roadmap progresses through engineering foundations; single-node storage; collaborative forge; distributed foundation; CI; packages and delivery; planning and discovery; application security; enterprise operations; ecosystem and portability; and full baseline qualification. AI and remote development are optional later modules whose hosting and implementation have not been selected.

Every production feature ships with UI/API behavior, permissions, export/restore behavior, telemetry, operational documentation, and tests appropriate to its risk. A checkbox in the UI does not make a feature complete.

An early single-node release may be useful before distributed mode is production-ready, but it must already use the intended identity, journal, service, and migration contracts. No production HA claim is made before the distributed qualification gate passes failure tests. The current Git prototype is a local experiment, not that release.

## 8. Planning honesty

The plan deliberately provides milestone gates instead of promising full parity within a few months. A complete forge is a multi-year undertaking at solo scale and requires ongoing maintenance after initial delivery. The unknowns with the greatest schedule impact are native Git integration, replicated write correctness, CI compatibility, and disaster recovery.

Before assigning calendar dates, finish the foundation experiments, measure available weekly engineering time, and estimate the next two milestones. Reforecast after each completed milestone. Preserve the full product scope while allowing evidence to change implementation choices and sequencing.

## 9. How to use this plan

Read this document and the metadata contract, then the [full roadmap](04-forge-roadmap.md), [M1 acceptance specification](05-first-milestone.md), [capability inventory](06-capability-inventory.md), and [qualification/decision register](07-qualification-and-decisions.md). The database is provisionally selected. Complete each milestone against its recorded evidence rather than repeating the completed screening.

Record design changes as new revisions of the relevant decision. Keep future capability inventories and compatibility matrices synchronized with actual tests. Use statuses `planned`, `experimental`, `beta`, and `stable`; the current executable components are experimental.
