# Milestone direction decisions

Date: 2026-11-15. Status: directions chosen by the project owner for M4.2–M12, recorded before any of this work starts. Nothing here is implemented. Each direction becomes binding for implementation only through an ADR that records alternatives, evidence and the conditions for reopening it, as [the decision register](07-qualification-and-decisions.md#decision-register) requires. Where a row says "confirmed by benchmark" or "chosen by test", the evidence may still change the detail.

This document adds to the [roadmap](04-forge-roadmap.md); the roadmap's build lists and exit criteria remain authoritative.

## Build order

1. **M4.1** usable team alpha (in progress).
2. **M4.2** platform slice: local object storage, scoped short-lived tokens, secrets at rest, per-organization limits and fairness, and single-node online backup. Added on 2026-11-15 because the identity slice and M6 both need the first four, and M4 left backup unable to run while the server accepts writes.
3. **M11a identity slice**: OpenID Connect sign-in, two-factor with authenticator apps and passkeys, service accounts, self-service sign-up (off by default) with email verification and recovery, and account and organization deletion. Depends on M4.1 and M4.2.
4. **M6** isolated CI. It exits on a single node.
5. **M5** single-region distributed foundation, including M6's cluster cases and the shared object store. Its design work (the open items in ADRs 0010–0012) proceeds as documents while M6 is built.
6. **M7** in three parts (below), then **M9**, **M8**, **M12**, **M10**, the rest of **M11**, and **M13**.

Reasoning: CI makes the existing checks, merge queue and auto-merge useful immediately. Features built before M5 are re-qualified in a cluster as M5 and M13 require; that cost is accepted. Reestimate after each gate, as the roadmap states.

## Decisions by milestone

| Milestone | Decision | Direction | Still to settle in the ADR |
| --- | --- | --- | --- |
| M4.2 | Object storage | One S3-shaped interface; the filesystem backend is the single-node implementation, built here. The shared S3-compatible store is M5; LFS and packages consume the same interface in M7 ([ADR 0014](../decisions/0014-platform-slice.md)) | Backend layout, resumable upload, GC, restore evidence |
| M4.2 | Token model | Subjects are user tokens, service accounts and CI run tokens; each non-account subject is a principal type with its own audit actor; a token carries a resource set, scopes, audience and expiry ([ADR 0014](../decisions/0014-platform-slice.md)) | Schema, issuance API, delivery file format, legacy-token end date |
| M4.2 | Secrets at rest | Envelope encryption under an operator-supplied installation key that never enters the database or a backup; the manifest carries a key id only; the audit IP key is included in the wrapped set ([ADR 0014](../decisions/0014-platform-slice.md)) | KMS versus file versus passphrase, rotation window, re-wrap batching |
| M4.2 | Backups | Encryption from M4.1 becomes mandatory for every archive; single-node online backup by recorded positions plus a filesystem snapshot ([ADR 0014](../decisions/0014-platform-slice.md)) | Snapshot mechanism per filesystem, verification depth |
| M4.2 | Fairness | Per-organization and per-account shares of Git and API capacity, per-address limits for anonymous traffic, an organization-creation cap, and per-organization shares of background jobs and email ([ADR 0014](../decisions/0014-platform-slice.md)) | Default numbers and the measured bound |
| M11a | Identity provider scope | OIDC is configured per installation (one issuer set), with a per-organization policy for offered, required or domain-restricted. SAML and SCIM stay in the later part of M11 ([ADR 0015](../decisions/0015-identity-slice.md)) | Account-link edge cases, multi-tenant IdP routing |
| M11a | Second factors | TOTP and WebAuthn passkeys; recovery codes; WebAuthn relying-party ID is the installation hostname | Per-organization enforcement, attestation policy |
| M11a | Sign-up | Off by default. Enabling it requires SPF/DKIM/DMARC, bounce handling, an abuse contact and takedown runbook, a velocity limit, and the deletion path | Exact velocity limits |
| M11a | Deletion | Tombstone and anonymization, with per-account and per-organization data keys so destroying a key makes old backups unreadable ([ADR 0015](../decisions/0015-identity-slice.md)) | Retention of the tombstone, audit reattribution |
| M5 | Git replication | Raft log per repository group; a committed entry is the only decision; every ref writer ships an object manifest to a quorum first; object completeness gates leadership, voting and membership ([ADR 0010](../decisions/0010-git-replication.md)) | Library, transfer, snapshots, membership proof sketch |
| M5 | PostgreSQL HA | Patroni with etcd on three nodes, `synchronous_mode: true`, `synchronous_mode_strict: true`, `synchronous_node_count: 2` (both standbys acknowledge), `synchronous_commit: on`, mandatory watchdog ([ADR 0011](../decisions/0011-postgresql-ha.md)) | Measured commit cost, primary-discovery route |
| M5 | Placement, reads, moves | Placement table with routing generation; read-index reads with per-endpoint staleness; a move is seal → adopt → switch → release with one PostgreSQL decision record ([ADR 0012](../decisions/0012-placement-routing-and-expansion.md)) | Schema, rebalancing policy, fork placement |
| M5 | Online backup | A metadata barrier at a PostgreSQL LSN, a barrier entry in every group at or after it, an object-store snapshot, and one recorded position per store; a store that disagrees fails the restore | Destination, retention, encryption |
| M5 | Failure model | One machine loss, not simultaneous, with no operator action for 60 seconds; minimum qualified topology of six machines (three metadata with etcd, three storage); co-locating a metadata node and a storage member is unsupported | Measured recovery bounds |
| M6 | Workflow format | GitHub Actions YAML, explicit supported subset with fixtures | The subset and its documented differences |
| M6 | Isolation | Firecracker microVMs on Linux/KVM for untrusted code. Without KVM, jobs run only on runners enrolled by and dedicated to one organization, never in containers on a host that serves several organizations or can reach the database, Git storage or secrets (corrected 2026-11-15) | Untrusted-run contract, network egress rules, image trust |
| M6 | Merge checks | Required checks run on the candidate the queue will merge; the intent binds the candidate OID; a later target move or a merged-tree-only failure blocks the merge | Retry and cancellation semantics |
| M6 | Fork pull requests | Approval binds to the reviewed commit SHA and is revoked by any later push. `pull_request_target` and `workflow_run` are outside the supported subset because they run base-branch code with secrets; an unsupported workflow is refused, not silently run (corrected 2026-11-15) | Exact policy surface if they are ever supported |
| M6 | Reusable actions | Installation-hosted by default. External sources, including github.com, only through an administrator allowlist, and pinned to a full commit SHA (corrected 2026-11-15) | Mirror and cache behaviour |
| M6 | Scheduler | PostgreSQL jobs with leases and fencing tokens, extending ADR 0006; a separate transport only if churn measurements require it | Churn benchmark, lease parameters |
| M6 | Runner agent | GitHearth's own Rust agent; a runner belongs to exactly one organization and can claim only that organization's jobs | Enrollment and revocation protocol |
| M7 | Delivery shape | Three separately signed-off parts: **M7a** object storage consumers (LFS, releases, OCI registry); **M7b** npm, Cargo, PyPI, Maven, NuGet, generic; **M7c** static sites, environments, deployment approvals | Acceptance cases per part |
| M7 | Large objects | The M4.2 interface only. On one node the local backend; in a cluster the M5 shared store, with one open-source reference backend chosen by failure tests | Reference backend, checksums, GC, restore evidence |
| M7 | Signing and provenance | Store and verify Sigstore/cosign signatures and SLSA provenance; no built-in signing authority at first | Trust-root configuration |
| M7 | Static sites | Published by a CI job; served from a hostname separate from the application; custom domains need ownership proof and automatic TLS | Private-site access model |
| M8 | Planning model | Projects spanning repositories, with custom fields and saved board/table/roadmap views | Incremental updates for large projects |
| M8 | Wiki | One Git repository per wiki, editable in the browser or by push | Rendering and sanitization rules |
| M9 | Search engine | Built-in indexes: trigram for code, Tantivy for issue/review/wiki text; no external search service | Confirmed by benchmark on real corpora and update churn |
| M9 | Code navigation | Tree-sitter for syntax, symbols and approximate definitions; precise references from SCIP indexes uploaded by CI | Published language/capability matrix |
| M9 | Index location | Decided in an ADR before selection: a separate index filesystem is the starting option, so repository content is not duplicated into the metadata database | Storage layout, rebuild and restore |
| M10 | Scanning | Built in: secret scanning on push and dependency matching against OSV. Other scanners run as CI jobs with open-source tools and upload SARIF | Feed freshness, false-positive handling, license review |
| M11 | Identity order | The identity slice first (above); then SAML and SCIM | Recovery flows, IdP outage behavior |
| M11 | Deployment options | Operator-provided HA PostgreSQL, then Kubernetes packaging; a built-in failover manager only after the Git Raft layer is proven (see ADR 0011) | Required-settings verification command |
| All | Namespaces | Organizations are the only namespace in the baseline; there are no personal namespaces, so every repository has exactly one owning organization | Whether a later release adds personal namespaces |
| All | Repository names | Reusing a name does not redirect the old URL to the new owner: the old path returns 410 after a rename, transfer or deletion, and never resolves to another tenant's repository | Redirect policy if one is ever added |
| All | Server-initiated requests | Every outbound request the server makes (webhooks, mirrors, importers, OIDC discovery, custom-domain checks) goes through one client with an egress allowlist, no redirects to a different host, no link-local or metadata addresses, and a recorded reason | Per-feature egress policy |
| M12 | REST API | GitHearth's own versioned API with OpenAPI, plus a listed and tested GitHub-compatible subset | The subset; deprecation policy |
| M12 | Importers | GitHub first, then GitLab, then Gitea and Forgejo | Source-version matrices, unsupported fields |
| All | Interface | Server-rendered pages that work without JavaScript, plus small TypeScript components where interaction needs them | Build tooling, CSP, accessibility checks for components |

## Deployment goal

Both uses are goals of one product: an organization self-hosting for itself, and an operator hosting many independent organizations on one installation in the manner of a public forge. The second adds requirements the first does not have: fairness and rate limits between organizations (M4.2), self-service sign-up and deletion (M11a), isolation of CI between organizations (M6), a many-organization capacity profile (M5, M13), and abuse controls (M11a baseline, rest in M11). Managed hosting as a business remains optional; the capability to host is required.

Public multi-organization hosting is not claimed as supported before M11a ships sign-up, deletion and the abuse baseline; M4.2's fairness work is the prerequisite, not the whole of it.

## Review record

Two rounds of independent review of this plan on 2026-11-15 produced the corrections marked with that date in the [roadmap](04-forge-roadmap.md), the [phase cases](08-two-person-roadmap.md), ADRs 0010–0012 and the M4 verification documents. Round two found that several round-one corrections were applied in one document and not its neighbours; the 2026-11-15 rewrite of the roadmap and phases applies the whole set.

Findings accepted but not yet turned into scope:

- Mirrors and repository restore are inventory rows with no build list; assign them when M12 and M5 are broken down. M13's exit requires every required row, so either they are allocated or the row is removed.
- The exit criteria of the remaining milestones (M7, M8, M9, M10, M11, M12) are rewritten against their full build lists when each milestone is selected. M6 and M11a now have theirs.
- Export is promised per feature: the contract is defined in M4.2 and full export/import is M12. Each milestone after M4.2 must add a round-trip test for the data it adds, which is now stated in its exit.
- M9's index-location decision is recorded above and needs its ADR before selection.

## What is not decided here

- Detailed designs. Every row needs its ADR before implementation; M4.2 has [ADR 0014](../decisions/0014-platform-slice.md), M11a has [ADR 0015](../decisions/0015-identity-slice.md), and M5 has three ADRs with open items that are implementation choices.
- The two-person increments and acceptance cases for milestones after M11a and M6. They are written when the milestone is selected, as phase 6 was for M4.1.
- Gate C production qualification of PostgreSQL, which remains open.
- M13 has no direction to choose: it is the qualification gate for everything above.
