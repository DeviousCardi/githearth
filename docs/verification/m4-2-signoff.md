# M4.2 sign-off

Date: 2026-11-16. State: **signed by both reviewers, with recorded conditions —
not a clean pass.** Four of the ten cases — U25, U27, U29 and U30 — carry a
clause that is not demonstrated, or is demonstrated only within a stated
boundary; those are marked below and are part of what the signatures assert, not
a footnote to them.

Scope: the M4.2 acceptance cases U24–U30c and the M4.2 exit sentence, as
recorded in [m4-2-evidence.md](m4-2-evidence.md). This document adds no code and
re-runs no gate: it is an assertion about the record, at the commits named below.

## What is being signed

| Item | Value |
| --- | --- |
| Case record | [m4-2-evidence.md](m4-2-evidence.md), collected on `m42/exit-evidence` at `a4541b5`, clean tree |
| Completion pass | `m42/complete` at `77e9dde`, corrected at `5913cd8` after cross-review (`fix-206`) |
| Merged as | PR #206, merge `f14d968` on `main` |
| Sign-off tree | `m42/signoff`, branched from `main` at `b3c7903`; both `a4541b5` and `5913cd8` are ancestors |
| Revert proofs | one per case, in a scratch worktree at `a4541b5`; each disables the mechanism and makes the named test fail |
| Gates | recorded in the evidence and the completion report (`fmt`, `clippy`, workspace lib 337 passed, the named integration targets); **not re-run for this document** |

The evidence collection and the completion pass ran on Apple M5, 10 cores,
arm64, macOS 26.6, rustc/cargo 1.99.1, git 2.50.1, psql 17.11, against private
PostgreSQL containers that were removed afterwards.

## Per case: what is met, what is not

| Case | Met | Not demonstrated |
| --- | --- | --- |
| **U24** | An object written through the store interface survives restart and a real `backup` → `restore`; a 9-byte ranged read crossing the 1 MiB chunk boundary is correct before and after restart and restore; an appended log keeps both old and new lines; a flipped byte returns `ChecksumMismatch` and never bytes. The manifest lists exactly the published objects and `verify_all` reports a removed sidecar as `Orphan` without failing. | Nothing. Every clause listed here has a committed test and a revert that fails it. |
| **U25** | An abandoned multipart upload is returned by `cleanup_abandoned` past its 60 s deadline, releases its 10 upload bytes, and is no longer listable; a resumed upload lists its parts from disk after reopen, replaces a re-uploaded part, completes once with the right bytes, and refuses a second completion. | The product wiring. `reconcile.rs` is inspected to call `cleanup_abandoned(now)` each pass, but no integration test drives the reconciler, so a reconciler that stopped calling it would not fail the suite. The clause is proved at the store interface only. See limitation 2. |
| **U26** | Account, service-account and run tokens are confined to their resource set on both the API and Git HTTP; scope (`package:read`, `repo:read`) and audience confinement hold; expired and revoked tokens are refused immediately; a run token is refused after `cancelled` and after `succeeded`; a service-account SSH key signs in and cannot exceed its grants. Issuance stores only `SHA-256(token)`, and each non-account subject has its own audit actor. | Nothing. The run record is a stub `ci_runs` row because runs arrive in M6, which is the case's own stated method. |
| **U27** | A legacy `repo:read` token is accepted inside the window; the stated end is enforced (default `2027-06-30` stores and enforces `2027-07-01T00:00:00Z`, and `doctor` prints that instant); a past deadline returns `401`; a later configured date cannot extend a shortened deadline and two invalid dates fail validation; the schema refuses a legacy row with no deadline. | The `repo:write` half. The shipped test issues only `["repo:read"]`; `repo:write` inside the window was proved by changing a scratch copy of that test, which returned `201 CREATED`. No committed test exercises it, so the `repo:write` clause rests on a one-off run, not on the suite. |
| **U28** | A real `pg_dump` and a decrypted archive contain no plaintext audit IP key, installation key (base64 or raw), archive key or SMTP credential; a planted plaintext key in the same search proves the search is sensitive; after decryption the only private material is `database.dump`, `config.toml`, `ssh_host_key` and `storage.tar`; stored secret columns are wrapped with `audit_ip_keys.key` NULL; the manifest carries `installation.key_id` and no key; no API, `doctor`, `secrets status`, error body or `Debug` output returns key material. | Nothing. |
| **U29** | Rotation rewraps data keys and keeps every secret readable; startup, migration and restore fail distinctly on a missing key, wrong key, wrong id, loose mode and a mismatched key, and restore drops the partial database and publishes nothing; a damaged wrapper archive is refused end to end. | The racing-write clause only for a boundary: a write that **starts before the backup's recorded PostgreSQL position** is restored or refused; a write acknowledged after that position is deliberately in neither the dump nor the storage snapshot. ADR 0014 states this, and the exit sentence's unqualified "a write racing an online backup is in the restore or was refused" is not demonstrated beyond it. See limitation 3. |
| **U30** | The Git scheduler keeps an organization at its configured share: worst measured ratio 1.09 against a 2.0 limit, with a no-cap control at 4.27–4.64×. Anonymous clone is limited per address (429 with `Retry-After`) and per installation; anonymous login is limited per address and per installation; organization creation is capped per account and per installation; one organization cannot take every job worker (2 of 4) or spend another organization's API budget. | The p95 bound for API and job capacity. Those legs have independent budget counters and a worker-share query, not a latency measurement at load; the exit sentence's "requests past the stated bound" is measured only for Git. The Git number is a scheduler simulation with a fixed 20 ms service time, not a load generator against `serve`. See limitation 1. |
| **U30a** | A seeded installation exports, imports into a fresh installation, re-exports and agrees with the source rows on counts, object and part checksums, Git ref digests, every metadata field and sampled content; six deliberate corruptions are refused before writing, so the round trip is falsifiable. | Nothing. |
| **U30b** | The abuse contact and six-step takedown runbook are published; `doctor` reports a missing SPF, DKIM or DMARC record and a missing bounce configuration, and treats NXDOMAIN as a missing record rather than an outage; the verification-mail budget is enforced per address and notification mail is charged per organization; the sign-up velocity limiter refuses above the configured per-address velocity at its interface. | Nothing for this build. The sign-up endpoint is M11a's, which is the case's own stated scope. |
| **U30c** | Link-local, loopback, private-range and metadata destinations are refused, with metadata behind its own option; a redirect to a different host is refused and recorded; a DNS answer that changes after the check cannot redirect the connection (real-socket pin); the allow path records the allow; a default client opens no socket; a guard test finds no other outbound client in any crate. | Nothing beyond the two ADR-named non-HTTP exceptions — SMTP and the maintenance loopback probe/control — which use the shared address policy rather than the one HTTP client. |

