# Two-person delivery roadmap

Date: 2026-10-28. Status: proposed work allocation; implementation is not claimed complete.

Planning baseline: `f40a031`, reviewed on 2026-10-27. M1 has recorded experimental verification. M2 has storage layout, journal, authorization policies and persisted facts, but still needs an integrated Git-and-issues workflow. Recheck the current branch before assigning tasks; this document does not independently verify later changes.

This plan allocates the existing [forge roadmap](04-forge-roadmap.md). Its acceptance requirements remain authoritative. Phase numbers below are delivery gates, not calendar weeks.

## Ownership

| Person A — Application | Person B — Git and infrastructure |
| --- | --- |
| Database migrations and repository lifecycle API | Storage lifecycle and filesystem recovery |
| Authentication, authorization facts and policy integration | Git HTTP/SSH transport, process limits and hooks |
| Issues/comments, including API and UI | Repository browsing, including Git reads, API and UI |
| Reviews, approvals and protection settings | Merge execution, journaling and concurrency |
| Metadata transactions and idempotency | Push pipeline end to end, including journal replay |
| Shared navigation and browser sessions | Backup/restore, deployment and operational tooling |

A owns shared navigation and browser session handling. B implements browsing and transport in dedicated server modules and wires their own routes through one `router()` function per module, so B does not wait on A for each route. This gives B a complete user-facing feature and avoids assigning all UI work to A.

B owns the push pipeline end to end (ADR 0003 treats it as one transaction): hooks, journal, metadata commit call and journal replay. A provides the database functions that push calls (ref/audit/outbox writes in one transaction) and reviews them. Splitting push between two owners would put a handoff on the critical path.

Both people write tests and documentation for their features and review each other's changes. B is not the sole tester. Rebalance after each phase using actual effort; ownership is a coordination boundary, not a ban on helping.

## Delivery phases

| Phase | Person A | Person B | Completion gate |
| --- | --- | --- | --- |
| 0. Integration baseline | Merge repository identity/lifecycle types, the `repositories` migration and the repository metadata functions push will call | Agree storage interface, reconcile the old flat-path reader with UUID storage, run existing quality gates | The contracts exist as merged code (types, migration, storage interface), not only as notes, and gates pass |
| 1. Repository lifecycle | Repository metadata and scoped grants; create/list/read/rename/archive/delete APIs; idempotency and metadata lifecycle reconciliation | Connect storage helpers, filesystem recovery and process bounds | Lifecycle survives retries and restart; unrelated users are denied |
| 2. Hosted Git | Admission/authentication integration; database functions for push metadata and audit/outbox; basic login/navigation | Smart HTTP clone/fetch/push, hooks, quarantine, push transaction, journal replay into PostgreSQL and bounded streaming | Real clients push and clone private repositories; interrupted operations reconcile |
| 3. Complete M2 | Issues/comments API and UI, pagination and replay semantics; application acceptance tests | Trees/files/commits/diffs API and UI; recovery/restore harness and workflow resource measurements | Fresh install → login → private repository → push/clone → browse → issue/comment passes, including access isolation and fresh restore |
| 4. M3 reviews and merges | Pull requests, threads, approvals/checks, protection configuration, persisted merge intents and UI | Comparisons, supported merge methods, expected-ref updates, journaled execution and duplicate-worker handling | Changed code cannot reuse stale authorization/checks; concurrent or retried merges cannot overwrite history or merge twice |
| 5. Finish M3 and qualify M4 | Remaining review requirements, code owners, queue/auto-merge orchestration, notifications and application quota behavior | SSH, installation, backup/export/restore, upgrades, diagnostics, durable job execution and storage quotas | Every M3 requirement is accounted for; independent installation, HTTP/SSH agreement, restore and upgrade drills pass |

Phase 4 is split into basic review, protected merging, then queue/auto-merge increments. A owns persisted scheduling intent and policy; B owns journaled Git execution. An initial review UI does not complete M3.

During phase 5, A owns transactional job/outbox records and domain handlers; B owns claiming, retry and recovery execution. Agree their interface before parallel implementation.

## Immediate task and merge order

1. Jointly agree repository identity/path lookup, lifecycle states, operation results, authorization context and errors.
2. A merges repository/operation schema and the minimal server integration points.
3. B merges storage-reader alignment and lifecycle integration against the agreed contract.
4. A connects repository APIs while B develops transport and hooks. B can begin storage work before step 2 where it does not depend on the database schema.
5. Integrate one private repository push/clone before expanding browsing and issue workflows.
6. A completes issues/comments while B completes browsing, then both execute M2 acceptance.

