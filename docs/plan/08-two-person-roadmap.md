# Two-person delivery roadmap

Date: 2026-10-28; updated 2026-11-15. Status: phases 0–5 are complete (M2 accepted 2026-10-29, M3 signed off 2026-11-13, M4 signed off 2026-11-14). Phase 6 (M4.1) is the current work allocation; see [Phase 6](#phase-6--m41-usable-team-alpha). The next two milestones, M4.2 and the identity slice, are allocated below as phases 7 and 8.

Planning baseline: `f40a031`, reviewed on 2026-10-27. M1 has recorded experimental verification. M2 shipped storage layout, journal, authorization policies, persisted facts and the integrated Git-and-issues workflow; its acceptance is recorded in [m2-workflow.md](../verification/m2-workflow.md). Recheck the current branch before assigning tasks; this document does not independently verify later changes.

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

Follow [ADR 0003](../decisions/0003-git-push-and-repository-lifecycle.md) for lifecycle and push durability. Merge execution follows [ADR 0004](../decisions/0004-merge-execution.md), accepted for M3. Revise either decision explicitly if implementation evidence requires a change.

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

## Phase 6 — M4.1 usable team alpha

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m41--usable-team-alpha). Ownership follows the same boundary as phases 0–5: A owns application behavior and metadata, B owns Git storage, execution and operator tooling. The conflict-prevention rules above apply unchanged.

| Increment | Person A — Application | Person B — Git and infrastructure | Completion gate |
| --- | --- | --- | --- |
| 6.0 Contracts | Account lifecycle schema: email verification token, reset token and `must_change_password`; membership, team and invitation facts; Cedar `account.*`, `invitation.*` and `me.reauthenticate` actions; outbox `(chain, seq)`, consumer and delivery tables; `pinned_commits` table | Housekeeping safety design (locking against push, merge, quarantine, forks; pin keep-set); outbox fan-out relay design; `setup`/`backup`/`restore` command surface including the write fence; revise ADR 0006 | Schema, action table, handler interface and ADR revision are merged; no caller is written against an unagreed interface |
| 6.1 People | Administrator account create/disable, password change and reset by one-time token, forced password change, session/token/SSH-key revocation, failed-login throttling (browser, API and Git HTTP basic authentication), re-authentication endpoint and UI; member add/remove/role change with last-owner rule; last site-administrator guard and `reset-admin`; API and UI | `setup` command (config, generated secrets, directory ownership, host key, migrate, first admin) safe to rerun; SSH and Git HTTP admission honor disabled accounts, forced password change and removed members | A clean install reaches a three-person organization with no SQL; a removed or disabled user is denied on HTTP, SSH, API and UI; a stale sensitive action succeeds after re-authentication |
| 6.2 Teams and maintenance | Teams with repository grants, invitations with expiry, repository transfer (metadata, grants, forks, open pull requests) with the cross-organization revoke rule; pin lifecycle and retention | Repository housekeeping job (repack, prune, quota accounting) with its own timeout and lease, `kill_on_drop`, fault injection and `pinned_commits` keep-set; transfer review for storage and fork-network assumptions | Team grants authorize identically on both transports; interrupted housekeeping loses no reachable object; a transferred repository keeps ID and history and leaves no cross-org grant |
| 6.3 Delivery and operations | Email notification handler, preferences and deduplication; domain handlers on the outbox interface; consumer registration and start position | Outbox fan-out relay execution (per-consumer jobs, `delivered_at`, retry in hours, dead-letter, requeue command); SMTP configuration, SPF/DKIM check and `doctor` diagnostic; metrics endpoint; encrypted `backup`/`restore` with the fence | Every committed event is delivered at least once after a crash and one consumer cannot block another; email failure retries and never blocks a request; `backup` refuses while writes are possible; `backup` → `restore` on a fresh host passes |
| 6.4 Acceptance | Application acceptance cases U01–U08, U15–U18, U21–U23; runbook review | Operations acceptance cases U09–U14 and U19; re-point `verify-m4.py` and `verify-offhost-backup.py` at `backup`/`restore`; merge-error investigation; optional Gitea/Forgejo comparison; U20 jointly | Every case has recorded evidence in `docs/verification/m4-1-signoff.md`; each person exercises the other's boundary; both sign off |

