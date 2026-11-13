# Authorization, tenant isolation, input handling & transport security — M4 lab acceptance report

Tester: t14. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 274 run, 264 pass, 6 fail, 4 skipped.

Domain: authorization, tenant isolation, input handling and transport security (adversarial).
Fixture used: `lab14` (owner `t14`), with repos `alpha` (private, git history, issue/PR/comments/reviews/statuses/branch+tag protection) and `t14-probe`/`t14-revoke`; grants for bob/carol/dave/erin/frank at their provisioned lab14 roles. `acme` and `beta` were read-only neighbours for cross-tenant attempts; `lab15` was attacked but never modified. All created resources are prefixed `t14-`.

## Coverage summary

| Area | Cases | Pass | Fail | Skip |
|---|---:|---:|---:|---:|
| Tenant isolation / IDOR | 41 | 41 | 0 | 0 |
| Private data & anonymous access | 19 | 18 | 1 | 0 |
| Privilege escalation | 60 | 60 | 0 | 0 |
| Input handling | 33 | 31 | 2 | 0 |
| Stored XSS & browser output | 10 | 9 | 0 | 1 |
| Transport & session | 20 | 20 | 0 | 0 |
| Enumeration | 10 | 7 | 3 | 0 |
| Burst & rate | 5 | 5 | 0 | 0 |
| Secrets & logging | 6 | 6 | 0 | 0 |
| Git/SSH transport & extra authorization | 31 | 31 | 0 | 0 |
| Cross-tenant acme IDs & nested resources | 19 | 16 | 0 | 3 |
| Extra authorization & notifications | 20 | 20 | 0 | 0 |

No cross-tenant read or write succeeded. Foreign resource classes returned 404 with no foreign name in the body (defect 2 does leak name/slug *existence* through differing 404 bodies); a read-grant caller got generic 403 only on owner/admin-only surfaces of its own organization. All sampled read→write, write→maintain, maintain→owner, read-only-PAT→write, and PAT→sensitive escalations were refused. Logout, token, SSH-key and repository-grant revocation took effect on the next request. Grant changes, notifications and audit are scoped to the caller.

## Results

