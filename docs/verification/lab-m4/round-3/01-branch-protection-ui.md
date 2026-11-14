# Branch protection UI — M4 closure round 3

Tester: t01. Date: 2026-11-14. Instance: main 800cad4.
Verdict: PASS WITH DEFECTS
Cases: 83 run, 81 pass, 2 fail, 0 skipped.

Test repositories (lab01, t01-owned): `r3-bp` (enforcement), `r3-bp-ui`
(create/delete/authorization), `r3-bp-tags` (tag parity). All cases used a
browser cookie (`githearth_session=<login token>`) with CSRF parsed from the
rendered page, plus HTTP Git and the JSON merge/review/status APIs for
enforcement. Every failure was re-checked once; no product failure changed on
re-run.

## Results
| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Page: anonymous GET `/settings/branches` | redirect to sign-in | 303 → `/login` | PASS |
| 2 | Page: owner GET | 200, add form with every field | 200; 12 named inputs; approvals `min=0 max=10` | PASS |
| 3 | Page: maintain GET | 200 + form | 200 + form | PASS |
| 4 | Page: write member GET with rules present | 200, rules listed, no add/delete controls | 200, 4 rows, 0 forms | PASS |
| 5 | Page: read member GET | 200, no add/delete controls | 200, 0 forms | PASS |
| 6 | Page: triage member GET | 200, no add/delete controls | 200, 0 forms | PASS |
| 7 | Page: base-role `member` GET | 404 (no implicit repo read) | 404 | PASS |
| 8 | Page: non-member (`alice`) GET | 404 | 404 | PASS |
| 9 | Page: unknown repository | 404 | 404 | PASS |
| 10 | Page: unknown organization | 404 | 404 | PASS |
| 11 | Link: owner repository page | Branch/Tag protection links shown | both links present | PASS |
| 12 | Link: maintain repository page | both links shown | both links present | PASS |
| 13 | Link: write/read/triage repository page | no settings links | 0 links for each | PASS |
| 14 | Page: owner with 4 rules | one delete form per rule | 4 rows, 4 delete forms | PASS |
| 15 | Create: owner, every field | 303; all 11 fields persisted | 303; pattern/flags/approvals/checks/signed/queue/code-owners all stored | PASS |
| 16 | Create: maintain | 303; rule stored | 303; appears in list | PASS |
| 17 | Create: write member forged POST (valid CSRF) | 403, no rule | 403 “Not permitted”; count unchanged | PASS |
| 18 | Create: read member forged POST | 403 | 403 “Not permitted” | PASS |
| 19 | Create: triage member forged POST | 403 | 403 “Not permitted” | PASS |
| 20 | Create: non-member forged POST (valid CSRF) | 404 | 404 “Not found”; count unchanged | PASS |
| 21 | Create: no session | 303 → `/login` | 303 → `/login` | PASS |
| 22 | Create: missing CSRF | 403 | 403 “Invalid CSRF token” | PASS |
| 23 | Create: wrong CSRF | 403 | 403 | PASS |
| 24 | Create: CSRF from another session | 403 | 403 | PASS |
| 25 | Create: duplicate pattern | 409 page | 409 page + “already exists” | PASS |
| 26 | Create: pattern with space | 400 page + draft kept | 400 + alert; pattern/approvals/checks/checkboxes retained | PASS |
| 27 | Create: glob without trailing slash (`release*`) | 400 | 400 “invalid input” | PASS |
| 28 | Create: whitespace-only pattern | 400 | 400 | PASS |
| 29 | Create: approvals 11 | 400 | 400 | PASS |
| 30 | Create: approvals −1 | 400 | 400 | PASS |
| 31 | Create: approvals “abc” | 400 | 400 | PASS |
| 32 | Create: approvals 0 | 303; stored | 303; stored | PASS |
| 33 | Create: approvals 10 | 303; stored | 303; stored | PASS |
| 34 | Create: newline-separated checks | two contexts stored | `['ci/a','ci/b']` | PASS |
| 35 | Create: duplicate checks `a,a` | 422 | 422 “must not contain duplicates” | PASS |
| 36 | Create: reserved legacy context | 422 | 422 “cannot include the reserved context” | PASS |
| 37 | Create: 101 status checks | 400 | 400 | PASS |
| 38 | Create: invalid idempotency key | 400 | 400 “Reload the page…” | PASS |
| 39 | Idempotency: replay same key and body | 303 twice, one rule | 303/303; count +1 only | PASS |
| 40 | Idempotency: same key, different body | 422, no rule | 422 “key reused with different request” | PASS |
| 41 | XSS: `<script>` in pattern | 400, escaped | 400; no raw `<script>`; escaped in draft | PASS |
| 42 | XSS: `<img onerror>` as check context | stored but escaped in table | 303; table has `&lt;img …&gt;`, no raw tag | PASS |
| 43 | Error page: draft preservation | values/checkboxes kept | all kept on 400 | PASS |
| 44 | Delete: owner | 303; rule gone | 303; gone | PASS |
| 45 | Delete: maintain | 303; rule gone | 303; gone | PASS |
| 46 | Delete: write member forged POST | 403; rule remains | 403; remains | PASS |
| 47 | Delete: read member forged POST | 403; rule remains | 403; remains | PASS |
| 48 | Delete: non-member forged POST | 404; rule remains | 404; remains | PASS |
| 49 | Delete: missing CSRF (form body) | 403 | 403 | PASS |
| 50 | Delete: wrong CSRF | 403 | 403 | PASS |
| 51 | Delete: unknown rule id | 404 page | 404 “Not found” | PASS |
| 52 | Delete: rule owned by another repo | 404; other rule remains | 404; remains | PASS |
| 53 | Delete: GET on delete route | 405 | 405 | PASS |
| 54 | Enforce: normal update of protected `main` | refused (require PR) | `protected branch: ref.push`; `main` unchanged | PASS |
| 55 | Enforce: genuine non-ff force push to `main` | refused | `protected branch: ref.force_push` | PASS |
| 56 | Enforce: delete protected `main` | refused | `protected branch: ref.delete` | PASS |
| 57 | Enforce: push unprotected `feature` | accepted | accepted | PASS |
| 58 | Enforce: force push with `allow_force_push=true` | accepted | forced update accepted | PASS |
| 59 | Enforce: delete with `allow_deletion=true` | accepted | branch deleted | PASS |
| 60 | Enforce: unsigned update under `require_signed_commits` (trusted committer email) | refused | “is not signed by a registered committer”; ref unchanged | PASS |
| 61 | Enforce: SSH-signed update with registered Ed25519 key | accepted | signed update accepted | PASS |
| 62 | Enforce: force push to `release/1.0` via `release/*` | refused | `protected branch: ref.force_push` | PASS |
| 63 | Enforce: delete `release/1.0` via `release/*` | refused | `protected branch: ref.delete` | PASS |
| 64 | Merge gate: approvals-only rule, no approval | 403 | 403 `{"error":"not permitted"}` | PASS |
| 65 | Merge gate: approvals-only rule, after maintain approval | 201; merged | 201; `main` history has merge commit | PASS |
| 66 | Merge gate: checks-only rule, `ci/build` missing | 403 | 403 | PASS |
| 67 | Merge gate: checks-only rule, after `ci/build=success` | 201; merged | 201; merge commit present | PASS |
| 68 | Tag parity: maintain GET `/settings/tags` | 200 + form | 200 + form | PASS |
| 69 | Tag parity: maintain create `v*` | 303; listed | 303; listed | PASS |
| 70 | Tag parity: maintain delete | 303; gone | 303; gone | PASS |
| 71 | Tag parity: write member forged create | 403 | 403 “Not permitted” | PASS |
| 72 | Tag parity: read member forged create | 403 | 403 | PASS |
| 73 | Tag parity: non-member page and delete | 404 | 404; rule remains | PASS |
| 74 | Tag parity: write member forged delete | 403; rule remains | 403; remains | PASS |
| 75 | Tag parity: missing CSRF | 403 | 403 | PASS |
| 76 | Tag parity: invalid pattern | 400 | 400 “Choose a valid pattern…” | PASS |
| 77 | Tag parity: duplicate pattern | 409 page | 409 + “already exists” | PASS |
| 78 | Tag enforce: write member creates `v1.2.3` under admin-only rule | refused | `protected tag: ref.create` | PASS |
| 79 | Tag enforce: owner creates `v1.0.0` | accepted | accepted | PASS |
| 80 | Tag enforce: owner deletes protected tag | refused | `protected tag: ref.delete` | PASS |
| 81 | Tag enforce: owner moves protected tag | refused | `protected tag: ref.update` | PASS |
| 82 | List: every rule field is visible on the page | `dismiss_stale_approvals` shown | no column/row value for it (9 columns only) | FAIL |
| 83 | Checks input: label matches the control | line-separated entry possible | single-line `<input>`; newlines cannot be entered | FAIL |

