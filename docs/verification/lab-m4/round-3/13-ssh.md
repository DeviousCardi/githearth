# SSH transport and signed commits — M4 closure round 3

Tester: t13 (owner of `lab13`). Date: 2026-11-14. Instance: main 800cad4.
Verdict: PASS
Cases: 101 run, 100 pass, 0 fail, 1 skipped.

## Results

Transport URLs: `ssh://git@127.0.0.1:2222/<org>/<repo>.git` with
`ssh -i <key> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -p 2222`.
Repos: `lab13/r3-ssh` (private), `lab13/r3-ssh-pub` (public), `lab13/r3-ssh-c1`, `lab13/r3-ssh-c2`.

### Key management API (`/api/v1/user/ssh-keys`)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| K1 | register Ed25519 key | 201 with id/algorithm/fingerprint | 201, `algorithm=ed25519`, fingerprint `SHA256:Nm5n6OV…` | PASS |
| K2 | list keys | 200, key present | 200, 1 match for `main` | PASS |
| K3 | re-register same key line | 409 | 409 `ssh key already registered` | PASS |
| K4 | re-register same key, different comment | 409 (fingerprint identity) | 409 same body | PASS |
| K5 | empty name | 400 | 400 `invalid input` | PASS |
| K6 | whitespace-only name | 400 | 400 `invalid input` | PASS |
| K7 | 101-character name | 400 | 400 `invalid input` | PASS |
| K8 | unparseable key text | 400 | 400 Ed25519/ECDSA-only message | PASS |
| K9 | RSA public key | 400 | 400 same message | PASS |
| K10 | fabricated `sk-ssh-ed25519@openssh.com` line | 400 | 400 same message | PASS |
| K11 | DSA key | 400 | not exercised (OpenSSH 9 cannot generate DSA; same parse path as RSA) | SKIPPED |
| K12 | `expires_in_days=0` | 400 | 400 `invalid input` | PASS |
| K13 | `expires_in_days=3651` | 400 | 400 `invalid input` | PASS |
| K14 | `expires_in_days=1` | 201, `expires_at` set | 201, `expires_at=2026-11-15T06:39:41Z` | PASS |
| K15 | unknown JSON field | rejected | 422 (axum body rejection) | PASS (note 1) |
| K16 | 17 KB public key | 400 | 400 `public key input too long` | PASS |
| K17 | list with `repo:read` access token | 403 | 403 `access tokens cannot manage ssh keys` | PASS |
| K18 | create with access token | 403 | 403 `read-only token` (router middleware) | PASS |
| K19 | ECDSA nistp256 key | 201, `algorithm=ecdsa-nistp256` | 201, `ecdsa-nistp256` | PASS |
| K20 | revoke own key | 204 | 204 | PASS |
| K21 | revoke the same key again | 404 | 404 `ssh key not found` | PASS |
| K22 | revoke random UUID | 404 | 404 | PASS |
| K23 | stored public key canonicalised | comment stripped | `ssh-ed25519 AAAA…` (2 tokens), fingerprint matches `ssh-keygen -lf` | PASS |
| K24 | list after revoke | `revoked_at` non-null | `revoked_at=2026-11-14T06:39:41Z` | PASS |
| K25 | re-register revoked key | 201, new id, old row kept | 201, new id `6442729b…`, old `cb6dc8d2…` still revoked | PASS |
| K26 | no credentials | 401 | 401 | PASS |
| K27 | register another account's active fingerprint (t13 → t12 key) | 409 | 409 `ssh key already registered` | PASS |

### SSH transport and authentication

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| S1 | clone private `r3-ssh` | success | rc=0 | PASS |
| S2 | clone public `r3-ssh-pub` | success | rc=0 | PASS |
| S3 | incremental fetch after an HTTP push | new commits visible | rc=0, `origin/main` 2 commits | PASS |
| S4 | push new branch | accepted | rc=0, `r3-branch` created | PASS |
| S5 | clone with unregistered key | auth denied | rc=128 `Permission denied (publickey…)` | PASS |
| S6 | key before/after revoke | works, then denied | before rc=0, after rc=128 | PASS |
| S7 | clone `beta/app` (not a member) | denied, no existence leak | rc=128 `githearth: repository not found` | PASS (re-ran, see note 2) |
| S8 | clone nonexistent repo | same refusal as S7 | rc=128 `githearth: repository not found` | PASS (re-ran) |
| S9 | shell/`echo` command | refused, exit 128 | rc=128 `githearth: repository not found` | PASS |
| S10 | `ls-remote` (protocol v2) | refs listed | rc=0, HEAD/main/r3-branch | PASS (re-ran) |
| S11 | scp-style URL | clone works | rc=0 | PASS |
| S12 | password authentication | refused | rc=255 `Permission denied (password,publickey…)` | PASS |
| X1 | `none` auth method | refused | rc=255 permission denied | PASS |
| X2 | SFTP subsystem | refused | rc=255, connection closed | PASS |
| X3 | PTY request | refused, no shell | `PTY allocation request failed`, rc=255 | PASS |
| X4 | `git-upload-archive` command | refused | rc=255 `githearth: repository not found` | PASS |
| X5 | `git-upload-pack '/lab13/../../acme/web.git'` | refused | rc=255 `githearth: repository not found` | PASS |
| X6 | three path segments | refused | rc=255 `githearth: repository not found` | PASS |
| E1 | clone as `erin` (triage in lab13) | allowed (read) | rc=0 | PASS |
| E2 | push as `erin` | denied at admission | rc=128 `githearth: not permitted` | PASS |
| F1 | push to public repo over SSH (forced, unrelated history) | accepted | rc=0, `main` forced `e4002be…17ad9f9` | PASS |
| F2 | anonymous HTTP clone of public repo | success | rc=0 | PASS |
| F3 | anonymous SSH clone of public repo | refused (SSH needs a key) | rc=128 permission denied | PASS |
| A5 | ECDSA key authenticates transport | success | rc=0, 11 refs | PASS |
| A6 | force push unprotected branch | allowed | rc=0 | PASS |