### M4.1 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U01 | A | Administrator creates, disables and re-enables accounts; a non-administrator cannot; the last site administrator cannot be disabled |
| U02 | A | Password change and administrator reset revoke sessions, access tokens and SSH keys; no default, administrator-chosen or emailed plaintext password |
| U03 | A | Failed-login throttling bounds guessing and cannot permanently lock out the administrator; the numbers are configured and the test asserts them |
| U04 | A | Member add/remove/role change; the last owner cannot be removed, demoted or disabled; changes are audited |
| U05 | A | Teams grant repository access; removing a team member or grant denies new admission |
| U06 | A | Invitations expire, are single-use and reveal nothing about private organizations to the wrong account |
| U07 | A | Repository transfer preserves ID, open pull requests and resolved fork state. All grants are revoked with audit on a cross-organization move. Refused without administration at the source and create permission at the target, and on a name collision. Queued merge intents are cancelled and an executing intent blocks the move. The old path of a private repository reveals nothing and never resolves to another tenant's repository |
| U08 | A | Email notifications go only to a verified address, deduplicate, honor preferences, retry from durable intents and never block the originating request |
| U09 | B | `setup` on a clean host yields a running, `doctor`-clean installation with generated secrets and correct ownership. A second run changes no recorded setting, refuses a loose configuration file, and starts cleanly; a run with PostgreSQL absent fails before writing configuration |
| U10 | B | HTTP and SSH agree for disabled accounts, removed members and team grants, including denial of an existing session, token and SSH key after removal |
| U11 | B | Housekeeping interrupted before repack, between repack and prune, during prune and before verification loses no object referenced by a ref, pull request, review comment, status or merge intent. A merged or closed pull request whose branch was deleted still renders its diff after a prune. Concurrent push/merge succeeds or is cleanly refused; quota reflects freed space; a run exceeds the shared job timeout without being killed |
| U12 | B | Outbox fan-out delivers every event committed after the cutover to every consumer at least once across crash and restart; one failing consumer does not block the others; duplicates are harmless; a consumer whose jobs fail for 30 minutes still receives every event afterwards; upgrading with an old backlog delivers nothing from before the cutover |
| U13 | B | `backup` refuses to run while writes are possible, naming the fence it used; the manifest holds checksums, positions, configuration, host key and key id (never the key); the archive is encrypted; `restore` on a fresh host verifies database-to-storage consistency; a push that starts before the storage snapshot and commits during it is in the restore or was refused; a corrupted, incomplete, wrong-key or missing-key backup is detected and refused |
| U14 | B | Metrics endpoint is off or loopback-only by default and exposes only the published metric and label list, which contains no organization, repository or account names |
| U15 | A | A disabled account's existing sessions, access tokens and SSH keys are denied on the API and UI as well as on both Git transports |
| U16 | A | Removing a member revokes their team memberships and direct grants in that organization in the same transaction; one authority decides organization role, and a demotion takes effect |
| U17 | A | An administrator password reset never transmits a reusable password: the account receives a single-use token, must set a new one before any other use, and the reset is audited |
| U18 | A | Login throttling is specified with numbers (attempts, window, per account and per address), configured, and asserted against its behaviour on the browser, the API and Git HTTP basic authentication; a per-account ceiling cannot be used to lock an ordinary user out indefinitely |
| U19 | B | Pull-request head pins exist for every existing and new pull request before the first prune runs (backfill verified on an upgraded installation); a pin is not advertised by `upload-pack`; a pin is charged to the owning organization and expires on the stated retention |
| U20 | Both | Runbooks under `docs/operations/` match the new commands and the backup fence; `verify-m4.py` and `verify-offhost-backup.py` use `backup`/`restore` and pass |
| U21 | Both | Two new accounts complete invite → join → clone → branch → push → open pull request → review → merge, on both transports, with no direct database access |
| U22 | A | A stale session (older than the fresh-auth window) is refused a sensitive action with a re-authentication prompt and succeeds after re-authenticating; the Git fresh-authentication decision is recorded with an agreement test on both transports |
| U23 | A | The merge-candidate correction is recorded: the lab round-4 merge error has a recorded outcome, and the M3 sign-off correction names the candidate-binding gap and points at M6 |

Corrections from the 2026-11-15 plan review: teams and grants already exist in migration `0002`, so increment 6.0 extends that schema; the outbox relay is a fan-out to per-consumer jobs, not a claim of outbox rows; housekeeping's keep-set includes object IDs referenced only from PostgreSQL; pull-request pins are rows, not Git refs, because the journaled path accepts only `refs/heads` and `refs/tags`; `backup` needs a fence mechanism, not a refusal sentence; and account recovery needs a token schema before it can be implemented.

Merge order: 6.0 contracts first; then 6.1 A and B in parallel; 6.2 and 6.3 may overlap once the 6.0 interfaces are merged. A allocates migration numbers as before.

## Phase 7 — M4.2 platform slice

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m42--platform-slice). Four increments, overlapping after 7A and 7B agree the interface.