Every case above has one revert spot-check recorded in the evidence that makes
its named test fail (U24 checksum comparison, U25 cleanup sweep, U26 run gate,
U27 legacy deadline, U28 plaintext audit key, U29 rotation selection, U30
scheduler floor and ceiling, U30a metadata count, U30b NXDOMAIN branch, U30c
cross-host redirect). A test nobody can break is not evidence, so those reverts
are part of the assertion.

## The measured fairness ratio

The only fairness measurement in the tree is
`fairness::tests::an_organization_at_its_share_stays_within_twice_the_single_organization_p95`,
run three consecutive times with `--nocapture`:

| Run | Single p95 | Shared p95 | Ratio | No-cap control |
| --- | --- | --- | --- | --- |
| 1 | 271.2 ms | 295.0 ms | **1.09** | 4.27× |
| 2 | 263.6 ms | 278.0 ms | **1.05** | 4.62× |
| 3 | 273.1 ms | 272.3 ms | **1.00** | 4.64× |

Worst measured ratio **1.09** against a limit of 2.0; the control breaches in
every run. The shape is fixed: 48 requests at 4 in flight for the measured
organization, the same 48-at-4 while a second organization offers 480 requests
at 32 in flight, 8 global slots with floor 1 and ceiling 4, 20 ms simulated
service time per request. It is reproducible for that harness. It measures the
scheduler, not Git, PostgreSQL or HTTP, so it does not turn the U30 API and job
legs into measured p95 bounds.

## Published claims that had to be corrected before this signature

The exit verification found three claims in `docs/operations/limits.md` that the
code did not enforce. Two were fixed in code and one was withdrawn from
publication; the earlier "all three fixed in code" wording is withdrawn. The
signature covers the corrected state, not the published one:

1. **Organization storage quota over Git repositories — fixed.** `storage_quota::measure`
   sums every non-deleted repository's `objects/` tree for the organization;
   `git_http.rs` and `ssh.rs` pass the tenant figures to `pre-receive`, and
   `git_hooks.rs` refuses with `organization storage quota exceeded`. Exercised
   at the published 50 GiB boundary and by
   `organization_quota_refuses_a_push_across_the_tenants_repositories`.
