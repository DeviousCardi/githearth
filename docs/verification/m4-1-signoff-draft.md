# M4.1 sign-off — draft

Date: 2026-11-16. State: **draft; not signed; awaiting two signatures.** This is
the evidence index for the two signatures. It is deliberately named
`-draft.md`: the signed document must be
`docs/verification/m4-1-signoff.md`, created only when both reviewers have signed
below and the pre-signature corrections are resolved.

Scope: every M4.1 acceptance case in
[08-two-person-roadmap.md](../plan/08-two-person-roadmap.md#m41-acceptance-cases),
U01–U23c, and the 6.4 acceptance row. Every row cites a path that exists on
`main`: an executable test that asserts the clause, or a recorded document. A
case is not closed on a test name alone, and a clause that was not exercised is
recorded as not met, not as passed. A bare `tests/...` path resolves under
`crates/githearth-server/`.

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
| U19 | **not met — three of four clauses met; expiry not demonstrated** | [m4-1-evidence-u19.md](m4-1-evidence-u19.md) records the upgrade from before the pin migrations, the `upload-pack` non-advertisement and the owning-organization charge as pass, and records expiry on the stated retention as **not demonstrated** because the shortest configurable window is one day and no live-clock expiry was observed. `crates/githearth-server/migrations/0058_pin_backfill_retention.sql` and `tests/pin_backfill_retention.rs` (merged as #203) repair the 30-day backfill and assert the configured window through `retain_until` timestamps, not through an observed expiry. On `main` #203 landed as a second migration numbered `0058`, alongside `0058_identity_schema_and_recovery.sql`; the renumbering required by the cross-review is outstanding. PRs #191, #203. |
| U20 | met | [offhost-backup.md](offhost-backup.md) 2026-11-16 records `scripts/verify-offhost-backup.py` at 5/5 scenarios with every clause exercised, including backup/restore, both fences and four refusals, on macOS; [backup-restore.md](../operations/backup-restore.md) states "U20's clause — both harnesses use `backup`/`restore` and pass — is met"; [m4-qualification.md](m4-qualification.md) records `scripts/verify-m4.py` at 13/13 scenarios and lists off-host backup under verified scope; runbooks in `docs/operations/` (install, upgrade, backup-restore, housekeeping, accounts, quota, metrics); PRs #187, #190, #191. The 2026-11-15 attempt stopped at `backup host sshd not reachable within 30 s` — harness plumbing (Darwin port publishing), not an unexecuted assertion — and the 2026-11-16 run supersedes it. |
| U21 | met | `tests/two_account_e2e.rs` (invite → join → clone → branch → push → pull request → review → merge, both transports, no SQL); PR #186 |
| U22 | met | `tests/fresh_auth.rs` (stale sensitive action refused, re-authentication, machine-readable reason, Git exception and HTTP/SSH agreement); browser prompt on site-administration paths; [0002-authorization.md](../decisions/0002-authorization.md) and [0003-git-push-and-repository-lifecycle.md](../decisions/0003-git-push-and-repository-lifecycle.md) state the 2026-11-15 Git token/SSH freshness decision; PRs #184, #188 |
| U23 | met | [m3-signoff.md](m3-signoff.md) 2026-11-15 correction and candidate-binding gap; [lab-m4/round-4/05-regression.md](lab-m4/round-4/05-regression.md) O1 outcome; [lab-m4/README.md](lab-m4/README.md) one-occurrence note; PR #185 |
| U23a | met | `tests/email_verification.rs` and `tests/notifications.rs` (single use, expired/reused refusal, duplicate address, no mail to unverified address); PR #180 |
| U23b | met | [m4-1-evidence-fresh.md](m4-1-evidence-fresh.md) §U23b (only administrator locked out, host `reset-admin`, single-use credential, old sessions revoked, audit); PRs #189, #191 |
| U23c | met | `tests/repository_restore.rs` (same ID/history/issues/pull requests, revoked grants stay revoked, purge/name/non-owner refusals, `410` until restore); PR #181 |

## Gate record at drafting

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo test --workspace --lib` | see the close-out report; run with `TMPDIR=/tmp/ght` |
| `--test backup_restore -- --include-ignored` | see the close-out report |
| `--test housekeeping -- --include-ignored` | see the close-out report |
| `--test setup -- --include-ignored` | see the close-out report |
| `--test doctor -- --include-ignored` | see the close-out report |

## Pre-signature corrections

1. **Reviewer 2's 2026-11-13 note is stale.** In
   [m3-signoff.md](m3-signoff.md), Reviewer 2's note says "No gap found" two
   paragraphs below the 2026-11-15 correction that says the candidate-binding
   gap is real. The note's author must append a superseding note (or this
   sign-off must record the supersession) before either signature is added.
2. **U19's expiry clause is not met.** No live-clock expiry was observed: the
   shortest configurable retention is one day, so the evidence records
   `retain_until` values rather than an expiry. The row says so. The reviewers
   may sign with that limitation stated, or hold U19 open; neither signature may
   record the clause as passed.
3. **U19's migration number is ambiguous on `main`.** #203 landed as
   `0058_pin_backfill_retention.sql` while #201 already owns
   `0058_identity_schema_and_recovery.sql`, so two migrations share version
   0058. The renumbering the cross-review required is outstanding and must land,
   with the pin test re-run, before the backfill and retention evidence can be
   relied on.

## Sign-off

Two reviewers each confirm the evidence above. This block is intentionally
empty in the draft.

### Reviewer 1

- Name:
- Date:
- Verdict:
- Notes:

### Reviewer 2

- Name:
- Date:
- Verdict:
- Notes:
