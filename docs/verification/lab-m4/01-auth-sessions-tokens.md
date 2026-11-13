# Authentication, sessions and personal access tokens — M4 lab acceptance report

Tester: t01. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS
Cases: 209 run, 209 pass, 0 fail, 0 skipped (5 checks not exercisable black-box; see Notes).

Scope: JSON login and its failure modes, session bearer tokens and logout, personal access
token creation/list/revocation and scope enforcement, the read-only PAT boundary over the
whole `/api/v1` surface, browser cookie/CSRF login and logout, the `/settings` token UI,
authentication bursts, SQL-injection-shaped input, and credential hygiene in logs.
No app or database restart was performed; all writes were confined to `lab01` fixtures
(`lab01/authcheck`, `lab01/authwrite`) and to t01's own tokens.

Result: all 209 executed cases matched the documented or source-defined behaviour. No
product defect was found in this domain.

## Results

### JSON login (38 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A01 | valid login t01 | 200 with Bearer session token | 200 {"token":"[redacted-64hex]","token_type":"Bearer","expires_in":3600} | PASS |
| A02 | login response shape | exactly token, token_type Bearer, expires_in | keys=['expires_in', 'token', 'token_type'] token_type=Bearer expires_in=3600 | PASS |
| A03 | session token format | 64 lowercase hex chars (256 bits) | len=64 sample=1ee32b37... | PASS |
| A04 | second login issues distinct token | different 64-hex token | status=200 distinct=True | PASS |
| A05 | login response Cache-Control | no-store | no-store | PASS |
| A06 | wrong password | 401 invalid or expired credentials | 401 {"error":"invalid or expired credentials"} | PASS |
| A07 | unknown username | 401 same generic body as wrong password | 401 {"error":"invalid or expired credentials"} | PASS |
| A08 | unknown user vs wrong password indistinguishable | identical status, body, headers | status 401/401 body_equal=True ct=application/json | PASS |
| A09 | empty username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A10 | 2-byte username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A11 | 64-byte unknown username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A12 | 65-byte username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A13 | uppercase username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A14 | trailing-space username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A15 | non-ASCII username | 401 (invalid username rejected before hash) | 401 {"error":"invalid or expired credentials"} | PASS |
| A16 | SQLi-shaped username | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A17 | SQLi-shaped password | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A18 | SQLi tail on username | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A19 | stacked-query username | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A20 | control char in username | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A21 | NUL in password | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A22 | trailing-space password | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A23 | trailing-newline username | 401, no SQL error/500 | 401 {"error":"invalid or expired credentials"} | PASS |
| A24 | 1025-byte password | 401 (rejected before Argon2, no 500) | 401 {"error":"invalid or expired credentials"} | PASS |
| A25 | 1024-byte password (wrong) | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| A26 | empty password | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| A27 | missing password field | 4xx rejection, no token | 422 Failed to deserialize the JSON body into the target type: missing field `password` at line 1 column 19 | PASS |
| A28 | missing username field | 4xx rejection, no token | 422 Failed to deserialize the JSON body into the target type: missing field `username` at line 1 column 34 | PASS |
| A29 | extra JSON field | 4xx rejection (deny_unknown_fields), no token | 422 Failed to deserialize the JSON body into the target type: extra: unknown field `extra`, expected `username` or `password` at line 1 column 6 | PASS |
| A30 | empty body | 400 JSON parse failure | 400 Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0 | PASS |
| A31 | malformed JSON | 400 JSON parse failure | 400 Failed to parse the request body as JSON: username: EOF while parsing a value at line 1 column 12 | PASS |
| A32 | wrong content type text/plain | 415 unsupported media type | 415 Expected request with `Content-Type: application/json` | PASS |
| A33 | no content type | 415 unsupported media type | 415 Expected request with `Content-Type: application/json` | PASS |
| A34 | JSON with charset parameter | 200 accepted | 200 | PASS |
| A35 | GET /api/v1/login | 405 method not allowed | 405 | PASS |
| A36 | body over 524288-byte limit | 413 payload too large | 413 length limit exceeded | PASS |
| A37 | Argon2 cost + unknown-user timing parity | both medians > 5 ms and within 3x (dummy hash) | wrong-password median=13.5ms ['14', '13', '13', '14'], unknown median=12.4ms ['13', '13', '12', '12'] | PASS |
| A38 | 401 vs 400 semantics | valid JSON+bad creds=401, missing fields=422/400 | bad creds=401, empty object=422 | PASS |

