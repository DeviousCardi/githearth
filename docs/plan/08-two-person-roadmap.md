# Two-person delivery roadmap

Date: 2026-10-28; updated 2026-11-15. Status: phases 0–5 are complete (M2 accepted 2026-10-29, M3 signed off 2026-11-13, M4 signed off 2026-11-14). Phase 6 (M4.1) is the current work allocation; see [Phase 6](#phase-6--m41-usable-team-alpha). M4.2, the identity slice, M6 and M5 are allocated below as phases 7–10.

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
| 6.0 Contracts | Account lifecycle schema: `accounts.email` with a uniqueness constraint, `email_verifications` and `password_resets` (hashed, single-use, expiring tokens) and `must_change_password`; membership, team and invitation facts; Cedar `account.*` and `invitation.*` actions (re-authentication is not a Cedar action); outbox `(chain, seq)`, consumer and delivery tables; `pinned_commits` table | Housekeeping safety design (locking against push, merge, quarantine, forks; pin keep-set); outbox fan-out relay design; `setup`/`backup`/`restore` command surface including the write fence; revise ADR 0006 | Schema, action table, handler interface and ADR revision are merged; no caller is written against an unagreed interface |
| 6.1 People | Administrator account create/disable, password change and reset by one-time token, email verification link and duplicate-address refusal, forced password change, session/token/SSH-key revocation, failed-login throttling (browser, API and Git HTTP basic authentication), re-authentication endpoint and UI; member add/remove/role change with last-owner rule; last site-administrator guard and `reset-admin`; API and UI | `setup` command (config, generated secrets, directory ownership, host key, migrate, first admin) safe to rerun; SSH and Git HTTP admission honor disabled accounts, forced password change and removed members | U01–U04, U15, U17, U18, U23a and U23b pass: a verification link is single-use, an expired or reused token and a duplicate address are refused, an unverified address receives nothing, and `reset-admin` restores administration from the host for the only site administrator who cannot sign in when no other administrator can act; a clean install reaches a three-person organization with no SQL; a removed or disabled user is denied on HTTP, SSH, API and UI; a stale sensitive action succeeds after re-authentication |
| 6.2 Teams and maintenance | Teams with repository grants, invitations with expiry, repository transfer (grants, forks, open pull requests) with the cross-organization revoke rule; repository restore from trash (API, UI, lifecycle state); pin lifecycle and retention | Repository housekeeping job (repack, prune, quota accounting) with its own timeout and lease, `kill_on_drop`, fault injection and `pinned_commits` keep-set; transfer review for storage and fork-network assumptions; storage side of repository restore (move back from trash, reconcile) | Team grants authorize identically on both transports; interrupted housekeeping loses no reachable object; a transferred repository keeps ID, history and its non-fork open pull requests (U07) and leaves no cross-org grant |
| 6.3 Delivery and operations | Email notification handler, preferences and deduplication; domain handlers on the outbox interface; consumer registration and start position | Outbox fan-out relay execution (per-consumer jobs, `delivered_at`, a per-consumer attempt budget spanning hours, dead-letter, requeue command); SMTP configuration, SPF/DKIM check and `doctor` diagnostic; metrics endpoint; encrypted `backup`/`restore` with the fence | Every committed event is delivered at least once after a crash and one consumer cannot block another; email failure retries and never blocks a request; `backup` refuses while writes are possible; `backup` → `restore` on a fresh host passes |
| 6.4 Acceptance | Application cases U01–U08, U15–U18 and U22, plus the verification and break-glass cases in 6.1; runbook review; joint U20, U21, U23, U23a, U23b and U23c | Operations acceptance cases U09–U14 and U19; re-point `verify-m4.py` and `verify-offhost-backup.py` at `backup`/`restore`; merge-error investigation; optional Gitea/Forgejo comparison | Every case has recorded evidence in `docs/verification/m4-1-signoff.md`; each person exercises the other's boundary; both sign off |

### M4.1 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U01 | A | Administrator creates, disables and re-enables accounts; a non-administrator cannot; the last site administrator cannot be disabled |
| U02 | A | Password change and administrator reset revoke sessions, access tokens and SSH keys; no default, administrator-chosen or emailed plaintext password |
| U03 | A | Failed-login throttling bounds guessing on the browser, the API and Git HTTP and cannot permanently lock out the administrator; the configured values are the ones U18 asserts |
| U04 | A | Member add/remove/role change; the last owner cannot be removed, demoted or disabled; changes are audited |
| U05 | A | Teams grant repository access; removing a team member or grant denies new admission |
| U06 | A | Invitations expire, are single-use and reveal nothing about private organizations to the wrong account |
| U07 | A | Repository transfer preserves ID and its non-fork open pull requests; fork pull requests are closed under the audited policy or the move is refused, and the resolved fork state is recorded. All grants are revoked with audit on a cross-organization move. Refused without administration at the source and create permission at the target, and on a name collision. Queued merge intents are cancelled and an executing intent blocks the move. The old path of a private repository reveals nothing and never resolves to another tenant's repository |
| U08 | A | Email notifications go only to a verified address, deduplicate, honor preferences, retry from durable intents and never block the originating request |
| U09 | B | `setup` on a clean host yields a running, `doctor`-clean installation with generated secrets and correct ownership. A second run changes no recorded setting, refuses a loose configuration file, and starts cleanly; a run with PostgreSQL absent fails before writing configuration |
| U10 | B | HTTP and SSH agree for disabled accounts, removed members and team grants, including denial of an existing session, token and SSH key after removal |
| U11 | B | Housekeeping interrupted before repack, between repack and prune, during prune and before verification loses no object referenced by a ref, pull request, review comment, status or merge intent. A merged or closed pull request whose branch was deleted still renders its diff after a prune, for the configured pin retention; a status older than the retention no longer pins its ancestry. Concurrent push/merge succeeds or is cleanly refused; quota reflects freed space; a run exceeds the shared job timeout without being killed |
| U12 | B | Outbox fan-out delivers every event committed after the cutover to every consumer at least once across crash and restart; one failing consumer does not block the others; duplicates are harmless; a consumer whose jobs fail for 30 minutes still receives every event afterwards, asserted through the next attempt time and not only the final state; a consumer registered after the cutover receives nothing older than its registration position; upgrading with an old backlog delivers nothing from before the cutover; one event produces one notification job, not one from the relay and one direct enqueue |
| U13 | B | `backup` refuses to run while writes are possible, naming the fence it used; the manifest holds checksums, positions, configuration, host key and key id (never the key); the archive is encrypted; `restore` on a fresh host verifies database-to-storage consistency; a push that starts before the storage snapshot and commits during it is in the restore or was refused; a corrupted, incomplete, wrong-key or missing-key backup is detected and refused |
| U14 | B | Metrics endpoint is off or loopback-only by default and exposes only the published metric and label list, which contains no organization, repository or account names |
| U15 | A | A disabled account's and a removed member's existing sessions, access tokens and SSH keys are denied on the API and UI as well as on both Git transports |
| U16 | A | Removing a member revokes their team memberships and direct grants in that organization in the same transaction; one authority decides organization role, and a demotion takes effect |
| U17 | A | An administrator password reset never transmits a reusable password: the account receives a single-use token, must set a new one before any other use, and the reset is audited |
| U18 | A | Login throttling asserts the plan's defaults — 5 failures per account per 15 minutes and 20 per address per 15 minutes, with an exponential delay after the fifth failure — on the browser, the API and Git HTTP basic authentication; token and SSH-key authentication are exempt from the per-account ceiling, so an attacker cannot lock an ordinary user out indefinitely |
| U19 | B | Pull-request head pins exist for every existing and new pull request before the first prune runs (backfill verified on an upgraded installation); a pin is not advertised by `upload-pack`; a pin is charged to the owning organization and expires on the stated retention |
| U20 | Both | Runbooks under `docs/operations/` match the new commands and the backup fence; `verify-m4.py` and `verify-offhost-backup.py` use `backup`/`restore` and pass |
| U21 | Both | Two new accounts complete invite → join → clone → branch → push → open pull request → review → merge, on both transports, with no direct database access |
| U22 | A | A stale session (older than the fresh-auth window) is refused a sensitive action and succeeds after re-authenticating; the browser re-authentication prompt exists only on the site-administration paths that render it, while API-only actions return the machine-readable `reauthentication required` reason. Over Git, a token or SSH key counts as fresh (decided 2026-11-15): `ref.force_push` and `ref.delete` are allowed or refused by the grant and branch protection identically on HTTP and SSH, and ADRs 0002 and 0003 say so |
| U23 | Both | The merge-candidate correction is recorded: the lab round-4 merge error has a recorded outcome, and the M3 sign-off correction names the candidate-binding gap and points at M6 |
| U23a | A | A verification link works once; an expired or reused token is refused; a duplicate address is refused; an unverified address receives no notification, no reset link and no sign-up mail |
| U23b | A | With the only site administrator unable to sign in (lost password, or a lost second factor once M11a ships) and no other administrator able to act, `reset-admin` run from the host restores administration, issues a single-use credential, revokes the old administrator sessions and is audited |
| U23c | Both | A deleted repository is restored from trash within the retention window with the same ID, history, issues and pull requests; revoked grants stay revoked; a restore after the purge, onto a taken name, or by a non-owner is refused; the old path returns `410` until the restore completes |

Corrections from the 2026-11-15 plan review: teams and grants already exist in migration `0002`, so increment 6.0 extends that schema; the outbox relay is a fan-out to per-consumer jobs, not a claim of outbox rows; housekeeping's keep-set includes object IDs referenced only from PostgreSQL; pull-request pins are rows, not Git refs, because the journaled path accepts only `refs/heads` and `refs/tags`; `backup` needs a fence mechanism, not a refusal sentence; and account recovery needs a token schema before it can be implemented.

Merge order: 6.0 contracts first; then 6.1 A and B in parallel; 6.2 and 6.3 may overlap once the 6.0 interfaces are merged. A allocates migration numbers as before.

## Phase 7 — M4.2 platform slice

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m42--platform-slice). Four increments, overlapping after 7A and 7B agree the interface.