## Defects
### Rules table omits `dismiss_stale_approvals`
- Severity: minor
- Repro: as owner, create a rule with “Dismiss stale approvals” checked
  (e.g. `staletest`), then reload
  `/organizations/lab01/repositories/r3-bp/settings/branches`. The row and the
  header list Pattern, Force push, Deletion, Require PR, Approvals, checks,
  Signed, Merge queue, Code owners — nine columns, no dismissal setting.
- Expected: the list lets a manager audit every stored field, including
  `dismiss_stale_approvals`.
- Actual: the value is accepted and persisted (API returns `true`) but the
  settings table never shows it, so a rule's stale-approval policy is invisible
  after creation.
- Evidence: table header/row built in
  `crates/githearth-server/src/branch_protection_ui.rs:217-229` and
  `:239-244`; field exists in `ProtectionRule`
  (`crates/githearth-server/src/protection.rs:52`). Observed page header list
  ends at `Code owners`; `grep -i stale` on the page only matches the add form.

### Status-checks field advertises line separation but is single-line
- Severity: minor
- Repro: open the add form and try to separate two required checks with a
  newline; the control is an `<input>` (single-line), so the browser strips the
  line break on submit. Only comma separation works in the browser.
- Expected: a `<textarea>` (or a label that says only “comma separated”),
  matching the server's `parse_status_checks`, which splits on both `,` and
  `\n` (`branch_protection_ui.rs:153-161`).