### Branch and tag protection

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| PB1 | direct push to protected `main` (PR required) | deny | `protected branch: ref.push` | PASS |
| PB2 | force push to protected `main` | deny | `protected branch: ref.force_push` | PASS |
| PB3 | delete protected `main` | deny | `protected branch: ref.delete` | PASS |
| PB4 | push unprotected `r3-branch` | allow | rc=0 | PASS |
| PB5 | create protected `signed` branch from unsigned history | deny (all reachable commits checked on creation) | `commit … is not signed by a registered committer` | PASS |
| PB6 | create/ff-push protected `no-force` branch (no PR rule) | allow | rc=0 | PASS |
| PB7 | force push `no-force` branch | deny | `protected branch: ref.force_push` | PASS |
| PB8 | delete `no-force` branch | deny | `protected branch: ref.delete` | PASS |
| PB9 | owner creates protected tag `v1.0.0` | allow | rc=0 | PASS |
| PB10 | move protected tag | deny | `protected tag: ref.update` | PASS |
| PB11 | delete protected tag | deny | `protected tag: ref.delete` | PASS |
| PB12 | create+delete unprotected tag `u1` | allow | rc=0 / rc=0 | PASS |
| PB13 | signed commit still pushed straight to PR-protected `main` | deny by PR rule | `protected branch: ref.push` | PASS |
| A1 | maintain (`carol`) direct push to `main` | deny | `protected branch: ref.push` | PASS |
| A2 | maintain push unprotected branch | allow | rc=0 | PASS |
| A3 | maintain creates protected tag | deny (rule allows `admin`) | `protected tag: ref.create` | PASS |
| A4 | base member (`grace`) push | deny | rc=128 `githearth: repository not found` | PASS |

### Signed commits (`require_signed_commits` on `signed`)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| T1 | unsigned commit | deny | `commit … is not signed by a registered committer` | PASS |
| T2 | signed by own registered Ed25519 key, committer `t13@users.noreply.githearth.invalid` | accept | rc=0 | PASS |
| T3 | signed by unregistered key | deny | pre-receive denial | PASS |
| T4 | commit by `t12`, signed with t12's registered key | accept | rc=0 | PASS |
| T5 | t12's registered key, committer `t13@…` | deny (per-committer allowed-signers) | pre-receive denial | PASS |
| T6 | own key, committer `other@example.com` | deny (not a noreply account) | pre-receive denial | PASS |
| T7 | one push with a signed and an unsigned commit | deny whole push | pre-receive denial | PASS |
| T8 | signed with registered ECDSA key | accept | rc=0 | PASS |
| T9 | signer key revoked (transport key still valid) | deny | pre-receive denial | PASS |
| T10 | signer re-registered | accept | rc=0 | PASS |
| SC11 | rule toggled off (`PATCH … {"require_signed_commits":false}` → 200) | unsigned accepted | rc=0 | PASS |
| SC12 | rule restored | unsigned denied again | pre-receive denial | PASS |

### Settings UI key management

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| U1 | `GET /settings` with session cookie | 200, keys and CSRF present | 200, `transport2` listed, 32-char CSRF | PASS |
| U2 | form add with valid CSRF + idempotency key | 303, key listed | 303, key `ui-added3` created | PASS |
| U3 | form post without CSRF | 403 | 403 | PASS |
| U4 | form revoke | 303, `revoked_at` set | 303, revoked | PASS |
| U5 | form post with wrong CSRF | 403 | 403 | PASS |
| U6 | form post without session cookie | redirect to sign-in | 303 | PASS |

