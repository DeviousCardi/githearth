# ADR 0007: Signed-commit policy and stale-approval dismissal

Date: 2026-11-11.

Status: **accepted for M3**.

## Scope

Two branch-protection flags land in M3, both stored on `branch_protection_rules`
(migration `0037`):

- `require_signed_commits` — refuse pushes to a protected branch unless every
  commit the push introduces carries an SSH signature that verifies against a
  key registered to the committer's account.
- `dismiss_stale_approvals` — count an approval only at the current head, so a
  push that moves the head makes earlier approvals stop counting.

## Signed commits

`require_signed_commits` defaults OFF. When a push targets a protected branch
with the flag set, the `pre-receive` hook forwards the receive-pack quarantine
object directory to the server, which lists the new commits, reads each
committer email, resolves it to an account, and runs
`git verify-commit` with `gpg.format=ssh` against an allowed-signers file built
from that account's active SSH keys. An unsigned commit, or one whose
signature does not verify against a registered key, is refused.

The committer identity is the account's `<username>@users.noreply.githearth.invalid`
address, matching the identities the merge path already emits.

### Merge exception

Merges are ref changes made outside `receive-pack` (ADR 0004); the server
constructs the candidate commit itself, under the per-repository lock, after
revalidating policy. Server-created merge commits therefore never pass through
`verify_signed_commits` and are allowed to merge into a `require_signed_commits`
branch. This is the simplest correct option: the policy governs commits a
client pushes, not commits the trusted server creates. If server signing is
wanted later, the merge path can add a server key in one place.

## Stale approvals

`dismiss_stale_approvals` defaults ON, so new rules keep the pre-flag semantics:
an approval is bound to the head it was given on, and a moved head invalidates
it. Existing rows inherit `true` from the migration default, so their behaviour
is unchanged. A rule can opt out (`false`) to count each reviewer's latest
verdict across every commit. The merge gate — the API, the browser form and the
worker revalidation — uses the target rule's flag when it counts approvals and
computes the approvals/checks digest. The pull request page marks an approval
given on an older commit with "approved an older commit".
