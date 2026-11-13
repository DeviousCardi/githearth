# SSH Git hosting, key lifecycle and admission — M4 lab acceptance report

Tester: t05. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 112 run, 110 pass, 2 fail, 1 skipped (113 reported cases).

Setup: all keys generated under `/tmp/lab05`; repositories created in `lab05`
(`ssh-private`, `ssh-public`, `ssh-prot`, `ssh-arch`, `ssh-sign`). SSH Git used
`GIT_SSH_COMMAND='ssh -i <key> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -p 2222'`.
Key ownership was registered through `POST /api/v1/user/ssh-keys` with session
tokens from `POST /api/v1/login`. SQL below is read-only `psql`; no app or
database restart. No acme, beta or other lab repository, rule or membership was
changed; the only cross-account writes were temporary SSH keys on alice, bob,
carol, dave, erin, frank and grace, all revoked during cleanup.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Register valid Ed25519 public key | 201 with SHA256 fingerprint | HTTP 201: {"id": "c3c19136-013b-4461-83e0-7a01d34476f8", "name": "lab05-test-ed25519", "algorithm": "ed25519", "fingerprint": "SHA256:7eyu1v7kKuIGQOx2XrHoliEJPlkdBbB6PvUQ3rPSFmc", "public_ke | PASS |
| 2 | Register valid ECDSA nistp256/384/521 keys | 201, matching algorithm labels | HTTP 201 for all three; algorithm ecdsa-nistp256/384/521 | PASS |
| 3 | Register RSA key | 400 unsupported key | HTTP 400: {"error": "must be an Ed25519 or ECDSA (nistp256/384/521) public key; RSA and DSA are not supported"} | PASS |
| 4 | Register SSH certificate line | 400 unsupported/malformed | HTTP 400: {"error": "must be an Ed25519 or ECDSA (nistp256/384/521) public key; RSA and DSA are not supported"} | PASS |
| 5 | Register malformed/empty/blank public_key | 400 | HTTP 400 for all three: "must be an Ed25519 or ECDSA ... public key" | PASS |
| 6 | Register public_key > 16 KiB | 400 public key input too long | HTTP 400: {"error": "public key input too long"} | PASS |
| 7 | Name bounds: empty, 100 chars, 101 chars, whitespace-only | 400, 201, 400, 400 | HTTP 400 empty; HTTP 201 100 chars; HTTP 400 101 chars; HTTP 400 whitespace-only | PASS |
| 8 | expires_in_days minimum 1 and maximum 3650 | 201, expires_at set | HTTP 201 both; +1 day and ~+10 years expires_at | PASS |
| 9 | expires_in_days 0, -1, 3651, 1.5 | 400/400/400/422 | HTTP 400, 400, 400, 422 (type error) | PASS |
| 10 | Unknown JSON field in key request | 422 | HTTP 422: Failed to deserialize the JSON body into the target type: foo: unknown field `foo`, expected one of `name`, `public_key`, `expires_in_days` at line 1 column 224 | PASS |
| 11 | Duplicate fingerprint in same account | 409 already registered | HTTP 409: {"error": "ssh key already registered"} | PASS |
| 12 | Duplicate fingerprint across accounts (bob registers t05 key) | 409 | HTTP 409: {"error": "ssh key already registered"} | PASS |
| 13 | Access token (read and write scope) cannot create a key | 403 | HTTP 403 "read-only token" (read token); HTTP 403 "access tokens cannot manage ssh keys" (write token) | PASS |
| 14 | Unauthenticated key creation | 401 | HTTP 401: {"error": "invalid or expired credentials"} | PASS |
| 15 | List keys: metadata only, no private/public material, stable order | 200, fingerprint and revoked_at present, no public_key | HTTP 200; 25 keys; no public_key field; identical arrays on repeat read | PASS |
| 16 | Access token cannot list keys | 403 | HTTP 403 for read-scope and write-scope tokens | PASS |
| 17 | Another account revokes t05's key | 404 | HTTP 404: {"error": "ssh key not found"} | PASS |
| 18 | Revoke unknown UUID and malformed id | 404 and 400 | HTTP 404 unknown id; HTTP 400 malformed UUID | PASS |
| 19 | Revoke own key, then list | 204, entry retained with revoked_at | HTTP 204; list shows revoked_at 2026-11-13T21:17:53Z | PASS |
| 20 | Revoke an already-revoked key | 404 | HTTP 404: {"error": "ssh key not found"} | PASS |
| 21 | Re-register a revoked fingerprint (same account) | 201 | HTTP 201: {"id": "5930a87e-4d10-40e2-996f-0d1c372d4916", "name": "lab05-test-ed25519-again", "algorithm": "ed25519", "fingerprint": "SHA256:7eyu1v7kKuIGQOx2XrHoliEJPlkdBbB6PvUQ3rPSFmc", "pub | PASS |
| 22 | Access token cannot revoke a key | 403 | HTTP 403 "read-only token" / "access tokens cannot manage ssh keys" | PASS |
| 23 | Key comment is stripped from the canonical stored key | 201, stored public_key has no comment | HTTP 201: {"id": "3a750764-4288-47cb-9c05-c15f6fcc85eb", "name": "lab05-test-comment", "algorithm": "ed25519", "fingerprint": "SHA256:oLT+xYuw+8qpls6VoTnOIaR/sFG5aGMrwXsZLaqV0RM", "public_ke | PASS |
| 24 | expires_at format and one-day value | ISO8601 UTC, now + 1 day | created_at 2026-11-13T21:17:45Z, expires_at 2026-11-14T21:17:45Z | PASS |
| 25 | Revoked key re-registered by another account, then revoked by it | 201 then 204 | HTTP 201 bob; HTTP 204 bob revoke | PASS |
| 26 | Key request missing public_key | 422 | HTTP 422: Failed to deserialize the JSON body into the target type: missing field `public_key` at line 1 column 23 | PASS |
| 27 | API fingerprint equals ssh-keygen -lf | identical SHA256 | SHA256:g0Zn9G7jniDblx9UXDlhqkb6h75ggsuUcS6ImoO1AiM both | PASS |
| 28 | Clone private repository with registered Ed25519 key | success | exit 0:  | PASS |
| 29 | Clone private repository with no key | publickey denied | exit 128: Permission denied (publickey) | PASS |
| 30 | Clone private repository with unregistered key | publickey denied | exit 128: Permission denied | PASS |
| 31 | Revoke a key: clone before, revoke, clone after | success, 204 + revoked_at, denied | exit 0 before; HTTP 204 and revoked_at set; exit 128 Permission denied after | PASS |
| 32 | Re-register a revoked key and clone with it | 201 and success | HTTP 201; exit 0 | PASS |
| 33 | URL user ignored: ghost@ URL with t05 key | success | exit 0:  | PASS |
| 34 | URL user grants nothing: t05@ URL with nonmember alice key | denied | exit 128: repository not found | PASS |
| 35 | scp-style t05@host:org/repo.git clone | success | exit 0:  | PASS |
| 36 | URL path without .git, extra segment, traversal, double slash | all refused | exit 128 "repository not found" for all four | PASS |
| 37 | Wrong-case organization path | refused | exit 128: repository not found | PASS |
| 38 | Nonexistent repository in ssh:// and scp forms | refused | exit 128: repository not found for both | PASS |
| 39 | Clone with registered ECDSA client key | success | exit 0:  | PASS |
| 40 | Wrong-case repository path | case-insensitive lookup accepts it | exit 0; note: implemented intentionally with lower(name)=lower($2), repositories.rs:279 | PASS |
| 41 | git-upload-archive command | refused | exit 128: repository not found | PASS |
| 42 | Interactive shell request | refused | exit 128: repository not found | PASS |
| 43 | sftp subsystem request | refused | exit 255: subsystem request failed | PASS |
| 44 | Password authentication attempt | rejected | exit 255: Permission denied; password handler rejects | PASS |
| 45 | none authentication attempt | rejected | exit 255: Permission denied | PASS |
| 46 | last_used_at refreshed after successful auth | non-null timestamp | last_used_at 2026-11-13T21:18:36Z | PASS |
| 47 | Five parallel clones with the same key | all succeed | 5/5 succeeded in 1.2s | PASS |
| 48 | Authentication with an expired key | denied | Cannot create an expired key through the API: expires_in_days is 1..=3650 and no DB writes are allowed. Source: ssh_keys.rs:136-140; authenticate checks expiry at ssh_keys.rs:302-314. | SKIP |
| 49 | bob (write): clone private, push branch | success, success | exit 0 clone; exit 0 push | PASS |
| 50 | carol (maintain): clone private, push branch | success, success | exit 0 clone; exit 0 push | PASS |
| 51 | dave (admin): clone private, push branch | success, success | exit 0 clone; exit 0 push | PASS |
| 52 | erin (read): clone private, push branch | success, denied ref.push | exit 0 clone; exit 1 remote: permission denied: ref.push | PASS |
| 53 | frank (triage): clone private, push branch | success, denied ref.push | exit 0 clone; exit 1 remote: permission denied: ref.push | PASS |
| 54 | grace (member): clone private, push branch | denied (no repository.read) | exit 128 repository not found for both | PASS |
| 55 | grace (member): clone public repository | success | exit 0:  | PASS |
| 56 | alice (nonmember): clone private repository | denied | exit 128: repository not found | PASS |
| 57 | alice (nonmember): clone public repository | success (authenticated public read) | exit 0:  | PASS |
| 58 | alice (nonmember): push to public repository | denied ref.push | exit 1: permission denied: ref.push | PASS |
| 59 | No key: clone public repository | denied (SSH always requires a key) | exit 128: Permission denied (publickey) | PASS |
| 60 | t05 owner: push to public repository | success | exit 0:  | PASS |
| 61 | Create branch over SSH and verify with ls-remote | success | exit 0 both; remote shows 4900ebae... refs/heads/d-branch | PASS |
| 62 | Fast-forward push to existing branch | success | exit 0:  | PASS |
| 63 | Non-fast-forward plain push | refused non-fast-forward | exit 1: [rejected] non-fast-forward | PASS |
| 64 | Force push to unprotected branch | success | exit 0:  | PASS |
| 65 | Delete existing and nonexistent branch | success; no-op warning | exit 0 deleted; exit 0 warning deleting a non-existent ref | PASS |
| 66 | Create and delete lightweight tag | success, success | exit 0: ; exit 0:  | PASS |
| 67 | Create and delete annotated tag | success, success | exit 0: ; exit 0:  | PASS |
| 68 | Push refs/notes/review | refused | exit 1: remote: ref namespace is read-only | PASS |
| 69 | Push refs/remotes/x/y | refused | exit 1: remote: ref namespace is read-only | PASS |
| 70 | Create and delete ordinary branch d-move | success, success | exit 0: ; exit 0:  | PASS |
| 71 | Owner creates branch rule main (require PR, no force, no delete) | 201 | HTTP 201: rule bfdab0bd... pattern main, require_pull_request true, allow_force_push false, allow_deletion false | PASS |
| 72 | Write-role bob creates branch rule | 403 | HTTP 403 not permitted | PASS |
| 73 | Maintain-role carol creates branch rule | 201 | HTTP 201: {"id": "a9352d42-1c5a-4ebb-8502-9c0cb23167da", "repository_id": "a93ac2c4-2a8e-490e-bced-7e00b0218ca1", "pattern": "carol-rule", "allow_force_push": false, "allow_deletion": false, | PASS |
| 74 | Triage-role frank creates branch rule | 403 | HTTP 403 not permitted | PASS |
| 75 | Invalid branch glob pattern | 400 | HTTP 400: {"error": "invalid input"} | PASS |
| 76 | Duplicate branch pattern | 409 | HTTP 409: {"error": "protection rule already exists"} | PASS |
| 77 | Read-role erin lists branch rules | 200 with generation | HTTP 200; generation 2, 2 rules | PASS |
| 78 | Direct push to protected main | denied protected branch: ref.push | exit 1: remote: protected branch: ref.push | PASS |
| 79 | Non-fast-forward force push to protected main | denied protected branch: ref.force_push | exit 1: remote: protected branch: ref.force_push | PASS |
| 80 | Delete protected main | denied protected branch: ref.delete | exit 1: remote: protected branch: ref.delete | PASS |
| 81 | Create release/* rule, then create release/2.0 | 201, creation allowed | HTTP 201; exit 0 creation | PASS |
| 82 | Update existing release/2.0 after rule applies | denied protected branch: ref.push | exit 1: remote: protected branch: ref.push | PASS |
| 83 | Push to an unprotected branch on the same repo | success | exit 0:  | PASS |
| 84 | Write-role bob creates tag protection rule (issue #135 gate) | 403 | HTTP 403 not permitted | PASS |
| 85 | Maintain-role carol creates tag protection rule | 201 | HTTP 201: {"id": "103b644b-afd8-4038-845d-6f9d73134320", "repository_id": "a93ac2c4-2a8e-490e-bced-7e00b0218ca1", "pattern": "v*", "allowed_to_create": "maintain", "allow_update": false, "al | PASS |
| 86 | Duplicate tag pattern and invalid allowed_to_create=member | 409 and 400 | HTTP 409; HTTP 400 | PASS |
| 87 | Write-role bob creates protected tag v9.0.0 | denied protected tag: ref.create | exit 1: remote: protected tag: ref.create | PASS |
| 88 | Maintain carol and owner create protected tags | success, success | exit 0: ; exit 0:  | PASS |
| 89 | Move protected tag v9.1.0 | denied protected tag: ref.update | exit 1: remote: protected tag: ref.update | PASS |
| 90 | Delete protected tag as carol and as owner (allow_delete false) | denied protected tag: ref.delete | exit 1 both: remote: protected tag: ref.delete | PASS |
| 91 | Write-role bob creates and deletes unprotected tag w1.0.0 | success, success | exit 0: ; exit 0:  | PASS |
| 92 | Rule with allow_force_push/allow_deletion true | force push and delete succeed | HTTP 201 rule; exit 0 force push; exit 0 delete | PASS |
| 93 | Permissive tag rule (write create, allow update/delete) | create, update, delete succeed | HTTP 201; exit 0 x3 | PASS |
| 94 | Tag rule allowed_to_create=read; read user erin creates tag | tag rule is a restriction, baseline ref.push still required | HTTP 201 rule; exit 1 erin: permission denied: ref.push (note: low thresholds cannot grant creation) | PASS |
| 95 | Read user deletes protected tag (allow_delete false) | denied protected tag: ref.delete | exit 1: remote: protected tag: ref.delete | PASS |
| 96 | Owner enables require_signed_commits on ssh-sign main | 201 | HTTP 201; rule created with require_signed_commits=true | PASS |
| 97 | Unsigned commit pushed to signed-commit branch | denied | exit 1: remote: commit ... is not signed by a registered committer | PASS |
| 98 | Commit signed with registered t05 key pushed | accepted | exit 1: remote: commit 9fc35dd4... is not signed by a registered committer | FAIL |
| 99 | Commit signed with unregistered key | denied | exit 1: is not signed by a registered committer | PASS |
| 100 | Commit signed with registered key but non-githearth committer email | denied | exit 1: is not signed by a registered committer | PASS |
| 101 | Same valid signed commit pushed over HTTP | accepted | HTTP git: remote: commit 9fc35dd4... is not signed by a registered committer | FAIL |
| 102 | Same signed commit pushed to an unprotected branch | success | exit 0 (signature creation and transport are fine) | PASS |
| 103 | Archive repository through API | archived true | HTTP 200 both; archived true | PASS |
| 104 | Clone archived repository over SSH | read allowed | exit 0 | PASS |
| 105 | Push new branch to archived repository | denied | exit 1: remote: permission denied: ref.push | PASS |
| 106 | Force push branch and push tag to archived repository | denied | exit 1 both: remote: permission denied: ref.push | PASS |
| 107 | Read-role erin clones archived repository | success | exit 0 | PASS |
| 108 | Unarchive, then push the same branch | 200, success | HTTP 200 archived false; exit 0 push | PASS |
| 109 | Host key fingerprint stable across three connections | identical key | identical ed25519 key line; SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE | PASS |
| 110 | Advertised host key fingerprint and type | SHA256, ED25519 | 256 SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE (ED25519) | PASS |
| 111 | Host key file in volume matches advertised key | same fingerprint | /var/lib/githearth/ssh_host_key derives same SHA256 | PASS |
| 112 | Host key file mode and owner | 0600 githearth | 600 githearth /var/lib/githearth/ssh_host_key | PASS |
| 113 | SSH identification banner | SSH-2.0-* | SSH-2.0-russh_0.65.1 | PASS |

## Defects

### 1. `require_signed_commits` rejects every push in the Compose image: git cannot find `ssh-keygen`

- Severity: **major**
- Repro:

  ```bash
  # 1. Protect a branch with the signed-commit gate (t05 is owner).
  curl -s -X POST http://127.0.0.1:8080/api/v1/organizations/lab05/repositories/ssh-sign/protection \
    -H "Authorization: Bearer $T05" -H 'Content-Type: application/json' \
    -d '{"pattern":"main","require_signed_commits":true}'      # -> 201

  # 2. Create an SSH-signed commit whose committer maps to the t05 account.
  git -c user.name=T05 -c user.email=t05@users.noreply.githearth.invalid \
      -c gpg.format=ssh -c user.signingkey=/tmp/lab05/id_ed25519 commit -S -m signed
  git verify-commit HEAD
  # Good "git" signature for t05@users.noreply.githearth.invalid with ED25519 key
  # SHA256:g0Zn9G7jniDblx9UXDlhqkb6h75ggsuUcS6ImoO1AiM

  # 3. Push over SSH (key registered to t05, the committer).
  GIT_SSH_COMMAND='ssh -i /tmp/lab05/id_ed25519 -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -p 2222' \
    git push ssh://127.0.0.1:2222/lab05/ssh-sign.git HEAD:main
  # remote: commit 9fc35dd41dbd228cf7ef89cab08ea766be866b95 is not signed by a registered committer

  # 4. The same push over HTTP gets the same denial.
  git push http://t05:<test-password>@127.0.0.1:8080/lab05/ssh-sign.git HEAD:main

  # 5. The runtime image has no ssh-keygen, which git needs for SSH signature verification.
  docker compose exec -T app sh -c 'command -v ssh-keygen || echo NO_SSH_KEYGEN'
  # NO_SSH_KEYGEN
  docker compose exec -T app sh -c \
    'cd /tmp/sigrepo && git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile=/tmp/allowed verify-commit HEAD'
  # error: cannot run ssh-keygen: No such file or directory
  ```

- Expected: the signed commit is accepted. `docs/foundation.md:182` documents
  that `require_signed_commits` accepts a push when every new commit carries an
  SSH signature verifying against a key registered to the committer.
- Actual: every push to the protected branch is refused with
  `is not signed by a registered committer`, including a signature that verifies
  locally with `git verify-commit`. Verification shells out through
  `git verify-commit` with `gpg.format=ssh`
  (`crates/githearth-git/src/signature.rs:78-97`), and git needs `ssh-keygen` for
  that. The image installs `git` but not `openssh-client`
  (`Dockerfile:18`), so the verifier fails closed and the feature is unusable.
- Evidence: cases 98 and 101; `F3` (SSH) and `F6` (HTTP) both return
  `remote: commit 9fc35dd4… is not signed by a registered committer`.
  `docker compose exec -T app sh -c 'command -v ssh-keygen'` prints nothing;
  `git verify-commit` in the container prints
  `error: cannot run ssh-keygen: No such file or directory` and exits 1. Running
  the same verification on the host succeeds. The hook call site is
  `crates/githearth-server/src/git_http.rs:747-751` and `:836-940`.
- Scope: systematic for the shipped Docker Compose deployment, both SSH and HTTP
  transports, for every rule with `require_signed_commits=true`. A host-run
  binary with `openssh-client` installed would verify correctly.

### 2. Refused SSH pushes leave orphan `push_credentials` rows (issue #143)

- Severity: **minor**
- Repro:

  ```bash
  # erin's registered key (read grant on lab05/ssh-private).
  KEY=43f17a1b-47e1-4b52-9960-29182b5dbc02

  docker compose exec -T postgres psql -U githearth -d githearth -Atc \
    "SELECT count(*) FROM push_credentials WHERE credential_kind='ssh_key' AND credential_id='$KEY';"
  # 1

  GIT_SSH_COMMAND='ssh -i /tmp/lab05/id_erin -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -p 2222' \
    git push ssh://127.0.0.1:2222/lab05/ssh-private.git HEAD:refs/heads/issue143-erin-2
  # remote: permission denied: ref.push
  # ! [remote rejected] (pre-receive hook declined)

  docker compose exec -T postgres psql -U githearth -d githearth -Atc \
    "SELECT count(*) FROM push_credentials WHERE credential_kind='ssh_key' AND credential_id='$KEY';"
  # 2

  docker compose exec -T postgres psql -U githearth -d githearth -Atc \
    "SELECT operation_id, created_at FROM push_credentials \
     WHERE credential_kind='ssh_key' AND credential_id='$KEY' ORDER BY created_at;"
  # ...|a8277d6a-401f-4e5c-a12c-e665ea319bd1|2026-11-13 21:21:58.333715+00

  docker compose exec -T postgres psql -U githearth -d githearth -Atc \
    "SELECT count(*) FROM repository_pushes WHERE operation_id='a8277d6a-401f-4e5c-a12c-e665ea319bd1';"
  # 0
  ```

- Expected: a push refused before any journal entry is applied leaves no
  `push_credentials` row (issue #143). The HTTP path already does this.
- Actual: the SSH path inserts the credential before spawning git
  (`crates/githearth-server/src/ssh.rs:362-384`) but after git exits only drops
  the repository guard and calls `pushes::replay`
  (`ssh.rs:432-437`); it never deletes the credential when no
  `repository_pushes` row was written. The HTTP path performs the guarded delete
  at `git_http.rs:528-545`. The cleanup sweep only removes rows that have an
  applied push (`pushes.rs:237-253`), so refused-push rows are retained
  indefinitely.
- Evidence: erin's key count went 1 → 2 after the refused push; the new
  operation `a8277d6a…` has no `repository_pushes` row. t05's primary key had 31
  credential rows, many with no applied push (from protected-branch, non-ff,
  notes/refs and signed-commit refusals). Contrast: a refused HTTP push using a
  `repo:write` access token left 0 rows
  (`credential_kind='token' AND credential_id='e7b0c323-805c-4b67-b892-56c214fca9b4'`).
- Scope: systematic for SSH pushes that never produce a journal entry: hook
  refusals (permission, protection, signed commits), non-fast-forward attempts
  rejected before the hook, and spawn failures. It overstates push activity in
  the database; it is not an authorization bypass.

## Notes / skipped

- **Live lab05 role ladder.** The grants actually present in `lab05` were
  t05=owner, bob=write, carol=maintain, dave=admin, erin=read, frank=triage,
  grace=member (all non-owners also hold a base `member` membership). The task
  note said carol=write and dave=maintain; testing followed the live grants.
  Because bob is a write member of `lab05`, the non-member cases used alice,
  who has no `lab05` membership.
- **Expired key authentication: skipped.** The API accepts only
  `expires_in_days` 1–3650 (`crates/githearth-server/src/ssh_keys.rs:136-140`),
  so an already-expired key cannot be created without a database write, which
  this campaign forbids. `authenticate` rejects expired keys at
  `ssh_keys.rs:302-314`; the 1-day boundary was verified (`expires_at` exactly
  +24 h).
- **SSH requires a key.** There is no anonymous SSH clone: `git clone` with no
  key or an unknown key fails with `Permission denied (publickey)`, and `none`
  and password authentication are rejected. Any registered key can clone a
  public repository (alice, a non-member of `lab05`, cloned `ssh-public`
  successfully), so public SSH access needs authentication but not membership.
  This matches the documented key-based SSH transport (`docs/foundation.md:188-204`).
- **Repository lookup is case-insensitive** (`repositories.rs:279` uses
  `lower(name)=lower($2)`), so `/lab05/SSH-Private.git` clones the same
  repository while the organization slug remains case-sensitive. Recorded as
  observed behavior, not a defect.
- **Advertised SSH methods.** The server advertises
  `password,publickey,hostbased,keyboard-interactive` in its SSH authentication
  method list, but only `publickey` is accepted; `auth_password` returns Reject
  (`ssh.rs:131-140`). Cosmetic only; no password credential can succeed.
- **Tag threshold below `write`.** `allowed_to_create` accepts
  `read`/`triage`/`write`/`maintain`/`admin`, but a tag rule is an additional
  restriction: a read user still needs the baseline `ref.push` (write) action.
  With `allowed_to_create=read`, erin's tag push was refused with
  `permission denied: ref.push` before the tag rule could allow it. The endpoint
  accepts the value and the behavior is consistent with the role matrix
  (`docs/decisions/0002-authorization.md:31`); the documentation could state
  that low thresholds cannot grant creation.
- **Deleting a nonexistent remote branch** returns exit 0 with
  `remote: warning: deleting a non-existent ref`, which is git's own no-op
  behavior; not a product defect.
- **Test mistakes, re-run.** The first protected-branch force-push attempt was a
  fast-forward (so it correctly reported `ref.push`, not `ref.force_push`) and
  the first protected-tag move pushed the same object (a no-op). Both were
  corrected and re-run; the re-runs are cases 79 and 89 and both behave as
  specified.
- **Cleanup.** The non-t05 SSH keys used for the access matrix (alice, bob,
  carol, dave, erin, frank, grace) were revoked after the run. `lab05` test
  repositories and rules remain for inspection; the t05 account keeps its
  primary Ed25519 and ECDSA keys.

## Round 2 — fixed build (2026-11-14)

Tester: t05. Build: fixed Compose image (`fix/m4-lab` 7e1ded3; app image
`githearth-app:latest` built 2026-11-13T22:15:49Z, Debian 13 trixie). Instance:
http://127.0.0.1:8080, SSH `127.0.0.1:2222`. No repository edit, no app or
database restart. The only extra container started was the one-shot
`docker compose run --rm --no-deps ssh-keygen` host-key service; the app stayed
`Up (healthy)` and `/health/live` returned 200 throughout.

Round-2 verdict: **PASS** — both round-1 defects are fixed on this image; no new
defect found. Cases re-run: 23, 23 pass, 0 fail, 0 skipped (11 defect-focused
checks + 12-case smoke). This supersedes the round-1 verdict.

### Defect re-tests

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| D1 | `ssh-keygen` available to the runtime user in the app image | present | `/usr/bin/ssh-keygen`; `OpenSSH_10.0p2 Debian-7+deb13u4`; `git 2.47.3`; Debian 13 (trixie) | PASS |
| D2 | Unsigned commit pushed to `ssh-sign` `main` (round-1 case 97) | refused | exit 1: `remote: commit 23311f4b… is not signed by a registered committer` | PASS |
| D3 | Commit signed with registered t05 Ed25519 key, committer `t05@users.noreply.githearth.invalid`, pushed over SSH (round-1 case 98) | accepted | exit 0: `4900eba..832d9c5 HEAD -> main`; commit 832d9c5 | PASS (was FAIL) |
| D4 | Same class of signed commit pushed over HTTP (round-1 case 101) | accepted | exit 0: `832d9c5..f61f805 HEAD -> main`; commit f61f805 | PASS (was FAIL) |
| D5 | Commit signed with unregistered key, committer t05 | refused | exit 1: `remote: commit c48f7eae… is not signed by a registered committer` | PASS |
| D6 | Commit signed with registered t05 key but committer `someone@example.com` | refused | exit 1: `remote: commit e8cda676… is not signed by a registered committer` | PASS |
| D7 | Signature on the accepted commits verifies against the registered key | Good signature | `git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile=… verify-commit` → `Good "git" signature for t05@… with ED25519 key SHA256:g0Zn9G7…`; `%G?` = `G` for 832d9c5 and f61f805 | PASS |
| D8 | Refused SSH push (permission denied) leaves no `push_credentials` row (round-1 defect 2) | 0 rows after | erin temp key `ed1a0da0-59ea-4725-9c46-0d8305454d49`: count 0 before, push refused `remote: permission denied: ref.push`, count 0 after | PASS (was FAIL) |
| D9 | Refused SSH push (signed-commit gate) leaves no `push_credentials` row | unchanged | t05-primary key `94f8c772…`: 41 rows / 17 historical orphans before, 41 / 17 after an unsigned push refused by the signed-commit hook | PASS |
| D10 | No orphan rows created since the fixed image started | 0 | `SELECT count(*) … NOT EXISTS (repository_pushes) AND created_at > '2026-11-13T22:15:00Z'` → 0 | PASS |
| D11 | Accepted SSH push still records its credential and journal row | +1 row with applied push | t05-primary 41 → 42; new operation `8e294a7c-ea00-4584-84d7-3038629c3fcc` has 1 `repository_pushes` row | PASS |

Notes on defect re-tests:

- The whole defect-1 repro was re-run end to end; the exact commands from
  round 1 now succeed. Verification runs inside the container as the
  `githearth` user (the earlier `cannot run ssh-keygen` path is gone).
- For defect 2 the refused-push check used a fresh key on erin (read role,
  so the push is denied with `ref.push`). It was registered through
  `POST /api/v1/user/ssh-keys` and revoked after the run (HTTP 204).
- The 27 orphan `push_credentials` rows still in the database all predate the
  fixed build (`min 21:19:16`, `max 21:24:33` on 2026-11-13; image built
  22:15:49). They are residue of the old build, not new writes; the fixed build
  stops the leak but does not retroactively purge them (the sweep only deletes
  rows with an applied push, by design). No new orphan rows appeared after the
  build.

### 12-case smoke re-run

Keys under `/tmp/lab05`; same SSH command shape as round 1
(`-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -p 2222`).

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| S1 | Register valid Ed25519 key | 201 with fingerprint | HTTP 201 id `064cf00b…`, fingerprint `SHA256:JcFp4Sxbjdyr7eIb0bUx84BpxxsI49qu65pFPfu3Ub4`, identical to `ssh-keygen -lf` | PASS |
| S2 | Register RSA key | 400 unsupported | HTTP 400: `must be an Ed25519 or ECDSA (nistp256/384/521) public key; RSA and DSA are not supported` | PASS |
| S3 | Register empty `public_key` | 400 | HTTP 400, same unsupported-key error | PASS |
| S4 | Register 17 KB `public_key` | 400 too long | HTTP 400: `{"error":"public key input too long"}` | PASS |
| S5 | Duplicate fingerprint in same account | 409 | HTTP 409: `{"error":"ssh key already registered"}` | PASS |
| S6 | Access token cannot manage SSH keys | 403 | HTTP 403 list (`access tokens cannot manage ssh keys`), HTTP 403 create (`read-only token`) | PASS |
| S7 | List keys: metadata only, stable order | no key material, repeated reads identical | 26 keys; no `public_key` field; every entry has `revoked_at`; two reads byte-identical | PASS |
| S8 | Clone before revoke, revoke, clone after | 0, 204, denied | clone exit 0; DELETE 204, `revoked_at` 2026-11-13T22:21:59Z; clone exit 128 `Permission denied (publickey,hostbased,keyboard-interactive)` | PASS |
| S9 | Re-register revoked fingerprint and clone | 201, success | HTTP 201 new id `9f87017f…`, same fingerprint; clone exit 0 | PASS |
| S10 | Clone private repo with unregistered vs registered key | denied / success | unregistered exit 128; registered t05 key exit 0 | PASS |
| S11 | Protected refs on `ssh-prot` `main` | create/force/delete all denied | direct push `remote: protected branch: ref.push`; non-ff force `ref.force_push`; delete `ref.delete`; all exit 1; `main` still 4900eba | PASS |
| S12 | Host-key stability and idempotent regeneration | stable, 0600, rerun keeps key | three `ssh-keyscan` reads identical `SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE`; volume `/var/lib/githearth/ssh_host_key` same fingerprint; `600 githearth`; `docker compose run --rm --no-deps ssh-keygen` exit 0, logged `SSH host key already exists; keeping it`, fingerprint unchanged | PASS |

### Updated verdict

**PASS.** Both round-1 defects are resolved on the fixed build:

1. `require_signed_commits` verifies SSH signatures in the shipped image;
   valid signed pushes are accepted over SSH and HTTP, unsigned/unregistered/
   foreign-committer pushes are still refused.
2. Refused SSH pushes no longer write an orphan `push_credentials` row; the
   accepted path still records exactly one credential and one journal row.

Remaining defects: none found in this build. Residual observation only: 27
`push_credentials` rows without an applied push (all created 21:19–21:24 on the
old build) remain in the live database; they are historical and no cleanup was
performed by this campaign.

Cleanup: erin's temporary smoke key was revoked. t05's new smoke/revoke keys
remain on the t05 account; `lab05` repositories and rules are unchanged, as are
the host key and app/database containers.
