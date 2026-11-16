# M4.2 exit evidence — U24–U30c

Date: 2026-11-16. Collected on branch `m42/exit-evidence` at `a4541b5`
(`main`), clean working tree. A completion pass on branch `m42/complete`
fixed two of the three published claims below in the code and withdrew the
third from publication; the fixes, their files and their proofs are in "The
three published claims — two fixed in code, one withdrawn from publication" at
the end of this document. This document records case-by-case acceptance
evidence for the M4.2 platform slice. It is not a code summary and not a
sign-off: `m4-2-signoff.md` is deliberately not written, and the two signature
lines are not mine to add.

No file under `docs/plan/` was changed and no code was modified in the evidence
tree. Revert proofs were run in a detached scratch worktree at the same commit
(`/tmp/ght/revert`) with a shared cargo target directory; the evidence tree
stayed clean throughout. The completion pass named in the header did modify
code, in its own worktree (`m42/complete`); its proofs and gate counts
are recorded in "The three published claims — two fixed in code, one withdrawn
from publication" below.

## Under test

| Item | Value |
| --- | --- |
| Worktree | `/Users/suniljadav/Aarav/Github/githearth-m42-ev` |
| Branch / commit | `m42/exit-evidence` at `a4541b5` (clean) |
| Binary | `target/debug/githearth-server`, built from that commit |
| PostgreSQL | private container `ghpg-m42-ev`, 17-bookworm, `127.0.0.1:64948` |
| Test database | `GITHEARTH_TEST_DATABASE_URL=postgres://githearth:githearth@127.0.0.1:64948/postgres` |
| `TMPDIR` | `/tmp/ght` (the long default breaks ten socket-path tests) |
| Host | Apple M5, 10 cores, arm64, macOS 26.6 |
| Toolchain | rustc/cargo 1.99.1, git 2.50.1 (Apple Git-155), psql 17.11 |

Commands below are run from the worktree root with
`GITHEARTH_TEST_DATABASE_URL` and `TMPDIR` as above.

Completion pass (2026-11-16): branch `m42/complete`, PostgreSQL container
`ghpg-m42-c1` (`127.0.0.1:63713`), same host and toolchain.
`cargo test --workspace --lib` = 337 passed, 0 failed, 31 ignored;
`git_push` 43, `limits` 6 and `merge_execute` 11 passed with
`--include-ignored` on each of three consecutive runs;
`setup` 25, `doctor` 6, `git_http` 23, `ssh` 14, `git_admission` 18,
`repository_api` 35 and `backup_restore` 13 passed once each with
`--include-ignored`. `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets -- -D warnings` are clean.

---

## U24 — object survives restart and backup/restore

> **U24 (B):** An object written through the interface survives restart and
> `backup` → `restore`; a ranged read and an appended log are correct; a
> corrupted object is detected and refused.

**Scenario.** The product test writes a whole object larger than one 1 MiB
chunk, creates an appendable log with two lines, and reads a 9-byte range that
crosses the chunk boundary. It then reopens the store (restart), reads and
appends again, runs the real `backup` command with an encryption key, restores
into a fresh installation with the real `restore` command, and finally flips one
byte in the restored payload.

```text
$ cargo test -p githearth-server --test backup_restore \
    objects_survive_restart -- --include-ignored --nocapture
running 1 test
test objects_survive_restart_and_backup_restore_and_corruption_is_refused ... ok
test result: ok. 1 passed; 0 failed; ... finished in 14.09s
```

The test also asserts the archive's manifest lists exactly the two published
objects (not the orphan), and that `verify_all` reports the deliberately
removed orphan sidecar as `Orphan` without failing the backup.

| Clause | Verdict | Evidence |
| --- | --- | --- |
| written through the interface | pass | `store.put` / `create_appendable` / `append` in the test; same trait the server holds |
| survives restart | pass | fresh `FsObjectStore::open` over the same files returns identical bytes |
| survives `backup` → `restore` | pass | real `backup` / `restore`; restored object and log compare byte-for-byte |
| ranged read correct (boundary-crossing) | pass | 9-byte range at `CHUNK_BYTES-4..=+4`, before and after restart and restore |
| appended log correct after restart | pass | `first line\nsecond line\n` then `third line\n` after restore |
| corrupted object detected and refused | pass | flipped byte returns `ChecksumMismatch`, never bytes |

**Proving test:**
`objects_survive_restart_and_backup_restore_and_corruption_is_refused`
(`crates/githearth-server/tests/backup_restore.rs`), plus the unit cases
`put_get_stat_checksum_list_and_delete_round_trip`,
`ranged_read_crosses_a_chunk_boundary_and_verifies_both_chunks` and
`a_corrupted_object_is_detected_and_refused`
(`crates/githearth-git/tests/object_store.rs`).

**Revert spot-check (U24).** In the scratch tree the whole-object checksum
comparison in `get` was disabled (`if actual != meta.sha256` →
`if false && …`):

```text
test a_corrupted_object_is_detected_and_refused ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 0.06s
```

The mechanism is load-bearing.

## U25 — abandoned upload and resumed upload

> **U25 (B):** An abandoned multipart upload is cleaned up after its deadline;
> a resumed upload completes without duplication.

**Scenario.** Two object-store cases: one creates a multipart upload with a
60-second deadline and one part, calls `cleanup_abandoned` inside and then past
the deadline; the other creates parts, drops the store handle, reopens it,
re-uploads a part, completes, and tries to complete the same upload again.