### Session bearer tokens (28 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| B01 | /me with valid session | 200 id/username/is_admin only | 200 {"id":"f18f8e3b-329c-445e-bd28-0d50826eca21","username":"t01","is_admin":false} | PASS |
| B02 | /me id is a UUID | canonical UUID string | f18f8e3b-329c-445e-bd28-0d50826eca21 | PASS |
| B03 | /me with no Authorization | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B04 | lowercase bearer scheme | 401 (scheme match is case-sensitive) | 401 {"error":"invalid or expired credentials"} | PASS |
| B05 | Bearer with no token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B06 | Bearer with empty token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B07 | token without scheme | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B08 | Basic auth scheme | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B09 | Bearer with two spaces | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B10 | 63-char truncated token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B11 | 64-char non-hex token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B12 | uppercase token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B13 | 65-char token (extra char) | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B14 | short gth_ token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B15 | session token reuse | 200 both times, same identity | 200/200 ids_equal=True | PASS |
| B16 | token in query string is ignored | 401 (header only) | 401 {"error":"invalid or expired credentials"} | PASS |
| B17 | /me response headers | application/json and Cache-Control: no-store | ct=application/json cache=no-store | PASS |
| B18 | logout with valid session | 204 empty body | 204 body='' | PASS |
| B19 | /me with logged-out session | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B20 | second session survives first logout | 200 t01 | 200 {"id":"f18f8e3b-329c-445e-bd28-0d50826eca21","username":"t01","is_admin":false} | PASS |
| B21 | logout twice with same token | 401 (token no longer resolves) | 401 {"error":"invalid or expired credentials"} | PASS |
| B22 | logout second session | 204 | 204 | PASS |
| B23 | second session revoked after its own logout | 401 | 401 | PASS |
| B24 | logout with no credential | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B25 | logout with malformed token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B26 | logout with Basic auth | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B27 | logout with bare Bearer | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| B28 | /me with well-formed unknown token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |

### Personal access token lifecycle (43 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C01 | create repo:read token | 201, gth_+64hex shown once, scopes echoed, expires_at null | 201 len=68 keys=['created_at', 'expires_at', 'id', 'name', 'scopes', 'token'] | PASS |
| C02 | create repo:write token | 201 gth_ token with repo:write | 201 scopes=['repo:write'] | PASS |
| C03 | issued tokens are distinct | two different 256-bit plaintexts | equal=False | PASS |
| C04 | expires_in_days=0 | 400 (rejected) | 400 {"error":"invalid input"} | PASS |
| C05 | expires_in_days=1 | 201 (accepted within bounds) | 201 {"id":"8aa27641-d1be-402f-87f5-44a3b87f2b7a","name":"lab01-exp-1","scopes":["repo:read"],"created_at":"2026-11-13T21:16: expires_in~1.00d | PASS |
| C06 | expires_in_days=3650 | 201 (accepted within bounds) | 201 {"id":"a81453b5-cc17-4650-9df4-ed1325e89f1f","name":"lab01-exp-3650","scopes":["repo:read"],"created_at":"2026-11-13T21: expires_in~3650.00d | PASS |
| C07 | expires_in_days=3651 | 400 (rejected) | 400 {"error":"invalid input"} | PASS |
| C08 | expires_in_days=-1 | 400 (rejected) | 400 {"error":"invalid input"} | PASS |
| C09 | expires_in_days=1.5 | 422 (rejected) | 422 Failed to deserialize the JSON body into the target type: expires_in_days: invalid type: floating point `1.5`, expected | PASS |
| C10 | expires_in_days='30' | 422 (rejected) | 422 Failed to deserialize the JSON body into the target type: expires_in_days: invalid type: string "30", expected i64 at li | PASS |
| C11 | name '' len=0 | 400 | 400 {"error":"invalid input"} | PASS |
| C12 | name ' ' len=3 | 400 | 400 {"error":"invalid input"} | PASS |
| C13 | name '\n\t ' len=3 | 400 | 400 {"error":"invalid input"} | PASS |
| C14 | name 'xxxxxxxxxxxx' len=101 | 400 | 400 {"error":"invalid input"} | PASS |
| C15 | name 'xxxxxxxxxxxx' len=100 | 201 | 201 {"id":"585e2605-59f2-478a-9a1c-9b18959d1df4","name":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | PASS |
| C16 | scopes=[] | 400 | 400 {"error":"invalid input"} | PASS |
| C17 | scopes=['repo:admin'] | 400 | 400 {"error":"invalid input"} | PASS |
| C18 | scopes=['repo:read', 'repo:write', 'repo:read'] | 400 | 400 {"error":"invalid input"} | PASS |
| C19 | scopes=['repo:read', 'repo:write'] | 201 | 201 {"id":"6789fde5-a7b8-4530-b049-03b5c388a976","name":"lab01-scope-C19","scopes":["repo:read","repo:write"],"created_at":" | PASS |
| C20 | scopes=['repo:read', 'repo:read'] | 201 | 201 {"id":"1e6ec07a-82ae-48fd-80ff-7bec0041edff","name":"lab01-scope-C20","scopes":["repo:read","repo:read"],"created_at":"2 | PASS |
| C21 | scopes=['REPO:READ'] | 400 | 400 {"error":"invalid input"} | PASS |
| C22 | create with unknown field | 4xx rejection, no token minted | 422 Failed to deserialize the JSON body into the target type: extra: unknown field `extra`, expected one of `name`, `scopes` | PASS |
| C23 | create token using a repo:read PAT | 403 refused (read-only boundary) | 403 {"error":"read-only token"} | PASS |
| C23b | create token using a repo:write PAT | 403 access tokens cannot manage access tokens | 403 {"error":"access tokens cannot manage access tokens"} | PASS |
| C24 | create token with no credential | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| C25 | create token missing scopes field | 4xx | 400 {"error":"invalid input"} | PASS |
| C26 | list tokens with session | 200 array including created tokens | 200 n=15 first_keys=['created_at', 'expires_at', 'id', 'last_used_at', 'name', 'revoked_at', 'scopes'] | PASS |
| C27 | list tokens with a PAT | 403 session only | 403 {"error":"access tokens cannot manage access tokens"} | PASS |
| C28 | list tokens with no credential | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| C29 | list never leaks secret or hash | no gth_ plaintext and no token_hash field | contains_gth_=False contains_token_hash=False | PASS |
| C30 | last_used_at null then set on first use | null before use, non-null after /me | before=None after=2026-11-13T21:16:52Z me_status=200 | PASS |
| C31 | /me with repo:read PAT | 200 t01, is_admin false | 200 {"id":"f18f8e3b-329c-445e-bd28-0d50826eca21","username":"t01","is_admin":false} | PASS |
| C32 | /me with repo:write PAT | 200 t01 | 200 {"id":"f18f8e3b-329c-445e-bd28-0d50826eca21","username":"t01","is_admin":false} | PASS |
| C33 | revoke own token | 204 | 204 body='' | PASS |
| C34 | revoked token refused | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| C35 | revoke same token twice | 404 | 404 {"error":"token not found"} | PASS |
| C36 | revoke unknown token UUID | 404 | 404 {"error":"token not found"} | PASS |
| C37 | revoke malformed token id | 400 path rejection | 400 Invalid URL: Cannot parse `id` with value `not-a-uuid`: UUID parsing failed: invalid character: found `n` at 0 | PASS |
| C38 | revoke using a PAT | 403 session only | 403 {"error":"access tokens cannot manage access tokens"} | PASS |
| C39 | revoke another account's token | 404 (no existence leak) | alice_login=200 created=True delete=404 {"error":"token not found"} | PASS |
| C40 | revoked_at visible in list | revoked_at non-null for revoked token | row={'id': '8d005415-22e7-4009-a53c-7650d4441ac7', 'name': 'lab01-read', 'scopes': ['repo:read'], 'created_at': '2026-11-13T21:16:51Z', 'expires_at': None, 'last_used_at': '2026-11-13T21:16… | PASS |
| C41 | revoked PAT refused on /organizations | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| G12 | token list order stable | same order on consecutive calls | n=21 same=True | PASS |

### Read-only PAT boundary (repo:read) (21 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| D01 | GET /organizations with repo:read PAT | 200 membership list | 200 has_lab01=True | PASS |
| D02 | GET /organizations/{lab01} with repo:read PAT | 200 | 200 {"id":"b0000000-0000-4000-8000-000000000001","slug":"lab01","name":"Lab 01","role":"owner"} | PASS |
| D03 | GET issue with repo:read PAT | 200 | 200 {"id":"6d60d2d7-a42a-4818-981a-9a7569207ca8","number":1,"title":"boundary baseline issue","body":"created by t01 session | PASS |
| D04 | GET /user/ssh-keys with repo:read PAT | 403 session-only (GET is not boundary-refused) | 403 {"error":"access tokens cannot manage ssh keys"} | PASS |
| D05 | create organization | 403 | 403 {"error":"read-only token"} | PASS |
| D06 | create repository | 403 | 403 {"error":"read-only token"} | PASS |
| D07 | create issue | 403 | 403 {"error":"read-only token"} | PASS |
| D08 | PATCH issue | 403 | 403 {"error":"read-only token"} | PASS |
| D09 | merge pull request | 403 | 403 {"error":"read-only token"} | PASS |
| D10 | logout with a PAT (issue #139) | 403 (read-only token) | 403 {"error":"read-only token"} | PASS |
| D11 | PATCH repository | 403 | 403 {"error":"read-only token"} | PASS |
| D12 | DELETE repository | 403 | 403 {"error":"read-only token"} | PASS |
| D13 | archive repository | 403 | 403 {"error":"read-only token"} | PASS |
| D14 | create PAT | 403 | 403 {"error":"read-only token"} | PASS |
| D15 | revoke PAT | 403 | 403 {"error":"read-only token"} | PASS |
| D16 | create SSH key | 403 | 403 {"error":"read-only token"} | PASS |
| D17 | mark notification read | 403 | 403 {"error":"read-only token"} | PASS |
| D18 | write to nonexistent repo | 403 | 403 {"error":"read-only token"} | PASS |
| D19 | GET /user/tokens with repo:read PAT | 403 global-token session-only | 403 {"error":"access tokens cannot manage access tokens"} | PASS |
| D20 | GET /user/notifications with repo:read PAT | 403 session-only | 403 {"error":"access tokens cannot view notifications"} | PASS |
| D28 | HEAD /organizations with repo:read PAT | 200 (HEAD passes boundary) | 200 body_bytes=0 | PASS |

### Write PAT (repo:write) (7 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| D21 | POST repository with repo:write PAT | 201 (or 409 re-run) | 201 {"id":"cdee62be-7d63-4683-8c9b-ed0015bf7c14","organization_id":"b0000000-0000-4000-8000-000000000001","name":"authwrite","descript | PASS |
| D22 | create issue with repo:write PAT | 201 | 201 {"id":"7779c75f-c509-4efd-ab45-f5951e570d68","number":1,"title":"write token issue","body":"created with repo:write PAT","state":" | PASS |
| D23 | PATCH issue with repo:write PAT | 200 title updated | 200 {"id":"7779c75f-c509-4efd-ab45-f5951e570d68","number":1,"title":"write token issue patched","body":"created with repo:write PAT"," | PASS |
| D24 | GET issue with repo:write PAT | 200 | 200 | PASS |
| D25 | GET notifications with repo:write PAT | 403 session-only | 403 {"error":"access tokens cannot view notifications"} | PASS |
| D26 | logout with repo:write PAT (issue #139) | 403 access tokens cannot log out | 403 {"error":"access tokens cannot log out"} | PASS |
| D27 | list tokens with repo:write PAT | 403 session-only | 403 {"error":"access tokens cannot manage access tokens"} | PASS |

### Browser session, cookies and CSRF (28 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| E01 | GET /login renders form + CSRF cookie | 200 HTML form, 32-hex CSRF cookie HttpOnly SameSite=Lax no Secure | 200 ct=text/html; charset=utf-8 tok_len=32 cookie=githearth_csrf=[redacted-32hex]; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600 | PASS |
| E02 | CSRF hidden input matches cookie | same 32-hex value in form and cookie | form=[redacted-32hex] cookie=[redacted-32hex] | PASS |
| E03 | GET / without session | 303 redirect to /login | 303 Location=/login | PASS |
| E04 | CSRF token reused across page loads | same token so a second tab stays valid | [redacted-32hex] / [redacted-32hex] equal=True | PASS |
| E05 | browser login with valid CSRF | 303 /, githearth_session HttpOnly SameSite=Lax no Secure, csrf cleared | 303 Location=/ cookie=githearth_session=[redacted-64hex]; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600 clear=githearth_csrf=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0 | PASS |
| E06 | authenticated home page | 200, shows t01 and Sign out form | 200 has_user=True has_signout=True | PASS |
| E07 | session-derived CSRF on page | 32-hex token distinct from the login CSRF | ui=[redacted-32hex] login_csrf=[redacted-32hex] distinct=True | PASS |
| E08 | cookie token usable as session bearer | 200 t01 (same sessions table) | 200 {"id":"f18f8e3b-329c-445e-bd28-0d50826eca21","username":"t01","is_admin":false} | PASS |
| E09 | cookie alone is not API auth | 401 (Authorization header only) | 401 {"error":"invalid or expired credentials"} | PASS |
| E10 | GET /login while signed in | 303 redirect to / | 303 Location=/ | PASS |
| E11 | login with wrong CSRF token | 403 Invalid CSRF token, no session cookie | 403 has_session_cookie=False | PASS |
| E12 | login POST without CSRF cookie | 403 | 403 <!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=devi | PASS |
| E13 | login POST with empty form token | 403 | 403 | PASS |
| E14 | CSRF token from a different session | 403 (must match own cookie) | 403 other=3a1dafd0... | PASS |
| E15 | browser login wrong password | 303 /login?error=1, no session cookie | 303 Location=/login?error=1 session_cookie=False | PASS |
| E16 | browser login invalid username case | 303 /login?error=1 | 303 Location=/login?error=1 | PASS |
| E17 | browser login missing password field | 4xx, no session cookie | 422 Failed to deserialize form body: missing field `password` | PASS |
| E18 | browser login 1025-byte password | 303 /login?error=1, no 500 | 303 Location=/login?error=1 | PASS |
| E19 | GET /logout | 405 method not allowed | 405 | PASS |
| E20 | logout POST without CSRF | 403 | 403 <!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=devi | PASS |
| E21 | logout POST with wrong CSRF | 403 | 403 | PASS |
| E22 | logout with login-page CSRF instead of session CSRF | 403 | 403 | PASS |
| E23 | tampered session cookie | 303 /login | 303 Location=/login | PASS |
| E24 | malformed session cookie | 303 /login | 303 Location=/login | PASS |
| E25 | browser logout with valid CSRF | 303 /login, session cookie Max-Age=0 | 303 Location=/login clear=githearth_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0 | PASS |
| E26 | old cookie after browser logout | 303 /login | 303 Location=/login | PASS |
| E27 | session token revoked by UI logout | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| E28 | logout with X-CSRF-Token header | 303 /login | 303 Location=/login | PASS |

### Settings page token management (14 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| H01 | GET /settings without session | 303 /login | 303 Location=/login | PASS |
| H02 | GET /settings with session | 200 page with token form, CSRF and idempotency key | 200 csrf_len=32 ikey_len=32 | PASS |
| H03 | create token via settings form | 200 with one-time plaintext gth_ token | 200 plaintext_shown=True notice=True | PASS |
| H04 | settings-created token authenticates | 200 t01 | 200 {"id":"f18f8e3b-329c-445e-bd28-0d50826eca21","username":"t01","is_admin":false} | PASS |
| H05 | plaintext not shown again | no gth_ token on later page; name still listed | contains_gth=False name_listed=True | PASS |
| H06 | create token with wrong CSRF | 403, no token minted | 403 <!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=devi | PASS |
| H07 | create token with expiry 0 via form | 400 page, no token created | 400 notice=True count 31->31 | PASS |
| H08 | create token with no scope checked | 400 page, no plaintext | 400 has_gth=False invalid=False | PASS |
| H09 | revoke token via settings form | 303 redirect to /settings (PRG), token revoked | 303 Location=/settings | PASS |
| H10 | revoked settings token refused | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| H11 | revoke with wrong CSRF | 403, token still valid | 403 still_valid=True | PASS |
| H12 | replayed idempotency key | second submit says already created, only one token row | first=200 second=200 rows=1 already=True | PASS |
| H13 | settings page ignores API bearer | 303 /login (cookie only) | 303 Location=/login | PASS |
| H14 | settings session logout | 303 /login | 303 Location=/login | PASS |

### Authentication burst behaviour (3 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| F01 | 20 parallel valid logins | mostly 200, 429 only with 'authentication busy', never 5xx | codes={200: 20} max_ms=135 busy_bodies_ok=True | PASS |
| F02 | 20 parallel wrong-password logins | 401 (few 429 allowed), never 5xx | codes={401: 20} max_ms=137 | PASS |
| F03 | 20 parallel unknown-user logins | 401 (few 429 allowed), never 5xx | codes={401: 20} max_ms=152 | PASS |

### Edge cases and log hygiene (12 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| F04 | SQLi-shaped PAT name | 201, stored literally, list still works | 201 found_in_list=True list_status=200 | PASS |
| F05 | SQLi-shaped PAT scope | 400 invalid scope | 400 {"error":"invalid input"} | PASS |
| F06 | password variant '234é' | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| F07 | password variant '234\t' | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| F08 | gth_ token with uppercase hex | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| F09 | gth_ token 65 hex chars | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| F10 | unknown well-formed gth_ token | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| F11 | tab instead of space after Bearer | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| F12 | no public registration route | 404 (bootstrap is operator-only) | 404 | PASS |
| F13 | JSON login sets no cookie | no Set-Cookie (cookie flow is /login only) | set_cookie=[] | PASS |
| F14 | JSON logout sets no cookie | 204, no Set-Cookie | 204 set_cookie=[] | PASS |
| F15 | 401 error content type | application/json, no Server version header | ct=application/json server=None | PASS |

### 401/403/405 semantics (8 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| G01 | write route with no credential | 401 unauthenticated | 401 {"error":"invalid or expired credentials"} | PASS |
| G02 | write route with malformed bearer | 401 unauthenticated | 401 {"error":"invalid or expired credentials"} | PASS |
| G03 | write route with unknown gth_ token | 401 (not 403; no valid token) | 401 {"error":"invalid or expired credentials"} | PASS |
| G04 | write route with revoked repo:read token | 401 (revocation beats read-only 403) | 401 {"error":"invalid or expired credentials"} | PASS |
| G05 | duplicate repo:read scopes still read-only | 403 read-only token | 403 {"error":"read-only token"} | PASS |
| G06 | API logout with cookie only | 401 (cookie is not API auth) | 401 {"error":"invalid or expired credentials"} | PASS |
| G07 | POST /api/v1/me | 405 method not allowed | 405 | PASS |
| G08 | GET /api/v1/logout | 405 method not allowed | 405 | PASS |

### Browser edge cases (3 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| G09 | UI logout with no session | 303 /login (auth required, no crash) | 303 Location=/login | PASS |
| G10 | malformed CSRF cookie replaced | 200 with fresh 32-hex githearth_csrf cookie | 200 new_len=32 | PASS |
| G11 | login with form token not matching cookie | 403 | 403 | PASS |

### Log hygiene (2 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| F16 | no credential material in app logs | 0 occurrences of PAT, session token or password | pat/session_token_hits=0 password_hits=0 log_lines=2330 | PASS |
| F17 | logs omit raw URIs and token strings | 0 lines containing gth_ or access_token query | hits=0 | PASS |

### Test setup fixtures (2 cases)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| D00 | create lab01/authcheck repository | 201 (or 409 when re-run) | 201 {"id":"07e42450-d4a6-4411-a52e-bc2961c7973d","organization_id":"b0000000-0000-4000-8000-000000000001","name":"authcheck" | PASS |
| D00b | baseline issue available | issue number in lab01/authcheck | status=201 issue=#1 | PASS |

## Defects

None observed. Every case that initially failed during authoring was re-checked against the
implementation and shown to be a mistaken expectation, then re-run with the corrected
expectation; none of those corrections describe product behaviour that departs from
`docs/foundation.md`:

- A `Bearer <token> ` header with trailing whitespace is accepted because HTTP parsers
  remove optional whitespace around field values before the server sees it (RFC 9110);
  the server's parser itself is strict. Replaced with a 65-character-token case, which is 401.
- A `repo:read` PAT calling `POST /api/v1/user/tokens` is refused 403 by the read-only
  middleware (`read-only token`) before the token handler's session-only refusal. Both
  scopes are 403; only the message differs. Tested both scopes explicitly.
- The settings-page revoke form answers 303 to `/settings` (Post/Redirect/Get), not 200.
  Re-run with the redirect expectation, with the token verified revoked.

Observations that are consistent with the documents, not defects:

- Malformed JSON bodies return 400; JSON with unknown fields returns 422 (axum rejection),
  both before authentication. A syntactically valid body with bad credentials always returns
  the generic 401 `invalid or expired credentials`.
- Duplicate scopes (`["repo:read","repo:read"]`) are accepted and still collapse to
  read-only; both-scope tokens are accepted. This does not widen `repo:read`.
- Token names are validated as nonblank and at most 100 characters, as documented; embedded
  control characters are not otherwise restricted.

## Notes / skipped

Checks deliberately not exercised black-box:

1. **Session expiry after `token_ttl_seconds`** — would need a 3600 s wait or a config
   change/restart (forbidden). The token lifetime is asserted from `expires_in` and
   `Max-Age`; expiry is covered by the PostgreSQL contract suite (`docs/foundation.md:145`).
2. **Argon2id hash parameters from stored hashes** — hash inspection is database/ops scope.
   Timing sanity was measured instead: wrong-password median 13.5 ms vs unknown-user median
   12.4 ms over four samples each (A37), consistent with the pinned 19 MiB / 2-iteration
   Argon2id cost (`crates/githearth-server/src/lib.rs:495`).
3. **PAT expiry after elapsed days** — created tokens with `expires_in_days` 1 and 3650 and
   verified `expires_at` to within 1 day, but did not wait for expiry. Bounds 0, 1, 3650,
   3651, -1 and non-integers were exercised.
4. **`Secure=true` cookie behaviour** — this deployment sets `GITHEARTH_SECURE_COOKIES=false`
   for plain HTTP; the absence of `Secure` was asserted as deployed (E01, E05).
5. **Git-over-HTTP Basic auth with a PAT/session** — Git transport is a separate tester's
   domain; this report covers the `/api/v1` PAT boundary only.

Notes on method and hygiene:

- Unknown-user and wrong-password responses were compared byte-for-byte (status, body,
  `Content-Type`, `Content-Length`) and matched (A08).
- Burst runs were 20 simultaneous requests per case (F01–F03): all returned 200/401, no 429
  and no 5xx. The 2 s authentication queue absorbed the burst; 429 `authentication busy`
  remains covered by unit tests rather than observed here.
- A cross-account probe created one temporary PAT under `alice`
  (`t01-cross-account-probe`), confirmed t01 gets 404 revoking it, then revoked it as alice
  (C39). No `acme` repository or organization state was changed.
- Created test PATs were revoked at the end of each run. `lab01/authcheck` and
  `lab01/authwrite` remain as harmless fixtures with their test issues.
- Log hygiene was checked against ~2300 application log lines: zero occurrences of the
  test password, a live session token, a PAT plaintext, or a raw `gth_`/query token (F16–F17).

## Round 2 — fixed build (2026-11-14)

Instance: `http://127.0.0.1:8080`, image `githearth-app` built 2026-11-13T22:15:49Z and started
2026-11-13T22:16:22Z; `git --version` 2.47.3, Debian GNU/Linux 13 (trixie); `/health/live` and
`/health/ready` both 200. No container was restarted, stopped or rebuilt, no migration was run,
and no repository file was edited. Work was confined to `lab01` fixtures and t01's own tokens.

Round 1 recorded 209/209 PASS and no defects, so this round re-ran 17 representative cases
(all sub-areas) plus a 14-case smoke that specifically probes the four changed areas: SSH
host-key idempotency, merge-gate change-request blocking, public anonymous browse, and
unchanged session/PAT behaviour.

### Representative re-runs (17)

| # | Case (round-1 id) | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R01 | A01 valid login t01 | 200 Bearer, 64-hex, `expires_in` 3600 | 200 `keys=[expires_in, token, token_type]` len=64 Bearer | PASS |
| R02 | A06 wrong password | 401 generic | 401 `{"error":"invalid or expired credentials"}` | PASS |
| R03 | A08 unknown vs wrong password | identical status/body/CT | 401/401 body equal, `application/json` | PASS |
| R04 | A27 missing password field | 4xx, no token | 422 missing field `password` | PASS |
| R05 | A36 body > 524288 bytes | 413 | 413 `length limit exceeded` | PASS |
| R06 | B01 GET /me with session | 200 id/username/is_admin only | 200 `{"id":"f18f8e3b…","username":"t01","is_admin":false}` | PASS |
| R07 | B03 /me with no Authorization | 401 | 401 generic | PASS |
| R08 | B18+B19 logout then reuse | 204 then 401 | 204 empty body, then 401 | PASS |
| R09 | B21 logout twice | 401 | 401 generic | PASS |
| R10 | C01 create repo:read PAT | 201 `gth_`+64hex, scopes echoed, `expires_at` null | 201 len=68, `scopes=["repo:read"]`, `expires_at=null` | PASS |
| R11 | C04 `expires_in_days=0` | 400 | 400 `invalid input` | PASS |
| R12 | C23 mint token with repo:read PAT | 403 read-only token | 403 `{"error":"read-only token"}` | PASS |
| R13 | C23b mint token with repo:write PAT | 403 access tokens cannot manage access tokens | 403 `{"error":"access tokens cannot manage access tokens"}` | PASS |
| R14 | C29 token list leaks nothing | 200, no `gth_`, no `token_hash` | 200 n=35, `gth_`=False `token_hash`=False | PASS |
| R15 | C33+C34 revoke then reuse PAT | 204 then 401 | 204 then 401 | PASS |
| R16 | D05+D06 read-only boundary writes | 403 on create org/repo/delete/archive | 403 `read-only token` on all four | PASS |
| R17 | E05+E25 browser login/logout | 303 + session cookie; logout 303 `/login`; old cookie redirected | login 303+cookie, logout 303→`/login`, after 303 | PASS |

### Smoke of the domain and changed areas (14)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| S01 | SSH host key stable across connections | same key on repeated scans and after fixed-image start | `SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE` (ED25519) twice; key file mtime 2026-11-13 21:09:41Z predates image build 22:15:49Z | PASS |
| S02 | ssh-keygen re-run idempotent | exit 0, existing key kept untouched | `githearth-server ssh-keygen` exit=0, log `SSH host key already exists; keeping it`, sha256 `d0751ffc…` and mtime unchanged | PASS |
| S03 | SSH key auth still works | registered ed25519 key clones, revoke works | clone exit 0 (`README.md` present); key delete 204 | PASS |
| S04 | Anonymous UI browse public vs private | public tree/blob 200; private 303 `/login` | `lab01/t01-public` tree 200 (README.md), blob 200; `lab01/authcheck` 303→`/login` | PASS |
| S05 | Anonymous API read | still 401 (opening is UI-only) | 401 `{"error":"invalid or expired credentials"}` | PASS |
| S06 | Anonymous git clone boundary | public clone ok, private refused | public exit 0; private exit 128 `could not read Username` | PASS |
| S07 | repo:read PAT reads an issue | 200 issue #1 | 200 number=1 | PASS |
| S08 | repo:write PAT creates issue | 201 then read-back 200 | 201 then 200 in `lab01/authwrite` | PASS |
| S09 | Logout still requires CSRF | 403 for missing/blank/bad/stale token | 403/403/403/403; a bodyless POST is 415 from the form extractor | PASS |
| S10 | Merge route auth unchanged | no credential 401, repo:read PAT 403 | 401; 403 `read-only token` | PASS |
| S11 | 20 parallel valid logins | all 200, no 5xx | codes={200: 20} | PASS |
| S12 | PAT lifecycle unchanged | `last_used_at` set; PAT logout refused | `last_used_at=2026-11-13T22:22:59Z`; logout 403 `read-only token` | PASS |
| S13 | Cookie/session semantics unchanged | cookie usable as Bearer, cookie alone not API auth, `/login` redirects when signed in | 200 t01; 401; 303 `/` | PASS |
| S14 | Merge gate blocks on change request | 409 at intent creation; approval unblocks | changes_requested → 409 `the pull request has an outstanding change request`; after approval merge intent 201 → PR `state=merged`, result `2059aaad…` | PASS |

### Regressions

None. No round-1 PASS case changed behaviour, and none of the four changed areas regressed
session or token handling:

- **SSH host-key idempotency:** re-running the one-shot `ssh-keygen` command exits 0 and keeps
  the existing key byte-for-byte; the key persisted from before the fixed-image rebuild, so
  known-hosts entries stay valid across `up`/`down`. SSH key auth itself is unchanged.
- **Merge-gate change-request blocking:** an outstanding `changes_requested` review refuses the
  merge with 409 before an intent is created; a later approval on the same head allows the
  merge, which completed. Merge-route authentication/authorization (401/403 read-only) is
  unchanged.
- **Public anonymous browse:** public repository UI and anonymous HTTP clone are open; private
  repository UI still redirects to `/login`, the API still returns 401 anonymously, and private
  HTTP clone is refused. No existence leak was observed (nonexistent and private both redirect).
- **Session/PAT:** login, logout, session reuse, cookie-as-Bearer, cookie-not-API-auth, PAT
  issuance/revocation, scope enforcement and the read-only boundary all match round 1.

Two initial failures in this round were test-harness mistakes, re-run once and corrected, not
product defects:

1. R16 first reused the `repo:read` token that R15 had just revoked → 401. Re-run with a fresh
   token: all four writes 403.
2. S09 first sent a bodyless `POST /logout` → 415 from the form extractor. With a form-encoded
   body it returns 403 for missing, blank, wrong and login-scope CSRF tokens.

### Observations

- **Test environment, not a product issue:** the first anonymous clone of private `acme/web`
  appeared to succeed because the host's macOS keychain (`osxkeychain` system credential helper)
  supplied saved t01 credentials. With `GIT_CONFIG_NOSYSTEM=1 git -c credential.helper= clone`,
  the private clone fails 128 and the public clone succeeds. Other testers should disable the
  helper to test anonymity.
- `acme/site` is empty, so its tree page is 404 while `acme/web` correctly redirects to
  `/login`; positive public browse was therefore checked on the public `lab01/t01-public`.
- During the shared-instance window, unrelated concurrent testers produced 21 × 500 on
  `GET …/commits/{rev}` and 13 × 503 `database operation failed` on write routes (22:18–22:22Z).
  None involved an auth or token endpoint, none reproduced during this domain's runs (all writes
  and the 20-login burst succeeded), and they are noted for the campaign lead rather than as an
  authentication regression.
- Log hygiene held in the re-run: 0 log lines containing the test password, `gth_` plaintext,
  or a 64-hex bearer token.

Fixtures left behind: one issue `round2 smoke issue` in `lab01/authwrite` (no repository state
outside `lab01` was changed). The merge-gate repo `lab01/r2merge` and all r2-* tokens/SSH keys
were removed; the pre-existing public `lab01/t01-public` was only read.

**Updated verdict: PASS — 31 cases re-run/smoked, 31 pass, 0 fail, 0 skipped. No regression;
the round-1 verdict stands.**