| Increment | Person A — Application | Person B — Git and infrastructure | Completion gate |
| --- | --- | --- | --- |
| 7A Object store and export | Export contract and its round-trip harness; object metadata, quota accounting and tenant prefixes | Filesystem object backend behind the S3-shaped interface (ranged get, multipart/resumable upload, append, checksum, prefix listing); abandoned-upload cleanup; backup/restore integration | An object survives restart and restore; corruption, abandoned upload and a ranged read are tested; U30a exports and re-imports into a fresh installation |
| 7B Tokens and principals | Token and principal schema, scopes, resource sets, audience, issuance, revocation, audit actor for non-human principals; legacy token migration | Admission for tokens on Git HTTP; service-account SSH keys; run-token lifetime enforced on every admission | Every subject is confined to its resource set and lifetime on both transports; legacy tokens still work |
| 7C Secrets and backup | Envelope schema, wrapped key set including the audit IP key, rotation and re-wrap, decryption at use; secrets never returned by an API | Operator key custody, key id in the manifest, encrypted archives, wrong-key and missing-key failure; single-node online backup with snapshot and positions | A dump contains no plaintext secret and the encrypted archive contains none (U28); rotation, wrong-key restore and pre-key backup retention are tested |
| 7D Fairness and abuse baseline | Limit schema and defaults, per-organization and per-account policies, organization-creation cap, published bound; abuse contact and takedown runbook; email rate limits; the sign-up velocity limiter; SPF/DKIM/DMARC and bounce requirements | Per-address and global limits for anonymous traffic, Git share scheduler replacing the global semaphore, per-organization job and email shares, trusted-proxy handling; the outbound request client (U30c) | An organization at its share stays within twice its single-organization p95 at the same total load; anonymous traffic cannot exhaust capacity; U30b finds the abuse baseline published and `doctor` reporting a missing DNS or bounce setting |