```text
$ cargo test -p githearth-git --test object_store
running 23 tests
test an_abandoned_upload_is_cleaned_up_after_its_deadline ... ok
test a_resumed_upload_completes_once_without_duplication ... ok
test result: ok. 23 passed; 0 failed; 0 ignored; ... finished in 1.97s
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| cleaned up after its deadline | pass | inside deadline returns empty and upload still resumable; at +61 s it returns the upload, releases 10 upload bytes, and `list_parts` is `InvalidUpload` |
| resumed upload completes without duplication | pass | all parts listed from disk after reopen; re-upload replaces part 2; result is `first/second/third`, 18 bytes; second completion refused |

**Proving tests:** `an_abandoned_upload_is_cleaned_up_after_its_deadline`,
`a_resumed_upload_completes_once_without_duplication`
(`crates/githearth-git/tests/object_store.rs`).

**Product wiring (not separately test-exercised).** `reconcile.rs` calls
`objects.cleanup_abandoned(now)` on every pass and `reconcile_usage()`
afterwards. The cleanup mechanism and deadline are tested at the interface;
the reconciler call itself is inspected, not covered by an integration test.
This is recorded as a condition below.

**Revert spot-check (U25).** Early-returning an empty vector from
`cleanup_abandoned`:

```text
test an_abandoned_upload_is_cleaned_up_after_its_deadline ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 0.06s
```

## U26 — subject confinement on API, Git HTTP and SSH

> **U26 (Both):** A token of each subject type (user, service account, run
> token) cannot act outside its resource set, scope or lifetime on the API or
> Git HTTP; a run token is denied after the run ends, tested against a stub run
> record because runs arrive in M6; a service account uses its SSH key and
> cannot exceed its grants.

**Scenario.** The integration suite creates two repositories in one
organization and issues scoped tokens for an account, a service account and a
stub `ci_runs` row, then attempts the inside and outside repository on both
transports. It also checks scope, audience, revocation, expiry, audit actor,
run cancellation/success, and a service-account SSH key with a grant on one
repository only.

```text
$ cargo test -p githearth-server --test token_principals \
    -- --include-ignored --nocapture
running 9 tests
test issuance_returns_the_token_once_and_revocation_is_immediate ... ok
test every_subject_is_confined_on_the_api ... ok
test legacy_token_works_until_its_stated_end_then_is_refused ... ok
test every_subject_is_confined_on_git_http ... ok
test ssh_service_account_key_cannot_exceed_its_grants ... ok
test result: ok. 9 passed; 0 failed; 0 ignored; ... finished in 6.99s
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| account token confined to resource set | pass | inside repo `200`/clone succeeds, outside `404`/clone fails |
| service-account token confined | pass | inside succeeds, outside fails on API and Git HTTP |
| run token confined | pass | inside succeeds, outside fails on API and Git HTTP |
| scope confinement | pass | `package:read` token cannot read a repository (`404`); `repo:read` cannot create a repository (`401`) |
| audience confinement | pass | API-only token is not a Git credential; Git-only token is `401` on the API |
| lifetime / revocation | pass | expired or revoked token `401` immediately on API and Git; run token `401` after `cancelled` and after `succeeded` |
| run denial after run end | pass | stub `ci_runs` row updated; next admission refused |
| service account SSH key cannot exceed grants | pass | key signs in over SSH, inside repo allowed, outside denied |

`issuance_returns_the_token_once_and_revocation_is_immediate` also asserts the
row stores only `SHA-256(token)`, no list response carries the plaintext, and
every non-account subject has its own audit actor (`service_account` resolves
to the service account; `run` resolves to the run and triggering account).

**Proving tests:** `every_subject_is_confined_on_the_api`,
`every_subject_is_confined_on_git_http`,
`ssh_service_account_key_cannot_exceed_its_grants`,
`issuance_returns_the_token_once_and_revocation_is_immediate`
(`crates/githearth-server/tests/token_principals.rs`).

**Revert spot-check (U26).** The run ended/status/suspension gate in
`authenticate_access_token_actor` was replaced with `if false`:

```text
test every_subject_is_confined_on_the_api ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 1.00s
```

## U27 — the legacy deprecation window

> **U27 (A):** Existing `repo:read`/`repo:write` tokens keep working during the
> deprecation window and are refused after their stated end.

**Scenario.** A legacy token (no resource set) is issued over the API. The test
reads its stored deadline, uses it inside the window, reads `doctor`'s announced
cutoff and count, changes the configured date to a later date (must not extend),
moves the stored instant into the past (the clock equivalent of the cutoff), and
then attempts the API again.

```text
test legacy_token_works_until_its_stated_end_then_is_refused ... ok
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| legacy `repo:read` token keeps working in the window | pass | `GET` on the repository is `200` |
| legacy `repo:write` token keeps working in the window | pass (supplementary scenario) | the shipped test issues only `["repo:read"]`. A scratch copy of the same test changed the scopes to `["repo:read","repo:write"]` and added a `POST /organizations/{org}/repositories`; it returned `201 CREATED`. The shipped tree still does not exercise this half. |
| the stated end is what is enforced | pass | default `2027-06-30` stores exactly `2027-07-01T00:00:00Z`; `doctor` prints the same instant |
| refused after the stated end | pass | past deadline returns `401` |
| window cannot be extended | pass | a later configured date does not move a shortened deadline; `2027-07-01` and `2027-02-30` fail validation |
| a legacy row cannot have no deadline | pass | `UPDATE … SET legacy_deadline = NULL` is rejected by the schema |

**Proving test:** `legacy_token_works_until_its_stated_end_then_is_refused`
(`crates/githearth-server/tests/token_principals.rs`).

**Revert spot-check (U27).** Removing the `legacy_deadline` term from the
actor-resolution expiry predicate:

```text
test legacy_token_works_until_its_stated_end_then_is_refused ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 1.53s
```

## U28 — no plaintext secret in a dump or decrypted archive

> **U28 (Both):** A database dump contains no plaintext secret, including the
> audit IP key; the encrypted archive contains no plaintext secret, and after
> decryption the only private material present is the database and SMTP values
> in the configuration file and the SSH host key, all operator-owned, while
> every stored secret column is encrypted under the installation key, which is
> not in the archive; the manifest has a key id and no key.

**Scenario.** A fresh installation logs in (creating an audit IP key), decrypts
that key to get the 32 plaintext bytes, takes a real `pg_dump` through the
product, runs the real `backup`, decrypts the archive, expands its
`database.dump` with `pg_restore`, and byte-searches the dump, the expanded
archive database and every decrypted file for the audit IP key, installation
key (base64 and raw), archive key and SMTP credential. It also asserts the
manifest's key id is present and contains no key, then plants a plaintext audit
key as a positive control that the byte search can find.

```text
$ cargo test -p githearth-server --test secrets_at_rest \
    -- --include-ignored --nocapture