### Host key and compose `ssh-keygen`

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| H1 | host key file exists, mode 0600 | present, 0600 | 387 bytes, mode 600, mtime `2026-11-13 21:09:41` (predates the 06:34 app recreation) | PASS |
| H2 | advertised host key equals the file | same fingerprint | `SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE` from `ssh-keyscan` and `ssh-keygen -lf` in the container; file sha256 `d0751ffc…` | PASS |
| H3 | `ssh-keygen` one-shot service idempotent | exit 0, key not rewritten | `docker compose ps -a`: `Exited (0)`; log `SSH host key already exists; keeping it`; mtime unchanged | PASS |
| H4 | key survives app recreation | same key after `app` container recreated | app `Up (healthy)` on the same volume, same fingerprint/mtime | PASS |

### Concurrency

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C1 | 8 parallel clones | all succeed | 8/8 | PASS |
| C2 | 12 parallel `ls-remote` | all succeed or clean refusal | 9/12; 3 got `githearth: service unavailable` (git_slots=8); re-ran when load dropped: 12/12 | PASS (note 3) |
| C3 | 2 concurrent pushes to the same new branch | one accepted, one refused, no corruption | rc 1/0; loser `fetch first`; branch at winner's commit | PASS |
| C4 | 4 parallel pushes (2 repos, 2 branches) | all accepted | rc 0,0,0,0 | PASS |
| C5 | key API during git load | responsive | 10 × 200 | PASS |
| C6 | `/health/ready` after load | 200 | 200 | PASS |

### `push_credentials` provenance (read-only SQL)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| P1 | refused pushes leave no credential row | none for refused ops | 0 dangling rows created after 06:38 (27 pre-existing from 2026-11-13 21:19–21:24) | PASS |
| P2 | credentials for my SSH keys | only for accepted pushes | 25 rows, 0 dangling | PASS |
| P3 | accepted SSH push records the key | `credential_kind=ssh_key`, `credential_id=<key>` | yes, e.g. `ssh_key/bf0d1b95…` | PASS |
| P4 | accepted pushes have a `repository_pushes` row | 1:1 | all 25 match | PASS |

## Defects

None found.

## Notes

1. **K15 status code.** An unknown JSON field is rejected by axum's extractor with
   422 and an internal message, while every other key-validation failure is the
   API's 400 `invalid input`. Rejection is correct and the API already uses 422
   for some validation (e.g. `protection.rs` reserved/duplicate status checks),
   so this is recorded as an inconsistency, not a defect.

2. **Test mistakes, re-run.** Three transport cases first failed because a bash
   `VAR=… r=$(…)` assignment leaked an unregistered key into `GIT_SSH_COMMAND`
   for the rest of the script (S7/S8/S10). They were re-run with the correct key
   and passed; the observed refusals were the expected ones. A later ERIN probe
   reused t13's key file (so it authenticated as t13); it was re-run with a
   dedicated erin key (E1/E2). No product failure was involved.

3. **Slot saturation is platform-wide, not a leak.** `git_slots` is 8
   (`config.rs:74`); SSH takes a slot with `try_acquire_owned` and refuses
   immediately with `githearth: service unavailable` (`ssh.rs:285-294`), the
   same policy as HTTP's 503. During one 90 s window the server logged 46 SSH
   connections and 149 git HTTP requests from other testers on the shared
   instance; 8 of 30 sequential `ls-remote` calls were refused, and 15/15
   simultaneous SSH+HTTP probe pairs plus 20 subsequent sequential calls
   succeeded. Permits are released on every path (including refused pushes), so
   no leak was observed. Worth knowing operationally: a single client can see a
   hard exit-128 failure when the whole instance is busy, rather than queueing.

4. **Pre-existing `push_credentials` rows.** 27 dangling rows exist from
   2026-11-13 21:19–21:24 (a build before `800cad4`); none were created in this
   round. They were not deleted (read-only SQL only).

5. **Fixture drift.** In `lab13` the `read` grant for `erin` is revoked and she
   holds `triage`, so E1/E2 exercise triage (read allowed, push denied). `bob`
   (write), `carol` (maintain), `dave` (admin), `grace` (member) match the brief.

6. **Expired-key path not exercised live.** Creating a key that is already
   expired would need a write to `expires_at`, which read-only SQL forbids.
   Source review: `ssh_keys::authenticate` refuses expired keys
   (`crates/githearth-server/src/ssh_keys.rs:310-315`) and
   `registered_keys` excludes them from allowed signers
   (`crates/githearth-server/src/git_http.rs:990-993`). Note that the partial
   unique index is `WHERE revoked_at IS NULL`
   (`migrations/0013_ssh_keys.sql:23`), so an expired but unrevoked key still
   blocks re-registering the same fingerprint; the user must revoke it first.

7. **Key names are escaped.** A pre-existing key named
   `<script>alert("key")</script>` (from an earlier round) renders escaped as
   `&lt;script&gt;alert(&quot;key&quot;)&lt;/script&gt;` on `/settings`; no
   stored XSS.

8. Signed-commit creation is checked against every commit reachable from the new
   tip when the ref is created (`signature::new_commits`, zero old oid), so
   creating a protected branch from unsigned history is refused until the branch
   exists with signed history (PB5). That is the documented behaviour
   (`docs/decisions/0007-signed-commits-and-stale-approvals.md`) and was used to
   set up the accepted cases.