| Increment | Person A — Application | Person B — Git and infrastructure | Completion gate |
| --- | --- | --- | --- |
| 7A Object store and export | Export contract and its round-trip harness; object metadata, quota accounting and tenant prefixes | Filesystem object backend behind the S3-shaped interface (ranged get, multipart/resumable upload, append, checksum, prefix listing); abandoned-upload cleanup; backup/restore integration | An object survives restart and restore; corruption, abandoned upload and a ranged read are tested |
| 7B Tokens and principals | Token and principal schema, scopes, resource sets, audience, issuance, revocation, audit actor for non-human principals; legacy token migration | Admission for tokens on Git HTTP; service-account SSH keys; run-token lifetime enforced on every admission | Every subject is confined to its resource set and lifetime on both transports; legacy tokens still work |
| 7C Secrets and backup | Envelope schema, wrapped key set including the audit IP key, rotation and re-wrap, decryption at use; secrets never returned by an API | Operator key custody, key id in the manifest, encrypted archives, wrong-key and missing-key failure; single-node online backup with snapshot and positions | A dump and a decrypted backup contain no plaintext secret; rotation and wrong-key restore are tested |
| 7D Fairness and abuse baseline | Limit schema and defaults, per-organization and per-account policies, organization-creation cap, published bound; abuse contact and takedown runbook | Per-address and global limits for anonymous traffic, Git share scheduler replacing the global semaphore, per-organization job and email shares, trusted-proxy handling | An organization at its share stays within twice its single-organization p95 at the same total load; anonymous traffic cannot exhaust capacity |

### M4.2 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U24 | A | An object written through the interface survives restart; a ranged read and an appended log are correct; a corrupted object is detected and refused |
| U25 | B | An abandoned multipart upload is cleaned up after its deadline; a resumed upload completes without duplication |
| U26 | A | A token of each subject type (user, service account, run token) cannot act outside its resource set, scope or lifetime on the API or Git HTTP; a run token is denied after the run ends; a service account uses its SSH key and cannot exceed its grants |
| U27 | B | Existing `repo:read`/`repo:write` tokens keep working during the deprecation window and are refused after their stated end |
| U28 | A | A database dump contains no plaintext secret, including the audit IP key; a decrypted backup contains no plaintext credential; the manifest has a key id and no key |
| U29 | B | Rotating the installation key keeps every secret readable; restoring with the wrong key or no key fails clearly and never starts with unreadable secrets; a write racing an online backup is in the restore or was refused |
| U30 | A | One organization at its configured share does not push another organization's p95 past twice the single-organization measurement; anonymous clone and login are limited per address and globally; organization creation is capped per account |

## Phase 8 — Identity slice (M11a)

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m11a--identity-slice). This is a separately signed-off slice, not the whole of M11.

| Increment | Person A — Application | Person B — Git and infrastructure | Completion gate |
| --- | --- | --- | --- |
| 8A Identity schema and recovery | Email verification and reset tokens, external identity table, authenticator seeds, passkey credentials, recovery codes, `must_change_password`; account-link rules | Crypto-shredding data keys per account and organization; backup interaction | Schema merged; a link requires a verified matching email; deleting a key makes old ciphertext unreadable |
| 8B Sign-in methods | OIDC client configuration and callback, account linking, per-organization policy; TOTP enrolment and verification; passkey registration and assertion with RP ID configuration | WebAuthn challenge storage, rate limits on sign-up and reset, break-glass and IdP-outage path | Sign-up, OIDC, second factor and recovery complete without operator SQL |
| 8C Service accounts and deletion | Service account administration and audit identity; account and organization tombstone, anonymization, quota release, deletion UI and API | Repository, fork, trash and object removal; cluster re-qualification hook for M5 | A deleted account is unrecoverable in the live system and unreadable in an older backup; the audit trail remains consistent |

### Identity-slice acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U31 | A | Sign-up is off by default; enabling it requires the recorded prerequisites; a new user verifies an address before any access |
| U32 | B | An unverified address receives no notification and no reset link; an expired, reused or wrong-account token is refused |
| U33 | A | OIDC sign-in links only on a verified matching email; an unverified or absent email never takes over an existing account |
| U34 | B | TOTP and passkey enrolment work, a lost factor is recovered through the one-time-token path, and recovery codes are single-use |
| U35 | A | Disabling a user or deleting an organization denies the API, Git HTTP, Git SSH, the browser and CI tokens at the next admission |
| U36 | B | A deleted account's data in an older backup is unreadable once its data key is destroyed; the live audit trail still resolves the actor |
| U37 | A | An IdP outage leaves local administrators able to sign in through break-glass; a federation-required organization is unavailable, and says so |
| U38 | B | Sign-up and organization creation are limited per address and per organization; the abuse contact and takedown runbook are published |

## Phase 9 — M6 isolated CI

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m6--isolated-ci). M6 exits on a single node.