running 15 tests
test dump_and_decrypted_archive_contain_no_plaintext_secret ... ok
test no_api_doctor_or_error_returns_a_secret ... ok
test a_plaintext_audit_key_is_refused_at_use_and_never_returned ... ok
test result: ok. 15 passed; 0 failed; 0 ignored; ... finished in 14.48s
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| dump has no plaintext audit IP key | pass | 32-byte key absent from `pg_dump`; planted-key control proves the search is sensitive |
| dump has no installation/archive key or SMTP credential | pass | base64 key, raw key, archive key and SMTP password all absent |
| encrypted archive has no plaintext secret | pass | archive is opaque; decrypted tree is searched file by file |
| after decryption only operator-owned private material | pass | `database.dump`, `config.toml` (database URL, username), `ssh_host_key`, `storage.tar`; SMTP password is a separate operator file and absent |
| every stored secret column encrypted under installation key | pass | `audit_ip_keys.key` is NULL after backfill, `secret_id` names a `stored_secrets` row; startup re-count refuses a residual plaintext row |
| installation key not in archive | pass | manifest carries `installation.key_id` only; key material absent from every file |
| manifest has key id and no key | pass | `manifest["installation"]["key_id"]` equals the current key id; manifest text contains no key |

`no_api_doctor_or_error_returns_a_secret` additionally checks that no API,
`doctor`, `secrets status` or error body returns key material, and that `Debug`
does not print it.

**Proving tests:** `dump_and_decrypted_archive_contain_no_plaintext_secret`,
`no_api_doctor_or_error_returns_a_secret`,
`a_plaintext_audit_key_is_refused_at_use_and_never_returned`
(`crates/githearth-server/tests/secrets_at_rest.rs`).

**Revert spot-check (U28).** The new audit IP key was also written to the
plaintext `key` column at creation (`INSERT … (id, secret_id, key)`):

```text
the database dump contains the plaintext audit IP key
test dump_and_decrypted_archive_contain_no_plaintext_secret ... FAILED
```

## U29 — rotation, wrong/missing key, racing write

> **U29 (Both):** Rotating the installation key keeps every secret readable;
> restoring with the wrong key or no key fails clearly and never starts with
> unreadable secrets; a write racing an online backup is in the restore or was
> refused.

**Scenario.** Three groups: (a) `secrets_at_rest` rotates to a new key and reads
every secret; (b) key-failure cases run startup, migration and restore with a
missing, wrong, loose-mode or mismatched key; (c) `online_backup` runs the real
command against a live server with writers, a snapshotless filesystem and
damaged archives; `backup_restore` runs the pre-position push race.

```text
$ cargo test -p githearth-server --test secrets_at_rest -- --include-ignored
running 15 tests ... test result: ok. 15 passed; 0 failed; ... 14.48s

$ cargo test -p githearth-server --test online_backup -- --include-ignored
running 11 tests
test online_backup_with_a_writer_running_restores_consistently ... ok
test a_write_that_starts_before_the_recorded_position_is_restored_or_refused ... ok
test a_write_after_the_recorded_position_is_acknowledged_and_absent_by_design ... ok
test key_failure_modes_are_distinct_and_never_start_with_unreadable_secrets ... ok
test a_damaged_wrapper_archive_is_refused_end_to_end_and_published_nowhere ... ok
test a_snapshotless_filesystem_falls_back_to_the_maintenance_fence ... ok
test a_gate_that_cannot_drain_is_released_and_the_server_keeps_writing ... ok
test result: ok. 11 passed; 0 failed; 0 ignored; ... 44.31s

$ cargo test -p githearth-server --test backup_restore \
    a_push_racing_the_snapshot -- --include-ignored
test a_push_racing_the_snapshot_is_in_the_restore_or_was_refused ... ok
test result: ok. 1 passed; 0 failed; ... 27.93s
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| rotation keeps every secret readable | pass | `rotation_rewraps_data_keys_and_keeps_every_secret_readable`; rotation unwraps/wraps DEKs, never decrypts payloads; `secrets status` reports complete only when every row opens |
| wrong key fails clearly | pass | distinct failures for missing file, wrong key, wrong id, loose mode and damaged wrapper; no server start, restore drops the partial database and publishes nothing |
| no key fails clearly, no unreadable start | pass | `missing_or_wrong_key_fails_at_startup_and_migration`, `restore_without_the_installation_key_fails_before_publishing` |
| a write racing an online backup is in the restore or refused | pass **with a stated boundary** | a push that starts before the recorded position is restored or refused (`a_push_racing_the_snapshot…`, `a_write_that_starts_before_the_recorded_position…`). A write acknowledged after the recorded position is absent from both dump and storage by design; ADR 0014 states this explicitly. The plan's exit sentence does not state that boundary. |

**Proving tests:** `rotation_rewraps_data_keys_and_keeps_every_secret_readable`,
`missing_or_wrong_key_fails_at_startup_and_migration`,
`restore_without_the_installation_key_fails_before_publishing`,
`key_failure_modes_are_distinct_and_never_start_with_unreadable_secrets`,
`a_damaged_wrapper_archive_is_refused_end_to_end_and_published_nowhere`,
`online_backup_with_a_writer_running_restores_consistently`,
`a_write_that_starts_before_the_recorded_position_is_restored_or_refused`,
`a_push_racing_the_snapshot_is_in_the_restore_or_was_refused`.

**Revert spot-check (U29).** The rotation selection was made empty
(`WHERE 1 = 0 AND …`):

```text
test rotation_rewraps_data_keys_and_keeps_every_secret_readable ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 1.22s
```

## U30 — the relative fairness bound and the anonymous caps

> **U30 (Both):** One organization at its configured share does not push
> another organization's p95 past twice the single-organization measurement;
> anonymous clone and login are limited per address and globally; organization
> creation is capped per account and per installation.

### The measured bound (not asserted)

The only measurement in the tree is the scheduler simulation
`fairness::tests::an_organization_at_its_share_stays_within_twice_the_single_organization_p95`.
It was run three times here with `--nocapture`:

```text
$ cargo test -p githearth-server --lib fairness::tests::an_organization_at_its_share -- --nocapture

run 1: single p95 271.243083ms, shared p95 295.009583ms, ratio 1.09;
       no-cap control p95 1.158576417s, control ratio 4.27