### M4.2 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U24 | B | An object written through the interface survives restart and `backup` → `restore`; a ranged read and an appended log are correct; a corrupted object is detected and refused |
| U25 | B | An abandoned multipart upload is cleaned up after its deadline; a resumed upload completes without duplication |
| U26 | Both | A token of each subject type (user, service account, run token) cannot act outside its resource set, scope or lifetime on the API or Git HTTP; a run token is denied after the run ends, tested against a stub run record because runs arrive in M6; a service account uses its SSH key and cannot exceed its grants |
| U27 | A | Existing `repo:read`/`repo:write` tokens keep working during the deprecation window and are refused after their stated end |
| U28 | Both | A database dump contains no plaintext secret, including the audit IP key; the encrypted archive contains no plaintext secret, and after decryption the only private material present is the database and SMTP values in the configuration file and the SSH host key, all operator-owned, while every stored secret column is encrypted under the installation key, which is not in the archive; the manifest has a key id and no key |
| U29 | Both | Rotating the installation key keeps every secret readable; restoring with the wrong key or no key fails clearly and never starts with unreadable secrets; a write racing an online backup is in the restore or was refused |
| U30 | Both | One organization at its configured share does not push another organization's p95 past twice the single-organization measurement; anonymous clone and login are limited per address and globally; organization creation is capped per account and per installation |
| U30a | A | The export contract round-trips: an installation exports, imports into a fresh installation, and counts, checksums and sampled content agree |
| U30b | A | The abuse contact and takedown runbook are published; `doctor` reports a missing SPF/DKIM/DMARC record or bounce configuration; the configured email rate limit is enforced; the sign-up velocity limiter refuses a request above the configured per-address velocity (tested at its interface; the endpoint is M11a's) |
| U30c | B | Every server-initiated request uses the one outbound client: a request to a link-local, loopback, private-range or metadata address, a redirect to a different host, and a DNS answer that changes after the check are all refused and recorded; an allowlisted destination succeeds |

## Phase 8 — Identity slice (M11a)

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m11a--identity-slice). This is a separately signed-off slice, not the whole of M11.