2. **Merge-created objects — fixed.** `merge::execute_intent` measures the
   repository and the organization after the candidate is written and before
   the ref is compare-and-swapped; over either quota the merge fails and the ref
   stays where it was
   (`a_merge_that_crosses_the_tenant_total_is_refused_and_the_ref_stays`).
3. **Anonymous search budget — withdrawn from publication.** No search route
   ships in M4.2, so the operator table publishes **Anonymous login** only.
   `Limits::allow_anonymous_search` spends the same 20/min and 300/min counters
   as sign-in, and M9's route will inherit that one budget; the limiter is
   exercised above both published numbers by
   `the_anonymous_search_limiter_refuses_above_the_published_budget`.

## The five recorded limitations

Accepted for this milestone and repeated in ADR 0014 ("Limitations recorded at
the M4.2 exit"). They are not claimed as done:

1. **No measured p95 for API or job capacity.** Git has the reproducible
   relative measurement above; API isolation is independent counters and job
   isolation is the 2-of-4 worker share. Neither is a latency measurement at
   load.
2. **Abandoned-upload cleanup is tested at the store interface, not through the
   reconciler.** The deadline and resume cases are in
   `crates/githearth-git/tests/object_store.rs`; nothing fails if `reconcile.rs`
   stops calling `cleanup_abandoned`.
3. **The U29 write guarantee is bounded by the recorded position.** Writes that
   start before the backup's recorded PostgreSQL position are restored or
   refused; writes acknowledged after it are in neither the dump nor the
   snapshot. ADR 0014 says so; the exit sentence does not.
4. **SMTP credentials remain an operator file, not an envelope column.**
   `smtp_password_file` names a file the operator owns and the archive does not
   carry. Only the audit IP key is a stored secret column in this schema.
5. **KMS is an interface only.** The shipped binary refuses `method = "kms"`
   with "no KMS adapter is built into this binary"; no adapter ships, and a
   deployment needing KMS custody must supply one out of tree.

## What the signatures assert

Both signers assert that the evidence record is an accurate account of what was
run at `a4541b5` and `5913cd8`; that each clause marked **met** above is backed
by a named executable test, and that each case has at least one recorded revert
which makes its named test fail; that the conditions on U25, U27, U29 and U30
are as stated, not narrower; that the three published `limits.md` claims were
corrected as described; that the five limitations are recorded rather than
silently closed; and that M4.2 is accepted for what is recorded here.

## What the signatures do not claim

They do not claim a clean pass, and they do not claim that every clause of the
M4.2 exit sentence is demonstrated. They do not claim a product-level p95 for
API or job capacity; a measured bound for any write racing a backup regardless
of when it started; a reconciler integration test for upload cleanup; a
committed test for legacy `repo:write`; SMTP credentials stored as a column; a
shipped KMS adapter; or a fresh gate run on this documentation-only commit. A
reader who needs any of those should treat the relevant case as open, not as
signed.

## Sign-off

Two reviewers, two independent assertions. Neither signature converts a clause
marked not demonstrated into a demonstrated one.

### Reviewer 1

- Name: Jadav Havish Sunil (`HavishJadav`)
- Date: 2026-11-16
- Verified: I reviewed PR #206 through its cross-review and the `5913cd8`
  correction, re-read each U24–U30c row in
  [m4-2-evidence.md](m4-2-evidence.md) against the test it names, checked the
  three published `limits.md` claims against the code and the withdrawal of the
  search claim, and I sign M4.2 with the U25 reconciler gap, the uncommitted
  `repo:write` half of U27, the recorded-position boundary on U29, the absent
  API/job p95 on U30 and the five recorded limitations standing as written.

### Reviewer 2

- Name: Aarav (`DeviousCardi`)
- Date: 2026-11-16
- Verified: I ran the evidence collection at `a4541b5`, the completion pass and
  its revert spot-checks at `5913cd8`, and the three fairness runs recorded
  above (worst ratio 1.09, control 4.27–4.64×); I checked that every case's
  revert makes its named test fail, that the doctor count agrees across the
  binary, `install.md` and `tests/setup.rs`, and that the five limitations are
  recorded in ADR 0014 and in the evidence, and I sign that record as an
  accurate account of what was run — not as proof of the clauses marked not
  demonstrated.
