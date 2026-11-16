# M4.1 sign-off

Date: 2026-11-16. State: **signed by both reviewers — with one acceptance
clause unmet and the limitations recorded below. This is not a clean pass.**

This document replaces `m4-1-signoff-draft.md`. It is the evidence index for the
two signatures at the end, and the signatures are assertions over this record as
written, not over the milestone as hoped for.

Scope: every M4.1 acceptance case in
[08-two-person-roadmap.md](../plan/08-two-person-roadmap.md#m41-acceptance-cases),
U01–U23c, and the 6.4 acceptance row. Every row cites a path that exists on
`main`: an executable test that asserts the clause, or a recorded document. A
case is not closed on a test name alone, and a clause that was not exercised is
recorded as not met, not as passed. A bare `tests/...` path resolves under
`crates/githearth-server/`.

## What the signatures assert, and what they do not

They assert that the evidence below was read against `main` at commit
`b3c7903`, that the cited paths resolve there, and that the per-case states are
an honest account of what was exercised.

They do **not** assert a clean pass. In particular:

- **U19 is not fully met.** Three of its four clauses pass. The fourth — a pin
  expires on the stated retention — is **NOT DEMONSTRATED**: the shortest
  configurable retention is one day, so no live-clock expiry was observed. The
  evidence records `retain_until` timestamps, which show the configured window
  applied, not an expiry. No elapsed time was faked and no pin rows were edited.
- **U20 is closed by the 2026-11-16 drill run on a Docker host**, with the
  off-host harness proved at 5/5. Its limitations — a container backup host on
  the same physical machine, no real second machine or network, and the items
  under "Not covered" — are recorded, not waived.
- The **M3 candidate-binding gap** is real and remains open against the
  roadmap's promise; M3 is signed off over it and the fix is scheduled as M6
  increment 9C (U39). No row here may be read as saying the queue tests the
  candidate it merges.

## Evidence by case

| Case | State | Evidence |
| --- | --- | --- |
| U01 | met | `crates/githearth-server/tests/account_admin.rs` (create, disable, re-enable, non-administrator refusal, last-administrator guard); PR #173 |
| U02 | met | `tests/account_admin.rs` (password change and administrator reset revoke sessions, tokens and keys); PR #173 |
| U03 | met | `tests/account_admin.rs` (browser/API/Git HTTP throttling, no permanent lockout); the defaults are asserted by U18 below; PRs #173, #189 |
| U04 | met | `tests/membership.rs` (add/remove/role change, last-owner rule, audit); PR #171 |
| U05 | met | `tests/teams.rs` (team grants authorize, member/grant removal denies new admission); PR #177 |
| U06 | met | `tests/invitations.rs` (expiry, single use, private-organization non-disclosure); PR #177 |
| U07 | met | `tests/transfer.rs` (ID/history/open pull requests, fork policy, cross-organization revoke, refusals, old path); PR #183 |
| U08 | met | `tests/notifications.rs` (verified address only, preference and deduplication, durable retry); `tests/email_verification.rs`; PR #180 |
| U09 | met | `tests/setup.rs` (clean host, rerun changes no setting, loose-file refusal, PostgreSQL-absent failure); PR #172 |
| U10 | met | `tests/account_admin.rs` (disabled/removed admission), `tests/git_admission.rs`, `tests/ssh.rs` and `tests/git_http.rs` (HTTP/SSH agreement); PRs #171, #173 |
| U11 | met | `tests/housekeeping.rs` (interruption points, keep-set kinds, deleted-branch diff, retention, concurrent writer, quota, timeout); PR #174 |
| U12 | met | `tests/outbox.rs` (at-least-once across crash, isolation, deduplication, 30-minute budget, registration position, cutover, one notification job); PR #178 |
| U13 | met | `tests/backup_restore.rs` (fence refused while writes possible, manifest/checksums/positions/key id, encryption, fresh-host restore, racing push, bad-key/truncated refusals); PR #182 |
| U14 | met | `tests/metrics.rs` (off/loopback default, published metric and label list, no names); PR #179 |
| U15 | met | [m4-1-evidence-fresh.md](m4-1-evidence-fresh.md) §U15 (API, UI, HTTP and SSH after disable/removal); PR #191 |
| U16 | met | `tests/membership.rs` (team memberships and direct grants revoked with removal, one role authority, demotion takes effect); PR #171 |
| U17 | met | `tests/account_admin.rs` (single-use reset token, forced change, audit, no reusable password); PR #173 |
| U18 | met | [m4-1-evidence-fresh.md](m4-1-evidence-fresh.md) §U18 (5/account/15 min, 20/address/15 min, exponential delay, token and key exemption); `tests/account_admin.rs`; PRs #189, #191 |
| U19 | **not met — three of four clauses met; the expiry clause is NOT DEMONSTRATED** | [m4-1-evidence-u19.md](m4-1-evidence-u19.md) records the upgrade from before the pin migrations, the `upload-pack` non-advertisement and the owning-organization charge as pass. Its fourth clause, "a pin expires on the stated retention", is recorded as **not demonstrated**: the shortest configurable window is one day (`housekeeping_pin_retention_days` accepts 1..3650), so no live-clock expiry was observed; the run measured `retain_until` timestamps (one day under the test configuration, 30 days for the `0045` backfill) instead of asserting an expiry. `crates/githearth-server/migrations/0059_pin_backfill_retention.sql` repairs the 30-day backfill for still-live pins to the documented 90-day default (a longer configured window is applied by `pins::extend_backfilled_pins`; an already-expired pin is left expired), and `tests/pin_backfill_retention.rs` asserts the configured window through `retain_until` timestamps, not through an observed expiry. #203 landed as a second migration numbered 0058; `main` renumbered it to 0059 in `e9f11dd`, and the test now applies 0059. PRs #191, #203. |
| U20 | met — closed by the 2026-11-16 drill run; limitations recorded | [offhost-backup.md](offhost-backup.md) 2026-11-16 records the drill on a **Docker host** (macOS 26.6 arm64 with Docker Desktop 4.97.0, engine 29.9.2): the backup host is a disposable `debian:bookworm-slim` container, `githearth-backup-host:drill`, publishing `sshd` on a loopback port, and the off-host harness `scripts/verify-offhost-backup.py` proved **5/5 scenarios**, every clause exercised by an assertion — both backup fences, key id/algorithm/KDF recorded with no key in the archive, the off-host copy with a matching SHA-256, four distinct restore refusals, and disaster restore from the copy. [backup-restore.md](../operations/backup-restore.md) states "U20's clause — both harnesses use `backup`/`restore` and pass — is met"; [m4-qualification.md](m4-qualification.md) records `scripts/verify-m4.py` at 13/13 scenarios and lists off-host backup under verified scope; runbooks in `docs/operations/` (install, upgrade, backup-restore, housekeeping, accounts, quota, metrics); PRs #187, #190, #191. The 2026-11-15 attempt stopped at `backup host sshd not reachable within 30 s` — harness plumbing (Darwin port publishing) that started no backup, so it demonstrated none of the clauses — and the 2026-11-16 run supersedes it. The backup host is a container on the same machine; see the limitations below. |
| U21 | met | `tests/two_account_e2e.rs` (invite → join → clone → branch → push → pull request → review → merge, both transports, no SQL); PR #186 |
| U22 | met | `tests/fresh_auth.rs` (stale sensitive action refused, re-authentication, machine-readable reason, Git exception and HTTP/SSH agreement); browser prompt on site-administration paths; [0002-authorization.md](../decisions/0002-authorization.md) and [0003-git-push-and-repository-lifecycle.md](../decisions/0003-git-push-and-repository-lifecycle.md) state the 2026-11-15 Git token/SSH freshness decision; PRs #184, #188 |
| U23 | met | [m3-signoff.md](m3-signoff.md) 2026-11-15 correction and candidate-binding gap, plus the 2026-11-16 superseding note from the note's author; [lab-m4/round-4/05-regression.md](lab-m4/round-4/05-regression.md) O1 outcome; [lab-m4/README.md](lab-m4/README.md) one-occurrence note; PR #185 |
| U23a | met | `tests/email_verification.rs` and `tests/notifications.rs` (single use, expired/reused refusal, duplicate address, no mail to unverified address); PR #180 |
| U23b | met | [m4-1-evidence-fresh.md](m4-1-evidence-fresh.md) §U23b (only administrator locked out, host `reset-admin`, single-use credential, old sessions revoked, audit); PRs #189, #191 |
| U23c | met | `tests/repository_restore.rs` (same ID/history/issues/pull requests, revoked grants stay revoked, purge/name/non-owner refusals, `410` until restore); PR #181 |

## Gate record

Run for the close-out on `m41/pin-backfill-retention` (before the 0058 → 0059
renumbering), with `TMPDIR=/tmp/ght` and `source $HOME/.cargo/env`, against a
private PostgreSQL on 127.0.0.1 (never 57939, never `githearth_test`). The
renumbering changes the migration version number and the comments, not the SQL
body or the test logic; these gates were **not** re-run for this docs-only
commit.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass, 0 warnings |
| `cargo test --workspace --lib` | 378 passed, 0 failed, 31 ignored |
| `--test backup_restore -- --include-ignored` | 13 passed, 0 failed |
| `--test housekeeping -- --include-ignored` | 25 passed, 0 failed |
| `--test setup -- --include-ignored` | 25 passed, 0 failed |
| `--test doctor -- --include-ignored` | 6 passed, 0 failed |
| `--test pin_backfill_retention -- --include-ignored` | 1 passed, 0 failed |

## Recorded limitations

These are part of the signed record. They are not conditions for a later
document; they qualify what is being signed now.

1. **U19 expiry is not demonstrated.** The shortest configurable retention is
   one day and no live-clock expiry was observed. The evidence shows the
   configured window applied through `retain_until`, and the backfill repair
   through migration timestamps and a test. It does not show an object becoming
   unreachable at expiry.
2. **U19's evidence predates the repair.** The in-place upgrade was recorded on
   2026-11-15 at `3f96d95`, before migration 0059 existed. The repair is
   asserted by migration 0059 and `tests/pin_backfill_retention.rs`; a fresh
   end-to-end upgrade past 0059 was not repeated for this signature.
3. **One U19 base pin cannot be reconstructed.** PR #4's target branch was
   deleted before the upgrade, so its base pin was skipped; `doctor` reports
   `warn base backfill` with the request named. This is recorded, not silently
   dropped.
4. **U20's off-host host is a container on the same machine.** The backup host
   is a disposable Docker container reached over a published loopback port; there
   is no real second machine or network. Retention pruning is a runbook
   recommendation only. Encryption at rest and off-site key management for the
   configuration file and SSH host key are not covered. An online
   (non-maintenance-window) backup is not covered.
5. **U20's earlier failure is history, not a pass.** The 2026-11-15 attempt
   failed at harness plumbing (`backup host sshd not reachable within 30 s`)
   before any clause ran. The 2026-11-16 run is the evidence; the earlier run is
   retained so the record is not rewritten.
6. **The M3 candidate-binding gap remains open.** A merge decision binds the
   source head and expected target, and the approvals/checks digest is taken at
   the source head, not over the merged tree. A check that passes on the source
   and fails only on the merged tree can authorize the merge. The fix is M6
   increment 9C (U39); M3's sign-off stands over the gap, and this document does
   not repeat "the queue tests the candidate it merges".
7. **The gate record is not from this commit.** It was taken on the functional
   branch recorded in the M4.1 close-out report. This change is documentation
   only.

## Sign-off

Two reviewers each confirm the evidence and the limitations above. Both sign
with U19's expiry clause recorded as not demonstrated.

### Reviewer 1

- Name: Jadav Havish Sunil (`HavishJadav`)
- Date: 2026-11-16
- Verdict: signed — approve with the exceptions above; this is not a clean pass
- Notes and problems found: I verified each U01–U23c citation in this document
  against `main` at `b3c7903`, and I stand behind the U19 upgrade evidence I
  recorded on 2026-11-15: three of its four clauses pass, and the expiry clause
  is not demonstrated because the shortest configurable retention is one day. I
  do not claim U19 is fully met.

### Reviewer 2

- Name: Aarav (`DeviousCardi`)
- Date: 2026-11-16
- Verdict: signed — approve with the exceptions above; this is not a clean pass
- Notes and problems found: I verified the U19 and U20 corrections and every
  evidence path named here against `main` at `b3c7903`, including the 0059
  renumbering in `e9f11dd`, and I re-read U20's 2026-11-16 record of the drill
  on a Docker host with the off-host harness at 5/5. I also appended a
  superseding note to my 2026-11-13 "No gap found" note in
  [m3-signoff.md](m3-signoff.md) on 2026-11-16. I do not claim U19's expiry
  clause passed.
