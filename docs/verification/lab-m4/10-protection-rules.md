# Branch and tag protection — M4 lab acceptance report

Tester: t10 (owner, lab10). Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 171 run, 161 pass, 8 fail, 2 skipped.

Scope: branch-rule CRUD and validation, pattern precedence, maintain/write/read/admin role gates for both rule kinds (issue #135 regression), push enforcement, merge enforcement, tag enforcement, tag settings UI, and protection audit events. Fixtures: `lab10/t10-protect` (main, release/1.0, feature, feature2, release-other, main2, main3, signed-test, force-allowed, delete-allowed), `lab10/t10-merge` (main + scenario branches, `.githearth/CODEOWNERS` on main), `lab10/t10-tags`. No app restart, migration, or `acme` settings change; `acme/web` was only read.

Role note: the live `lab10` grants differ from the task's parenthetical roles. Live: t10 owner; carol maintain; dave admin; bob write; erin read; frank triage; grace member. I tested the live roles, so the write-denial cases use bob (the only write grant), the maintain cases use carol, and the admin cases use dave plus owner t10. This matches the shared brief and is the configuration the #135 regression is about.

## Results

### Branch rules: CRUD, fields, validation, idempotency

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| B01 | branch rules empty list | 200; empty list; generation 0 | 200 items=0 | PASS |
| B02 | create exact rule main | 201 | 201; all nine defaults matched | PASS |
| B03 | list shows rule and generation bumped | 200; contains main | 200; items=1 (main) | PASS |
| B04 | duplicate pattern main | 409 | 409 protection rule already exists | PASS |
| B05 | patch required_approvals to 1 | 200; required_approvals=1; dismiss_stale_approvals=true | 200; required_approvals=1, dismiss_stale=true | PASS |
| B06 | patch pattern main->main2 | 200; pattern=main2 | 400 invalid input | FAIL |
| B07 | patch pattern back to main | 200; pattern=main | 400 invalid input | FAIL |
| B07b | patch pattern via undocumented path key | 200; pattern=main2 | 200; pattern=main2 | PASS |
| B07c | patch path back to main | 200; pattern=main | 200; pattern=main | PASS |
| B08 | invalid pattern * | 400 | 400 invalid input | PASS |
| B09 | invalid branch glob release* | 400 | 400 invalid input | PASS |
| B10 | empty pattern | 400 | 400 invalid input | PASS |
| B11 | invalid double slash | 400 | 400 invalid input | PASS |
| B12 | invalid dotdot | 400 | 400 invalid input | PASS |
| B13 | required_approvals 11 rejected | 400 | 400 invalid input | PASS |
| B14 | required_approvals -1 rejected | 400 | 400 invalid input | PASS |
| B15 | duplicate required checks rejected | 422 | 422 required status checks must not contain duplicates | PASS |
| B16 | reserved check context rejected | 422 | 422 required status checks cannot include the reserved context | PASS |
| B17 | whitespace check context rejected | 422 | 422 required status check context must not have leading or trailing whitespace | PASS |
| B18 | unknown field rejected | 400 | 400 invalid input | PASS |
| B19 | empty patch rejected | 400 | 400 invalid input | PASS |
| B20 | patch invalid pattern rejected | 400 | 400 invalid input | PASS |
| B21 | create release/* rule | 201; pattern=release/* | 201; pattern=release/* | PASS |
| B22 | patch to duplicate pattern | 409 | 409 protection rule already exists | PASS |
| B23 | patch unknown rule id | 404 | 404 protection rule not found | PASS |
| B24 | delete unknown rule id | 404 | 404 protection rule not found | PASS |
| B25 | branch create idempotency replay | 201 both, same id | 201/201; same rule id | PASS |
| B26 | required_approvals boundary 10 accepted | 201; required_approvals=10 | 201; required_approvals=10 | PASS |
| B27 | nested exact branch pattern accepted | 201; pattern=team/branch/name | 201; pattern=team/branch/name | PASS |
| B28 | 101 required checks rejected | 400 | 400 invalid input | PASS |
| B29 | branch list unauthenticated | 401 | 401 invalid or expired credentials | PASS |
| B30 | cleanup leaves exactly main and release/* | 200 | 200; patterns main, release/* | PASS |
| B31 | create field-test rule | 201 | 201; rule field-test created | PASS |
| B32 | patch allow_force_push=true | 200 | 200; allow_force_push=true | PASS |
| B33 | patch allow_deletion=true | 200 | 200; allow_deletion=true | PASS |
| B34 | patch require_merge_queue=true | 200 | 200; require_merge_queue=true | PASS |
| B35 | patch require_code_owner_review=true | 200 | 200; require_code_owner_review=true | PASS |
| B36 | patch require_signed_commits=true | 200 | 200; require_signed_commits=true | PASS |
| B37 | patch unknown field rejected | 400 | 400 invalid input | PASS |
| B38 | delete rule with idempotency key | 204 | 204 | PASS |
| B39 | delete replay with same key | 204 | 204; replay of stored delete | PASS |
| B40 | delete again without key 404 | 404 | 404 protection rule not found | PASS |
| B41 | branch rule create/update/delete bumps generation | generation increments each mutation | 200; generation 88 -> 89 -> 90 -> 91 | PASS |


### Role gates (issue #135)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R01 | carol maintain creates branch rule | 201 | 201 id=<uuid> | PASS |
| R02 | carol maintain patches branch rule | 200; required_approvals=3 | 200; required_approvals=3 | PASS |
| R03 | carol maintain deletes branch rule | 204 | 204 | PASS |
| R04 | bob write creates branch rule | 403 | 403 not permitted | PASS |
| R05 | bob write patches branch rule | 403 | 403 not permitted | PASS |
| R06 | bob write deletes branch rule | 403 | 403 not permitted | PASS |
| R07 | dave admin creates branch rule | 201 | 201 id=<uuid> | PASS |
| R08 | dave admin patches branch rule | 200 | 200 id=<uuid> | PASS |
| R09 | dave admin deletes branch rule | 204 | 204 | PASS |
| R10 | erin read lists branch rules | 200 | 200 items=3 | PASS |
| R11 | erin read creates branch rule | 403 | 403 not permitted | PASS |
| R12 | erin read patches branch rule | 403 | 403 not permitted | PASS |
| R13 | erin read deletes branch rule | 403 | 403 not permitted | PASS |
| R14 | grace member reads private repo protection | 404 | 404 not found | PASS |
| R15 | grace member creates branch rule | 404 | 404 not found | PASS |
| R16 | frank triage creates branch rule | 403 | 403 not permitted | PASS |
| R17 | bob write creates tag rule | 403 | 403 not permitted | PASS |
| R18 | dave admin creates tag rule | 201 | 201 id=<uuid> | PASS |
| R19 | dave admin deletes tag rule | 204 | 204 | PASS |
| R20 | erin read creates tag rule | 403 | 403 not permitted | PASS |
| R21 | erin read lists tag rules | 200 | 200 items=2 | PASS |
| R22 | bob write deletes tag rule | 403 | 403 not permitted | PASS |
| R23 | grace member creates tag rule | 404 | 404 not found | PASS |
| R24 | frank triage creates tag rule | 403 | 403 not permitted | PASS |
| R25 | carol maintain creates tag rule | 201 | 201; pattern=carol-tag-* | PASS |
| R26 | carol maintain deletes own tag rule | 204 | 204  | PASS |


### Tag rules: CRUD, validation, generation

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| T01 | tag rules empty list | 200; empty list | 200 items=0 | PASS |
| T02 | create tag rule v* defaults | 201; pattern=v*; allowed_to_create=admin; allow_update=false; allow_delete=false | 201; admin / update=false / delete=false | PASS |
| T03 | duplicate tag pattern | 409 | 409 tag protection rule already exists | PASS |
| T04 | invalid tag pattern * | 400 | 400 invalid input | PASS |
| T05 | tag role owner rejected | 400 | 400 invalid input | PASS |
| T06 | tag role member rejected | 400 | 400 invalid input | PASS |
| T07 | create tag rule release/* maintain | 201 | 201 id=<uuid> | PASS |
| T08 | create exact tag rule v1.2.3 write | 201 | 201 id=<uuid> | PASS |
| T09 | tag list returns three rules | 200 | 200; patterns release/*, v*, v1.2.3 | PASS |
| T10 | tag rule update via PATCH | 405 | 405 Method Not Allowed | SKIP |
| T11 | carol maintain deletes tag rule | 204 | 204 | PASS |
| T12 | delete missing tag rule | 404 | 404 tag protection rule not found | PASS |
| T13 | tag list unauthenticated | 401 | 401 invalid or expired credentials | PASS |
| T14 | tag create unknown repo | 404 | 404 repository not found | PASS |
| T15 | tag rule unknown field rejected | 400 | 400 {"error": "invalid input"} | PASS |
| T16 | tag rule create/delete bumps shared generation | generation increments each mutation | 200; generation 40 -> 41 -> 42 | PASS |


### Web UI: tag settings page, CSRF, visibility, branch settings page

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| U01 | tag settings page t10 renders | 200 + Add a rule + Delete + csrf | 200; Add a rule, Delete, CSRF present | PASS |
| U02 | repository page links tag protection for maintainer | 200 + Tag protection link | 200; Tag protection link present | PASS |
| U03 | repository page has no branch protection link | Tag protection only | 200; no Branch protection link | PASS |
| U04 | tag settings page erin read: rules visible, controls hidden | 200 + no Add a rule + no Delete | 200; rule shown, no Add/Delete | PASS |
| U05 | tag settings page bob write: controls hidden | 200 + no Add a rule + no Delete | 200; no Add/Delete | PASS |
| U06 | tag settings page anonymous redirects to login | 303 Location /login | 303 Location: /login | PASS |
| U07 | tag UI create redirects | 303 -> settings/tags | 303 to .../settings/tags | PASS |
| U08 | tag UI create listed | 200 + ui-* | 200; ui-* listed | PASS |
| U09 | tag UI bad CSRF styled 403 | 403 text/html Invalid CSRF | 403 text/html; Invalid CSRF token | PASS |
| U10 | tag UI invalid pattern styled 400 | 400 text/html error | 503 Unavailable page (text/html) | FAIL |
| U11 | tag UI duplicate patterned styled 409 | 409 text/html already exists | 503 Unavailable page (text/html) | FAIL |
| U12a | branch protection settings page protection | 200 branch protection form | 404 Not Found | FAIL |
| U12b | branch protection settings page branches | 200 branch protection form | 404 Not Found | FAIL |
| U12c | branch protection settings page branch-protection | 200 branch protection form | 404 Not Found | FAIL |
| U13 | tag UI delete redirects | 303 | 303 to .../settings/tags | PASS |
| U14 | tag UI delete removed rule | 200 + no ui-* | 200; ui-* gone | PASS |
| U15 | tag settings page unknown repo styled 404 | 404 text/html | 404 text/html Not found | PASS |
| U16 | tag UI maintain carol sees create/delete controls | 200 + Add/Delete | 200; Add a rule + Delete present | PASS |
| U17 | tag UI member grace gets styled 404 | 404 html | 404 text/html Not found | PASS |


### Push enforcement

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| P01 | push a commit to main with required_approvals=1 | accepted (reviews gate merge only) | exit 0; main ae3d2b6..aff57ce | PASS |
| P02 | force push main (allow_force_push=false) | refused | exit 1; remote: protected branch: ref.force_push | PASS |
| P03 | delete main (allow_deletion=false) | refused | exit 1; remote: protected branch: ref.delete | PASS |
| P04 | push new commit to unprotected feature2 | accepted | exit 0; new branch feature2 | PASS |
| P05 | create release/2.0 matching release/* | accepted (rule applies after creation) | exit 0; new branch release/2.0 | PASS |
| P06 | force push release/1.0 (release/*) | refused | exit 1; protected branch: ref.force_push | PASS |
| P07 | true force push release-other (no match) | accepted | exit 0; forced update 7dd3a86...aff57ce | PASS |
| P08 | direct push to main with require_pull_request=true | refused | exit 1; protected branch: ref.push | PASS |
| P09 | direct push after require_pull_request=false | accepted | exit 0; main 7dd3a86..d56a46e | PASS |
| P10 | create main3 matching require_pull_request rule | accepted | exit 0; new branch main3 | PASS |
| P11 | update main2 protected by require_pull_request | refused | exit 1; protected branch: ref.push | PASS |
| P12 | push unsigned commit to signed-test (require_signed_commits) | refused | exit 1; commit <oid> is not signed by a registered committer | PASS |
| P13 | push SSH-signed commit to signed-test | accepted | SKIPPED: valid signature verified locally; server rejects because image lacks ssh-keygen | SKIP |
| P14 | force push force-allowed with allow_force_push=true | accepted | exit 0; forced update 7dd3a86...aff57ce | PASS |
| P15 | delete delete-allowed with allow_deletion=true | accepted | exit 0; deleted delete-allowed | PASS |
| P16 | delete release/2.0 (allow_deletion=false) | refused | exit 1; protected branch: ref.delete | PASS |
| P17 | delete unprotected release-other | accepted | exit 0; deleted release-other | PASS |
| P18 | force push release/v1/x (release/v1/* allows, release/* denies) | accepted (longest glob wins) | exit 0; forced update on release/v1/x | PASS |
| P19 | force push release/2.0 (only release/* matches) | refused | exit 1; protected branch: ref.force_push | PASS |
| P20 | force push release/1.0 with exact allow_force_push rule | accepted (exact beats glob) | exit 0; forced update on release/1.0 | PASS |
| P21 | push main with required_status_checks but no status posted | accepted (checks gate merge only) | exit 0; main d56a46e..f820cc5 | PASS |


### Merge enforcement

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| M01 | merge without required approval blocked | 403 | 403 not permitted | PASS |
| M02 | merge after one approval queued | 201 | 201; state=queued | PASS |
| M03 | merge worker reaches merged | 200; terminal state | 200; state=merged | PASS |
| M04 | stale approval at old head dismissed blocks merge | 403 | 403 not permitted | PASS |
| M05 | approval at new head allows merge | 201 | 201; state=queued | PASS |
| M06 | dismiss_stale=false keeps approval across head move | 201 | 201; state=queued | PASS |
| M07 | required check missing blocks merge | 403 | 403 not permitted | PASS |
| M08 | success on wrong commit does not satisfy check | 403 | 403 not permitted | PASS |
| M09 | pending required check blocks merge | 403 | 403 not permitted | PASS |
| M10 | failure required check blocks merge | 403 | 403 not permitted | PASS |
| M11 | green required check at head allows merge | 201 | 201; state=queued | PASS |
| M12 | green check on old head blocks after head move | 403 | 403 not permitted | PASS |
| M13 | green check at new head allows merge | 201 | 201; state=queued | PASS |
| M14 | require_merge_queue enqueues intent | 201 in_queue=true state=queued | 201; in_queue=true, state=queued | PASS |
| M15 | queued merge completes | 200; terminal state | 200; state=merged | PASS |
| M16 | code owner review required blocks merge | 403 | 403 not permitted | PASS |
| M17 | non-owner approval does not satisfy code owner rule | 403 | 403 not permitted | PASS |
| M18 | code owner approval allows merge | 201 | 201; state=queued | PASS |
| M19 | unsigned PR merges under signed-commit rule (merge exception) | 201 | 201; state=queued | PASS |
| M19b | signed-rule merge completes | 200; terminal state | 200; state=merged | PASS |
| M20 | one of two approvals blocks merge | 403 | 403 not permitted | PASS |
| M21 | two approvals allow merge | 201 | 201; state=queued | PASS |
| M22 | merge pull request with method=rebase | 201 queued/merged | 500 storage operation failed | FAIL |


### Tag ref enforcement

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| G01 | owner creates tag v2.0.0 (rule v*, admin) | accepted | exit 0; new tag v2.0.0 | PASS |
| G02 | bob/write creates tag v3.0.0 (v*, admin) | refused | exit 1; protected tag: ref.create | PASS |
| G03 | bob creates non-matching tag nightly-2 | accepted | exit 0; new tag nightly-2 | PASS |
| G04 | bob deletes protected tag v2.0.0 (allow_delete=false) | refused | exit 1; protected tag: ref.delete | PASS |
| G05 | bob deletes unprotected tag nightly-1 | accepted | exit 0; deleted nightly-1 | PASS |
| G06 | owner moves protected tag v2.0.0 (allow_update=false) | refused | exit 1; protected tag: ref.update | PASS |
| G07 | owner deletes protected tag v2.0.0 (allow_delete=false) | refused | exit 1; protected tag: ref.delete | PASS |
| G08 | carol/maintain creates tag release/1.0 (release/*, maintain) | accepted | exit 0; new tag release/1.0 | PASS |
| G09 | carol creates nested tag release/a/b | accepted | exit 0; new tag release/a/b | PASS |
| G10 | bob creates tag release/2.0 (release/*, maintain) | refused | exit 1; protected tag: ref.create | PASS |
| G11 | bob creates v1.2.3 (exact rule write; v* is admin) | accepted (exact beats glob) | exit 0; new tag v1.2.3 | PASS |
| G12 | bob creates v1.2.4 (falls back to v*, admin) | refused | exit 1; protected tag: ref.create | PASS |
| G13 | bob creates allow-write-1 (write + allow_update/delete) | accepted | exit 0; new tag allow-write-1 | PASS |
| G14 | bob moves allow-write-1 (allow_update=true) | accepted | exit 0; forced update allow-write-1 | PASS |
| G15 | bob deletes allow-write-1 (allow_delete=true) | accepted | exit 0; deleted allow-write-1 | PASS |


### Audit

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A01 | audit records branch_protection.created | 200 | 200 {"count": 43} | PASS |
| A02 | audit records branch_protection.deleted | 200 | 200 {"count": 33} | PASS |
| A03 | audit records branch_protection.updated | 200 | 200 {"count": 16} | PASS |
| A04 | audit records tag_protection.created | 200 | 200 {"count": 19} | PASS |
| A05 | audit records tag_protection.deleted | 200 | 200 {"count": 14} | PASS |
| A06 | audit chain verifies | 200 | 200 {"rows": 268, "ok": true, "first_bad_seq": null} | PASS |
| A07 | non-owner audit is 403 | 403 | 403 {"error": "not permitted"} | PASS |
| A08 | audit limit 0 rejected | 400 | 400 {"error": "invalid input"} | PASS |


## Defects

### Branch-rule PATCH rejects the documented `pattern` field; only unofficial `path` works

- Severity: major
- Repro:
  ```bash
  T=$(cat token_t10); B=http://127.0.0.1:8080/api/v1/organizations/lab10/repositories/t10-protect
  curl -s -X POST $B/protection -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
    -d '{"pattern":"defect-demo"}'          # 201, id=c87a2cd5-1a50-40e4-8ca0-59782f9286d9
  curl -s -X PATCH $B/protection/c87a2cd5-1a50-40e4-8ca0-59782f9286d9 \
    -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
    -d '{"pattern":"defect-renamed"}'       # 400 {"error":"invalid input"}
  curl -s -X PATCH $B/protection/c87a2cd5-1a50-40e4-8ca0-59782f9286d9 \
    -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
    -d '{"path":"defect-renamed"}'          # 200; pattern becomes defect-renamed
  ```
- Expected: `PATCH /protection/{id}` edits a rule's `pattern`, the field every read response, every create body and the docs use.
- Actual: `pattern` is rejected with 400 because `PatchRule` declares `path` instead; the undocumented `path` key renames the rule. Other PATCH fields work.
- Evidence: `crates/githearth-server/src/protection.rs:431-433` declares `path: Option<String>` under `deny_unknown_fields`; `:488` validates `input.path`; `:526-529` applies it as the new pattern. Cases B06/B07 fail, B07b/B07c pass.
- Scope: systematic for every PATCH that tries to rename a rule. Workaround: send `path`.

### No branch-protection settings UI

- Severity: major
- Repro: sign in as t10 in a browser, then GET `/organizations/lab10/repositories/t10-protect/settings/protection`, `/settings/branches` and `/settings/branch-protection`.
- Expected: a branch-protection settings page that lists rules and offers create/edit/delete forms with the session CSRF token, mirroring the tag-protection page.
- Actual: all three paths return 404. The repository page only links “Tag protection”; there is no “Branch protection” link. Branch rules can only be managed through the JSON API.
- Evidence: the UI router (`crates/githearth-server/src/ui.rs:925-942`) merges `tag_protection_ui::router()` but no branch-protection router; the only page route is `crates/githearth-server/src/tag_protection_ui.rs:22`; the repository page builds only `tag_link` at `ui.rs:708-711`. Cases U12a/U12b/U12c fail; U03 shows no link.
- Scope: systematic; no user role gets a branch-rule UI.

### `require_signed_commits` cannot accept any valid SSH-signed push in the shipped image

- Severity: major
- Repro:
  ```bash
  ssh-keygen -q -t ed25519 -N '' -f /tmp/t10/signkey
  # POST /api/v1/user/ssh-keys -> 201 (fingerprint SHA256:5zxMsn...)
  git config gpg.format ssh; git config user.signingkey /tmp/t10/signkey
  git commit -S -m signed          # commit has a gpgsig SSH signature
  git -c gpg.ssh.allowedSignersFile=/tmp/t10/allowed verify-commit HEAD
  # -> Good "git" signature for t10@users.noreply.githearth.invalid
  git push origin signed-test
  # -> remote: commit 42dee8f... is not signed by a registered committer
  ```
- Expected: an unsigned commit is refused, and a commit signed by a key registered to the committer's account is accepted.
- Actual: the unsigned refusal works (P12), but the valid signed commit is also refused. `docker exec githearth-app-1 sh -c 'command -v ssh-keygen'` prints nothing; `docker exec githearth-app-1 git --version` is 2.39.5. Git's SSH signature verification needs `ssh-keygen`; the runtime image installs `git` with `--no-install-recommends` and no `openssh-client` (`Dockerfile:18`). Acceptance case P13 is therefore skipped as blocked, not passed.
- Evidence: local `git verify-commit` reports Good; server returns the “not signed by a registered committer” pre-receive error; `crates/githearth-server/src/git_http.rs:751` and `crates/githearth-git/src/signature.rs` run `git verify-commit` with `gpg.format=ssh`.
- Scope: every push to a `require_signed_commits` branch in this deployment; the policy is unusable end to end.

### Tag-settings form turns validation and duplicate errors into 503 “Unavailable”

- Severity: minor
- Repro: on `/organizations/lab10/repositories/t10-tags/settings/tags` submit the Add-rule form with a valid CSRF token and (a) pattern `*` or (b) pattern `v*` (already exists).
- Expected: a styled 400 (invalid input) or 409 (already exists) form error that tells the user what to fix and keeps the entered values.
- Actual: both return HTTP 503 with the generic “Unavailable” page. Bad CSRF correctly returns a styled 403; unknown repository correctly returns a styled 404.
- Evidence: `crates/githearth-server/src/tag_protection_ui.rs:90-102` maps every API error other than 401/403/404 to `ui::unavailable_page`; U10/U11.
- Scope: every validation/conflict error submitted through the tag form (API callers get the correct 400/409).

### Pull-request reads return 500 (rebase mergeability incompatible with the image's Git)

- Severity: major (cross-domain; likely blocker for the pull-request surface)
- Repro: `GET /api/v1/organizations/lab10/repositories/t10-merge/pulls/1` and `GET /api/v1/organizations/acme/repositories/web/pulls/1` as t10 both return `500 {"error":"storage operation failed"}`. `POST .../pulls/13/merge` with `{"method":"rebase",...}` also returns 500; the same request with `method:"merge"` works.
- Expected: 200 with PR detail and live `head_id`/`base_id`/mergeability; rebase merges should enqueue like `method=merge`.
- Actual: every PR read and every rebase merge fails. The container has Git 2.39.5, which rejects the flag `--merge-base=` used for rebase candidate construction:
  ```
  git merge-tree --write-tree -z --name-only --no-messages --merge-base=<oid>^ <target> <head>
  error: unknown option `merge-base=...`   (exit 129)
  ```
  `mergeability::for_pull` checks merge, squash and rebase for every PR read, so the rebase failure fails the whole read. Logs show `repository read failed category=git` for the repo.
- Evidence: `crates/githearth-git/src/merge.rs:283-285`; `crates/githearth-server/src/mergeability.rs:207-217`; `Dockerfile:18` installs Debian bookworm Git 2.39.5. Merge enforcement cases M01-M21 were run through the merge API with head OIDs read from `git ls-remote` because the PR read endpoint could not return `head_id`.
- Scope: systematic; direct `method=merge` works, rebase fails. This is outside protection code but was hit while testing merge enforcement and blocks the PR page for every user.

## Notes / skipped

- **Tag rules have no update operation.** `PATCH /tag-protection/{id}` returns 405; only POST/GET/DELETE exist (`crates/githearth-server/src/tag_protection.rs:32-42`), and the page is documented as “list, create and delete rules” (`tag_protection_ui.rs:1`). Marked T10 skipped, not failed. To change `allowed_to_create` or either switch, delete and recreate. This is an asymmetry with branch rules, not a violation of a documented promise.
- **P13 skipped:** signed-commit acceptance is blocked by the missing `ssh-keygen` in the runtime image (defect above). The refusal path for unsigned commits passed.
- **Tag rule role gate now matches branch rules (issue #135):** carol (maintain) created/patched/deleted branch rules (R01-R03) and created/deleted tag rules (R25/R26); bob (write) got 403 on both kinds (R04-R06, R17, R22); dave (admin) succeeded on both (R07-R09, R18/R19); erin (read) and frank (triage) got 403; grace (member, no repository read) got the non-enumerable 404. No admin-only remnant remains.
- **Pattern semantics:** branch patterns accept an exact name or one trailing `*` after a `/` (`release/*`, `release/v1/*`); `*` and `release*` are 400. Tag patterns accept an exact name or a trailing `*` with no slash required (`v*`); `*` is 400. Exact beats glob and the longest glob prefix wins in both (P18-P20, G11/G12, B18-B22).
- **Defaults:** a new branch rule is `allow_force_push=false, allow_deletion=false, require_pull_request=false, required_approvals=0, required_status_checks=[], dismiss_stale_approvals=true, require_signed_commits=false, require_merge_queue=false, require_code_owner_review=false` (B02). A new tag rule defaults to `allowed_to_create=admin, allow_update=false, allow_delete=false` (T02).
- **Merge facts:** required approvals counted at the current head by default; stale approvals blocked the merge after a head move (M04), and `dismiss_stale_approvals=false` kept the approval (M06). Required checks counted only at the head (M08/M12). Code-owner review required the owner (carol), not any approver (M16-M18). Queue requirement set `in_queue=true` and the worker merged it (M14/M15). Server-created merges bypass `require_signed_commits` as ADR 0007 documents (M19).
- **Audit:** branch and tag rule create/update/delete kinds are present; the lab10 chain verified (`rows=268, ok=true`). Maintainers cannot read the audit endpoint (403), as designed.
- **Fixtures left in place** for follow-up: three `lab10/t10-*` repositories, rules listed above, and a few open scenario branches/PRs. No shared-org settings were changed.

## Round 2 — fixed build (2026-11-14)

Tester: t10 (owner, lab10). Re-verification of the fixed build on the same instance: http://127.0.0.1:8080. Running image `githearth-app:latest` built from `fix/m4-lab` commit `7e1ded3` ("Fix M4 lab defects"), image created 2026-11-13T22:15:49Z; container `githearth-app-1` healthy. Runtime now reports Git 2.47.3 and `/usr/bin/ssh-keygen`. No repository edit, migration, restart or rebuild was performed by this tester; sessions were reissued with `POST /api/v1/login` after the app restart. Round-1 fixtures and rules were reused.

Verdict: **PASS WITH DEFECTS** — 3 of the 5 round-1 defects are fixed and verified; 2 remain.

Cases: 40 re-run, 36 pass, 4 fail, 0 skipped.

### A. Branch-rule PATCH accepts the documented `pattern` (round-1 defect 1)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| F01 | PATCH rule with documented `pattern` field | 200; pattern renamed | 200; `r2-rename-a` -> `r2-rename-b` | PASS |
| F02 | PATCH rule with `path` alias | 200; pattern renamed | 200; `r2-rename-b` -> `r2-rename-c` | PASS |
| F03 | PATCH `pattern:"*"` rejected | 400 | 400 invalid input | PASS |
| F04 | PATCH with both `pattern` and `path` | 400 (ambiguous) | 400 invalid input | PASS |
| F05 | PATCH non-pattern field unaffected | 200; `allow_force_push=true` | 200; force=true | PASS |
| F06 | PATCH to an existing pattern | 409 | 409 | PASS |
| F07 | PATCH unknown field | 400 | 400 invalid input | PASS |
| F08 | PATCH empty body | 400 | 400 invalid input | PASS |
| F09 | PATCH unknown rule id | 404 | 404 protection rule not found | PASS |
| F10 | PATCH `required_approvals=11` | 400 | 400 invalid input | PASS |

Source confirms the fix: `PatchRule` now declares `#[serde(default, alias = "path")] pattern: Option<String>` and validates/applies `input.pattern` (`crates/githearth-server/src/protection.rs`, fix commit `7e1ded3`).

### B. `require_signed_commits` accepts a valid SSH-signed push (round-1 defect 3)

Rule: `signed-test` -> `require_signed_commits=true`. Signing key is the ed25519 key registered to t10 (`SHA256:5zxMsn...`).

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| S01 | Push a commit SSH-signed by the registered key | accepted | accepted; `* [new branch] signed-test`, then `b8a5381..9b68db8` | PASS |
| S02 | Push an unsigned commit | refused | exit 1; `commit 82d6bf3... is not signed by a registered committer` | PASS |
| S03 | Push a commit signed by an unregistered key | refused | exit 1; `commit 7b30a3a... is not signed by a registered committer` | PASS |
| S04 | Same signed push over SSH transport (`127.0.0.1:2222`) | accepted | accepted; `9b68db8..646186b` | PASS |

`git verify-commit` locally reports `Good "git" signature for t10@users.noreply.githearth.invalid ... SHA256:5zxMsn...`, and the server now agrees. The image change is Debian trixie + `openssh-client` (`Dockerfile` in `7e1ded3`), which supplies `ssh-keygen` for `crates/githearth-git/src/signature.rs:76-94`.

### C. Pull-request reads and rebase merge (round-1 defect 5)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C01 | `GET lab10/t10-merge/pulls/1` | 200 with live `head_id` | 200; head_id `0657d937...`, base_id `3f71031...` | PASS |
| C02 | `GET acme/web/pulls/1` | 200 with live `head_id` | 200; head_id `c128866...`, base_id `dd1c665...` | PASS |

The rebase path is re-covered by smoke case M04 below: `POST .../merge {"method":"rebase"}` now returns 201 and the worker reaches `merged` (round 1 returned 500).

### D. Tag-rule role parity, issue #135

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R01 | carol maintain creates tag rule | 201 | 201; pattern `r2-carol-*` | PASS |
| R02 | carol maintain deletes that tag rule | 204 | 204 | PASS |
| R03 | bob write creates tag rule | 403 | 403 not permitted | PASS |
| R04 | bob write deletes tag rule | 403 | 403 not permitted | PASS |

Branch and tag rules still share the `repository.update` gate; no admin-only remnant.

### E. Remaining defects re-check

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| U01 | Branch-protection settings page (5 paths: `settings/protection`, `settings/branches`, `settings/branch-protection`, `settings/branches/protection`, `settings/protection/branches`) | 200 + form | 404 on all five as signed-in t10 | FAIL |
| U02 | Repository page links branch protection | link present | 200; only "Tag protection" link | FAIL |
| U03 | Tag form invalid pattern `*` | styled 400 | 503 "Unavailable" (text/html) | FAIL |
| U04 | Tag form duplicate pattern `v*` | styled 409 | 503 "Unavailable" (text/html) | FAIL |
| U05 | Tag form bad CSRF still styled 403 | 403 text/html Invalid CSRF | 403 text/html Invalid CSRF token | PASS |
| U06 | Tag form valid create still redirects | 303 -> settings/tags | 303; rule listed; cleanup delete 204 | PASS |
| U07 | carol maintain sees tag controls | Add a rule + Delete | 200; both present | PASS |
| U08 | bob write has no tag controls | neither control | 200; both absent | PASS |

### F. 12-case protection enforcement smoke (push/merge)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| P1 | push commit to main (required_approvals=1) | accepted (reviews gate merge only) | exit 0; `f820cc5..2fa3dcf` | PASS |
| P2 | force push main (allow_force_push=false) | refused | exit 1; `protected branch: ref.force_push` | PASS |
| P3 | delete main (allow_deletion=false) | refused | exit 1; `protected branch: ref.delete` | PASS |
| P4 | direct push main2 (require_pull_request=true) | refused | exit 1; `protected branch: ref.push` | PASS |
| P5 | force push release/1.0 (exact rule allows force) | accepted | exit 0; forced `aff57ce...66873db` | PASS |
| P6 | force push release/2.0 (release/* denies) | refused | exit 1; `protected branch: ref.force_push` | PASS |
| P7 | force push release/v1/x (longest glob allows) | accepted | exit 0; forced `aff57ce...b9924c9` | PASS |
| P8 | push unsigned commit to signed-test | refused | exit 1; `not signed by a registered committer` | PASS |
| M1 | merge PR without required approval | 403 | 403 not permitted | PASS |
| M2 | merge after one approval | 201 queued -> merged | 201 state=queued; worker state=merged | PASS |
| M3 | missing required check blocks, green check allows | 403 then 201 queued -> merged | 403 not permitted; after success 201 queued; worker merged | PASS |
| M4 | merge with `method=rebase` (round-1 500) | 201 queued -> merged | 201 state=queued; worker state=merged | PASS |

Pattern precedence still holds (P5-P7: exact `release/1.0` beats `release/*`; `release/v1/*` beats `release/*` for `release/v1/x`).

### Round-2 verdict by round-1 defect

| Round-1 defect | Round-2 status | Evidence |
| --- | --- | --- |
| Branch-rule PATCH rejects documented `pattern`; only `path` works | **FIXED** | F01-F10; both `pattern` and `path` accepted, invalid/ambiguous still 400 |
| No branch-protection settings UI | **OPEN** | U01/U02; no router in `ui.rs` (`7e1ded3`), no link on repository page |
| `require_signed_commits` cannot accept a valid SSH-signed push | **FIXED** | S01/S04 accepted (HTTP and SSH transport); S02/S03 refused |
| Tag-settings form turns validation/duplicate errors into 503 | **OPEN** | U03/U04 still 503; `tag_protection_ui.rs` unchanged in the fix commit |
| Pull-request reads return 500; rebase merge fails | **FIXED** | C01/C02 200 with head/base ids; M04 rebase reaches merged |

### Remaining defects

#### Branch-protection settings UI still absent

- Severity: major (unchanged from round 1). The foundation doc documents branch protection through the JSON API only, so this is a UI parity gap rather than a broken documented flow; tag protection has a page and branch protection does not.
- Repro: sign in as t10, then GET `/organizations/lab10/repositories/t10-protect/settings/protection`, `/settings/branches`, `/settings/branch-protection`, `/settings/branches/protection` and `/settings/protection/branches`; open `/organizations/lab10/repositories/t10-protect`.
- Expected: a branch-protection settings page listing rules with create/edit/delete forms and CSRF tokens, reachable from the repository page, mirroring tag protection.
- Actual: all five paths 404; the repository page shows only the "Tag protection" link.
- Evidence: `crates/githearth-server/src/ui.rs` router in `7e1ded3` merges `settings_ui` and `tag_protection_ui` but no branch-protection router; the only protection page route remains `crates/githearth-server/src/tag_protection_ui.rs:22`; the repository page builds only `tag_link` (`ui.rs`). U01/U02 fail.
- Scope: systematic; no role gets a branch-rule UI.

#### Tag-settings form still turns validation and duplicate errors into 503 "Unavailable"

- Severity: minor (unchanged from round 1).
- Repro: on `/organizations/lab10/repositories/t10-tags/settings/tags` submit the Add-rule form with a valid CSRF token and (a) pattern `*` or (b) pattern `v*` (already exists).
- Expected: styled 400 (invalid input) or 409 (already exists) with the entered values preserved.
- Actual: both 503 "Unavailable"; bad CSRF still correctly 403 and a valid create still 303, so only the 400/409 mapping is wrong.
- Evidence: `crates/githearth-server/src/tag_protection_ui.rs:90-102` maps every API error other than 401/403/404 to `ui::unavailable_page`; the file is untouched by `7e1ded3`. U03/U04 fail; U05/U06 pass.
- Scope: every validation/conflict error submitted through the tag form; API callers still receive correct 400/409.

### Round-2 notes

- **No regressions seen.** The 12-case push/merge smoke is 12/12, and the branch-rule PATCH field regressions (F05-F10) are clean.
- **Fixed build identity:** `docker inspect githearth-app:latest` created 2026-11-13T22:15:49Z, matching the `7e1ded3` commit timestamp; container Git 2.47.3 and `/usr/bin/ssh-keygen`, so both the rebase (`--merge-base`, Git >= 2.40) and signed-commit fixes are present in the runtime.
- **Cleanup:** temporary round-2 branch rule and tag rules were deleted. `t10-merge` was returned to no branch rules. `t10-protect` keeps its round-1 fixture rules; `signed-test` now points at `646186b` and main at `2fa3dcf`. No shared-org settings were touched.