run 2: single p95 263.619833ms, shared p95 277.973791ms, ratio 1.05;
       no-cap control p95 1.219085917s, control ratio 4.62
run 3: single p95 273.097333ms, shared p95 272.314959ms, ratio 1.00;
       no-cap control p95 1.266483041s, control ratio 4.64

test result: ok. 1 passed (each run)
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 10 cores, arm64, macOS 26.6 |
| Load, single leg | 48 requests, 4 in flight, one organization |
| Load, shared leg | the same 48-at-4 for the measured organization, while a second organization offers 480 requests at 32 in flight |
| Service time | fixed 20 ms simulated per request (scheduler, not a real Git/PostgreSQL operation) |
| Scheduler | 8 global slots, floor 1, ceiling 4 |
| Single p95 | 263.6–273.1 ms |
| Shared p95 | 272.3–295.0 ms |
| Ratio | **1.00–1.09**, worst 1.09, against a limit of 2.0 |
| Negative control | **4.27–4.64×**, must breach and does |
| Reproducible? | yes for this harness: three consecutive runs, same shape, same control |

**What the measurement is not.** It is a scheduler simulation with a fixed
service time, not a load generator against a running `serve` over real HTTP and
Git with real repositories. A product-level p95 measurement would need: a
release build on a quiet machine, at least two organizations with real
repositories, a client pool producing the stated request rate, `info/refs` +
`upload-pack` traffic (or the API paths for the API leg), and a recorded p95
per organization. That was not run here. The number above is reproducible and
it is the number the gate/ADR use, but it measures the scheduler, not Git or
PostgreSQL.

### Anonymous and creation caps

```text
$ cargo test -p githearth-server --test limits -- --include-ignored
running 5 tests
test the_api_budget_is_charged_per_organization_on_every_route ... ok
test the_api_budget_is_charged_per_account_without_an_organization_path ... ok
test organization_creation_is_capped_at_the_endpoint ... ok
test sign_in_is_refused_over_the_anonymous_login_budget ... ok
test a_stranger_cannot_spend_an_organizations_api_budget ... ok
test result: ok. 5 passed; 0 failed; ... 2.58s

$ cargo test -p githearth-server --test git_http anonymous_fetch_is_limited -- --include-ignored
test anonymous_fetch_is_limited_per_address_with_retry_after ... ok
test result: ok. 1 passed; 0 failed; ... 2.12s

$ cargo test -p githearth-server --lib limits::tests -- --nocapture
running 11 tests ... test result: ok. 11 passed; 0 failed; ... 0.02s

$ cargo test -p githearth-server --test jobs one_organization_cannot_take_every_job_worker -- --include-ignored
test one_organization_cannot_take_every_job_worker ... ok
test result: ok. 1 passed; 0 failed; ... 6.30s
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| p95 within 2× at configured share | pass (scheduler; see scope above) | worst measured 1.09; control 4.27–4.64× |
| anonymous clone limited per address and globally | pass | 429 with `Retry-After` at the per-address limit; unit tests cover the installation budget a second address cannot escape |
| anonymous login limited per address and globally | pass | `sign_in_is_refused_over_the_anonymous_login_budget`; installation budget unit test |
| organization creation capped per account and installation | pass | endpoint test; unit test covers both counters |
| one organization cannot take every job worker | pass | 2 of 4 workers; other organization's jobs complete while the first still has work |
| one organization cannot spend another's API budget | pass | per-organization middleware; a stranger cannot charge a victim organization |
| one organization saturating API/job capacity stays within a **measured p95** bound | **not demonstrated** | API isolation is proven by independent budget counters, jobs by the worker-share query; neither has a p95 measurement like the Git scheduler |

**Proving tests:** `an_organization_at_its_share_stays_within_twice_the_single_organization_p95`
(`src/fairness.rs`), `the_api_budget_is_charged_per_organization_on_every_route`,
`organization_creation_is_capped_at_the_endpoint`,
`sign_in_is_refused_over_the_anonymous_login_budget`,
`a_stranger_cannot_spend_an_organizations_api_budget`
(`tests/limits.rs`), `anonymous_fetch_is_limited_per_address_with_retry_after`
(`tests/git_http.rs`), `one_organization_cannot_take_every_job_worker`
(`tests/jobs.rs`).

**Revert spot-check (U30).** `GitScheduler::new` was changed to
`floor = ceiling = capacity` (the old single semaphore):

```text
shared p95 1.227851125s (4.45x) exceeds twice the single-organization p95 275.734333ms
test an_organization_at_its_share_stays_within_twice_the_single_organization_p95 ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 2.76s
```

## U30a — the export contract round-trips

> **U30a (A):** The export contract round-trips: an installation exports,
> imports into a fresh installation, and counts, checksums and sampled content
> agree.

**Scenario.** `tests/export_import.rs` seeds two organizations, accounts, a
repository with two commits, an issue, a pull request and review, a team with a
member and grant, a fork and an object. It exports, imports into a fresh
installation, exports again and compares counts, checksums, native Git refs and
every metadata field against the **source database rows**. It also damages an
object, changes a manifest count, drops an object while restating the part
checksum, alters a metadata field while restating the part, sets an unknown
version, changes a team name, swaps two grant ids, and reverses a fork's UUID
order.

```text
$ cargo test -p githearth-server --test export_import -- --include-ignored --nocapture
running 5 tests
test export_import_round_trip_agrees_on_counts_checksums_and_content ... ok
test deliberate_failures_are_refused_before_writing ... ok
test the_harness_refuses_a_wrong_name_and_a_swapped_grant_id ... ok
test a_fork_parent_survives_in_both_uuid_orders ... ok
test unknown_versions_and_non_empty_destinations_are_refused ... ok
test result: ok. 5 passed; 0 failed; 0 ignored; ... finished in 12.94s
```

The captured deliberate-failure output shows each refusal reason (object bytes,
metadata count, object manifest, referential field, wrong team name, swapped
grant ids), which is what makes the round trip falsifiable rather than
self-agreeing.

| Clause | Verdict | Evidence |
| --- | --- | --- |
| an installation exports | pass | real export of a seeded installation |
| imports into a fresh installation | pass | import creates a new database, migrations, Git and storage in a staging tree |
| counts agree | pass | manifest counts vs records vs destination |
| checksums agree | pass | object SHA-256, part checksums and Git refs digests compared |
| sampled content agrees | pass | every metadata record/field compared to source rows; files and commits sampled |
| a round trip that cannot fail is not evidence | pass | six deliberate corruptions all refused before writing |

**Proving test:** `export_import_round_trip_agrees_on_counts_checksums_and_content`
(`crates/githearth-server/tests/export_import.rs`), with the four falsifiability
cases in the same file.

**Revert spot-check (U30a).** The metadata count check was disabled
(`if false && actual != manifest.counts`):

```text
test deliberate_failures_are_refused_before_writing ... FAILED
test result: FAILED. 0 passed; 1 failed; ... finished in 6.44s
```

## U30b — abuse baseline and `doctor`

> **U30b (A):** The abuse contact and takedown runbook are published; `doctor`
> reports a missing SPF/DKIM/DMARC record or bounce configuration; the
> configured email rate limit is enforced; the sign-up velocity limiter refuses
> a request above the configured per-address velocity (tested at its interface;
> the endpoint is M11a's).

**Scenario.** Read `docs/operations/abuse.md`; run `doctor`'s DNS/bounce unit
cases; run the verification-mail budget case; run the sign-up limiter unit
case; run a clean `setup` + `doctor`.

```text
$ cargo test -p githearth-server --lib 'doctor::tests' -- --nocapture
running 25 tests
test doctor::tests::email_dns_reports_a_missing_record_and_bounce_setting ... ok
test doctor::tests::nxdomain_is_a_missing_record_not_an_outage ... ok
... 14 checks, 1 failed, 0 warnings, 7 skipped   (unreachable-database case)
test result: ok. 25 passed; 0 failed; 8 ignored; ... 2.08s