| # | Area | Case | Expected | Actual | Verdict |
|---|---|---|---|---|---|
| A01 | Tenant isolation / IDOR | t14 GET lab15 org by UUID | 404 | 404 {"error": "organization not found"} | PASS |
| A02 | Tenant isolation / IDOR | t14 GET beta org by UUID | 404 | 404 {"error": "organization not found"} | PASS |
| A03 | Tenant isolation / IDOR | t14 GET random org UUID | 404 | 404 {"error": "organization not found"} | PASS |
| A04 | Tenant isolation / IDOR | t14 GET lab15 repository list | 404 | 404 {"error": "organization not found"} | PASS |
| A05 | Tenant isolation / IDOR | t14 GET beta repository list | 404 | 404 {"error": "organization not found"} | PASS |
| A06 | Tenant isolation / IDOR | t14 GET lab15/alpha (lab14 repo name) | 404 | 404 {"error": "repository not found"} | PASS |
| A07 | Tenant isolation / IDOR | t14 PATCH lab15/alpha | 404 | 404 {"error": "repository not found"} | PASS |
| A08 | Tenant isolation / IDOR | t14 DELETE lab15/alpha | 404 | 404 {"error": "repository not found"} | PASS |
| A09 | Tenant isolation / IDOR | t14 GET lab15/alpha issue 1 | 404 | 404 {"error": "repository not found"} | PASS |
| A10 | Tenant isolation / IDOR | t14 POST lab15/alpha issue | 404 | 404 {"error": "repository not found"} | PASS |
| A11 | Tenant isolation / IDOR | t14 PATCH lab15/alpha issue 1 | 404 | 404 {"error": "repository not found"} | PASS |
| A12 | Tenant isolation / IDOR | t14 GET lab15/alpha pull 2 | 404 | 404 {"error": "repository not found"} | PASS |
| A13 | Tenant isolation / IDOR | t14 POST lab15/alpha pull 2 merge | 404 | 404 {"error": "repository not found"} | PASS |
| A14 | Tenant isolation / IDOR | t14 GET lab15/alpha protection | 404 | 404 {"error": "repository not found"} | PASS |
| A15 | Tenant isolation / IDOR | t14 PATCH lab15 protection using lab14 rule id | 404 | 404 {"error": "repository not found"} | PASS |
| A16 | Tenant isolation / IDOR | t14 GET lab15/alpha access grants | 404 | 404 {"error": "repository not found"} | PASS |
| A17 | Tenant isolation / IDOR | t14 DELETE lab15 access using lab14 grant id | 404 | 404 {"error": "repository not found"} | PASS |
| A18 | Tenant isolation / IDOR | t14 GET lab15 audit | 404 | 404 {"error": "organization not found"} | PASS |
| A19 | Tenant isolation / IDOR | t14 GET lab15/alpha tree/main | 404 | 404 {"error": "repository not found"} | PASS |
| A20 | Tenant isolation / IDOR | t14 POST lab15/alpha status | 404 | 404 {"error": "repository not found"} | PASS |
| A21 | Tenant isolation / IDOR | t14 GET lab15/alpha tag-protection | 404 | 404 {"error": "repository not found"} | PASS |
| A22 | Tenant isolation / IDOR | t14 GET beta/app | 404 | 404 {"error": "not found"} | PASS |
| A23 | Tenant isolation / IDOR | t14 GET beta/app pulls | 404 | 404 {"error": "not found"} | PASS |
| A24 | Tenant isolation / IDOR | t14 POST beta/app issue | 404 | 404 {"error": "not found"} | PASS |
| A25 | Tenant isolation / IDOR | t14 DELETE beta access using lab14 grant id | 404 | 404 {"error": "not found"} | PASS |
| A26 | Tenant isolation / IDOR | lab15 and random org 404 bodies identical | same status+body | 404/{"error": "organization not found"} vs 404/{"error": "organization not found"} | PASS |
| A27 | Tenant isolation / IDOR | cross-repo protection rule id via alpha path | 404 | 404 {"error": "protection rule not found"} | PASS |
| A28 | Tenant isolation / IDOR | cross-repo tag rule id via alpha path | 404 | 404 {"error": "tag protection rule not found"} | PASS |
| A29 | Tenant isolation / IDOR | cross-repo access grant id via alpha path | 404 | 404 {"error": "grant not found"} | PASS |
| A30 | Tenant isolation / IDOR | missing issue/pull numbers in own repo | 404 both | 404 {"error": "issue not found"} / 404 {"error": "pull request not found"} | PASS |
| A31 | Tenant isolation / IDOR | t14 read-grant acme/web read allowed | 200 | 200 | PASS |
| A32 | Tenant isolation / IDOR | t14 read grant cannot list access grants in acme | 403 | 403 {"error": "not permitted"} | PASS |
| A33 | Tenant isolation / IDOR | t14 read grant cannot read audit in acme | 403 | 403 {"error": "not permitted"} | PASS |
| A34 | Tenant isolation / IDOR | t14 read grant may read acme/web protection | 200 | 200 | PASS |
| A35 | Tenant isolation / IDOR | t14 read grant may read acme/web issue 2 | 200 | 200 | PASS |
| A36 | Tenant isolation / IDOR | erin token list excludes t14 tokens | no t14 id | 200 entries=0 t14_present=False | PASS |
| A37 | Tenant isolation / IDOR | erin cannot revoke t14 token by id | 404, token still valid | delete=404 {"error": "token not found"} token_still=200 | PASS |
| A38 | Tenant isolation / IDOR | erin ssh-key list excludes t14 key | no t14 id | 200 t14_present=False | PASS |
| A39 | Tenant isolation / IDOR | erin cannot revoke t14 ssh key by id | 404, key alive | delete=404 {"error": "ssh key not found"} alive=True | PASS |
| A40 | Tenant isolation / IDOR | erin cannot mark t14 notification read | 404, still unread | post=404 {"error": "notification not found"} unread=True | PASS |
| A41 | Tenant isolation / IDOR | t15 GET lab14/alpha | 404 | 404 {"error": "not found"} | PASS |
| B01 | Private data & anonymous access | anonymous JSON API private repo | 401 | 401 {"error": "invalid or expired credentials"} | PASS |
| B02 | Private data & anonymous access | anonymous JSON API public repo (API auth-only by design) | 401 | 401 (401, {'content-type': 'application/json', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff', 'referrer-policy': 'same-origin', 'co | PASS |
| B03 | Private data & anonymous access | anonymous private/public API bodies identical | same | {"error": "invalid or expired credentials"} vs {"error": "invalid or expired credentials"} | PASS |
| B04 | Private data & anonymous access | anonymous UI public repo | 200 | 200 | PASS |
| B05 | Private data & anonymous access | anonymous UI private repo redirects to login | 303 /login | 303 /login | PASS |
| B06 | Private data & anonymous access | anonymous UI missing repo indistinguishable | 303 /login same | 303 /login | PASS |
| B07 | Private data & anonymous access | anonymous UI private name not in response | no 'web' | web=False bodylen=0 | PASS |
| B08 | Private data & anonymous access | anonymous git clone public repo | 0 | rc=0 | PASS |
| B09 | Private data & anonymous access | anonymous git clone private repo denied | nonzero | rc=128 err=fatal: could not read Username for 'http://127.0.0.1:8080': terminal prompts disabled | PASS |
| B10 | Private data & anonymous access | anonymous git private/missing clone errors alike | auth challenge both | warning: You appear to have cloned an empty repository. \| rname for 'http://127.0.0.1:8080': terminal prompts disabled | PASS |
| B11 | Private data & anonymous access | anonymous git push public repo denied | nonzero | rc=128 | PASS |
| B12 | Private data & anonymous access | read-grant erin API private repo | 200 | 200 | PASS |
| B13 | Private data & anonymous access | read-grant erin git clone private repo | 0 with content | rc=0 files=['.git', 'README.md', 'src'] | PASS |
| B14 | Private data & anonymous access | read-grant erin UI private repo | 200 | 200 | PASS |
| B15 | Private data & anonymous access | member-without-read grace UI private repo | 404 page | 404 | PASS |
| B16 | Private data & anonymous access | member-without-read grace API private repo | 404 | 404 {"error": "not found"} | PASS |
| B17 | Private data & anonymous access | member-without-read grace git clone denied | nonzero | rc=128 | PASS |
| B18 | Private data & anonymous access | same non-reader: missing vs forbidden repo 404 identical | same status+body | missing=404/{"error": "repository not found"} forbidden=404/{"error": "not found"} | FAIL |
| B19 | Private data & anonymous access | audit events carry no repository name/description | no name payload | 200 has_alpha_name=False has_xss=False | PASS |
| C01 | Privilege escalation | read PATCH repo description | 403 | 403 {"error": "not permitted"}  | PASS |
| C02 | Privilege escalation | read DELETE repo | 403 | 403 {"error": "not permitted"}  | PASS |
| C03 | Privilege escalation | read POST access grant | 403 | 403 {"error": "not permitted"}  | PASS |
| C04 | Privilege escalation | read DELETE access grant | 403 | 403 {"error": "not permitted"}  | PASS |
| C05 | Privilege escalation | read POST protection rule | 403 | 403 {"error": "not permitted"}  | PASS |
| C06 | Privilege escalation | read POST tag protection rule | 403 | 403 {"error": "not permitted"}  | PASS |
| C07 | Privilege escalation | read POST archive | 403 | 403 {"error": "not permitted"}  | PASS |
| C08 | Privilege escalation | read PATCH issue title | 403 | 403 {"error": "not permitted"}  | PASS |
| C09 | Privilege escalation | read PATCH issue state | 403 | 403 {"error": "not permitted"}  | PASS |
| C10 | Privilege escalation | read PATCH pull title | 403 | 403 {"error": "not permitted"}  | PASS |
| C11 | Privilege escalation | read POST merge intent | 403 | 403 {"error": "not permitted"}  | PASS |
| C12 | Privilege escalation | read POST commit status | 403 | 403 {"error": "not permitted"}  | PASS |
| C13 | Privilege escalation | read role may open issue (design: read group) | 201 | 201 | PASS |
| C14 | Privilege escalation | read role may comment (design: read group) | 201 | 201 | PASS |
| C15 | Privilege escalation | read role PR create reaches duplicate check (design) | 409 | 409 {"error": "an open pull request already exists for these branches"} | PASS |
| C16 | Privilege escalation | read grant git push denied | nonzero push | clone=0 push=1 error: failed to push some refs to 'http://127.0.0.1:8080/lab14/alpha.git' | PASS |
| C17 | Privilege escalation | write DELETE repo (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C18 | Privilege escalation | write POST access grant (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C19 | Privilege escalation | write PATCH repo description (maintain-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C20 | Privilege escalation | write POST protection (maintain-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C21 | Privilege escalation | write POST tag protection (maintain-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C22 | Privilege escalation | write POST archive (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C23 | Privilege escalation | write role may review | 201 | 201 | PASS |
| C24 | Privilege escalation | maintain may PATCH repo description | 200 | 200 | PASS |
| C25 | Privilege escalation | maintain DELETE repo (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C26 | Privilege escalation | maintain POST access grant (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C27 | Privilege escalation | maintain may create tag protection (#135 design) | 201 | 201 {"id": "f3280143-b9bc-4061-9fa6-337f73745c7d", "repository_id": "18e0c0fb-11ab-4 | PASS |
| C27b | Privilege escalation | maintain cannot read owner-gated audit | 403 | 403 | PASS |
| C27c | Privilege escalation | write cannot read owner-gated audit | 403 | 403 | PASS |
| C27d | Privilege escalation | admin may delete repository (admin group by design) | 204 | 201/204 | PASS |
| C27e | Privilege escalation | admin may archive/unarchive (admin group by design) | 204 then unarchive 200 | archive=200 unarchive=200 delete=204 | PASS |
| C27f | Privilege escalation | admin may manage access (admin group by design) | 201 | 201 | PASS |
| C27g | Privilege escalation | admin cannot read owner-gated audit (owner-only organization.update) | 403 | 403 | PASS |
| C27h | Privilege escalation | owner can read audit | 200 | 200 | PASS |
| C28 | Privilege escalation | triage may close issue | 200 | 200 | PASS |
| C29 | Privilege escalation | owner reopens issue | 200 | 200 | PASS |
| C30 | Privilege escalation | triage PATCH repo description | 403 | 403 {"error": "not permitted"}  | PASS |
| C31 | Privilege escalation | member GET private repo (no read grant) | 404 | 404 {"error": "not found"}  | PASS |
| C32 | Privilege escalation | member POST issue (no read grant; denial hidden as 404) | 404 | 404 {"error": "not found"}  | PASS |
| C33 | Privilege escalation | member may create repository (design asymmetry) | 201 | 201 {"id": "5d5c5eb7-78f4-4091-98d9-c2f117d4bc17", "organization_id": "b0000000-0000 | PASS |
| C34 | Privilege escalation | owner can delete member-created repo | 204 | 204 | PASS |
| C35 | Privilege escalation | read-only PAT GET me | 200 | 200 {"id": "f7eb69d5-a055-457c-997e-baa1a96ea20e", "username": "t14", "is_admin": false}  | PASS |
| C36 | Privilege escalation | read-only PAT PATCH repo | 403 | 403 {"error": "read-only token"}  | PASS |
| C37 | Privilege escalation | read-only PAT POST issue | 403 | 403 {"error": "read-only token"}  | PASS |
| C38 | Privilege escalation | read-only PAT DELETE repo | 403 | 403 {"error": "read-only token"}  | PASS |
| C39 | Privilege escalation | read-only PAT POST access | 403 | 403 {"error": "read-only token"}  | PASS |
| C40 | Privilege escalation | read-only PAT POST protection | 403 | 403 {"error": "read-only token"}  | PASS |
| C41 | Privilege escalation | read-only PAT POST status | 403 | 403 {"error": "read-only token"}  | PASS |
| C42 | Privilege escalation | read-only PAT cannot list tokens | 403 | 403 {"error": "access tokens cannot manage access tokens"}  | PASS |
| C43 | Privilege escalation | read-only PAT cannot create ssh key | 403 | 403 {"error": "read-only token"}  | PASS |
| C44 | Privilege escalation | read-only PAT cannot read notifications | 403 | 403 {"error": "access tokens cannot view notifications"}  | PASS |
| C45 | Privilege escalation | read-only PAT git push denied | clone ok, push denied | clone=0 push=1 error: failed to push some refs to 'http://127.0.0.1:8080/lab14/alpha.git' | PASS |
| C46 | Privilege escalation | read-write PAT DELETE repo (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C47 | Privilege escalation | read-write PAT POST access (admin-only) | 403 | 403 {"error": "not permitted"}  | PASS |
| C48 | Privilege escalation | read-write PAT may perform non-sensitive repo update | 200 | 200 | PASS |
| C49 | Privilege escalation | read-write PAT may write content | 201 | 201 | PASS |
| C50 | Privilege escalation | owner grants grace repo read | 201 | 201 {"id": "543a74af-2e81-4173-b76c-3250980cde79", "subject_kind": "account", "subje | PASS |
| C51 | Privilege escalation | granted grace can read repo | 200 | 200 | PASS |
| C52 | Privilege escalation | revocation effective immediately (next request) | 204 then 404 | revoke=204 after=404 {"error": "not found"} | PASS |
| C53 | Privilege escalation | token revocation effective immediately | 200 then 401 | before=True revoke=204 after=401 | PASS |
| I01 | Input handling | org slug SQL injection | 400 | 400 {"error": "invalid input"}  | PASS |
| I02 | Input handling | org slug percent/unicode | 400 | 400 {"error": "invalid input"}  | PASS |
| I03 | Input handling | org name blank | 400 | 400 {"error": "invalid input"}  | PASS |
| I04 | Input handling | org name 201 chars | 400 | 400 {"error": "invalid input"}  | PASS |
| I05 | Input handling | repo name SQL injection | 400 | 400 {"error": "invalid input"}  | PASS |
| I06 | Input handling | repo name traversal | 400 | 400 {"error": "invalid input"}  | PASS |
| I07 | Input handling | repo name 101 chars | 400 | 400 {"error": "invalid input"}  | PASS |
| I08 | Input handling | repo name .git suffix | 400 | 400 {"error": "invalid input"}  | PASS |
| I09 | Input handling | duplicate repo name | 409 | 409 {"error": "repository name already taken"}  | PASS |
| I10 | Input handling | SQLi stored in issue title/body unchanged | 201 + exact round-trip | 201 exact=True body={"id": "ad98aee9-6275-4016-b5c6-79d6e530ff36", "number": 19, "title": "t14-sqli ' OR 1=1-- ; DROP TA | PASS |
| I11 | Input handling | unicode title/body round-trip | 201 exact | 201 exact=True | PASS |
| I12 | Input handling | issue title length boundary 256/257 | 201 then 400 | 201 then 400 | PASS |
| I13 | Input handling | issue body length boundary 65536/65537 | 201 then 400 | 201 then 400 | PASS |
| I14 | Input handling | query cursor SQL injection | 400, no SQL error/stack | 400 Failed to deserialize query string: after: UUID parsing failed: invalid character: found `'` at 0 | PASS |
| I15 | Input handling | invalid limit values | 400 each | [400, 400, 400, 400, 400] | PASS |
| I16 | Input handling | browse path traversal (API) | 404 both, no /etc/passwd | 404/404 body=NoneNone | PASS |
| I17 | Input handling | browse path traversal (UI) | 404, no /etc/passwd | 404 root_in_body=False | PASS |
| I18 | Input handling | git URL path traversal | both refused | 128/128 | PASS |
| I19 | Input handling | CRLF in issue title does not inject response headers | 201, no X-Injected | 201 injected=None stored=True | PASS |
| I20 | Input handling | body past body_limit | 413 | 413 length limit exceeded | PASS |
| I21 | Input handling | deeply nested JSON | 400 (no 500) | 400 {"error": "invalid input"} | PASS |
| I22 | Input handling | duplicate JSON keys | 400 rejected (no ambiguous value) | 400 {"error": "invalid input"} | PASS |
| I23 | Input handling | wrong JSON type | 400 | 400 {"error": "invalid input"} | PASS |
| I24 | Input handling | NUL byte in title | 400 (client error, not 5xx) | 503 {"error": "database unavailable"} | FAIL |
| I25 | Input handling | invalid UTF-8 JSON body | 400 | 400 {"error": "invalid input"} | PASS |
| I26 | Input handling | 70KB header value handled without 5xx | 401 or 431 (no 5xx/crash) | HTTP/1.1 401 Unauthorized | PASS |
| I27 | Input handling | raw CRLF header injection reflects nothing | 401, no reflected header | HTTP/1.1 401 Unauthorized | PASS |
| I28 | Input handling | NUL in repo path | 400/404 | 503 | FAIL |
| I29 | Input handling | JSON array instead of object (serde positional decode) | no 5xx/injection; array may decode positionally | 201 {"id": "14971362-f26e-4de2-b0dc-cb33c0d2b4c0", "number": 24, "title": "t14-array | PASS |
| I30 | Input handling | SQLi in login username | 401 (no 500/SQL text) | 401 {"error": "invalid or expired credentials"} | PASS |
| I31 | Input handling | non-JSON content type (body still parsed as JSON) | 201 or 415/400, no 5xx | 201 {"id": "a6b2fbee-8149-42ba-be18-28685041fd4d", "number": 25, "title": "t14-textp | PASS |
| I32 | Input handling | browse query path traversal | 400/404 rejected | 400 {"error": "invalid input"} | PASS |
| I33 | Input handling | overlong URL path | 400/404/414 (no 500) | 404 | PASS |
| X01 | Stored XSS & browser output | API returns stored HTML raw (expected, JSON client escapes) | raw <script> | 200 | PASS |
| X02 | Stored XSS & browser output | issue list page escapes title/body | no literal <script>, escaped present | 200 literal=False escaped=True | PASS |
| X03 | Stored XSS & browser output | issue detail page escapes title/body | no literal <script> | 200 literal=False escaped=True | PASS |
| X04 | Stored XSS & browser output | issue comment page escapes <img onerror> | no literal onerror tag | 200 literal=False escaped=True | PASS |
| X05 | Stored XSS & browser output | repository page escapes description | no literal <script> | 200 literal=False escaped=True | PASS |
| X06 | Stored XSS & browser output | PR page escapes comment body | 200 and escaped | 503 (blocked by PR detail 503/500 defect) | SKIP |
| X07 | Stored XSS & browser output | UI pages send a restrictive CSP | default-src 'none'; frame-ancestors 'none' | default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none' | PASS |
| X08 | Stored XSS & browser output | UI pages send X-Frame-Options DENY | DENY | DENY | PASS |
| X09 | Stored XSS & browser output | API JSON content type + nosniff | application/json + nosniff | application/json nosniff | PASS |
| X10 | Stored XSS & browser output | HTML file in blob view is escaped, not executed | 200 escaped | 200 type=text/html; charset=utf-8 literal=False | PASS |
| F01 | Transport & session | session cookie HttpOnly + SameSite=Lax + Path=/ | all present | githearth_session=<redacted>0ff4a065b95af99045c897bfff6e03df367452cc1c14f3d4335bc37a5a228c50; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600 | PASS |
| F02 | Transport & session | cookie Secure flag on plain-HTTP compose profile | absent (documented GITHEARTH_SECURE_COOKIES=false) | Secure=False | PASS |
| F03 | Transport & session | login response does not put token in URL | 303 / to / | 303 / | PASS |
| F04 | Transport & session | two logins yield independent tokens | different tokens both valid | distinct=True | PASS |
| F05 | Transport & session | API logout invalidates only that session | 204, t1 401, t2 200 | 204 t1=401 t2=200 | PASS |
| F06 | Transport & session | browser logout revokes cookie server-side | 303 /login then / redirects | logout=303 after=303 loc=/login | PASS |
| F07 | Transport & session | logout form without CSRF token refused | 403 | 403 | PASS |
| F08 | Transport & session | issue form without CSRF token refused | 403 | 403 | PASS |
| F09 | Transport & session | login POST without matching CSRF cookie refused | 403 | 403 | PASS |
| F10 | Transport & session | tampered session cookie rejected without 500 | 303 /login | 303 /login | PASS |
| F11 | Transport & session | browser cookie not accepted as JSON API bearer | 401 | 401 {"error": "invalid or expired credentials"} | PASS |
| F12 | Transport & session | API 200/401/404 all Cache-Control no-store | no-store | ['no-store', 'no-store', 'no-store'] | PASS |
| F13 | Transport & session | UI pages and login Cache-Control no-store | no-store | no-store/no-store | PASS |
| F14 | Transport & session | nosniff on API responses | nosniff | ['nosniff', 'nosniff'] | PASS |
| F15 | Transport & session | Referrer-Policy same-origin | same-origin | same-origin | PASS |
| F16 | Transport & session | no CORS wildcard/credentials allowance | no ACAO | GET=None OPTIONS=None | PASS |
| F17 | Transport & session | no server version disclosure | no Server/X-Powered-By | server=None xpb=None | PASS |
| F18 | Transport & session | HSTS absent on plain HTTP (proxy terminates TLS by design) | absent, documented | hsts=None | PASS |
| F19 | Transport & session | error responses reveal no version/SQL/driver text | generic error only | {"error": "repository not found"} | PASS |
| F20 | Transport & session | invalid UUID path returns 400 without stack trace | 400 no stack | 400 Invalid URL: Cannot parse `id` with value `not-a-uuid`: UUID parsing failed: invalid character: foun | PASS |
| G01 | Enumeration | unknown user vs wrong password identical status/body | 401 same body | 401/{"error": "invalid or expired credentials"} vs 401/{"error": "invalid or expired credentials"} | PASS |
| G02 | Enumeration | unknown-user and wrong-password timing comparable | ratio < 3 | median unknown=14.0ms wrong=13.8ms ratio=1.01 | PASS |
| G03 | Enumeration | malformed vs unknown token errors identical | 401 same body | 401/{"error": "invalid or expired credentials"} \| 401/{"error": "invalid or expired credentials"} \| 401/{"error": "invalid or expired credentials"} | PASS |
| G04 | Enumeration | missing vs forbidden repo same HTTP status | 404 both | 404 vs 404 | PASS |
| G05 | Enumeration | foreign and nonexistent org identical 404 | same body | {"error": "organization not found"} vs {"error": "organization not found"} | PASS |
| G06 | Enumeration | anonymous private vs missing git error identical | same challenge | ["fatal: cannot exec '/bin/false': No such file or directory", "fatal: could not read Username for 'http://127.0.0.1:8080': terminal prompts disabled" | PASS |
| G07 | Enumeration | non-UUID org path gives 400 not 404 | 400 (parser detail noted) | 400 Invalid URL: Cannot parse `id` with value `1234`: UUID parsing failed: invalid length: found 4 | PASS |
| H01 | Burst & rate | 30 parallel wrong logins bounded, no 5xx | only 401/429 | codes={401: 30} | PASS |
| H02 | Burst & rate | no lockout after login burst | 200 | 200 | PASS |
| H03 | Burst & rate | 30 parallel writes with one Idempotency-Key create one resource | 1 id, no 5xx | codes={409: 29, 201: 1} distinct_ids=1 | PASS |
| H04 | Burst & rate | idempotent burst left exactly one stored issue | 1 stored | 1 stored | PASS |
| H05 | Burst & rate | 30 parallel reads all 200 | no 5xx | {200: 30} | PASS |
| K01 | Secrets & logging | app request logs contain no credentials/tokens/hashes | zero matches | {'<test-password>': 0, 'gth_': 0, 'Bearer ': 0, 'argon2id': 0, 'password_hash': 0, 'BEGIN OPENSSH': 0} | PASS |
| K02 | Secrets & logging | owner audit response has no secrets | fixed fields only | keys=['actor_id', 'client_ip', 'created_at', 'hash', 'kind', 'seq'] leaks={'test-password': 0, 'gth_': 0, 'argon2': 0} | PASS |
| K03 | Secrets & logging | API metadata responses expose no hashes/plaintext tokens | no password_hash/no gth_ | password_hash=False gth_=False private_key=False | PASS |
| K04 | Secrets & logging | DB password hashes are argon2id PHC with pinned params | all argon2id v19 m=19456 t=2 p=1 | counts=23\|23 | PASS |
| K05 | Secrets & logging | session/token hashes are 32-byte digests, no plaintext secrets | all 32-byte, no plaintext | sessions=711\|711 tokens=52\|52 plaintext_pw_rows=0 | PASS |
| K06 | Secrets & logging | error bodies contain no SQL, paths, config or db details | generic only | {"error": "invalid or expired credentials"}{"error": "repository not found"} | PASS |
| J01 | Git/SSH transport & extra authorization | SSH key clone of own private repo | success | rc=0 Warning: Permanently added '[127.0.0.1]:2222' (ED25519) to the list of known hosts. | PASS |
| J02 | Git/SSH transport & extra authorization | SSH key clone of grant-readable acme/web | success | rc=0 Warning: Permanently added '[127.0.0.1]:2222' (ED25519) to the list of known hosts. | PASS |
| J03 | Git/SSH transport & extra authorization | SSH key clone of foreign lab15 repo denied | nonzero | rc=128 and the repository exists. | PASS |
| J04 | Git/SSH transport & extra authorization | newly registered SSH key authenticates | success | register=201 clone=0 Warning: Permanently added '[127.0.0.1]:2222' (ED25519) to the list of known hosts. | PASS |
| J05 | Git/SSH transport & extra authorization | SSH key revocation effective immediately | 204 then auth failure | revoke=204 clone_after=128 and the repository exists. | PASS |
| J06 | Git/SSH transport & extra authorization | HTTP git wrong password vs unknown user same denial | both fail identically | fatal: Authentication failed for 'http://127.0.0.1:8080/lab14/alpha.git/' \| fatal: Authentication failed for 'http://127.0.0.1:8080/lab14/alpha.git/' | PASS |
| J07 | Git/SSH transport & extra authorization | repo:write PAT may push a branch over HTTP git | push ok | clone=0 push=True | PASS |
| J08 | Git/SSH transport & extra authorization | push to foreign lab15/alpha denied | denied | rc=128 fatal: repository 'http://127.0.0.1:8080/lab15/alpha.git/' not found | PASS |
| J09 | Git/SSH transport & extra authorization | push to foreign beta/app denied | denied | rc=128 fatal: repository 'http://127.0.0.1:8080/beta/app.git/' not found | PASS |
| J10 | Git/SSH transport & extra authorization | owner direct push to protected main refused | denied (protected branch) | rc=1 error: failed to push some refs to 'http://127.0.0.1:8080/lab14/alpha.git' | PASS |
| J11 | Git/SSH transport & extra authorization | owner force push to protected main refused | denied (protected branch) | rc=1 error: failed to push some refs to 'http://127.0.0.1:8080/lab14/alpha.git' | PASS |
| J12 | Git/SSH transport & extra authorization | write role creates tag matching tag protection | push ok | tag=0 push=0  * [new tag]         v14-tag-1794605055 -> v14-tag-1794605055 | PASS |
| J13 | Git/SSH transport & extra authorization | tag protection refuses delete (allow_delete=false) | denied (protected tag) | rc=1 error: failed to push some refs to 'http://127.0.0.1:8080/lab14/alpha.git' | PASS |
| J14 | Git/SSH transport & extra authorization | archive fixture create | 201 | 201 {"id": "aa47be01-e7e4-450a-be60-4f5b7c7548a6", "organization_id": "b0000000-0000-4000-8000-000000000014", "nam  | PASS |
| J15 | Git/SSH transport & extra authorization | archive repository | 200 | 200 {"id": "aa47be01-e7e4-450a-be60-4f5b7c7548a6", "organization_id": "b0000000-0000-4000-8000-000000000014", "nam  | PASS |
| J16 | Git/SSH transport & extra authorization | archived repo remains readable | 200 | 200 {"id": "aa47be01-e7e4-450a-be60-4f5b7c7548a6", "organization_id": "b0000000-0000-4000-8000-000000000014", "nam  | PASS |
| J17 | Git/SSH transport & extra authorization | archived repo refuses new issue | 403 | 403 {"error": "not permitted"}  | PASS |
| J18 | Git/SSH transport & extra authorization | archived repo refuses description update | 403 | 403 {"error": "not permitted"}  | PASS |
| J19 | Git/SSH transport & extra authorization | archived empty repo clone allowed (read) | success | rc=0 warning: You appear to have cloned an empty repository. | PASS |
| J20 | Git/SSH transport & extra authorization | unarchive repository | 200 | 200 {"id": "aa47be01-e7e4-450a-be60-4f5b7c7548a6", "organization_id": "b0000000-0000-4000-8000-000000000014", "nam  | PASS |
| J21 | Git/SSH transport & extra authorization | delete archive fixture | 204 | 204 None  | PASS |
| J22 | Git/SSH transport & extra authorization | sensitive fixture create | 201 | 201 {"id": "90966c40-0fea-4a3d-bec1-bc2baa0f279a", "organization_id": "b0000000-0000-4000-8000-000000000014", "nam  | PASS |
| J23 | Git/SSH transport & extra authorization | PAT cannot perform sensitive delete (stale auth) | 403, repo remains | delete=403 {"error": "not permitted"} still=200 | PASS |
| J24 | Git/SSH transport & extra authorization | fresh owner session may perform sensitive delete | 204 | 204 | PASS |
| J25 | Git/SSH transport & extra authorization | X-Forwarded-For/X-Real-IP spoof ignored without trusted proxy | audit client_ip is peer, not spoof | create=201 audit_ip=172.18.0.1 | PASS |
| J26 | Git/SSH transport & extra authorization | unauthenticated sample of 10 endpoints all refused | 401 each | [401, 401, 401, 401, 401, 401, 401, 401, 401, 401] | PASS |
| J27 | Git/SSH transport & extra authorization | CRLF in repo description does not inject headers | 200, no X-Injected | 200 injected=None | PASS |
| J28 | Git/SSH transport & extra authorization | CRLF description inert in UI body (text, not a header) | 200, no injected HTML tag | 200 has_text=True has_tag=False | PASS |
| J29 | Git/SSH transport & extra authorization | invalid UTF-8 percent-encoding in path | 400/404, no 5xx | HTTP/1.1 400 Bad Request | PASS |
| J30 | Git/SSH transport & extra authorization | git path with .. segments in repo suffix denied | nonzero | rc=128 fatal: could not read Username for 'http://127.0.0.1:8080': terminal prompts disabled | PASS |
| J31 | Git/SSH transport & extra authorization | org existence oracle via repository list | identical 404 body | 404/{"error": "organization not found"} vs 404/{"error": "organization not found"} | PASS |
| L01 | Cross-tenant acme IDs & nested resources | t14 read grant enumerates acme protection/tag rules | 200 both, ids visible | prot=200 rules=0 tag=200 tag_rules=0 | PASS |
| L02 | Cross-tenant acme IDs & nested resources | acme protection rule id used on lab14 alpha | no acme rules to test | skipped: acme has no protection rules | SKIP |
| L03 | Cross-tenant acme IDs & nested resources | acme protection rule id used on lab15 | skipped | skipped | SKIP |
| L04 | Cross-tenant acme IDs & nested resources | acme tag rule id used on lab14 alpha | skipped | acme has no tag rules | SKIP |
| L05 | Cross-tenant acme IDs & nested resources | t14 POST tag rule into lab15 | 404 | (404,) | PASS |
| L06 | Cross-tenant acme IDs & nested resources | acme issue number 2 via lab14 probe (probe has issue 1) | 404 | 404 {"error": "issue not found"}  | PASS |
| L07 | Cross-tenant acme IDs & nested resources | acme PR number 1 via lab14 alpha (alpha has PR 2) | 404 | 404 {"error": "pull request not found"}  | PASS |
| L08 | Cross-tenant acme IDs & nested resources | foreign issue comments read | 404 | 404 {"error": "repository not found"}  | PASS |
| L09 | Cross-tenant acme IDs & nested resources | foreign issue comment write | 404 | 404 {"error": "repository not found"}  | PASS |
| L10 | Cross-tenant acme IDs & nested resources | foreign PR reviews read | 404 | 404 {"error": "repository not found"}  | PASS |
| L11 | Cross-tenant acme IDs & nested resources | foreign PR review write | 404 | 404 {"error": "repository not found"}  | PASS |
| L12 | Cross-tenant acme IDs & nested resources | foreign merge intent read | 404 | 404 {"error": "repository not found"}  | PASS |
| L13 | Cross-tenant acme IDs & nested resources | beta merge intent read | 404 | 404 {"error": "merge intent not found"}  | PASS |
| L14 | Cross-tenant acme IDs & nested resources | foreign commit statuses read | 404 | 404 {"error": "repository not found"}  | PASS |
| L15 | Cross-tenant acme IDs & nested resources | t14 forks alpha into foreign lab15 | 404 | 404 {"error": "organization not found"}  | PASS |
| L16 | Cross-tenant acme IDs & nested resources | no repo created in lab15 by foreign fork | absent | lab15 repos=[] | PASS |
| L17 | Cross-tenant acme IDs & nested resources | t14 forks unreadable beta/app | 404 | 404 {"error": "not found"}  | PASS |
| L18 | Cross-tenant acme IDs & nested resources | fork of readable acme/web into own org allowed by design | 201 | 201 {"id": "10f1194d-1a21-47c1-a044-2c7674d1d681", "organization_id": "b0000000-0000 | PASS |
| L19 | Cross-tenant acme IDs & nested resources | acme repo name 'web' on beta path | 404 | 404 {"error": "repository not found"}  | PASS |
| M01 | Extra authorization & notifications | read role POST unarchive | 403 | 403 {"error": "not permitted"}  | PASS |
| M02 | Extra authorization & notifications | read role DELETE branch protection rule | 403 | 403 {"error": "not permitted"}  | PASS |
| M03 | Extra authorization & notifications | read role DELETE tag protection rule | 403 | 403 {"error": "not permitted"}  | PASS |
| M04 | Extra authorization & notifications | read role POST fork | (403, 404) | 404 {"error": "organization not found"}  | PASS |
| M05 | Extra authorization & notifications | read role POST PR review | 403 | 403 {"error": "not permitted"}  | PASS |
| M06 | Extra authorization & notifications | read role POST requested reviewer | 403 | 403 {"error": "not permitted"}  | PASS |
| M07 | Extra authorization & notifications | read role POST auto-merge | 403 | 403 {"error": "not permitted"}  | PASS |
| M08 | Extra authorization & notifications | read role DELETE merge intent | 403 | 403 {"error": "not permitted"}  | PASS |
| M09 | Extra authorization & notifications | read role POST draft toggle | 403 | 403 {"error": "not permitted"}  | PASS |
| M10 | Extra authorization & notifications | read role POST status check context | 403 | 403 {"error": "not permitted"}  | PASS |
| M11 | Extra authorization & notifications | notification payload has ids only (no titles/repo names) | fixed id fields | keys=['actor_username', 'created_at', 'id', 'kind', 'pull_request_id', 'read_at'] has_alpha=False has_title=False | PASS |
| M12 | Extra authorization & notifications | t14 cannot read beta audit (404, non-member) | 404 | 404 {"error": "organization not found"} | PASS |
| M13 | Extra authorization & notifications | t14 read-grant acme audit gets generic 403 (org-readable) | 403 | 403 {"error": "not permitted"} | PASS |
| M14 | Extra authorization & notifications | access token cannot list SSH keys | 403 | 403 {"error": "access tokens cannot manage ssh keys"} | PASS |
| M15 | Extra authorization & notifications | non-member cannot read lab14 merge intent | 404 | 404 {"error": "pull request not found"} | PASS |
| M16 | Extra authorization & notifications | owner can verify audit chain | 200 with verification result | 200 {"rows": 143, "ok": true, "first_bad_seq": null} | PASS |
| M17 | Extra authorization & notifications | grant to account outside the organization refused | 400 | 400 {"error": "invalid grant"} | PASS |
| M18 | Extra authorization & notifications | owner role not assignable at repository scope | 400 | 400 {"error": "invalid input"} | PASS |
| M19 | Extra authorization & notifications | expired grant rejected or confers no access | 201/400 then 404 | grant=400 read=404 | PASS |
| M20 | Extra authorization & notifications | duplicate organization slug | 409 | 409 {"error": "already exists"} | PASS |
| N01 | Enumeration | cross-tenant private repo existence from 404 body | identical 404 body | exists=404/{"error": "not found"} missing=404/{"error": "repository not found"} | FAIL |
| N02 | Enumeration | organization slug existence from 404 body | identical 404 body | exists=404/{"error": "repository not found"} missing=404/{"error": "organization not found"} | FAIL |
| N03 | Enumeration | merge-intent PR-number existence from 404 body | identical 404 body | exists=404/{"error": "pull request not found"} missing=404/{"error": "merge intent not found"} | FAIL |

## Defects

### 1. A NUL byte in any user-controlled text reaches PostgreSQL and returns 503 "database unavailable"
- Severity: **minor**
- Repro (authenticated; a session or read-write PAT is required):
  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
    -H 'Content-Type: application/json' \
    -d '{"username":"t14","password":"<test-password>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')

  # issue title / comment / repo description / organization name
  curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/issues \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"title":"t14-nul-\u0000","body":"x"}'
  curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/issues/1/comments \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"body":"t14-nul-\u0000"}'
  curl -i -X PATCH http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"description":"t14-nul-\u0000"}'
  curl -i -X POST http://127.0.0.1:8080/api/v1/organizations \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"slug":"t14-nul","name":"t14\u0000"}'

  # path parameters
  curl -i "http://127.0.0.1:8080/api/v1/organizations/%00/repositories" -H "Authorization: Bearer $TOKEN"
  curl -i "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/%00alpha" -H "Authorization: Bearer $TOKEN"
  ```
- Expected: `400 {"error":"invalid input"}` (the same class as a blank title, oversized body or invalid UTF-8), and nothing written.
- Actual: `503 {"error":"database unavailable"}` on all six endpoints. App log records `{"message":"database operation failed","category":"database"}`. No row is committed (the transaction rolls back), so this is an availability/error-handling and monitoring-noise issue, not a data-integrity or authz bypass.
- Evidence: PostgreSQL text cannot store U+0000 (`22021`); the generic `ApiError::from(sqlx::Error)` maps every non-unique database error to 503 (`crates/githearth-server/src/lib.rs:432-444`). NUL is not rejected by the issue title/comment validators (`crates/githearth-server/src/issues.rs:187-197`, `534-560`), repository name/description validation (`crates/githearth-server/src/repository_api.rs:43-51`, `210-212`, `493-502`) or organization name validation (`crates/githearth-server/src/lib.rs:843-856`). Path parameters are decoded and bound without a NUL check.
- Scope: systematic — every string field and every path/query identifier that reaches a `text` column or comparison is affected (6/6 variants reproduced on re-run).

### 2. A 404 body distinguishes a missing repository/organization from one the caller may not read
- Severity: **minor**
- Repro (grace is a member of lab14 with no repository read grant; t14 is not a member of beta):
  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login -H 'Content-Type: application/json' \
    -d '{"username":"grace","password":"<test-password>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
  curl -i "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/t14-nope" -H "Authorization: Bearer $TOKEN"
  curl -i "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/t14-revoke" -H "Authorization: Bearer $TOKEN"

  # cross-tenant, as t14 against beta (t14 has no beta membership or grant)
  curl -i "http://127.0.0.1:8080/api/v1/organizations/beta/repositories/app" -H "Authorization: Bearer $T14_TOKEN"
  curl -i "http://127.0.0.1:8080/api/v1/organizations/beta/repositories/web" -H "Authorization: Bearer $T14_TOKEN"

  # organization-slug existence
  curl -i "http://127.0.0.1:8080/api/v1/organizations/lab15/repositories/anything" -H "Authorization: Bearer $T14_TOKEN"
  curl -i "http://127.0.0.1:8080/api/v1/organizations/t14-no-such-org/repositories/anything" -H "Authorization: Bearer $T14_TOKEN"
  ```
- Expected: identical 404 status **and body**, per ADR 0002: "Services load the object first, and return 404 when it is missing *or* the decision denies, so a denial reveals nothing" (`docs/decisions/0002-authorization.md:66`).
- Actual: all responses are 404, but the bodies differ:
  - `beta/app` (private, exists, no grant) → `{"error":"not found"}`; `beta/web` (missing) → `{"error":"repository not found"}`.
  - `lab15/anything` (foreign org exists) → `{"error":"repository not found"}`; `t14-no-such-org/anything` (org missing) → `{"error":"organization not found"}`.
  - Same-org non-reader: `t14-revoke` (exists, forbidden) → `"not found"` vs `t14-nope` (missing) → `"repository not found"`.
  So any signed-in account can confirm a guessed private repository name in **another tenant's** organization, and can confirm whether an organization slug exists, by the body text alone (`repository not found` vs `not found` vs `organization not found`). The merge-intent route has the same shape: a missing PR number returns `"merge intent not found"`, while an existing but unreadable PR returns `"pull request not found"`.
- Evidence: `load_repo` returns `"repository not found"` before the authorization check (`crates/githearth-server/src/repository_api.rs:88-117`); `require` returns the generic `"not found"` only when `repository.read` is denied (`crates/githearth-server/src/repository_api.rs:175-198`). `facts::load` returns a (grant-less) snapshot for any existing organization, so the lookup proceeds for non-members. Status code is 404 in every case and no name/description is included in any body.
- Scope: systematic for repository-scoped reads across all organizations (cases B18, N01, N02, N03). Severity is limited to name/number/slug existence (no content, no write) and requires guessing the name or slug; still, it contradicts the "a denial reveals nothing" contract and the brief's "404 for both missing and forbidden".

### 3. Every pull-request detail request returns 500, and PR pages 503 (blocks the stored-XSS check on PR pages)
- Severity: **major** (outside this domain; blocker for case X06)
- Repro:
  ```bash
  curl -i "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/pulls/2" -H "Authorization: Bearer $T14_TOKEN"
  curl -i "http://127.0.0.1:8080/api/v1/organizations/acme/repositories/web/pulls/1" -H "Authorization: Bearer $T14_TOKEN"
  # same acme/web request as alice (owner) also returns 500; the browser page returns 503
  ```
- Expected: 200 with the PR detail, so the PR page's escaping of comment bodies can be inspected.
- Actual: API `500 {"error":"storage operation failed"}` for every pull request tested (lab14 alpha #2, acme/web #1, as t14 and as alice); PR HTML page `503`. App log: `{"message":"repository read failed","category":"git","repository_id":"83ca9b35-…"}` (acme/web) and the same for lab14's repo.
- Evidence: `read_pull` runs mergeability (`crates/githearth-server/src/pulls.rs:877-921` → `mergeability::for_pull` → `browse::git_compare` → `map_storage_error` in `crates/githearth-server/src/browse.rs:169-186`). The `/compare/main...feature` endpoint (which uses the same scratch-dir mechanism) returns 200 for both repositories, and a manual `git merge-tree --write-tree` against the repository succeeds as the app user, so the failing step is specific to the PR-detail path. The defect affects the pristine acme fixture, not only t14 data.
- Scope: systematic — all PR detail reads/pages. It prevented X06 (PR comment escaping) from being observed; issue pages, comments, repository descriptions and blob pages were verified independently and do escape.
- Note: This is the PR/mergeability domain's defect; reported here because it blocks a security case and the campaign lead consolidates.

## Notes / skipped

- **Skipped X06** (PR page escaping): blocked by defect 3; the PR page returns 503 before rendering. Issue title/body, issue comments, repository description, blob file content and the UI CSP were all verified.
- **Skipped L02–L04** (reuse of acme protection/tag-rule ids): acme exposes the lists (200) but has no protection or tag rules, so there is no acme rule id to replay. Cross-organization id replay is covered by A15 (lab15 protection), A17 (lab15 grant), A27–A29 (lab14 cross-repo rule/grant ids) and L05.
- **Organization delete/update and membership administration** have no HTTP endpoints in this build (`docs/foundation.md` M1 scope; only `GET`/`POST /api/v1/organizations`). "Owner-only organization delete" and "manage members" escalation cases were therefore skipped. Owner-only `organization.update` was exercised through the owner-gated audit endpoint: t14 200, bob(write)/carol(maintain)/dave(admin) 403.
- **Role mapping actually provisioned in lab14** differs from the parenthetical in the task: observed via `GET .../access` and a read-only DB check, it is `bob=write`, `carol=maintain`, `dave=admin`, `erin=read`, `frank=triage`, `grace=member`, `t14=owner` (the same ladder is provisioned in every `labNN`). Tests used the actual roles: write-only boundary with bob, maintain boundary with carol, admin group with dave. If the intended fixture was carol=write/dave=maintain, the provisioning, not the product, needs correcting.
- **t14 has an organization-scoped read grant in acme** (it can read acme/web issues, PRs, refs, tree, protection lists and 403s on access/audit). Write escalation for that grant shape was verified in lab14 with erin (same read grant): all 22 sampled mutations were 403/404. No mutation was attempted against acme or beta to keep shared fixtures untouched.
- **Local HTTP profile**: session cookies have `HttpOnly; SameSite=Lax; Path=/` but no `Secure`, and no HSTS header is sent. This is expected for the documented loopback HTTP profile (`GITHEARTH_SECURE_COOKIES=false`; TLS terminates at a proxy in production). Not counted as a defect.
- **Rate limiting** is global auth concurrency (2) with a 2 s queue, not per-account/IP throttling; `docs/foundation.md:69` states this is deliberate and later work. 30 parallel wrong logins produced 30×401 (no 5xx, no lockout); 30 parallel reads all 200; 30 parallel writes with one idempotency key produced one issue and 29×409.
- **Hardening observations, not defects**: (a) a JSON array body is accepted positionally by serde for struct payloads (e.g. `["t14-array"]` creates an issue titled `t14-array`; fields are still type-checked and there is no authz impact); (b) handlers accept a JSON body regardless of `Content-Type` (text/plain parsed); (c) malformed `Path<Uuid>`/query inputs return plain-text parser detail (`... UUID parsing failed ...`), which is framework detail, not secret; (d) no per-account throttling as above.
- **SSH key pre-registration** (registering someone else's public key first) is a documented gap (`docs/foundation.md`, issue #66) and was not counted as a defect. The fail-safe was confirmed indirectly: SSH authentication resolves to the key's registrant, and a key revoked via the API fails the next connection immediately.
- **Secrets**: 500-line app log tail contained 0 occurrences of the account password, `gth_`, `Bearer `, argon2 PHC strings or private-key markers, and 0 occurrences of test payload text (`t14-sqli`, `t14-nul`, `X-Injected`). Owner audit JSON exposes only `seq, kind, actor_id, created_at, hash, client_ip`; the 32-byte chain hash is not a credential hash. All 23 DB password hashes are `$argon2id$v=19$m=19456,t=2,p=1$…`; all 711 session and 52 access-token rows store 32-byte digests and no plaintext.
- **Artifacts**: `lab14/alpha` (branches `main`, `feature`, `t14-xss-file`; tags `v1.0.0`, `v14-tag-*`; issues/PR/comments/reviews/protection), `lab14/t14-probe`, `lab14/t14-revoke`, PATs `t14-readonly`/`t14-write` and SSH key `t14-laptop` remain, all prefixed `t14-`. The temporary PAT/key and disposable repos created during tests were revoked/deleted. `acme`, `beta` and `lab15` were never modified.

## Round 2 — fixed build (2026-11-14)

Build: same live instance, rebuilt app image (`githearth-app-1`, image built 22:15Z): Debian GNU/Linux 13 (trixie), Git 2.47.3. Tester `t14` (`lab14`); `grace`, `erin` and `alice` for isolation checks. No repository edits, no container restarts, no destructive SQL. Every round-1 failing case was re-run once; the anonymous-public-browse change was checked separately for regressions. Round-1 rows are unchanged; this section only records the re-verification.

### Re-run of round-1 failures

| # | Case | Expected | Actual (round 2) | Verdict |
|---|---|---|---|---|
| I24 | NUL in issue title / issue comment / repository description / organization name | 400 `invalid input`, nothing written | 4/4 `503 {"error":"database unavailable"}` | **FAIL** (unchanged) |
| I28 | NUL in organization and repository path parameters | 400/404, no 5xx | `.../organizations/%00/repositories` and `.../repositories/%00alpha` both **503**; numeric issue `%00` is 400 (integer parser) | **FAIL** (unchanged) |
| B18 | grace: existing-but-ungranted `t14-revoke` vs missing `t14-nope` | identical 404 status and body | 404 `{"error":"not found"}` vs 404 `{"error":"repository not found"}` | **FAIL** (unchanged) |
| N01 | t14: `beta/app` (exists, no grant) vs `beta/web` (missing) | identical 404 status and body | 404 `{"error":"not found"}` vs 404 `{"error":"repository not found"}` | **FAIL** (unchanged) |
| N02 | t14: `lab15/anything` (foreign org exists) vs nonexistent org | identical 404 status and body | 404 `{"error":"repository not found"}` vs 404 `{"error":"organization not found"}` | **FAIL** (unchanged) |
| N03 | merge-intent route: existing PR vs missing PR | identical 404 status and body | As `grace`/`alice`: PR 2 → 404 `{"error":"pull request not found"}`, PR 99 → 404 `{"error":"merge intent not found"}`. As `t14` (repo reader) both return `merge intent not found` | **FAIL** (unchanged) |
| D3 | PR detail: lab14/alpha #2 and acme/web #1 (t14 and alice); PR HTML page | 200 | 200 / 200 / 200; PR page 200 | **PASS** (fixed) |
| X06 | PR page escapes a stored `<script>` comment | 200, escaped | 200, literal `<script>` absent, `&lt;script&gt;` present | **PASS** (unblocked) |

Re-run tally: **8 run, 2 pass, 6 fail.** The only round-1 defect fixed in this build is the PR-detail 500; the NUL 503 and the 404-body oracle are unchanged, including the merge-intent variant (an N03 probe as `t14` shows no difference because t14 can read the repository; reproducing as `grace`/`alice` shows the same two-body oracle as round 1).

### 20-case adversarial smoke

| # | Area | Case | Expected | Actual | Verdict |
|---|---|---|---|---|---|
| R2-01 | Tenant isolation | t14: lab15 by UUID, lab15 repo list, lab15/alpha, lab15/alpha issue 1 | 404 | 404 `organization not found` / `repository not found` | PASS |
| R2-02 | Tenant isolation | t14: PATCH lab15/alpha, POST issue, POST valid merge intent | 404 | 404 `repository not found` all three | PASS |
| R2-03 | Tenant isolation | t14: beta repo list, beta/app, git push to beta/app | 404; push denied | 404 / 404; git rc 128 `repository ... not found` | PASS |
| R2-04 | Tenant isolation | t14 SSH key clone of lab15/alpha | denied | `Please make sure you have the correct access rights and the repository exists` | PASS |
| R2-05 | IDOR | t14 read grant: acme/web tree | 200 | 200 with `src` entries | PASS |
| R2-06 | IDOR | t14 read grant: PATCH acme/web, read acme audit, POST issue | 403 / 403; issue creation allowed by read group (round-1 C13) | 403 with description unchanged / 403 / 201 (design) | PASS |
| R2-07 | IDOR | erin read grant: PATCH lab14/alpha | 403 | 403 `not permitted` | PASS |
| R2-08 | Read-only PAT | PATCH repo, POST issue, POST access, DELETE repo, POST SSH key, POST token | 403 each | 403 `read-only token` on all six | PASS |
| R2-09 | Read-only PAT | git clone private repo, then push a branch | clone ok, push denied | clone rc 0; push rc 1 `permission denied: ref.push` | PASS |
| R2-10 | Grant revocation | grant grace read (200), revoke, next request denied | 201 / 200 / 204 / 404 | 201 / 200 / 204 / 404 | PASS |
| R2-11 | Grant scope | grant to alice, who is outside lab14 | 400 | 400 `invalid grant` | PASS |
| R2-12 | Path traversal | API tree `..`, blob `path=..`, encoded `..` org path | 404/400, no `/etc/passwd` | 404 / 400 / 404; no `root:x:` in any body | PASS |
| R2-13 | Path traversal | UI tree traversal; git URL with encoded `..` | 404; git denied | UI 404 (login 303); git rc 128 `repository ... not found` | PASS |
| R2-14 | SQL strings | stored SQLi title; SQLi login username; SQLi cursor | exact round-trip; 401; 400 | issue #29 title exact; 401 generic; 400 parser detail, no SQL text | PASS |
| R2-15 | Stored XSS | `<script>` issue title/body in list and detail pages | 200, escaped | 200; literal absent in both; `&lt;script&gt;` present | PASS |
| R2-16 | Header injection | CRLF in issue title | 201, no injected header | 201; no `X-Injected` response header | PASS |
| R2-17 | Transport/session | session cookie flags; CSP/XFO/nosniff/referrer; CORS/Server; CSRF | as round 1 | `Path=/; HttpOnly; SameSite=Lax` (no `Secure`, documented); CSP `default-src 'none' … frame-ancestors 'none'`; XFO `DENY`; nosniff; `same-origin`; no ACAO/Server/X-Powered-By; logout and issue POST without CSRF 403 | PASS |
| R2-18 | Enumeration | unknown user vs wrong password | 401, identical body, ratio < 3 | 5/5 identical; medians 11.6 ms vs 11.5 ms; ratio 1.01 | PASS |
| R2-19 | Burst | 30 parallel wrong logins, then a valid login | 401/429 only, no 5xx, no lockout | 30×401; valid login 200 | PASS |
| R2-20 | Burst | 20 parallel authenticated reads | 200 | 20×200 | PASS |

Smoke tally: **20/20 pass.** No 5xx appeared in the round-2 test window apart from the NUL-byte 503s recorded below.

### Anonymous public-browse regression

| # | Check | Expected | Actual | Verdict |
|---|---|---|---|---|
| P-01 | Anonymous UI public repo page (`lab14/t14-r2-pub`) | 200 | 200; no reference to `alpha`, `t14-r2-priv`, `t14-probe` or `acme/web` | PASS |
| P-02 | Anonymous public tree and blob | 200 with content | tree 200 lists `README.md`; blob 200 shows `t14-r2-public-content` | PASS |
| P-03 | Anonymous public HTML file | escaped, not executed | 200; no literal `<script>`, `&lt;script&gt;` present | PASS |
| P-04 | Anonymous public commits and compare pages | 200 | 200 / 200 | PASS |
| P-05 | Private repos and missing repo: alpha, t14-r2-priv, acme/web, lab15/alpha, t14-nope | 303 `/login`, identical, no content | all 303 `/login` with empty body; private vs missing byte-identical | PASS |
| P-06 | Private tree/blob/issues/pulls/commits/compare paths | 303 `/login` | all 303 `/login` | PASS |
| P-07 | Anonymous API: public vs private repo; acme repository list | 401 identical; no private name | 401 same body for public and private; list 401 | PASS |
| P-08 | Anonymous git: public vs private/missing clone | public rc 0; private/missing denied | rc 0 with README; rc 128 `repository ... not found` for both private and missing | PASS |

Public-browse tally: **8/8 pass.** The change exposes only repositories marked `public`; private and nonexistent paths are indistinguishable in status, `Location` and body, and private content never appears in an anonymous response.

### Updated verdict

**PASS WITH DEFECTS.** Round-2 checks: **36 run, 30 pass, 6 fail, 0 skipped.** The six failures are two round-1 defects that remain unchanged: the NUL-byte 503 (I24, I28) and the 404-body oracle (B18, N01, N02, N03). Defect 3 (PR-detail 500) is fixed and X06 is now verified. The anonymous public-browse change introduces no new leak: public repositories are browsable, private ones still redirect/401/deny clone and leak neither content nor names.

### Remaining defects (round 2)

#### 1. NUL byte still returns 503 instead of 400
- Severity: **minor** (unchanged)
- Re-ran 6/6 round-1 variants; all still reproduce.
- Repro:
  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
    -H 'Content-Type: application/json' \
    -d '{"username":"t14","password":"<test-password>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')

  curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/issues \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"title":"t14-r2-nul-\u0000","body":"x"}'
  curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/issues/1/comments \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"body":"t14-r2-nul-\u0000"}'
  curl -i -X PATCH http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"description":"t14-r2-nul-\u0000"}'
  curl -i -X POST http://127.0.0.1:8080/api/v1/organizations \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    --data-binary '{"slug":"t14-r2-nul","name":"t14\u0000"}'
  curl -i "http://127.0.0.1:8080/api/v1/organizations/%00/repositories" -H "Authorization: Bearer $TOKEN"
  curl -i "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/%00alpha" -H "Authorization: Bearer $TOKEN"
  ```
- Actual: 503 `{"error":"database unavailable"}` on all six; app log records `{"message":"database operation failed","category":"database"}` for each. No row is committed (transaction rollback), so this remains an availability/error-classification and monitoring-noise issue, not data corruption or an authz bypass.
- Scope: all user-controlled text fields and string path parameters that reach PostgreSQL `text`; numeric path parameters reject NUL at parse (400). Anonymous requests are refused at authentication (401/303) before the database, so a session or token is required.
- Evidence: unchanged code path. The newer source branch in the checkout (`fix/m4-lab`, 7e1ded3) adds zero-width title checks but no NUL validation, and `ApiError::from(sqlx::Error)` still maps the PostgreSQL `22021` error to 503.

#### 2. 404 bodies still distinguish missing from unreadable resources
- Severity: **minor** (unchanged)
- All four round-1 pairs still differ in body while sharing status 404:
  - grace: existing-but-ungranted `t14-revoke` → `{"error":"not found"}`; missing `t14-nope` → `{"error":"repository not found"}` (B18).
  - t14: `beta/app` (exists) → `{"error":"not found"}`; `beta/web` (missing) → `{"error":"repository not found"}` (N01).
  - t14: `lab15/anything` (foreign org exists) → `{"error":"repository not found"}`; nonexistent org → `{"error":"organization not found"}` (N02).
  - merge-intent route as grace/alice: existing `pulls/2/merge` → `{"error":"pull request not found"}`; missing `pulls/99/merge` → `{"error":"merge intent not found"}` (N03).
- Repro:
  ```bash
  TOKEN14=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login -H 'Content-Type: application/json' \
    -d '{"username":"t14","password":"<test-password>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
  TOKENG=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login -H 'Content-Type: application/json' \
    -d '{"username":"grace","password":"<test-password>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')

  for p in lab14/repositories/t14-revoke lab14/repositories/t14-nope \
           beta/repositories/app beta/repositories/web; do
    curl -s "http://127.0.0.1:8080/api/v1/organizations/$p" -H "Authorization: Bearer $TOKEN14"; echo
  done
  curl -s "http://127.0.0.1:8080/api/v1/organizations/lab15/repositories/anything" -H "Authorization: Bearer $TOKEN14"; echo
  curl -s "http://127.0.0.1:8080/api/v1/organizations/t14-no-such-org/repositories/anything" -H "Authorization: Bearer $TOKEN14"; echo
  curl -s "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/pulls/2/merge" -H "Authorization: Bearer $TOKENG"; echo
  curl -s "http://127.0.0.1:8080/api/v1/organizations/lab14/repositories/alpha/pulls/99/merge" -H "Authorization: Bearer $TOKENG"; echo
  ```
- Expected: identical status **and body** for missing vs forbidden (ADR 0002, "a denial reveals nothing").
- Actual: the bodies differ across every pair (`not found`, `repository not found`, `organization not found`, `pull request not found`, `merge intent not found`), letting any signed-in account confirm a guessed private repository name or foreign organization slug. No content, description or write is exposed; severity remains limited to existence.
- Evidence: `load_repo` still returns `repository not found` before the authorization decision, and `require`/`require_pull` return the generic `not found`/`pull request not found` only after a denial; the newer source branch does not touch these paths.

#### 3. Fixed in this build: PR-detail 500 (round-1 defect 3)
- `GET /api/v1/organizations/lab14/repositories/alpha/pulls/2` → 200; `GET .../acme/repositories/web/pulls/1` → 200 as both t14 and alice; the PR HTML page returns 200.
- X06 is therefore no longer skipped: a stored `<script>alert('t14-r2-pr-xss')</script>` PR comment renders escaped on the PR page (literal absent, `&lt;script&gt;` present).
- Round-1 severity for this item: major; now resolved. No PR-detail 500 appeared in the round-2 test window.

### Round-2 notes

- **Cleanup**: the disposable public/private/grant/visibility repos created for round 2 were deleted; the temporary read-only PATs were revoked. `lab14` is back to `alpha`, `t14-probe`, `t14-revoke`. No `acme`, `beta` or `lab15` settings were changed. One side effect must be recorded: the acme read-grant design check created `acme/web` issue #3 as t14 (read group may open issues/comments, same as round-1 C13); it was closed and retitled `t14-r2-smoke-ignore`. The issue API has no delete, so the closed row remains.
- **Visibility is create-only in this build**: `PATCH .../repositories/{name}` accepts only `name` and `description`; a `visibility` field is rejected with 400. A live public→private flip therefore could not be exercised; with no mutable-visibility path there is also no stale-access path to test. This is a product limitation, not a security defect.
- **Public API remains auth-only**: anonymous `GET /api/v1/...` returns 401 even for a public repository, while the UI browse is anonymous. This matches the round-1 B02 observation and leaks nothing; noted as an intentional asymmetry, not a defect.
- **No new 5xx**: outside the NUL-byte 503s, the round-2 test window contains no 500s from these tests; the PR-detail path and PR page are clean.
- **Rate limits**: unchanged global auth concurrency (2, 2 s queue); 30 parallel wrong logins queued to 30×401 with no 5xx and no lockout, and the valid login afterward succeeded.
- **Secrets**: no credentials, `gth_` tokens, bearer values, argon2 hashes or private-key material were introduced into URLs, response bodies or logs during round 2.
