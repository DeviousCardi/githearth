# Baseline capability inventory

Baseline: 2026-10-27. All rows below are required unless marked optional. A milestone allocation is a scope commitment, not implementation status. Only the existing M0 experiments and reviewed M1 foundation are implemented; no future row is implicitly complete.

A row closes when UI/API, permission boundaries, error/pagination behavior, data/export/restore, diagnostics and appropriate tests exist. Integrations may satisfy scanner/protocol outcomes if openly usable, documented and tested. Every compatibility target receives a source-version/feature fixture matrix.

| Domain | Required capabilities | Milestone | Acceptance focus |
| --- | --- | --- | --- |
| Runtime | Configuration, migrations, probes, logs, bounded resources, shutdown | M1, M4 | Clean start, invalid config, unavailable dependency |
| Local identity | Bootstrap, login/logout, expiry, provisioning, recovery, credential lifecycle | M1, M4, M11 | No default secrets; revocation/recovery |
| Tenancy | Organizations, memberships, teams, invitations, roles | M1–M4, M11 | Tenant isolation, nested policy, last-owner rules |
| Repository management | Namespaces, visibility, lifecycle, transfers, archive/delete/restore | M2, M4 | IDs survive rename/transfer; resumable lifecycle |
| Git protocols | HTTP/SSH clone/fetch/push, tags, branches, multi-ref updates | M2, M4 | Standard clients; permissions; durable acknowledgement |
| Git browsing | Files/trees, commits, diffs, blame, history, compare, archives | M2, M9 | Giant/binary files, object formats, path bounds |
| Git operations | Forks, mirrors, imports, protected refs, signing policy, housekeeping | M3–M5, M12 | Mirror credentials; GC; race/crash behavior |
| Review | Requests/drafts, threaded reviews, suggestions, approvals, reviewers/code owners | M3 | Exact revision binding; stale approval policy |
| Merge | Methods, conflicts, checks, queue, auto-merge | M3, M5 | Expected target, policy version, duplicate workers |
| Issues | CRUD, comments, templates/forms, labels, assignments, milestones, dependencies | M2, M8 | Atomic comments/events; privacy and pagination |
| Planning | Boards/tables, fields, iterations, roadmaps, time tracking, saved views | M8 | Large boards and cross-repo access |
| Community | Discussions/Q&A, wiki/docs, reactions, mentions, moderation | M8 | Rich content, privacy, export |
| Notifications | In-app/email, subscriptions, routing/preferences | M4, M8 | Durable intent, retries, deduplication |
| Search | Code/issues/reviews/wiki/package discovery, repo/org/global scopes | M9 | Permissions at query and materialization |
| Navigation | Syntax, symbols, semantic navigation for published languages | M9 | Explicit syntax versus semantic matrix |
| CI definition | Versioned YAML, events, expressions, DAGs, matrices, reusable actions/workflows | M6 | Versioned compatibility fixtures |
| CI execution | Leases, fencing, runner enrollment, isolation, quotas, cancellation/retries | M6 | Runner loss, stale completion, secret boundaries |
| CI data | Logs, artifacts, caches, variables/secrets, schedules, checks/statuses | M6 | Retention, redaction, poisoning defense |
| LFS/uploads | LFS protocol, attachments, resumable large uploads | M7; attachments M8 | Digest access, integrity, cleanup |
| Packages | OCI, Cargo, npm, PyPI, Maven, NuGet, generic registry | M7 | Standard clients, immutability, permissions, GC |
| Releases | Tags, notes/assets, provenance/signatures | M7 | Exact revision/artifact binding; restore |
| Sites | Static pages, custom domains/TLS, access policy | M7 | Domain ownership, private-site boundary |
| Delivery | Environments, approvals, deployment records, credentials, rollback workflows | M7 | Artifact identity; external outcome tracking |
| Security intake | Private reports, advisories, disclosure workflow | M10 | Restricted access, audit, export |
| Supply chain | Dependency graph, SBOM, vulnerabilities, update automation, provenance | M10 | Feed freshness; exact revision; policy |
| Scanning | Secrets, SAST/SCA, license, container, IaC, DAST integrations | M10 | Fixtures, false positives, scoped findings |
| Enterprise identity | OIDC, SAML, SCIM, MFA/passkeys, recovery, service accounts | M11 | Deprovisioning and IdP outage |
| Enterprise policy | Inheritance, custom roles, delegated admin, network rules, access reviews | M11 | Policy races and privilege boundaries |
| Audit/compliance | Durable audit, export, retention/legal hold, usage/quotas | M4, M11 | Concurrent accounting, restore, controlled deletion |
| Storage scale | Placement, replication, repair, GC, migration, object backend | M5 | Objects durable before committed refs |
| Metadata scale | PostgreSQL HA/fencing; tenant sharding if measured need | M5, M11 | Safe promotion and migration; global catalog |
| Installation | One setup/config/release, local and external dependency options | M4, M5 | Clean host, preflight, secrets and expansion |
| Recovery | Consistent backups, external copies, fresh restore, repair | M4, M5, M13 | Cross-store consistency; missing keys/disk |
| Upgrades | Supported version window, expand/contract, format compatibility, recovery | M4, M11, M13 | Rolling versions and failed migration |
| Operations | Metrics/traces, alerts, diagnostics, capacity, maintenance | M4, M11 | Bounded overhead, redacted support bundles |
| API ecosystem | REST/OpenAPI, CLI/SDKs, webhooks, scoped integrations/extensions | M12 | Stable event IDs, signed delivery, deprecation |
| Migration | GitHub/GitLab/Gitea/Forgejo import; full export/import | M12 | Version matrices, resumability, unsupported fields |
| UX | Accessible responsive UI, keyboard access, localization-ready text, admin flows | M2–M13 | No raw HTML injection; critical task walkthroughs |
| Freedom/release | Permissive product license decision, notices, SBOM, standard protocols, no forced telemetry | M4, M13 | Distribution audit and independent install |
| Capacity | 1/100/10k/100k-account fixtures with active load and full resources | M4, M5, M13 | Sustained SLO evidence, not account-count extrapolation |
| Optional evolution | AI, workspaces, federation, multi-region, managed hosting | M14 | Separate scope/decision/qualification |

## Cross-cutting completion contract

A required capability is incomplete if it has only a UI, only a mock endpoint, no authorization, no migration/recovery path, or no test for its important failure mode. Durable records have stable identities and schema versions. Mutations specify rejection/commit/unknown/unavailable outcomes. External effects use retries and stable deduplication identifiers; exactly-once delivery is never assumed.

Large objects and Git history use native export formats alongside versioned product metadata. Customer freedom includes usable restoration and identity mapping, not just source-code availability. No license has been silently adopted by this inventory; final product licensing remains a recorded decision before distribution.
