# Security — M4 closure round 3

Tester: t14. Date: 2026-11-14. Instance: main `800cad4` (PRs #161 and #162 merged).
Verdict: **PASS WITH DEFECTS**
Cases: 303 run, 299 pass, 4 fail, 0 skipped. The four failures are one
defect reproduced on four inputs (see Defects); every failure was re-run once
and reproduced identically.

## Scope

Adversarial sweep of the M4 closure build after the branch-protection settings
page and the shared-form authorization changes: tenant isolation and IDOR for
every repository-scoped resource (repo, branches UI, tags UI, pulls, issues,
statuses, forks, audit, protection APIs) against `beta`, `lab15`, `lab16`,
`lab18`, `lab01` and `acme`; uniform-404 and timing-oracle checks; the new
branch- and tag-protection forms (CSRF, forged POSTs, cross-repo actions,
idempotency, NUL, traversal, stored XSS); the role ladder (write, maintain,
admin, read, triage, base member, stranger); read-only PAT boundary over JSON
and Git; grant and token revocation; session/cookie flags; headers/CSP; login
enumeration, timing and burst; and SQL-shaped input.

All work was black-box over HTTP as `t14` (owner of `lab14`, org-level `read`
grant on `acme`), with `t15`, `bob`, `carol`, `dave`, `erin`, `frank`, `grace`,
`alice` and `t02` used for the authorization matrix. Source was read only for
expected behaviour and root causes; no repository files were changed.


## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| | **A. Tenant isolation and IDOR (t14 vs beta, lab15/16/18, lab01, acme, own lab14)** | | | |
| A01 | UI repo page beta/app (private, no access) | 404 | 404 | PASS |
| A02 | UI branch-protection page beta/app | 404 | 404 | PASS |
| A03 | UI tag-protection page beta/app | 404 | 404 | PASS |
| A04 | UI pulls page beta/app | 404 | 404 | PASS |
| A05 | UI issues page beta/app | 404 | 404 | PASS |
| A06 | API repo read beta/app | 404 | 404 | PASS |
| A07 | API protection list beta/app | 404 | 404 | PASS |
| A08 | API tag protection list beta/app | 404 | 404 | PASS |
| A09 | UI branch page lab15/x (org exists, no repo) | 404 | 404 | PASS |
| A10 | API repo lab15/x | 404 | 404 | PASS |
| A11 | API protection lab15/x | 404 | 404 | PASS |
| A12 | UI branch page lab16/x | 404 | 404 | PASS |
| A13 | UI branch page lab18/x | 404 | 404 | PASS |
| A14 | API repo lab18/x | 404 | 404 | PASS |
| A15 | UI branch page lab01/authcheck (private, no membership) | 404 | 404 | PASS |
| A16 | UI repo page acme/web (private, read grant) | 200 | 200 | PASS |
| A17 | UI branch page acme/web (read: no form) | 200 | 200 | PASS |
| A18 | API protection acme/web (read allowed) | 200 | 200 | PASS |
| A19 | UI branch page lab17/r3-api (public) | 200 | 200 | PASS |
| A20 | API repo lab17/r3-api (public) | 200 | 200 | PASS |
| A21 | UI repo page lab14/r3-sec-bp (own) | 200 | 200 | PASS |
| A22 | UI branch page lab14/r3-sec-bp (owner: form) | 200 | 200 | PASS |
| A23 | UI branch page lab14/nonexistent repo | 404 | 404 | PASS |
| A24 | UI branch page nonexistent org | 404 | 404 | PASS |
| A25 | API protection nonexistent repo | 404 | 404 | PASS |
| A26 | API protection nonexistent org | 404 | 404 | PASS |
| A27 | acme/web read member: page hides create form | no "Add a rule" | 200 — add_form=False | PASS |
| A28 | lab14 owner: page shows create form | has "Add a rule" | 200 — add_form=True | PASS |
| A29 | UI 404 bodies identical (private/no-repo/no-org) | all equal | 404 — sizes=[1611, 1611, 1611, 1611, 1611] | PASS |
| A30 | UI 404 does not reflect target slug/name | no beta/app/lab15 | 404 | PASS |
| A31 | API 404 bodies identical (private/no-repo/no-org) | all equal | 404 — sizes=[21, 21, 21, 21, 21] | PASS |
| A32 | API 404 message is only "not found" | {"error":"not found"} | 404 — {"error":"not found"} | PASS |
| A33 | t14 create rule on beta/app via UI form | 404 | 404 | PASS |
| A34 | beta/app rules unchanged after forged create | 0->0 | 404 | PASS |
| A35 | t14 delete random rule id on beta/app | 404 | 404 | PASS |
| A36 | t14 create rule on lab15/x via UI form | 404 | 404 | PASS |
| A37 | t14 (read grant) create rule on acme/web | 403 | 403 | PASS |
| A38 | acme/web has no rule created by t14 | 0 | 403 — count=0 | PASS |
| A39 | t14 create rule on nonexistent repo | 404 | 404 | PASS |
| | **B. Branch-protection settings UI: CSRF, validation, stored XSS, delete and idempotency** | | | |
| B01 | owner creates rule via UI form | 303 | 303 | PASS |
| B02 | created rule is listed | 200 and release/* | 200 | PASS |
| B03 | duplicate pattern rejected on page | 409 | 409 | PASS |
| B04 | duplicate created no second row | 1 | 409 — 1 | PASS |
| B05 | create without CSRF token | 403 | 403 | PASS |
| B06 | create with wrong CSRF token | 403 | 403 | PASS |
| B07 | create with another session CSRF token | 403 | 403 | PASS |
| B08 | other tenant (t15) own CSRF create on lab14 repo | 404 | 404 | PASS |
| B09 | t14 CSRF token with t15's session cookie | 403 | 403 | PASS |
| B10 | anonymous create (no cookie, no CSRF) | 303 | 303 | PASS |
| B11 | create with well-formed but unknown session | 303 | 303 | PASS |
| B12 | create without idempotency key | 400 | 400 | PASS |
| B13 | create with 129-character idempotency key | 400 | 400 | PASS |
| B14 | approvals 11 | 400 | 400 | PASS |
| B15 | approvals -1 | 400 | 400 | PASS |
| B16 | approvals non-numeric | 400 | 400 | PASS |
| B17 | empty pattern | 400 | 400 | PASS |
| B18 | pattern with .. segment | 400 | 400 | PASS |
| B19 | bare glob pattern | 400 | 400 | PASS |
| B20 | pattern with forbidden character | 400 | 400 | PASS |
| B21 | pattern containing a real NUL byte | 400 | 400 | PASS |
| B22 | status check containing a real NUL byte | 400 | 400 | PASS |
| B23 | UI trims a whitespace-padded status check | 303 | 303 | PASS |
| B24 | reserved legacy context | 422 | 422 | PASS |
| B25 | duplicate status checks | 422 | 422 | PASS |
| B26 | XSS payload as pattern | 400 | 400 | PASS |
| B27 | status check with script tag accepted as data | 303 | 303 | PASS |
| B28 | stored status-check XSS is HTML-escaped | escaped, no raw | 200 — raw=False esc=True | PASS |
| B29 | status check with img/onerror accepted as data | 303 | 303 | PASS |
| B30 | stored img/onerror XSS is escaped | escaped, no raw | 200 — raw=False | PASS |
| B31 | validation error page re-displays draft | 400 | 400 | PASS |
| B32 | validation error page escapes draft | no raw svg | 400 | PASS |
| B33 | status check with closing-tag payload accepted | 303 | 303 | PASS |
| B34 | closing-tag payload escaped in table | no raw </td><script> | 200 | PASS |
| B35 | delete without CSRF | 403 | 403 | PASS |
| B36 | rule survives CSRF-less delete | 1 | 403 | PASS |
| B37 | owner deletes rule | 303 | 303 | PASS |
| B38 | rule removed after delete | absent | 303 | PASS |
| B39 | delete already-deleted rule | 404 | 404 | PASS |
| B40 | delete with malformed rule id | 400 | 400 | PASS |
| B41 | same key + same body replays (303) | 303 | 303 | PASS |
| B42 | replay created one rule | 1 | 303 — 1 | PASS |
| B43 | same key + different body rejected | 422 | 422 | PASS |
| B44 | delete bp2 rule through bp path | 404 | 404 | PASS |
| B45 | bp2 rule intact after cross-repo delete | 1 | 404 | PASS |
| | **C. Cross-repo form authorization and role ladder on branch protection** | | | |
| C00 | seed rule as t14 owner | 201 | 201 | PASS |
| C01 | write member can view rules page | 200 | 200 — add_form=False | PASS |
| C02 | write member cannot create a rule | 403 | 403 | PASS |
| C03 | write member created nothing | absent | 403 | PASS |
| C04 | write member cannot delete a rule | 403 | 403 | PASS |
| C05 | rule survives write member delete | present | 403 | PASS |
| C06 | write member forged create in second repo | 403 | 403 | PASS |
| C07 | maintain member sees create form | 200 with form | 200 | PASS |
| C08 | maintain member can create a rule | 303 | 303 | PASS |
| C09 | maintain rule stored | present | 303 | PASS |
| C10 | maintain member can delete a rule | 303 | 303 | PASS |
| C11 | seed rule removed by maintain | absent | 303 | PASS |
| C12 | read member sees no create form | 200 no form | 200 | PASS |
| C13 | read member cannot create a rule | 403 | 403 | PASS |
| C14 | read member cannot delete a rule | 403 | 403 | PASS |
| C15 | rule survives read member delete | present | 403 | PASS |
| C16 | triage member sees no create form | 200 no form | 200 | PASS |
| C17 | triage member cannot create a rule | 403 | 403 | PASS |
| C18 | base member without repo grant gets 404 | 404 | 404 | PASS |
| C19 | base member cannot create a rule | 404 | 404 | PASS |
| C20 | admin member sees create form | 200 with form | 200 | PASS |
| C21 | admin member can create a rule | 303 | 303 | PASS |
| C22 | admin member can delete own rule | 303 | 303 | PASS |
| C23 | write member on public repo sees no form | 200 no form | 200 | PASS |
| C24 | write member cannot protect a public repo | 403 | 403 | PASS |
| C25 | acme write member cannot create protection | 403 | 403 | PASS |
| C26 | acme write member sees no form | 200 no form | 200 | PASS |
| C27 | acme maintain member can create protection | 303 | 303 | PASS |
| C28 | acme maintain member deletes own rule | 303 | 303 | PASS |
| C29 | acme owner can create protection | 303 | 303 | PASS |
| C30 | acme owner deletes own rule | 303 | 303 | PASS |
| C31 | t14 cleans up seeded rule | 303 | 303 | PASS |
| | **D. NUL bytes and path traversal on the new settings pages** | | | |
| D01 | NUL in repo path segment (UI) | 400-404 | 503 — 5xx | FAIL |
| D02 | NUL in org slug (UI) | 400-404 | 503 — 5xx | FAIL |
| D03 | NUL suffix in repo name (UI) | 400-404 | 503 — 5xx | FAIL |
| D04 | encoded ../ traversal to acme/web | 400-404 | 404 | PASS |
| D05 | dot-dot segments in repo path | 400-404 | 404 | PASS |
| D06 | encoded backslash in repo name | 400-404 | 404 | PASS |
| D07 | NUL in tag-protection path | 400-404 | 503 — 5xx | FAIL |
| D08 | over-long repo name (5000 chars) | 400-404-414 | 404 | PASS |
| D09 | NUL in rule id path (delete) | 400-404 | 400 | PASS |
| D10 | delete path with encoded slash id | 400-404 | 400 | PASS |
| D11 | NUL in API org slug | 400-404 | 400 | PASS |
| D12 | NUL in API repo name | 400-404 | 400 | PASS |
| D13 | API protection path traversal | 400-404 | 404 | PASS |
| D14 | API protection NUL repo | 400-404 | 400 | PASS |
| D15 | API create rule with NUL in pattern | 400 | 400 | PASS |
| D16 | API create rule with NUL in status check | 400 | 400 | PASS |
| D17 | API create repo with NUL in name | 400 | 400 | PASS |
| D18 | API create repo with NUL in description | 400 | 400 | PASS |
| D19 | control: plain API create succeeds | 201 | 201 | PASS |
| | **E. Read-only PAT boundary (JSON API and HTTP Git)** | | | |
| E01 | create repo:read PAT | 201 | 201 | PASS |
| E02 | create repo:write PAT | 201 | 201 | PASS |
| E03 | read PAT can list protection | 200 | 200 | PASS |
| E04 | read PAT cannot create protection rule | 403 | 403 | PASS |
| E05 | read-PAT write denied as read-only token | read-only token | 403 — {"error":"read-only token"} | PASS |
| E06 | read PAT cannot update repository | 403 | 403 | PASS |
| E07 | read PAT cannot create issue | 403 | 403 | PASS |
| E08 | read PAT cannot grant access | 403 | 403 | PASS |
| E09 | read PAT cannot delete protection rule | 403 | 403 | PASS |
| E10 | read PAT cannot manage tokens | 403 | 403 | PASS |
| E11 | read PAT cannot mint another token | 403 | 403 | PASS |
| E12 | write PAT can list protection | 200 | 200 | PASS |
| E13 | write PAT can create protection rule | 201 | 201 | PASS |
| E14 | write PAT can delete protection rule | 204 | 204 | PASS |
| E15 | read PAT can read over HTTP Git | 0 | - | PASS |
| E16 | read PAT cannot push over HTTP Git | nonzero | - — fatal: unable to access 'http://127.0.0.1:8080/lab14/r3-sec-grant.git/': The requested URL… | PASS |
| E17 | session credential is not blocked by read-only boundary | 201 | 201 | PASS |
| E18 | session credential can clean up | 204 | 204 | PASS |
| | **F. Grant revocation and access-management authorization** | | | |
| F01 | owner grants grace read on private repo | 201 | 201 | PASS |
| F02 | grantee (grace) can read repo | 200 | 200 | PASS |
| F03 | grantee sees protection page without form | 200 no form | 200 | PASS |
| F04 | grantee can list protection | 200 | 200 | PASS |
| F05 | owner can list access grants (subject id present) | 200 with t02 | 200 | PASS |
| F06 | owner revokes grant | 204 | 204 | PASS |
| F07 | revocation takes effect immediately (API) | 404 | 404 | PASS |
| F08 | revocation takes effect immediately (UI) | 404 | 404 | PASS |
| F09 | revocation blocks protection list | 404 | 404 | PASS |
| F10 | revoking an already-revoked grant | 404 | 404 | PASS |
| F11 | seed second grant on another repo | 201 | 201 | PASS |
| F12 | revoke other repo grant through this repo path | 404 | 404 | PASS |
| F13 | second grant still effective after cross-repo revoke | 200 | 200 | PASS |
| F14 | owner revokes second grant properly | 204 | 204 | PASS |
| F15 | write member cannot grant access | 403 | 403 | PASS |
| F16 | write member cannot list access grants | 403 | 403 | PASS |
| F17 | grantee cannot re-grant itself | 403 | 403 | PASS |
| F18 | invalid grant role rejected | 400 | 400 | PASS |
| F19 | account id as team subject rejected | 400 | 400 | PASS |
| F20 | grant with past expiry rejected | 400 | 400 | PASS |
| F21 | no expired grant row was created | 404 | 404 | PASS |
| | **G. Session and cookie handling** | | | |
| G01 | login page issues CSRF cookie | HttpOnly+SameSite=Lax | 200 — githearth_csrf=63cc115abec7771f978b8dd212dcd5c5; Path=/; HttpOnly; SameSite=Lax; Max-Age=3… | PASS |
| G02 | form login redirects and sets session | 303 | 303 | PASS |
| G03 | session cookie flags (HttpOnly, SameSite=Lax, Path, Max-Age) | all flags | 303 — githearth_session=<redacted>; Path=/… | PASS |
| G04 | Secure flag absent on plain-HTTP deployment (documented) | no Secure | 303 — secure_cookies=false | PASS |
| G05 | session token is 64 hex characters | 64 hex | 303 — ef267a0cdfc7b86c | PASS |
| G06 | successive logins issue distinct tokens | different | 303 | PASS |
| G07 | unknown 64-hex session cookie -> sign-in redirect | 303 | 303 — /login | PASS |
| G08 | malformed session cookie -> sign-in redirect | 303 | 303 | PASS |
| G09 | bearer header alone cannot load a UI page | 303 | 303 — /login | PASS |
| G10 | session token works as API bearer (documented) | 200 | 200 | PASS |
| G11 | session token in query string is ignored | 303 | 303 | PASS |
| G12 | logout without CSRF is refused | 403 | 403 | PASS |
| G13 | session still valid after refused logout | 200 | 200 | PASS |
| G14 | logout with CSRF succeeds | 303 | 303 | PASS |
| G15 | logged-out session no longer honoured | 303 | 303 | PASS |
| G16 | login overwrites a fixed session cookie | new value | 303 | PASS |
| | **H. Security headers and CSP** | | | |
| H01 | login page carries CSP/XFO/nosniff/referrer/no-store | all present | 200 — csp=default-src 'none'; style-src 'unsafe-inline'; img | PASS |
| H02 | authenticated branch page carries security headers | all present | 200 | PASS |
| H03 | 404 page carries security headers | all present | 404 | PASS |
| H04 | API responses carry nosniff and referrer-policy | present | 200 | PASS |
| H05 | CSP locks down scripts, framing and form targets | all directives | - — default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-a… | PASS |
| H06 | no permissive CORS headers on API | no ACAO | 401 | PASS |
| H07 | preflight does not grant cross-origin access | no ACAO | 405 | PASS |
| H08 | no version-revealing Server/X-Powered-By header | absent | 200 | PASS |
| | **I. Login enumeration, timing and burst** | | | |
| I01 | unknown user and wrong password return same status | 401/401 | 401 — 401/401 | PASS |
| I02 | unknown user and wrong password return identical body | equal | 401 — {"error":"invalid or expired credentials"} | PASS |
| I03 | login timing does not reveal account existence | median delta < 50ms | - — unknown=13.8ms wrongpw=24.0ms delta=10.2ms | PASS |
| I04 | SQL-shaped username rejected generically | 401 | 401 | PASS |
| I05 | SQL-shaped password rejected generically | 401 | 401 | PASS |
| I06 | empty credentials rejected | 400/401 | 401 | PASS |
| I07 | oversized username rejected without 5xx | 400/401/413 | 401 | PASS |
| I08 | oversized password rejected without 5xx | 400/401/413 | 401 | PASS |
| I09 | wrong JSON type rejected with 4xx | 400/422 | 422 | PASS |
| I10 | username is case-sensitive (no alias) | 401 | 401 | PASS |
| I11 | trailing space in username rejected | 401 | 401 | PASS |
| I12 | 20 parallel failed logins: no 5xx | all 401/429 | - — codes=[401] counts={401: 20} | PASS |
| I13 | busy limiter reports 429 rather than 500 under pressure | no 5xx | - | PASS |
| | **J. SQL-shaped input** | | | |
| J01 | SQL payload in branch pattern rejected | 400 | 400 | PASS |
| J02 | SQL payload in status context stored as data | 201 | 201 | PASS |
| J03 | no sleep/injection side effect (fast response) | <1s | 201 — 4ms | PASS |
| J04 | branch_protection_rules table intact | >0 | - — rows=57 | PASS |
| J05 | SQL payload in org slug -> 404 | 404 | 404 | PASS |
| J06 | SQL payload in repo name -> 404 | 404 | 404 | PASS |
| J07 | SQL payload in rule id -> 400 | 400 | 400 | PASS |
| J08 | SQL payload in grant role rejected | 400 | 400 | PASS |
| J09 | SQL payload in query parameter -> 400/404 | 400/404 | 400 | PASS |
| | **K. Burst/concurrency on the new pages** | | | |
| K01 | 40 parallel branch-page reads | all 200 | - — {200: 40} | PASS |
| K02 | 15 parallel identical creates: no 5xx | all <500 | - — {409: 14, 303: 1} | PASS |
| K03 | 15 parallel identical creates produce exactly one rule | 1 | - — rules=1 | PASS |
| K04 | 30 parallel protection-list reads | all 200 | - — {200: 30} | PASS |
| K05 | 20 parallel read-only PAT writes all denied | all 403 | - — {403: 20} | PASS |
| K06 | owner sees tag-protection form | 200 with form | 200 | PASS |
| K07 | t14 forged tag create on beta/app | 404 | 404 | PASS |
| K08 | t14 (read grant) forged tag create on acme/web | 403 | 403 | PASS |
| K09 | acme write member cannot create tag protection | 403 | 403 | PASS |
| K10 | lab14 write member cannot create tag protection | 403 | 403 | PASS |
| K11 | lab14 maintain member can create tag protection | 303 | 303 | PASS |
| K12 | maintain member deletes tag rule | 303 | 303 | PASS |
| K13 | no timing oracle: private vs nonexistent repo page | median delta < 30ms | - — private=9.3ms missing=6.6ms | PASS |
| | **L. API cross-role checks and token revocation** | | | |
| L01 | seed rule via API | 201 | 201 | PASS |
| L02 | write member cannot create rule via API | 403 | 403 | PASS |
| L03 | write member cannot delete rule via API | 403 | 403 | PASS |
| L04 | rule survives write-member API delete | present | - | PASS |
| L05 | read-only member cannot create rule via API | 403 | 403 | PASS |
| L06 | base member without repo grant gets 404 via API | 404 | 404 | PASS |
| L07 | delete rule id through another repo path (API) | 404 | 404 | PASS |
| L08 | rule intact after cross-repo API delete | present | - | PASS |
| L09 | create PAT for revocation test | 201 | 201 | PASS |
| L10 | PAT works before revocation | 200 | 200 | PASS |
| L11 | session revokes the PAT | 204 | 204 | PASS |
| L12 | revoked PAT is refused immediately | 401 | 401 | PASS |
| L13 | revoked PAT cannot read protection | 401 | 401 | PASS |
| L14 | PAT in the session cookie is not accepted | 303 | 303 | PASS |
| | **M. Login CSRF, uniform 404 and page hygiene** | | | |
| M01 | login form POST without CSRF | 403 | 403 | PASS |
| M02 | login form POST with wrong CSRF | 403 | 403 | PASS |
| M03 | login form POST with another session CSRF | 403 | 403 | PASS |
| M04 | login page ignores an external next parameter | 200 no redirect | 200 | PASS |
| M05 | successful login redirect stays on the instance | relative / | 303 — / | PASS |
| M06 | tag-page 404 is uniform for private/missing/no-org | identical bodies | 404 | PASS |
| M07 | page does not embed the raw session token | token absent | 200 | PASS |
| M08 | page embeds only the derived CSRF token | derived != session | 200 — 28429bafd9a11189 | PASS |
| M09 | CSRF token is stable within a session | same value | 200 | PASS |
| M10 | CSRF tokens differ between accounts | different | - | PASS |
| M11 | login page never reflects a password | no password field value | 200 | PASS |
| M12 | branch page has no inline event handlers | no on*= handlers | 200 | PASS |
| M13 | branch page lacks raw <script> elements | no <script | 200 | PASS |
| | **N. Broader API isolation (statuses, forks, issues, pulls, audit)** | | | |
| N01 | statuses list on beta/app | 404 | 404 | PASS |
| N02 | forks list on beta/app | 404 | 404 | PASS |
| N03 | issues list on beta/app | 404 | 404 | PASS |
| N04 | pulls list on beta/app | 404 | 404 | PASS |
| N05 | statuses on lab15/x | 404 | 404 | PASS |
| N06 | issues on lab15/x | 404 | 404 | PASS |
| N07 | audit on beta (no membership) | 404 | 404 | PASS |
| N08 | audit on lab15 (no membership) | 404 | 404 | PASS |
| N09 | audit on acme (read grant, no admin) | 403 | 403 | PASS |
| N10 | issue read on beta/app #1 | 404 | 404 | PASS |
| N11 | pull read on beta/app #1 | 404 | 404 | PASS |
| N12 | audit on nonexistent org -> 404, same body as beta | 404 + equal | 404 — 404 sizes=21/21 | PASS |
| N13 | write a status on beta/app | 404 | 404 | PASS |
| N14 | create an issue on beta/app | 404 | 404 | PASS |
| N15 | fork beta/app with a valid body | 404 | 404 | PASS |
| N16 | write to beta audit endpoint | 404/405 | 405 | PASS |
| N17 | anonymous private repo redirects uniformly | 303 all + same Location | 303 — /login | PASS |
| N18 | anonymous API 401 is uniform | 401 identical | 401 | PASS |
| | **O. Idempotency scoping across actors** | | | |
| O01 | t14 creates with a chosen idempotency key | 303 | 303 | PASS |
| O02 | carol reusing t14 key is not replayed | 303 | 303 | PASS |
| O03 | each actor got its own rule | both present | - | PASS |
| O04 | API key scoped per actor: t14 creates | 201 | 201 | PASS |
| O05 | API key scoped per actor: carol creates too | 201 | 201 | PASS |
| | **P. Credential parsing variants around the read-only boundary** | | | |
| P01 | lowercase bearer scheme with read PAT | 401/403 | 401 — {"error":"invalid or expired credentials | PASS |
| P02 | double space after Bearer | 401/403 | 401 — {"error":"invalid or expired credentials | PASS |
| P03 | tab instead of space after Bearer | 401/403 | 401 — {"error":"invalid or expired credentials | PASS |
| P04 | trailing space after read PAT | 401/403 | 403 — {"error":"read-only token"} | PASS |
| P05 | token without scheme | 401/403 | 401 — {"error":"invalid or expired credentials | PASS |
| P06 | read PAT in a non-standard header is ignored | 401 | 401 | PASS |
| P07 | control: session credential can create | 201 | 201 | PASS |
| | **Q. Tag-protection form: XSS, NUL, role injection** | | | |
| Q01 | tag pattern with script payload rejected | 400 | 400 | PASS |
| Q02 | tag pattern with NUL rejected | 400 | 400 | PASS |
| Q03 | tag rule with repository-invalid role rejected | 400/422 | 400 | PASS |
| Q04 | SQL-shaped tag role rejected | 400/422 | 400 | PASS |
| | **R. Regression smoke** | | | |
| R01 | health live is public | 200 | 200 | PASS |
| R02 | health ready is public | 200 | 200 | PASS |
| R03 | anonymous can browse a public repository page | 200 | 200 | PASS |
| R04 | anonymous private repository page redirects uniformly | 303 | 303 | PASS |
| R05 | anonymous settings page redirects to sign-in | 303 | 303 | PASS |
| R06 | authenticated API identity still works | 200 | 200 | PASS |
| R07 | unknown UI path returns styled 404 | 404 | 404 | PASS |
| R08 | unknown API path returns JSON 404 | 404 | 404 | PASS |
| R09 | organization list still scoped to the account | 200 only acme+lab14 | 200 | PASS |

## Defects

### Client-controlled NUL in a settings path is answered with 503, not 400/404
- Severity: **minor**
- Repro: with any valid browser session, request
  `GET /organizations/lab14/repositories/%00/settings/branches` (also
  `%00` in the org slug, a `%00` suffix on the repo name, and the same on
  `/settings/tags`). The API equivalent
  `GET /api/v1/organizations/lab14/repositories/%00` correctly returns 400.
- Expected: 400 `invalid input` (what the shared loader produces), or the
  uniform 404 used for every other unusable repository name. A malformed
  client path should never produce a 5xx.
- Actual: `503 Service Unavailable`, HTML page `<h1>Unavailable</h1>`.
  Reproduced twice, 60 s apart, for all four URL shapes; identical for every
  target, so it does not distinguish an existing private repository from a
  missing one.
- Evidence: status 503; body opens
  `<title>Unavailable</title> … <h1>Unavailable</h1>`. The shared loader rejects
  NUL as invalid input (`repository_api.rs:96-97` → `invalid()` 400,
  `lib.rs:464-466`), but the page handlers map every non-404 error from
  `list_rules` to `unavailable_page`:
  `branch_protection_ui.rs:196-199` and `tag_protection_ui.rs:146-149`;
  `ui.rs:259-272` renders that as 503.
- Impact: no authorization bypass, data exposure or existence oracle; it is a
  client-triggerable 5xx that pollutes error metrics/alerts and misclassifies
  bad input. Fix by mapping `BAD_REQUEST` (and other 4xx) in those two match
  arms to a 4xx page, leaving 503 for real store failures.

## Notes

- **Uniform 404 holds.** Five UI 404s (private-no-access, missing repo, missing
  org, and both tag/branch routes) are byte-identical (1611 bytes, no target
  name reflected); API 404s are the identical `{"error":"not found"}`.
  Anonymous requests for private and missing repos redirect identically to
  `/login` (303), and anonymous API requests return an identical 401. Median
  page latency for a private repo vs a missing repo differed by 0.0–2.7 ms
  across runs (one noisy run 9.3 vs 6.6 ms), with no usable oracle.
- **Role ladder is enforced by the shared API, not only by hiding forms.**
  `carol` (maintain), `dave` (admin) and `alice` (owner) can create/delete;
  `bob` (write), `erin` (read), `frank` (triage) get 403 on create and delete
  via UI and JSON; `grace` (base member with no repository grant) gets 404;
  `t14` on `acme/web` (read grant) gets 403; `t15` on `lab14` gets 404. The
  write role cannot mutate protection even with a valid CSRF token; rules
  survive the attempt. `repository.update` is in the `maintain` action group
  (`githearth.cedarschema:159`), so the `write` grant is insufficient.
- **CSRF and forged forms.** Missing, wrong, and another session's CSRF token
  give 403; no cookie/unknown session gives the sign-in redirect; login-form
  POSTs enforce their own CSRF token. The form action is derived from the
  request path, so repointing a saved form at another repo still passes
  through that repo's authorization. Login has no open redirect (`next` is
  ignored) and always redirects to `/`.
- **Stored XSS.** Required-status-check contexts accept HTML as data and both
  the list and the validation-error re-display escape it (`&lt;script&gt;`,
  `&lt;img`, `&lt;/td&gt;`); no raw `<script>` or `on*=` handler appears on any
  page. Branch/tag patterns are charset-restricted, so HTML there is rejected
  400. CSP `default-src 'none'` (no `script-src`) is a second layer.
- **Read-only PAT boundary.** A `repo:read` PAT can list protection and read
  Git, but is refused 403 `read-only token` on create/update/delete issues,
  grants, protection and token endpoints, and on `git push` (403). A
  `repo:write` PAT can create/delete. Lowercase/malformed schemes, extra
  spaces, wrong headers and a revoked PAT are all refused; a PAT is not
  accepted as a session cookie.
- **Revocation is immediate.** Revoking `grace`'s read grant (or a PAT) makes
  the next API and UI request 404/401; a grant ID revoked through a different
  repository path is a 404 no-op and does not revoke it; past `expires_at`
  grants are rejected at insert. Grant/revoke require admin: write members get
  403 and cannot even list grants.
- **Sessions and cookies.** Session cookie `HttpOnly; SameSite=Lax; Path=/;
  Max-Age=3600`; `Secure` is absent because this instance is deliberately
  plain HTTP (`secure_cookies` config exists for TLS). CSRF cookie is
  HttpOnly/SameSite too. Two logins get distinct 64-hex tokens; logout deletes
  the server-side session; a token in the query string is ignored; login does
  not adopt a planted session cookie. Note: the UI CSRF token is derived
  deterministically from the session token hash (`ui.rs:119-123`), so it is
  stable per session and not a random per-form nonce; with HttpOnly +
  SameSite=Lax + no script execution there is no forgery path, but it has no
  independent entropy.
- **Headers.** Every UI response, including 404s, carries
  `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline';
  img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`,
  `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: same-origin`, `Cache-Control: no-store`. API responses add
  nosniff/referrer-policy and no CORS `Access-Control-Allow-Origin`; no
  version-revealing server header.
- **Login.** Unknown user and wrong password return the same 401 body; median
  timing delta 1.7 ms over 15 samples each (dummy Argon2 hash is used for
  missing accounts). SQL- and NUL-shaped usernames/passwords return generic
  401; 10 k-char and 100 k-char credentials are handled without 5xx.
- **Burst.** 60 parallel failed logins all 401 in about 0.5 s, and 20 parallel
  failures produced no 429: there is no per-account/IP throttle. That matches
  the documented position (`docs/foundation.md:69`: per-account/IP throttling
  is a later production requirement), so it is a known limitation, not a
  regression. 40 parallel page reads and 30 parallel API lists were all 200;
  15 simultaneous identical creates with one idempotency key produced exactly
  one rule (6×303, 9×409) with no 5xx; 20 parallel read-only PAT writes were
  all 403.
- **SQL-shaped input.** Patterns fail the ASCII-segment charset (400); a
  `'; SELECT pg_sleep(2)--` status context is stored literally and answered in
  6 ms; table counts are unchanged; org/repo-slug payloads 404; UUID and role
  payloads 400. No 5xx and no timing side effect.
- **Test-data note.** New fixtures `lab14/r3-sec-{bp,bp2,xss,pub,grant}` were
  created with the `r3-` prefix and left in place for reuse. All branch- and
  tag-protection rules created during the sweep were deleted, all PATs minted
  for the sweep were revoked, and the test grants are revoked (rows retained
  as audit history). No repository was deleted and no other tester's data was
  modified.
- **Test-mistake corrections** (not product defects) during the sweep: fresh
  idempotency keys are required between runs (the 24 h key store replays the
  old response); `approvals` out of range is 400, not 422; a reused key with a
  different body is 422, not 409; the fork body requires `organization` and
  validates it before repository lookup; `x` is a legal 1-char idempotency
  key. All were re-run green.