| Increment | Person A — Application | Person B — Git and infrastructure | Completion gate |
| --- | --- | --- | --- |
| 8A Identity schema and recovery | Change of address and self-service recovery, external identity table, authenticator seeds, passkey credentials, recovery codes; account-link rules | Crypto-shredding data keys per account and organization; backup interaction | Schema merged; a link requires a verified matching email; deleting a key makes old secret ciphertext unreadable; U32 passes: a change of address is inactive until verified and an expired, reused or wrong-account token is refused |
| 8B Sign-in methods | OIDC client configuration and callback, account linking, per-organization policy; TOTP enrolment and verification; passkey registration and assertion with RP ID configuration; self-service sign-up and the prerequisite check | WebAuthn challenge storage, rate limits on reset, break-glass and IdP-outage path | Sign-up, OIDC, second factor and recovery complete without operator SQL; U31 holds sign-up off until the M4.2 abuse prerequisites are recorded, and U38 checks the M11a endpoint against that limit |
| 8C Service accounts and deletion | Service account administration and audit identity; account and organization in-place tombstone, quota release, deletion UI and API | Repository, fork, trash and object removal; cluster re-qualification hook for M5 | U38a, U38b and U38c pass: a deleted account is unrecoverable in the live system, the audit chain still verifies, its secrets are unreadable in an older backup and its pseudonymous row is bounded by retention; an organization deletion leaves nothing live behind and releases quota |