$ cargo test -p githearth-server --test email_verification \
    a_verification_mail_is_refused -- --include-ignored
test a_verification_mail_is_refused_over_the_per_address_budget ... ok
test result: ok. 1 passed; 0 failed; ... 1.23s

$ cargo test -p githearth-server --lib limits::tests::signup_velocity -- --nocapture
test limits::tests::signup_velocity_is_per_address ... ok
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| abuse contact and takedown runbook published | pass | `docs/operations/abuse.md` has the contact, categories and six-step takedown |
| `doctor` reports missing SPF/DKIM/DMARC | pass | `email_dns_reports_a_missing_record_and_bounce_setting` asserts SPF/DMARC/DKIM failures; NXDOMAIN is a missing record, not an outage |
| `doctor` reports missing bounce configuration | pass | empty `smtp_bounce_address` fails and names the setting |
| configured email rate limit enforced | pass | verification mail refused over the per-address security budget; notification mail charged per organization |
| sign-up velocity limiter refuses above per-address velocity | pass | `signup_velocity_is_per_address`; endpoint is M11a's, as stated |

**Proving tests:** `doctor::tests::email_dns_reports_a_missing_record_and_bounce_setting`,
`doctor::tests::nxdomain_is_a_missing_record_not_an_outage`,
`a_verification_mail_is_refused_over_the_per_address_budget`,
`limits::tests::signup_velocity_is_per_address`.

**Revert spot-check (U30b).** The NXDOMAIN branch was removed from
`dns::parse_response`:

```text
NXDOMAIN is an answer: "DNS server returned error code 3"
test dns::tests::nxdomain_is_an_empty_answer_not_an_outage ... FAILED
test result: FAILED. 1 passed; 1 failed; ...
```

## U30c — the one outbound client

> **U30c (B):** Every server-initiated request uses the one outbound client: a
> request to a link-local, loopback, private-range or metadata address, a
> redirect to a different host, and a DNS answer that changes after the check
> are all refused and recorded; an allowlisted destination succeeds.

**Scenario.** The unit suite exercises destination parsing, address
classification, metadata by name and address (including IPv6 transition forms),
redirect refusal, DNS pinning over a real socket, bounds and deadlines. The
integration guard scans every crate for raw sockets, resolvers, HTTP clients
and proxy reads.

```text
$ cargo test -p githearth-server --lib outbound:: -- --nocapture
running 32 tests ... test result: ok. 32 passed; 0 failed; ... 0.27s

$ cargo test -p githearth-server --test outbound_client -- --include-ignored
running 5 tests
test only_the_outbound_module_opens_outbound_connections ... ok
test no_proxy_variable_can_move_the_pinned_connection ... ok
test the_transport_is_private_and_resolves_in_exactly_one_place ... ok
test a_default_client_refuses_everything_without_opening_a_socket ... ok
test the_refusal_classification_is_the_public_contract ... ok
test result: ok. 5 passed; 0 failed; ... 0.26s
```

| Clause | Verdict | Evidence |
| --- | --- | --- |
| link-local/loopback/private/metadata refused | pass | every range classified; metadata needs its own option and private does not open it |
| redirect to a different host refused | pass | cross-host (including uppercase scheme / protocol-relative) refused and recorded |
| DNS answer changing after the check refused | pass | real-socket pin test: one resolution, next answer unconsumed; redirect-hop re-resolution test |
| refused and recorded | pass | refusal records name the host and reason; `a_default_client_refuses_everything…` opens no socket |
| allowlisted destination succeeds | pass | `an_allowlisted_destination_succeeds_and_the_allow_is_recorded` |
| every server request uses the one client | pass, with named non-HTTP exceptions | guard test scans all `crates/*/src`; SMTP and the maintenance loopback probe/control are the ADR-documented exceptions and use the shared address policy |

**Proving tests:** the 32 `outbound::tests` cases, including
`a_resolved_answer_is_pinned_to_the_connected_address_over_a_real_socket`,
`a_redirect_to_a_different_host_is_refused_and_recorded`,
`a_dns_answer_that_changes_after_the_check_cannot_redirect_the_connection`,
`an_allowlisted_destination_succeeds_and_the_allow_is_recorded`; and
`only_the_outbound_module_opens_outbound_connections`
(`tests/outbound_client.rs`).

**Revert spot-check (U30c).** The cross-host redirect comparison was disabled
(`if false && next.host != destination.host`):