- Actual: label reads “comma or line separated” while the control is
  `<input id="required_status_checks" …>`
  (`branch_protection_ui.rs:261-262`); newline input is unreachable from the
  UI, though it works over curl.
- Evidence: rendered form markup; newline-separated `ci/a` + `ci/b` via curl
  stored both contexts (case 34), proving the backend supports what the
  browser control cannot enter.

## Notes
- Issue #150 is functionally closed: the page exists at the documented path,
  is linked only for managers, lists/creates/deletes rules with all required
  fields, keeps 400/409/422 as rendered pages, enforces owner/maintain versus
  write/read/non-member authorization, rejects bad/missing CSRF, replays
  idempotency keys safely, maps duplicates to 409 and bad patterns to 400, and
  escapes pattern/check XSS. The two defects above are display/UX polish and
  do not affect authorization, validation or enforcement.
- Read member (`erin`) intentionally receives 200 with no controls; only
  non-members and base-role `member` get 404. This matches the existing
  expectation in `crates/githearth-server/tests/protection.rs:1058-1080`.
- No 503 was returned in any case, including validation, duplicate, missing
  rule and unauthorized paths. Statuses observed: 200, 201, 303, 400, 403,
  404, 405, 409, 415, 422. The only 415 was a POST with no Content-Type and an
  empty body (axum `Form` rejection before the handler); a form-encoded POST
  without `csrf_token` is a 403 (cases 22/49).
- Enforcement used real HTTP Git: `main` force-push/deletion and normal push
  were refused with `ref.force_push`/`ref.delete`/`ref.push`; the same
  operations succeeded where the rule allowed them; an unsigned commit was
  refused and an SSH-signed update from a key registered to `t01` was
  accepted. The signed-commit gate only trusts committer addresses of the form
  `<username>@users.noreply.githearth.invalid`
  (`crates/githearth-server/src/git_http.rs:965-977`), so the positive case had
  to use that identity. Merge gates were isolated with approvals-only and
  checks-only rules: 403 before the fact, 201 after it, with the merge commit
  landing in the target branch.
- One git push under load returned `git requests busy` (429-class concurrency
  response, not a branch-protection result); it was re-run once and the
  expected refusal followed. No branch-protection page or form returned 503.
- Tag-protection parity (#135) holds: maintain can create/delete, write gets
  403, read gets 403 on forged posts, non-member gets 404, and the tag rules
  enforce create/update/delete roles.
- Test mistakes (corrected, not product failures): the first non-member POST
  used the empty CSRF from the 404 page (403 was the CSRF gate) and was re-run
  with a valid session CSRF → 404; an empty-body delete returned 415 and was
  re-run form-encoded → 403; two tag enforcement pushes initially targeted the
  wrong repository (`r3-bp`) and were re-run against `r3-bp-tags`; the first
  signed-commit positive attempt used `t01@example.com`, which the gate
  intentionally does not trust, and was re-run with the noreply committer
  address.
- Cleanup: `r3-bp-ui` has no remaining rules; `r3-bp-tags` retains only the
  `v*` rule; `r3-bp` keeps the seven rules used for enforcement
  (`main`, `release/*`, `sandbox`, `signedtest`, `signedok`, `approveonly`,
  `checkonly`) for reproduction. No other tester's data was touched.
