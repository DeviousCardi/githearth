# Baseline capability inventory

Baseline: 2026-10-27; updated 2026-11-15. All rows below are required unless marked optional. A milestone allocation is a scope commitment, not implementation status. M1–M4 are verified experimental as of 2026-11-14; [the status section](#status-of-m1m4-allocations-2026-11-14) records which parts of rows allocated to them are delivered and which moved to M4.1. No future row is implicitly complete.

A row closes when UI/API, permission boundaries, error/pagination behavior, data/export/restore, diagnostics and appropriate tests exist. Integrations may satisfy scanner/protocol outcomes if openly usable, documented and tested. Every compatibility target receives a source-version/feature fixture matrix.

| Domain | Required capabilities | Milestone | Acceptance focus |
| --- | --- | --- | --- |
| Runtime | Configuration, migrations, probes, logs, bounded resources, shutdown | M1, M4 | Clean start, invalid config, unavailable dependency |
| Local identity | Bootstrap, login/logout, expiry, provisioning, recovery, credential lifecycle, re-authentication | M1, M4.1, M11a | No default secrets; revocation and recovery; a sensitive action fails closed |
| Tenancy | Organizations, memberships, teams, invitations, roles, one role authority | M1–M4.1, M11a | Tenant isolation, nested policy, last-owner rules, demotion takes effect |
| Repository management | Namespaces, visibility, lifecycle, transfers, archive/delete/restore | M2, M4.1 | IDs survive rename/transfer; old paths never resolve to a new owner; resumable lifecycle |
| Git protocols | HTTP/SSH clone/fetch/push, tags, branches, multi-ref updates | M2, M4.1 | Standard clients; permissions; durable acknowledgement |
| Git browsing | Files/trees, commits, diffs, blame, history, compare, archives | M2, M9 | Giant/binary files, object formats, path bounds |
| Git operations | Forks, mirrors, imports, protected refs, signing policy, housekeeping | M3–M5, M12 | Mirror credentials; GC; race/crash behavior |
| Review | Requests/drafts, threaded reviews, suggestions, approvals, reviewers/code owners | M3 | Exact revision binding; stale approval policy |
| Merge | Methods, conflicts, checks, queue, auto-merge, candidate-tested queue | M3, M6 | Expected target, policy version, duplicate workers, the merged candidate |
| Issues | CRUD, comments, templates/forms, labels, assignments, milestones, dependencies | M2, M8 | Atomic comments/events; privacy and pagination |
| Planning | Boards/tables, fields, iterations, roadmaps, time tracking, saved views | M8 | Large boards and cross-repo access |
| Community | Discussions/Q&A, wiki/docs, reactions, mentions, moderation | M8 | Rich content, privacy, export |
| Notifications | In-app/email, subscriptions, routing/preferences | M4.1, M8 | Durable intent, retries, deduplication, verified address only |
| Search | Code/issues/reviews/wiki/package discovery, repo/org/global scopes | M9 | Permissions at query and materialization; index location decided |
| Navigation | Syntax, symbols, semantic navigation for published languages | M9 | Explicit syntax versus semantic matrix |
| CI definition | Versioned YAML, events, expressions, DAGs, matrices, reusable actions/workflows | M6 | Versioned compatibility fixtures; installation-hosted actions by default |
| CI execution | Leases, fencing, runner enrollment, isolation, quotas, cancellation/retries | M6 | Runner loss, stale completion, organization binding, secret boundaries |
| CI data | Logs, artifacts, caches, variables/secrets, schedules, checks/statuses | M6 | Retention, redaction, poisoning defense, shared object store in a cluster |
| Object storage | S3-shaped interface, local filesystem backend, shared backend, integrity, cleanup, quota | M4.2, M5 | Corruption, abandoned upload, ranged read, restore |
| Scoped tokens | User, service-account and run-token subjects; resource sets, audience, expiry, revocation | M4.2 | Denied outside scope on every transport; principal audit identity |
| Secrets at rest | Envelope encryption, operator key custody, rotation, wrapped audit key | M4.2 | No plaintext secret in a dump or backup; rotation; wrong key |
| Fairness | Per-organization and per-account limits, anonymous per-address limits, job and email shares | M4.2 | Measured bound; one tenant cannot deny another |
| Backup | Fenced maintenance window, encrypted archive, single-node online backup, cluster backup | M4.1, M4.2, M5 | Write racing backup; missing/wrong key; cross-store consistency |
| LFS/uploads | LFS protocol, attachments, resumable large uploads on the object-store interface | M7; attachments M8 | Digest access, integrity, cleanup |
| Packages | OCI, Cargo, npm, PyPI, Maven, NuGet, generic registry | M7 | Standard clients, immutability, permissions, GC |
| Releases | Tags, notes/assets, provenance/signatures | M7 | Exact revision/artifact binding; restore |
| Sites | Static pages, custom domains/TLS, access policy | M7 | Domain ownership, private-site boundary |
| Delivery | Environments, approvals, deployment records, credentials, rollback workflows | M7 | Artifact identity; external outcome tracking |
| Security intake | Private reports, advisories, disclosure workflow | M10 | Restricted access, audit, export |
| Supply chain | Dependency graph, SBOM, vulnerabilities, update automation, provenance | M10 | Feed freshness; exact revision; policy |
| Scanning | Secrets, SAST/SCA, license, container, IaC, DAST integrations | M10 | Fixtures, false positives, scoped findings |
| Enterprise identity | OIDC, SAML, SCIM, MFA/passkeys, recovery, service accounts, sign-up | M11a, M11 | Deprovisioning and IdP outage |
| Enterprise policy | Inheritance, custom roles, delegated admin, network rules, access reviews | M11 | Policy races and privilege boundaries |
| Audit/compliance | Durable audit, export, retention/legal hold, usage/quotas, deletion | M4, M4.1, M11a, M11 | Concurrent accounting, restore, controlled and provable deletion |
| Storage scale | Placement, replication, repair, GC, migration, shared object backend | M5 | Objects durable before committed refs; lifecycle commands replicated |
| Metadata scale | PostgreSQL HA/fencing; tenant sharding if measured need | M5, M11 | Safe promotion and migration; global catalog |
| Installation | One setup/config/release, local and external dependency options | M4, M4.1, M5 | Clean host, preflight, secrets and expansion |
| Recovery | Consistent backups, external copies, fresh restore, repair | M4, M4.1, M4.2, M5, M13 | Cross-store consistency; missing keys/disk; online backup |
| Upgrades | Supported version window, expand/contract, format compatibility, recovery | M4, M5, M11, M13 | Rolling versions and failed migration; Git log skew in M5 |
| Operations | Metrics/traces, alerts, diagnostics, capacity, maintenance | M4.1, M11 | Bounded overhead, redacted support bundles |
| API ecosystem | REST/OpenAPI, CLI/SDKs, webhooks, scoped integrations/extensions | M12 | Stable event IDs, signed delivery, deprecation |
| Migration | GitHub/GitLab/Gitea/Forgejo import; full export/import | M12; contract in M4.2 | Version matrices, resumability, unsupported fields, round-trip |
| UX | Accessible responsive UI, keyboard access, localization-ready text, admin flows | M2–M13 | No raw HTML injection; critical task walkthroughs |
| Freedom/release | BSL 1.1 with commercial production use, notices, SBOM, standard protocols, no forced telemetry | M4, M13 | Distribution audit and independent install |
| Capacity | 1/100/10k/100k-account fixtures and the many-organization profile with active load and full resources | M4, M5, M13 | Sustained SLO evidence, not account-count extrapolation |
| Optional evolution | AI, workspaces, federation, multi-region, managed hosting | M14 | Separate scope/decision/qualification |

## Status of M1–M4 allocations (2026-11-14)

Checked against the code on `main` at `747c65c`. "Delivered" means implemented with tests and covered by the M2–M4 verification reports; it is experimental maturity, and rows that also name a later milestone stay open for that part.

| Domain | Delivered | Not delivered from the M1–M4 allocation |
| --- | --- | --- |
| Runtime | Configuration, migrations, probes, logs, bounds, shutdown | — |
| Local identity | Bootstrap, login/logout, expiry, tokens, SSH keys | Account provisioning, password change/reset, re-authentication, login throttling → M4.1; OIDC/MFA/sign-up → M11a |
| Tenancy | Organizations, owner membership, roles in authorization, repository grants | Member administration, teams, invitations → M4.1 |
| Repository management | Namespaces, visibility, create/rename/archive/delete | Transfer → M4.1 |
| Git protocols | HTTP and SSH clone/fetch/push, tags, branches, multi-ref | — |
| Git browsing | Files/trees, commits, diffs, compare | Blame, history, archives stay M9 |
| Git operations | Forks, protected refs, signing policy | Housekeeping → M4.1; mirrors, imports stay M5/M12 |
| Review | All listed capabilities | — |
| Merge | Methods, conflicts, checks, queue, auto-merge | Candidate-tested queue → M6; cluster behavior stays M5 |
| Issues | CRUD and comments | Remainder stays M8 |
| Notifications | In-app from durable jobs | Email, preferences → M4.1; subscriptions/routing stay M8 |
| Audit/compliance | Hash-chained audit, IP retention, per-repository storage quota | Usage accounting, export, legal hold stay M11; deletion → M11a |
| Installation | Runbook install, `doctor` preflight, Compose | One `setup` command with generated secrets → M4.1 |
| Recovery | Maintenance-window backup, off-host copy, fresh restore (runbook procedure) | `backup`/`restore` commands with a write fence → M4.1; encrypted archive and online backup → M4.2; cluster backup → M5 |
| Upgrades | Forward-only migrate with schema checks | Git log version skew → M5; PostgreSQL expand/contract → M11/M13 |
| Operations | `doctor`, structured logs, measured hardware minimums | Metrics → M4.1; traces, alerts, support bundles stay M11 |
| UX | Server-rendered accessible UI, WCAG 2.2 AA basics audit | Membership/administration flows → M4.1 |
| Freedom/release | License, generated third-party notices | SBOM, signed artifacts stay M13 |
| Capacity | One-host measurement with soak | Open-loop profile runs and the hosting profile stay M5/M13 |

Rows added on 2026-11-15 and allocated in the table above: secrets at rest and key rotation, scoped short-lived tokens, per-organization rate limits and fairness (M4.2); self-service sign-up, email verification and recovery, and account and organization deletion (M11a); rolling upgrade with adjacent-version skew (M5).

Two required rows still have no build list: repository restore and mirrors. They are allocated to no milestone, so M13's "every required inventory row" cannot close until they are either given a milestone or removed from the required set. This is recorded in the [review record](09-milestone-decisions.md#review-record). The per-repository storage quota does not count merge-created objects or fork copies; M4.1's housekeeping work makes it count total repository bytes.

## Cross-cutting completion contract

A required capability is incomplete if it has only a UI, only a mock endpoint, no authorization, no migration/recovery path, or no test for its important failure mode. Durable records have stable identities and schema versions. Mutations specify rejection/commit/unknown/unavailable outcomes. External effects use retries and stable deduplication identifiers; exactly-once delivery is never assumed.

Large objects and Git history use native export formats alongside versioned product metadata. The export contract is defined in M4.2 and every later milestone round-trips the data it adds. Customer freedom includes usable restoration and identity mapping, not just source-code availability. This inventory adopts no license silently: the product license is the recorded BSL 1.1 decision, changing to Apache-2.0 after four years.