```text
test outbound::tests::a_redirect_to_a_different_host_is_refused_and_recorded ... FAILED
test result: FAILED. 0 passed; 1 failed; ...
```

---

## Exit sentence, clause by clause

> **Exit:** an object written through the interface survives restart and
> `backup` → `restore`, a corrupted object is detected, an abandoned upload is
> cleaned up, and a ranged read and an appended log are correct after a
> restart.

| Clause | Verdict | Case / evidence |
| --- | --- | --- |
| object survives restart and `backup` → `restore` | pass | U24 |
| corrupted object detected | pass | U24 |
| abandoned upload cleaned up | pass (mechanism); product reconciler call not integration-tested | U25 |
| ranged read correct after restart | pass | U24 |
| appended log correct after restart | pass | U24 |

> A token of every subject type cannot act outside its resource set, scope or
> lifetime on the API or Git HTTP; a run token is denied after the run ends; a
> service account authenticates over SSH with its key and cannot exceed its
> grants.

| Clause | Verdict | Case / evidence |
| --- | --- | --- |
| account subject confined on API and Git HTTP | pass | U26 |
| service-account subject confined on API and Git HTTP | pass | U26 |
| run subject confined on API and Git HTTP | pass | U26 |
| run token denied after the run ends | pass | U26 |
| service account authenticates over SSH with its key | pass | U26 |
| service account cannot exceed its SSH grants | pass | U26 |

> A database dump contains no plaintext secret, including the audit IP key; the
> encrypted archive contains no plaintext secret, and after decryption the only
> private material present is the database and SMTP values in the configuration
> file and the SSH host key, all operator-owned, while every stored secret
> column is encrypted under the installation key, which is not in the archive;
> rotating the installation key keeps every secret readable; restoring without
> the key, or with the wrong key, fails with a clear error and does not silently
> start with unreadable secrets.

| Clause | Verdict | Case / evidence |
| --- | --- | --- |
| dump has no plaintext secret including audit IP key | pass | U28 |
| encrypted archive has no plaintext secret | pass | U28 |
| after decryption only DB/SMTP config values and SSH host key are private | pass | U28 (SMTP password is an operator file, absent from config) |
| every stored secret column encrypted under installation key | pass | U28 (only `audit_ip_keys.key` is a stored secret column in this schema) |
| installation key not in archive | pass | U28 |
| rotation keeps every secret readable | pass | U29 |
| missing/wrong key fails clearly, no unreadable start | pass | U29 |

> One organization saturating Git, API or job capacity does not push another
> organization's requests past the stated bound, and anonymous traffic cannot
> exhaust either.

| Clause | Verdict | Evidence |
| --- | --- | --- |
| Git saturation does not push a peer past 2× | pass | measured 1.00–1.09, control 4.27–4.64× |
| API saturation does not push a peer past the bound | pass at the published budget; **latency bound not measured** | independent per-organization counters; no p95 |
| job saturation does not push a peer past the bound | pass at the worker share; **latency bound not measured** | 2-of-4 claim test |
| anonymous traffic cannot exhaust either | pass | per-address + installation clone and login budgets |

> A write racing an online backup is in the restore or was refused.

| Clause | Verdict | Evidence |
| --- | --- | --- |
| a write racing an online backup is in the restore or refused | pass **for writes that start before the recorded position** | U29; the ADR states writes acknowledged after the position are absent by design, which the exit sentence does not state |

> Exporting the installation, importing it into a fresh installation and
> comparing counts, checksums and sampled content succeeds, and the harness is
> part of the gate.

| Clause | Verdict | Case / evidence |
| --- | --- | --- |
| export/import into a fresh installation | pass | U30a |
| counts, checksums and sampled content agree | pass | U30a |
| the harness is part of the gate | pass | `tests/export_import.rs` is a normal test target; five cases run here |