### Identity-slice acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U31 | A | Sign-up is off by default; enabling it with a missing prerequisite fails and `doctor` names it; a new user verifies an address before any access |
| U32 | A | A change of address is not active until the new address is verified; an expired, reused or wrong-account token is refused on change-of-address and on self-service recovery, and a reset request above the configured per-address velocity is refused |
| U33 | A | OIDC sign-in links only on a verified matching email; an unverified or absent email never takes over an existing account |
| U34 | A | TOTP and passkey enrolment work, a lost factor is recovered through the one-time-token path, and recovery codes are single-use |
| U35 | A | Disabling a user or deleting an organization denies the API, Git HTTP, Git SSH, the browser and CI tokens at the next admission |
| U36 | B | A deleted account's secrets and encrypted columns are unreadable in an older backup once its data key is destroyed; the pseudonymous account row it leaves remains in a backup taken before deletion until that backup's retention expires, and a backup predating the per-account keys cannot be shredded at all; the live audit trail still resolves the actor |
| U37 | B | An IdP outage leaves local administrators able to sign in through break-glass; a federation-required organization is unavailable, and says so |
| U38 | A | The sign-up endpoint refuses above the M4.2 per-address velocity and does not rebuild the limiter |
| U38a | A | Deleting an account tombstones the row in place, keeping its UUID and clearing username, email, display name, password hash, second factors, sessions, tokens and SSH keys; the audit chain still verifies afterwards and resolves the actor to the tombstone; deleting the last owner of an organization or the last site administrator is refused until ownership is transferred |
| U38b | B | Deleting an organization removes its repositories, forks, trash and stored objects and releases its quota; no object or path remains readable in the live installation |
| U38c | A | A service account is created, rotated and deleted with its own audit identity; a revoked service account is denied at the next admission |

## Phase 9 — M6 isolated CI

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m6--isolated-ci). M6 exits on a single node.

| Increment | Person A leads | Person B leads |
| --- | --- | --- |
| 9A Workflow format and compatibility | Versioned YAML, events, DAGs, matrices, expressions, reusable workflows and actions, variables and secrets; compatibility fixtures (U44a) and the documented supported subset | Pinned and cached actions, schedule storage, the check/status record |
| 9B Execution, data and scheduling | Checks and statuses, schedules and manual runs, log/artifact/cache lifecycle and retention, quota accounting (U44d) | Fenced leases, cancellation, deadlines, concurrency groups, retries (U44b, U44c), runner enrollment and revocation, the worker that runs a job |
| 9C Isolation and policy | Tested merge candidates, fork approval binding, the actions allowlist and source policy, default token scope | Isolation boundary, egress rules, cache and artifact poisoning boundary, secret redaction |

### M6 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U39 | A | The merge queue runs required checks on the candidate it merges; a target moving after approval, and a check that fails only on the merged tree, both block the merge |
| U40 | A | Approval binds to the reviewed commit SHA; a push after approval revokes it; `pull_request_target` and `workflow_run` are refused or carry no secrets |
| U41 | B | A job cannot read another organization's workspace, cache, artifacts or secrets; an untrusted job cannot poison a trusted cache; the default token is read-only |
| U42 | B | A runner belongs to one organization and cannot claim another's job; an enrollment is revocable and revocation takes effect on the next claim |
| U43 | A | Actions load from installation repositories by default; an external source requires an administrator allowlist entry and a full commit-SHA pin |
| U44 | B | Egress rules block link-local and metadata endpoints; CPU, memory, disk and time limits stop a job that exceeds them; secrets are redacted from logs |
| U44a | A | The compatibility fixtures pass for the supported subset; an unsupported syntax or event is refused with a named reason, not run silently |
| U44b | B | Cancelling a run kills its jobs; a deadline expires and marks the job failed; a retry re-runs only the failed job and a successful job is not repeated |
| U44c | B | A completion from a stale lease or attempt changes nothing after a leader change; duplicate workers publish one result |
| U44d | A | A log, an artifact and a cache survive restart and are readable by the next job; retention deletes them on schedule and quota refuses a job over its limit |