Critical path: repository contracts → lifecycle → durable push → integrated M2 → safe merges → recoverable alpha.

Two existing boundaries need explicit resolution in phase 0:

- The prototype reader uses flat `<id>.git` paths; the newer storage uses sharded UUID paths. B connects them before browsing depends on the reader.
- JSON body/time limits currently apply globally in the server. A and B agree separate bounded Git routes before transport integration.

Follow [ADR 0003](../decisions/0003-git-push-and-repository-lifecycle.md) for lifecycle and push durability. Merge execution follows [ADR 0004](../decisions/0004-merge-execution.md) (proposed for M3, pending Person A review). Revise either decision explicitly if implementation evidence requires a change.

## Conflict prevention

| Shared area | Rule |
| --- | --- |
| Server `lib.rs`, `AppState`, config and navigation | A owns shared structure. B adds their own module routers with a one-line `merge` in `lib.rs`; anything larger is agreed first. Avoid broad refactors during feature development. |
| Migrations | A allocates numbers and sequences merges. Feature owners can author migrations; never rewrite an already-merged migration. |
| Git interfaces and identifiers | B owns implementation; A reviews contract changes before dependent application work starts. |
| Cargo manifests and lockfile | Designate one owner for each dependency-changing PR and coordinate changes before editing shared files. |
| Tests and fixtures | Separate feature test files and disposable databases. Avoid both people expanding the existing large PostgreSQL test file. |
| Branches and worktrees | One short-lived branch per reviewable deliverable; separate worktrees; update after prerequisite merges. |
| Cross-boundary review | B reviews storage calls and recovery assumptions; A reviews admission, authorization and metadata integration. |
| Merging to `main` | The author never merges their own branch. The other person reviews, runs `scripts/ci.sh` and merges. |
| Acceptance | Implementer supplies evidence; the other person reviews and exercises the boundary. |

Each task records its owner, dependency, files touched, acceptance scenario, check command and recovery impact. Interface changes are agreed before callers are changed. Keep one active implementation task per person, plus review work.

Run `scripts/ci.sh` on the integrated revision before merging. It includes PostgreSQL tests and the existing 95% line-coverage gate. Plain `cargo test` skips required database tests. CI is currently manual; this plan does not change that policy.

## M2 acceptance checklist

- Clean installation, login and private repository creation.
- Real Git push/clone and supported browsing operations.
- Issue/comment creation with pagination and idempotent retry behavior.
- Denial to unrelated users and enforcement of archive/protection rules.
- Lifecycle, push and metadata recovery after interruption, including multi-ref behavior.
- Restart and fresh restore preserve Git/metadata relationships.
- Retries do not duplicate comments or events.
- Record whole-workflow resource measurements and required comparison evidence, including unavailable comparison systems and limitations.
- Record remaining scope against the [M2 specification](05-first-milestone.md) before declaring the milestone complete.

## Beyond the single-node alpha

These are broad allocations, not implementation-ready tickets. Break them down after M4 using the dependencies and acceptance gates in the full roadmap.

| Milestone | Person A leads | Person B leads |
| --- | --- | --- |
| M5 distributed foundation | Metadata HA integration and application behavior during failures | Replicated Git journal/objects, placement, fencing and recovery |
| M6 isolated CI | Workflow model, checks, secrets policy and orchestration | Isolated runners, leases, execution lifecycle and cleanup |
| M7 packages and delivery | Publication APIs/UI, releases and delivery policy | Registry/storage behavior, retained artifacts and recovery |
| M8 planning/community | Planning and collaboration workflows | Supporting storage, notifications and operational integration |
| M9 search/navigation | Permission-aware query API and user experience | Indexing, rebuilds and measured search performance |
| M10 security workflows | Findings, triage, policy and access control | Scanner integration, ingestion and operational isolation |
| M11 enterprise operations | Identity, administration and policy | Deployment qualification, observability and disaster recovery |
| M12 portability/ecosystem | Import/export mappings, APIs and integrations | Resumable transfer, storage export and restore verification |
| M13 stable qualification | Capability acceptance, accessibility and product completeness | Capacity, deployment, recovery and release qualification |

Select one major milestone at a time. Supporting work may proceed in parallel after its contracts stabilize. Keep M14 optional extensions deferred until separately chosen.

## Estimates and limits

Estimate the next phase after measuring the first phase's actual effort. Durable push/recovery and safe merging are the largest early schedule uncertainties. Two-person ownership cannot guarantee equal effort or eliminate all merge conflicts.

This document saves the agreed planning direction; it does not assign named people, create tickets, rerun tests or certify current implementation completeness.