> The abuse contact and takedown runbook are published, `doctor` reports a
> missing SPF/DKIM/DMARC record or bounce configuration, the configured email
> rate limit is enforced, and the sign-up velocity limiter refuses a request
> above the configured per-address velocity (tested at its interface; the
> endpoint is M11a's).

| Clause | Verdict | Case / evidence |
| --- | --- | --- |
| abuse contact and takedown runbook published | pass | U30b / `docs/operations/abuse.md` |
| `doctor` reports missing SPF/DKIM/DMARC | pass | U30b |
| `doctor` reports missing bounce configuration | pass | U30b |
| email rate limit enforced | pass | U30b |
| sign-up velocity limiter at its interface | pass | U30b |

> A server-initiated request to a link-local, loopback, private or metadata
> address, or one redirected to a different host, is refused and recorded.

| Clause | Verdict | Case / evidence |
| --- | --- | --- |
| link-local/loopback/private/metadata refused and recorded | pass | U30c |
| redirect to a different host refused and recorded | pass | U30c |
| DNS answer changing after the check cannot redirect the connection | pass | U30c (lifted from the build bullet, tested) |

---

## `doctor` count and the published limits

### The count

Clean install, run through the binary on the private database:

```text
$ ./target/debug/githearth-server doctor --config /tmp/ght/clean/config.toml
ok   git: 2.50
ok   storage: /tmp/ght/clean/storage writable
ok   disk: 274.34 GiB free
ok   email: not configured (smtp_host is empty); no mail is sent
ok   email DNS: not configured; no SPF, DKIM or DMARC record is required until SMTP is enabled
ok   limits: per-address budgets use the connection peer; no reverse proxy is configured
ok   database: connected
ok   schema: latest v56
ok   tokens: no legacy tokens; legacy repo:read/repo:write tokens are refused from the UTC midnight after 2027-06-30
ok   installation key: sha256:735e2ed62dd97ebfcf226b2578fe6d4f, 0 secret(s), rotation complete
ok   lifecycle: no stuck repositories
ok   jobs: no dead, late or stuck jobs
ok   outbox: 0 pending, 0 delivering, 0 delivered delivery(ies)
ok   housekeeping: repositories are up to date
14 checks, 0 failed, 0 warnings, 0 skipped
```

| Source | Clean | With base backfill | Agree? |
| --- | --- | --- | --- |
| binary (`doctor::report`) | 14 | 15 (one warning) | — |
| `docs/operations/install.md` | fourteen, example `14 checks` | fifteenth `base backfill`, `15 checks` | yes |
| `tests/setup.rs` | `clean_host_reaches_a_doctor_clean_running_installation` asserts `14 checks, 0 failed` (lines 614, 843, 939) | `the_doctor_count_is_fourteen_clean_and_fifteen_with_a_base_backfill_warning` inserts the skip row, reruns the binary, and asserts `15 checks, 0 failed, 1 warnings` and the `warn base backfill:` line naming `#PR-1` | yes |
| `tests/housekeeping.rs` | — | asserts `checks.len() == 15` in `a_request_whose_target_branch_is_gone_is_reported_by_the_base_backfill` | yes |

The base-backfill case was run:

```text
test a_request_whose_target_branch_is_gone_is_reported_by_the_base_backfill ... ok
```

It asserts the fifteenth check is `base backfill`, status `Warn`, and names the
repository and pull request. The three sources agree.

### Every row of `docs/operations/limits.md`

The defaults in code match the published numbers: 8 Git slots; floor 1;
ceiling 4 clamped to `git_concurrency - 1`; 1 s wait; 300/60 API; 600/120
authenticated Git; 120/2,000 anonymous fetch; 20/300 anonymous login; 3/50
organization creation; 5 GiB/50 GiB storage; 2-of-4 job share; 200/h and 5/h
email; 3/address/hour sign-up. Enforcement points match the table for the API,
Git HTTP and SSH, anonymous clone/login, organization creation, job claim,
email sends, and the sign-up limiter.

The exit verification found three rows where the published claim was not what
the code did. Two were fixed in the completion pass and the third was withdrawn
from publication rather than left in `limits.md`:

| Row / claim | Was | Now |
| --- | --- | --- |
| **Anonymous login/search**: 20/min per address, 300/min installation | Only sign-in was charged. There is no search route in this build, so the "search" half of the row promised a budget nothing could spend. | The operator row is **Anonymous login**, because it names the surface this build routes. The budget in code is one: `Limits::allow_anonymous_search` spends the same per-address and installation counters as sign-in, and `tests/limits.rs::the_anonymous_search_limiter_refuses_above_the_published_budget` exercises both refusals. Search is M9's route; it calls that limiter. |
| **Repository storage**: 5 GiB per repository, **50 GiB per organization**, charged at "push hook and object store" | The push hook checked only `repository_quota_bytes`; `organization_quota_bytes` was consulted only by the object store. Git repository bytes were never summed per organization. | `storage_quota::measure` sums every non-deleted repository's `objects/` tree in the organization; `git_http.rs` and `ssh.rs` pass the tenant figure to `pre-receive`, which refuses with "organization storage quota exceeded". The page and row now name the merge path too, and say plainly that the configured number bounds the Git ledger and the object-store ledger separately (ADR 0014 §1) rather than their sum. |
| "Storage quota counts objects, **merge-created objects**, fork copies and stored large objects" | `git_hooks.rs` carried `// shortcut: merge-created objects are not counted`, because a server-side merge bypasses `pre-receive`. | `merge::execute_intent` measures the repository and the organization after writing the candidate and before moving the ref, so a merge commit, a squash's tree and a fetched fork head are charged. The shortcut comment is gone. |

The remaining rows are what the code enforces.

### The three published claims — two fixed in code, one withdrawn from publication

Two of the three were fixed end to end in the code: the Git half of the
organization quota, and the merge charge. The third, the anonymous **search**
budget, had no route to fix — no search surface ships in M4.2 (search is M9's).
The number is now a real limiter arm that M9's route will call, and the
operator documentation stopped publishing a limit for a route that is not
served. That is a withdrawal from publication, not a code fix, and it is
recorded here as such.

**1. Anonymous search budget — the number is enforced, the publication is
withdrawn.** The operator table names **Anonymous login**, because that is the
only anonymous surface this build routes; `githearth.example.toml` and the
M4.2 roadmap line say the same. The budget itself is one mechanism:
`Limits::allow_anonymous_search` spends the same `anonymous_login_per_address`
and `anonymous_login_global` counters as sign-in, so "20/minute for login and
search" will be one number when M9's route lands, not two. The refusal was
exercised at the limiter above both published budgets —
`tests/limits.rs::the_anonymous_search_limiter_refuses_above_the_published_budget`:

```text
running 1 test
test the_anonymous_search_limiter_refuses_above_the_published_budget ... ok
```

Twenty searches from `192.0.2.9` are admitted and the twenty-first is refused;
nineteen sign-ins plus one search fill the same twenty; fifteen addresses at
twenty each spend exactly 300 installation-wide and the 301st request is
refused. Making the search arm ignore the window (`allow_anonymous_search_at`
returning `true`) fails the test:

```text
thread 'the_anonymous_search_limiter_refuses_above_the_published_budget' panicked at
crates/githearth-server/tests/limits.rs:385:5:
the 21st search in the same minute is refused
test result: FAILED. 0 passed; 1 failed; ... filtered out; finished in 0.00s
```

The routed sign-in half is still enforced by `lib.rs` (`state.limits.
allow_anonymous_login(request.client_ip())`) before hashing; reverting that call
to `if false && …` fails `sign_in_is_refused_over_the_anonymous_login_budget`
with `429` expected and `401` returned.

**2. Organization quota covers Git repository storage.** The push path measures
the tenant in `git_http.rs` and `ssh.rs` (`storage_quota::measure`) and passes
`GITHEARTH_ORGANIZATION_QUOTA_BYTES` / `GITHEARTH_ORGANIZATION_OTHER_BYTES` to
the hook; `git_hooks.rs:188` refuses the push. The cost is one blocking walk of
the organization's `objects/` set per push while the repository lock is held,
documented in `docs/operations/quota.md`.

The published 50 GiB boundary was exercised through the shipped hook with
sparse `objects/` trees at the default (`organization_quota_bytes =
53687091200`): one repository carrying 9 GiB, a 1 GiB quarantine, and 40 GiB in
another repository of the same organization.

```text
--- exactly 50 GiB total (9 own + 1 quarantine + 40 other)
cannot reach the push service: No such file or directory (os error 2)
--- own grows by 1 byte => 50 GiB + 1
organization storage quota exceeded: 53687091201 of 53687091200 bytes
```

Exactly 50 GiB passes the quota check and fails later on the deliberately
absent socket; one byte more is refused with the published message. The
two-repository push over the tenant total also passes:
`tests/git_push.rs::organization_quota_refuses_a_push_across_the_tenants_repositories`
(`... ok`, 4.47 s). Reverting the hook's organization branch to
`if false && organization_quota > 0`:

```text
thread 'organization_quota_refuses_a_push_across_the_tenants_repositories' panicked at
crates/githearth-server/tests/git_push.rs:3401:5:
push over the tenant total must be rejected
test result: FAILED. 0 passed; 1 failed; ... finished in 6.24s
```

**3. Merge-created objects are counted.** `merge::execute_intent` measures both
quotas after the candidate exists and before the compare-and-swap, returning
`MergeOutcome::Failed { reason: "organization storage quota exceeded: …" }` and
leaving the ref untouched. The cost is the same tenant walk, once per merge.
The exercise leaves 8 bytes under the limit before the merge, so only the
objects the merge itself creates can take the repository over; the refused
merge is confirmed to leave the target ref where it was
(`tests/merge_execute.rs::a_merge_that_crosses_the_tenant_total_is_refused_and_the_ref_stays`,
`... ok`, 1.65 s, asserting `main_tip` is unchanged). Reverting the organization
branch to `if false && …`:

```text
thread 'a_merge_that_crosses_the_tenant_total_is_refused_and_the_ref_stays' panicked at
crates/githearth-server/tests/merge_execute.rs:1067:18:
a merge over the tenant total must be refused, got Merged { oid: "d7e1e3bfaa9a9df1f5490f5bc719962fef472e8d" }
test result: FAILED. 0 passed; 1 failed; ... finished in 3.57s
```

The new unit cases `git_hooks::tests::organization_quota_decisions` and
`storage_quota::tests::*` fail if the arithmetic or the tenant sum is removed.

## Joins between slices

| Join | Result |
| --- | --- |
| Object store interface used by export | pass: `export.rs` opens `FsObjectStore`, validates the inventory before and after, and reads every payload through `store.get`; it does not read `objects/data` directly for payload bytes. |
| Object store interface used by `backup`/`restore` | pass: `backup.rs` uses `FsObjectStore::open_existing`, `inventory`, `verify_all`; the tar of the storage root is the backup's own job, not a second object API. |
| Nothing opens its own storage path | pass for production code: `FsObjectStore` is constructed once in `lib.rs` and by `export`/`backup` through the interface. The only other direct opens are test helpers in `browse.rs`, `codeowners.rs` and `git_admission.rs`. |
| A token of each subject is confined on API and Git HTTP | pass: U26. |
| Legacy window is what the code enforces | pass: U27; default `2027-06-30` stores and enforces `2027-07-01T00:00:00Z`, `doctor` prints it, config cannot extend it. |
| No plaintext secret in `pg_dump` or a decrypted archive, including audit IP key | pass: U28, with a planted-key control. |
| Outbound client is the only outbound path | pass for HTTP: the guard scans every crate; SMTP and the maintenance loopback probe/control are the two ADR-named exceptions. |
| Migrations 0041–0056 apply on an empty database, no gap, no duplicate | pass: fresh install reports `schema: latest v56`; `_sqlx_migrations` shows 41–56 all `success = t`, one row each. The highest migration is `0056_secrets_at_rest.sql` (there is no 0057). Pre-existing numbering gaps at 0024–0027 and 0029–0031 are outside M4.2 and were not introduced here. |

---

## Summary

| Case | Verdict | Clauses not demonstrated |
| --- | --- | --- |
| U24 | **pass** | none |
| U25 | **pass** (mechanism) | product reconciler call has no integration test |
| U26 | **pass** | none |
| U27 | **pass** | none functionally; the shipped test exercises only `repo:read`, and the `repo:write` half was proved by a scratch scenario rather than a committed test |
| U28 | **pass** | none |
| U29 | **pass** | the racing-write clause holds only for writes starting before the recorded position (ADR boundary, not in the exit sentence) |
| U30 | **pass** on Git bound, anonymous caps and creation caps | no measured p95 for API or job capacity; only budget/worker-share isolation |
| U30a | **pass** | none |
| U30b | **pass** | none |
| U30c | **pass** | none |

## What M4.2 left out, recorded as limitations

The five below are accepted for this milestone and repeated in ADR 0014
("Limitations recorded at the M4.2 exit"). They are not claimed as done.

1. **The API and job legs of the fairness sentence have no measured p95.**
   Git has a real, reproducible relative measurement (worst 1.09, control
   4.27–4.64×). API isolation is proven by independent counters and jobs by the
   2-of-4 worker share, but neither is a latency measurement at load.
2. **Abandoned-upload cleanup is tested at the store interface, not through the
   reconciler.** `cleanup_abandoned` has deadline and resume cases in
   `crates/githearth-git/tests/object_store.rs`; `reconcile.rs` calls it but no
   integration test drives that wiring, so a reconciler that stopped calling it
   would not fail the suite.
3. **The U29 racing-write guarantee is bounded by the recorded position.** A
   write that starts before the backup's recorded PostgreSQL position is
   restored or refused; a write acknowledged after that position is
   deliberately in neither the dump nor the storage snapshot. ADR 0014 states
   this; the exit sentence does not.
4. **SMTP credentials remain an operator file, not an envelope column.**
   `smtp_password_file` names a file the operator owns and the archive does not
   carry. Only the audit IP key is a stored secret column in this schema, and it
   is wrapped under the installation key; the build bullet's phrase "every
   secret column … SMTP credentials" describes a column that does not exist.
5. **KMS is an interface only; the shipped binary refuses `method = "kms"`**
   with "no KMS adapter is built into this binary". A deployment that wants key
   custody from a KMS must supply an adapter out of tree; no adapter ships.

The search claim is no longer a limitation: the operator limits publish only
sign-in, and the shared limiter already refuses above the 20/300 budget M9's
route will inherit (see "The three published claims").