| ID | Owner | Requirement |
| --- | --- | --- |
| U39 | A | The merge queue runs required checks on the candidate it merges; a target moving after approval, and a check that fails only on the merged tree, both block the merge |
| U40 | A | Approval binds to the reviewed commit SHA; a push after approval revokes it; `pull_request_target` and `workflow_run` are refused or carry no secrets |
| U41 | B | A job cannot read another organization's workspace, cache, artifacts or secrets; an untrusted job cannot poison a trusted cache; the default token is read-only |
| U42 | B | A runner belongs to one organization and cannot claim another's job; an enrollment is revocable and revocation takes effect on the next claim |
| U43 | A | Actions load from installation repositories by default; an external source requires an administrator allowlist entry and a full commit-SHA pin |
| U44 | B | Egress rules block link-local and metadata endpoints; CPU, memory, disk and time limits stop a job that exceeds them; secrets are redacted from logs |

## Phase 10 — M5 distributed foundation

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m5--single-region-distributed-foundation); the protocol is in ADRs 0010–0012. M5 runs after M6 and includes M6's cluster cases.

| Increment | Person A leads | Person B leads |
| --- | --- | --- |
| 10A Commit point and lifecycle | Metadata apply, operation-ID retry, audit for replicated refs | Raft log, object manifests, hook behaviour, create/adopt/delete entries |
| 10B Placement, reads and moves | Placement schema, routing generation, move record and switch decision | Seal, import, release, stale-route refusal, follower read-index |
| 10C Metadata HA | Patroni and etcd configuration, primary discovery, connection recovery | Authorization reads at a known position; refusal when the primary is not authoritative |
| 10D Shared object store | CI data on the object-store interface, tenant prefixes | S3-compatible store integration and the reference backend chosen by failure tests |
| 10E Conversion, upgrade, backup, capacity | Conversion procedure and rolling upgrade; online backup cut protocol; capacity model and machine counts | Journal format migration, mixed-version test, backup across groups, M6 cluster cases |

### M5 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U45 | B | A push reports `ok` only for a committed entry. A leader killed between proposal and commit gives the client no verdict, and a retry with the operation ID returns the committed result; no local ref is ever ahead of the log |
| U46 | B | Repository create, fork adoption and delete survive the loss of the node that served them; a member that missed a delete cannot serve the repository |
| U47 | A | A move aborted before the switch leaves the source serving; a target that dies after import leaves exactly one readable copy; a gateway holding a stale route cannot read the released source |
| U48 | A | A write racing the online backup is in the restore or was refused; a restore whose metadata, Git groups and object store disagree is refused, as is a repository recorded mid-move |
| U49 | B | A member that lacks a committed manifest cannot lead, vote or be counted in that entry's quorum; a membership change that would leave fewer than two holders of a committed manifest is refused |
| U50 | A | A single node converts with both learners caught up before either is promoted; an N/N+1 cluster upgrades followers before the leader and old voters apply and ignore unknown entries without stalling quorum |

M5 implementation needs the open items in ADRs 0010–0012 closed first; Docker Compose serves M5 development, and qualification runs on the available independent machines.

## Beyond M6

These are broad allocations, not implementation-ready tickets. Break each one down into increments and acceptance cases, as phases 6–9 do, when it is selected. The order after M4.1 is M4.2, the identity slice, M6, then M5; see [milestone direction decisions](09-milestone-decisions.md).

| Milestone | Person A leads | Person B leads |
| --- | --- | --- |
| M7 packages and delivery | Publication APIs/UI, releases and delivery policy | Registry/storage behavior on the object-store interface, retained artifacts and recovery |
| M8 planning/community | Planning and collaboration workflows | Supporting storage, notifications and operational integration |
| M9 search/navigation | Permission-aware query API and user experience | Indexing, rebuilds and measured search performance |
| M10 security workflows | Findings, triage, policy and access control | Scanner integration, ingestion and operational isolation |
| M11 enterprise operations | Identity administration and policy, beyond the slice | Deployment qualification, observability and disaster recovery |
| M12 portability/ecosystem | Import/export mappings, APIs and integrations | Resumable transfer, storage export and restore verification |
| M13 stable qualification | Capability acceptance, accessibility and product completeness | Capacity, deployment, recovery and release qualification |

Select one major milestone at a time. Supporting work may proceed in parallel after its contracts stabilize. Keep M14 optional extensions deferred until separately chosen.

## Estimates and limits

Estimate the next phase after measuring the first phase's actual effort. Durable push/recovery and safe merging are the largest early schedule uncertainties. Two-person ownership cannot guarantee equal effort or eliminate all merge conflicts.

This document saves the agreed planning direction; it does not assign named people, create tickets, rerun tests or certify current implementation completeness.