## Phase 10 — M5 distributed foundation

Scope and exit criteria are in the [roadmap](04-forge-roadmap.md#m5--single-region-distributed-foundation); the protocol is in ADRs 0010–0012. M5 runs after M6 and includes M6's cluster cases.

| Increment | Person A leads | Person B leads |
| --- | --- | --- |
| 10A Commit point and lifecycle | Metadata apply, operation-ID retry, audit for replicated refs | Raft log, object manifests, hook behaviour, create/adopt/delete entries; replica rebuild and interrupted membership (U53) |
| 10B Placement, reads and moves | Placement schema, routing generation, move record and switch decision | Seal, import, release, stale-route refusal, follower read-index |
| 10C Metadata HA | Patroni and etcd configuration, primary discovery, connection recovery; metadata primary loss (U51) and Gate C (U54) | Authorization reads at a known position; refusal when the primary is not authoritative |
| 10D Shared object store | CI data on the object-store interface, tenant prefixes | S3-compatible store integration and the reference backend chosen by failure tests |
| 10E Conversion, upgrade, backup, capacity | Conversion procedure and rolling upgrade; online backup cut protocol; capacity model and machine counts | Journal format migration, mixed-version test with the schema-compatibility window, backup across groups, M6 cluster cases (U52) |

### M5 acceptance cases

| ID | Owner | Requirement |
| --- | --- | --- |
| U45 | B | A push reports `ok` only for a committed entry. A leader killed between proposal and commit gives the client no verdict, and a retry with the operation ID returns the committed result; no local ref is ever ahead of the log |
| U46 | B | Repository create, fork adoption and delete survive the loss of the node that served them; a member that missed a delete cannot serve the repository |
| U47 | A | A move aborted before the switch leaves the source serving; a target that dies after import leaves exactly one readable copy; a gateway holding a stale route cannot read the released source |
| U48 | A | A write racing the online backup is in the restore or was refused; a restore whose metadata, Git groups and object store disagree is refused, as is a repository recorded mid-move |
| U49 | B | A member that lacks a committed manifest cannot lead, vote or be counted in that entry's quorum; a membership change that would leave fewer than two holders of a committed manifest is refused |
| U50 | B | A single node converts with both learners caught up before either is promoted; an N/N+1 cluster upgrades followers before the leader and old voters apply and ignore unknown entries without stalling quorum; the N binary starts against the N+1 schema because the migration is expand-only, and a destructive migration is refused |
| U51 | A | Killing the metadata primary leaves every acknowledged commit on the promoted node; killing the synchronous standby loses none and pauses writes only until the other standby is caught up and named synchronous; killing the asynchronous standby pauses nothing; with two nodes down writes stop and never fall back to asynchronous commit; the watchdog stops a primary partitioned from etcd and its window is measured |
| U52 | B | A runner that lost its lease across a leader change cannot publish a winning result; the retried job's result is the only one recorded |
| U53 | B | A metadata outage that outlasts the log's retention size stops Git ref writes and trims no entry whose metadata commit is unconfirmed. A member that missed commits rebuilds and rejoins; an interrupted placement or membership change recovers to one write authority; a deleted account and a deleted organization stay deleted across a leader change and a replica rebuild, and the tombstone replicates |
| U54 | A | Gate C runs on independent machines: primary loss, synchronous-standby loss, etcd partition, two etcd members lost, and a commit cancelled mid-flight all produce the stated outcomes with the measured window |

M5 implementation needs the open items in ADRs 0010–0012 closed first; Docker Compose serves M5 development, and qualification runs on the available independent machines.

## Beyond M5

These are broad allocations, not implementation-ready tickets. Break each one down into increments and acceptance cases, as phases 6–10 do, when it is selected. The order after M4.1 is M4.2, the identity slice, M6, then M5; see [milestone direction decisions](09-milestone-decisions.md).

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
