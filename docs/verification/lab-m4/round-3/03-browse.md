# Browse — M4 closure round 3

Tester: t03. Date: 2026-11-14. Instance: main 800cad4.
Verdict: PASS
Cases: 138 run, 138 pass, 0 fail, 0 skipped.

## Environment and fixtures

- Live instance `http://127.0.0.1:8080`, server `800cad4` (`/health/live` and
  `/health/ready` both 200).
- Signed in as `t03` (owner of `lab03`). Browser pages used
  `Cookie: githearth_session=<login token>`; JSON API used the bearer token.
- Created `lab03/r3-main` (private), `r3-public` (public), `r3-empty` (public,
  empty), `r3-private` (private), `r3-xssdesc` (private, XSS description).
- Pushed `r3-main` `main` with 153 commits (150 generated history commits plus
  3 fixture commits) to exercise pagination, plus branches `feature` (47
  behind), `short` (3 commits), `exactly100` (exactly 100), `xss<b>` (HTML in
  the name), `orphan` (unrelated history), and tag `v1.0.0`.
  Fixture content: a 2 MiB text file, a binary file, relative and absolute
  symlinks, a file whose name is `<img src=x onerror=alert(1)>.txt`, a file
  containing a script tag, and a commit message containing a script tag.
- Reused fixtures `lab01/t01-public` (public) and `acme/web` (private) for the
  anchored anonymous checks.

## Issue #155 summary

- `GET .../commits/main?cursor=2147483648` (2^31) → **400**
  `{"error":"invalid input"}`.
- `GET .../commits/main?cursor=2147483647` (2^31-1) → **200** with
  `items: []` and `next: null`.
- 2^32, 2^64, `-1`, `abc`, `1.5`, and an empty cursor are all 400.
- 153 commits paginate as 50+50+50+3 (`limit=50`) and 100+53 (`limit=100`),
  newest first, no duplicates; the last page has `next: null`.
- UI `/commits/main` shows exactly 100 items plus “Showing the first 100
  commits; older history is not listed.”; a 3-commit branch shows the notice
  absent.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A01 | commit cursor 2^31 -> 400 | 400 invalid input | 400 {"error":"invalid input"} | PASS |
| A02 | commit cursor 2^31-1 -> 200 empty, no next | 200 items=[] next=null | 200 items=0 next=None | PASS |
| A03 | commit cursor 2^32 -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| A04 | commit cursor 2^64 -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| A05 | commit cursor -1 -> 400 | 400 | 400 | PASS |
| A06 | commit cursor abc -> 400 | 400 | 400 | PASS |
| A07 | commit cursor 1.5 -> 400 | 400 | 400 | PASS |
| A08 | commit cursor empty -> 400 | 400 | 400 | PASS |
| A09 | commits default page is 30 with next cursor | 200, 30 items, next=30 | 200 items=30 next=30 | PASS |
| A10 | limit=50 pagination covers all commits, no dupes | 153 unique commits | pages=4 seen=153 unique=153 expected=153 | PASS |
| A11 | limit=100 pagination covers all commits, no dupes | 153 unique commits | pages=2 seen=153 unique=153 expected=153 | PASS |
| A12 | 100-commit page boundary splits 153 into 100+53 | 100 then 53, next null | p1=100 next=100 p2=53 next2=None | PASS |
| A13 | cursor at/after end yields tail/empty, not 500 | 200 with 1/0/0 items | 152:200/1 153:200/0 100000:200/0 | PASS |
| A14 | cursor on short branch works | 200, 3 items, next null | 200 items=3 | PASS |
| A15 | limit=0 -> 400 | 400 | 400 | PASS |
| A16 | limit=101 -> 400 | 400 | 400 | PASS |
| A17 | limit=abc -> 400 | 400 | 400 | PASS |
| A18 | unknown revision -> 404 | 404 revision not found | 404 {"error":"revision not found"} | PASS |
| A19 | rev with .. -> 400 | 400 | 400 | PASS |
| B01 | commits UI on 153 commits shows 100 + cap notice | 200, 100 items, notice | 200 items=100 notice=True | PASS |
| B02 | commits UI on 3 commits shows no cap notice | 200, 3 items, no notice | 200 items=3 notice=False | PASS |
| B03 | commits UI at exactly 100 shows cap notice | 200, 100 items, notice | 200 items=100 notice=True | PASS |
| B04 | commits UI unknown rev -> styled 404 | 404 html | 404 | PASS |
| B05 | commits UI has breadcrumb to repo and tree | breadcrumb links | 200 | PASS |
| C01 | unmatched URL -> styled 404 with CSP/XFO | 404 html + CSP + XFO | 404 styled=True headers=all present | PASS |
| C02 | authenticated unmatched URL -> styled 404 | 404 html | 404 styled=True headers=all present | PASS |
| C03 | missing repo -> styled 404 (auth) | 404 html | 404 styled=True headers=all present | PASS |
| C04 | missing tree path -> styled 404 | 404 html | 404 styled=True headers=all present | PASS |
| C05 | non-hex commit id UI -> styled 404 not 400 | 404 html | 404 styled=True headers=all present | PASS |
| C06 | unknown commit UI -> styled 404 | 404 html | 404 styled=True headers=all present | PASS |
| D01 | tree page carries CSP/XFO/nosniff | CSP frame-ancestors none, XFO DENY, nosniff, no-store | 200 all present | PASS |
| D02 | blob page carries CSP/XFO/nosniff | CSP frame-ancestors none, XFO DENY, nosniff, no-store | 200 all present | PASS |
| D03 | commits page carries CSP/XFO/nosniff | CSP frame-ancestors none, XFO DENY, nosniff, no-store | 200 all present | PASS |
| D04 | commit page carries CSP/XFO/nosniff | CSP frame-ancestors none, XFO DENY, nosniff, no-store | 200 all present | PASS |
| D05 | compare page carries CSP/XFO/nosniff | CSP frame-ancestors none, XFO DENY, nosniff, no-store | 200 all present | PASS |
| D06 | repository page carries CSP/XFO/nosniff | CSP frame-ancestors none, XFO DENY, nosniff, no-store | 200 all present | PASS |
| E01 | tree table has a caption | caption Files at main | 200 | PASS |
| E02 | compare table has a caption and scoped headers | caption Commits in this comparison | 200 | PASS |
| E03 | tree subdirectory caption names revision | caption Files at main | 200 | PASS |
| F01 | tree breadcrumb links repo, path segments and Commits | breadcrumb + files link | 200 present=['href="/organizations/lab03/repositories/r3-main/tree/main/docs"', 'href="/organizations/lab03/repositories/r3-main/commits/main"', '>docs</a>'] | PASS |
| F02 | blob breadcrumb links repo, tree and file | 3 breadcrumb links | 200 present=['href="/organizations/lab03/repositories/r3-main"', 'href="/organizations/lab03/repositories/r3-main/tree/main"', 'docs/guide.md'] | PASS |
| F03 | commits breadcrumb links repo and tree | breadcrumb links | 200 present=['href="/organizations/lab03/repositories/r3-main"', 'href="/organizations/lab03/repositories/r3-main/tree/main"'] | PASS |
| F04 | commit breadcrumb links repo and Commits | breadcrumb links | 200 present=['href="/organizations/lab03/repositories/r3-main"', 'href="/organizations/lab03/repositories/r3-main/commits/e0a6417e9aeb29cfb957e7093ff2d1b75a3... | PASS |
| F05 | compare breadcrumb links repo | breadcrumb | 200 present=['href="/organizations/lab03/repositories/r3-main"'] | PASS |
| G01 | repository page links to tree, /commits/{rev}, compare | 3 default-branch links | 200 links=['/tree/main"', '/commits/main"', '/compare/main...main"'] | PASS |
| G02 | tree page links to /commits/{rev} | commits link with rev | 200 | PASS |
| G03 | tree commits link follows tag revision | commits/v1.0.0 link | 200 | PASS |
| H01 | empty repo clone URL is absolute (auth) | absolute http clone | 200 clone_abs=True browse=False | PASS |
| H02 | empty public repo clone URL anonymous | absolute http clone | 200 clone_abs=True browse=False | PASS |
| H03 | empty repo has no browse-files link | no Browse files | 200 | PASS |
| I01 | anonymous public repo page 200 with browse links | 200 + Browse files | 200 needle=yes | PASS |
| I02 | anonymous public tree 200 | 200 | 200 | PASS |
| I03 | anonymous public blob 200 with content | 200 + public fixture | 200 needle=yes | PASS |
| I04 | anonymous public commits 200 | 200 | 200 | PASS |
| I05 | anonymous public compare 200 | 200 | 200 | PASS |
| I06 | anonymous public page has no sign-out form | no /logout form | 200 | PASS |
| I07 | anonymous private repo redirects to /login | 303 /login | 303 /login | PASS |
| I08 | anonymous private tree redirects to /login | 303 /login | 303 /login | PASS |
| I09 | anonymous acme/web (private fixture) redirects | 303 /login | 303 /login | PASS |
| I10 | anonymous lab01/t01-public (public fixture) 200 | 200 | 200 | PASS |
| I11 | anonymous JSON tree API -> 401 | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| I12 | authenticated private repo 200 | 200, private visibility | 200 visibility=private | PASS |
| J01 | path traversal UI /tree/main/%2e%2e -> styled 404 | 404 html no leak | 404 styled=True headers=all present | PASS |
| J02 | path traversal UI /tree/main/docs/%2e%2e/%2e%2e/etc -> styled 404 | 404 html no leak | 404 styled=True headers=all present | PASS |
| J03 | path traversal UI /blob/main/..%2F..%2Fetc%2Fpasswd -> styled 404 | 404 html no leak | 404 styled=True headers=all present | PASS |
| J04 | path traversal UI /tree/main/..%5C..%5Cetc%5Cpasswd -> styled 404 | 404 html no leak | 404 styled=True headers=all present | PASS |
| J05 | path traversal UI /tree/main..feature -> styled 404 | 404 html no leak | 404 styled=True headers=all present | PASS |
| J06 | path traversal UI /tree/-main -> styled 404 | 404 html no leak | 404 styled=True headers=all present | PASS |
| J07 | path traversal API /tree/main?path=.. -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J08 | path traversal API /tree/main?path=a/../b -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J09 | path traversal API /tree/main?path=/etc/passwd -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J10 | path traversal API /tree/main?path=%2e%2e%2fetc -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J11 | path traversal API /blob/main?path=.. -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J12 | path traversal API /blob/main?path=docs/../../etc/passwd -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J13 | path traversal API /blob/main?path=/etc/passwd -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J14 | path traversal API /blob/main?path=README%00.md -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| J15 | path traversal API /tree/%2e%2e -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| K01 | binary blob flagged binary with base64, no text | binary=true base64 set | 200 binary=True text=None b64=True size=8 | PASS |
| K02 | large 2 MiB blob truncated at cap | truncated=true size=2097152 | 200 truncated=True size=2097152 text_len=1048576 | PASS |
| K03 | blob page hides binary content | notice, no raw bytes | 200 | PASS |
| K04 | blob page hides truncated large content | notice with 2097152 | 200 | PASS |
| K05 | blob page renders text file in pre | pre content | 200 | PASS |
| K06 | blob API missing path -> 400 | 400 | 400 | PASS |
| K07 | blob API directory path -> 404 | 404 | 404 | PASS |
| L01 | tree API marks symlink kind | link kind=symlink | 200 kinds={'escape': 'symlink', 'link': 'symlink'} | PASS |
| L02 | symlink blob returns link text, not target content | text docs/guide.md | 200 text='docs/guide.md' | PASS |
| L03 | absolute symlink blob returns link text, not /etc/passwd | text /etc/passwd | 200 text='/etc/passwd' | PASS |
| L04 | symlink renders as text in blob page, no leak | pre /etc/passwd | 200 | PASS |
| L05 | tree UI shows symlink type | symlink row | 200 | PASS |
| M01 | tree page escapes XSS filename and encodes href | escaped + percent-encoded | {"tree filename escaped": true, "tree href encoded": true, "space href encoded": true} | PASS |
| M02 | commits UI escapes script in commit summary | escaped, no raw script | escaped=True raw=False | PASS |
| M03 | commit page escapes script in message | escaped, no raw script | 200 escaped=True | PASS |
| M04 | blob page escapes script in file content | escaped in pre | escaped=True | PASS |
| M05 | repository description escaped on repo page | escaped description | escaped=True | PASS |
| M06 | compare page escapes branch name in heading/form | escaped branch, no raw tag | 200 escaped=True raw=False | PASS |
| N01 | compare API main...feature -> 200 up_to_date | 200 up_to_date ahead=0 | 200 merge=up_to_date ahead=0 behind=47 | PASS |
| N02 | compare API feature...main -> 200 clean | 200 clean ahead=47 | 200 merge=clean ahead=47 behind=0 | PASS |
| N03 | compare API main...main -> 200 up_to_date | 200 up_to_date ahead=0 | 200 merge=up_to_date ahead=0 behind=0 | PASS |
| N04 | compare API main...orphan -> 200 unrelated | 200 unrelated ahead=1 | 200 merge=unrelated ahead=1 behind=153 | PASS |
| N05 | compare API malformed main..feature -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N06 | compare API malformed a...b...c -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N07 | compare API malformed ... -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N08 | compare API malformed -x...main -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N09 | compare API malformed main...a..b -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N10 | compare API malformed main -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N11 | compare API malformed main... -> 400 | 400 | 400 {"error":"invalid input"} | PASS |
| N12 | compare API unknown ref -> 404 | 404 | 404 {"error":"revision not found"} | PASS |
| N13 | compare form redirects to canonical spec | 303 canonical URL | 303 loc=/organizations/lab03/repositories/r3-main/compare/main...feature | PASS |
| N14 | compare form with empty base -> styled 404 | 404 html | 404 styled=True headers=all present | PASS |
| N15 | compare UI page shows ahead/behind and caption | 45 ahead text + caption | 200 | PASS |
| N16 | compare UI up-to-date text | Up to date | 200 | PASS |
| N17 | compare UI unrelated histories text | Unrelated histories | 200 | PASS |
| N18 | compare UI unknown ref -> styled 404 | 404 html | 404 styled=True headers=all present | PASS |
| N19 | compare UI malformed spec -> styled 404 | 404 html | 404 styled=True headers=all present | PASS |
| O01 | commit API tip 200 with files | 200 id=tip files>0 | 200 id_match=True files=1 truncated=False | PASS |
| O02 | commit API uppercase sha 200 | 200 same id | 200 | PASS |
| O03 | commit API initial commit 200 with binary notice | 200, a binary file entry | 200 files=8 binary=1 trunc=True | PASS |
| O04 | commit API short sha -> 400 | 400 | 400 | PASS |
| O05 | commit API non-hex -> 400 | 400 | 400 | PASS |
| O06 | commit API 40 zeros -> 404 | 404 revision not found | 404 {"error":"revision not found"} | PASS |
| O07 | commit API 64-hex in sha1 repo -> 404 | 404 | 404 | PASS |
| O08 | commit UI tip 200 with diff | 200 + pre diff | 200 | PASS |
| O09 | commit UI initial shows Binary file notice | Binary file: | 200 | PASS |
| O10 | commit UI uppercase sha 200 | 200 | 200 | PASS |
| P01 | refs API lists branches and tag | branches + v1.0.0 | 200 branches=6 tags=['v1.0.0'] | PASS |
| P02 | tree API root id is commit sha with entries | id=tip, README entry | 200 id_match=True entries=11 | PASS |
| P03 | tree API tag resolves to tagged commit | id=tag target | 200 id_match=True | PASS |
| P04 | tree API commit sha resolves | id=tip | 200 | PASS |
| P05 | tree API subdirectory lists guide.md | guide.md blob | 200 ['guide.md'] | PASS |
| P06 | tree API missing path -> 404 | 404 | 404 | PASS |
| P07 | tree UI subdirectory breadcrumb has nested path link | docs segment link | 200 | PASS |
| Q01 | health live 200 | 200 | 200 | PASS |
| Q02 | health ready 200 | 200 | 200 | PASS |
| R01 | commits UI escapes branch name in header | escaped xss<b> | 200 escaped=True | PASS |
| R02 | tree caption escapes branch name | escaped caption | 200 escaped=True | PASS |
| R03 | commit diff escapes script text in patch | escaped patch | 200 escaped=True | PASS |
| R04 | API backslash traversal path is 404, not a read | 404 | 404 {"error":"not found"} | PASS |
| R05 | styled 404 does not reflect the request path | 404, no reflection | 404 reflected=False | PASS |

### Evidence for content checks

- Captions observed: `<caption class="muted">Files at main</caption>` on
  `/tree/main` and `/tree/main/docs`; `<caption class="muted">Commits in this
  comparison</caption>` on `/compare/main...feature`, both tables with
  `<th scope="col">` headers.
- 404 shell observed on `/this/does/not/exist` and every traversal URL:
  status 404, `<!doctype html>`, `<h1>Not found</h1>`,
  `content-security-policy: default-src 'none'; ... frame-ancestors 'none'`,
  `x-frame-options: DENY`, `x-content-type-options: nosniff`,
  `cache-control: no-store`; the injected `<script>` in the requested path was
  not reflected.
- Empty repository body:
  `<p>This repository is empty. Clone it with:</p><pre>git clone http://127.0.0.1:8080/lab03/r3-empty.git</pre>`
  and no “Browse files” link.
- Binary page: `Binary or truncated file (8 bytes); content is not shown.`;
  large page: `Binary or truncated file (2097152 bytes); content is not shown.`;
  text page: `<pre># r3-main ...`.
- Symlink page for `escape` renders `<pre>/etc/passwd</pre>` and no `root:`
  content from the host file.
- XSS: the tree row shows `&lt;img src=x onerror=alert(1)&gt;.txt` with href
  `%3Cimg%20src%3Dx%20onerror%3Dalert%281%29%3E.txt`; commits and commit pages
  show `&lt;script&gt;alert(&#39;xss-commit&#39;)&lt;/script&gt;`; the
  `xss.txt` blob, its diff, the repository description and the `xss<b>`
  compare/tree/commits headers are all escaped, with no raw tag in the body.


## Defects

None found in the product. Every assigned check and every browse regression
case passed. Six failures in an earlier harness pass were defects in the
harness, not the server, and were corrected before the final run:

- A13 unpacked the harness's `(response, json)` pair one level short.
- F01 expected a link to the repository root in the tree breadcrumb; that
  breadcrumb intentionally starts at the tree at the revision and then adds
  path segments (`browse_ui.rs:232-255`).
- I12 compared the capitalised word “Private”; the page renders the
  visibility value lower-case.
- K01 read `text` from a JSON object that omits `text` when it is `None`
  (`browse.rs:319`, `skip_serializing_if`).
- K03 expected 9 bytes for an 8-byte binary fixture.
- M03 opened the tip commit instead of the commit carrying the script tag.

Each corrected case was re-run; the final suite ran 138/138 pass with no
flaky result (the harness re-runs any failure once; none occurred).

## Notes

- Cursor guard: `crates/githearth-server/src/browse.rs:420-428` parses the
  cursor as `u64` and rejects anything above `i32::MAX` as `invalid()` before
  calling Git; `browse.rs:416-419` bounds `limit` to 1..=100.
- Styled 404: `ui.rs:240-257` is the router fallback and
  `ui.rs:975-981` adds `Content-Security-Policy` (with `frame-ancestors
  'none'`), `X-Frame-Options: DENY`, and `no-store` to every UI response,
  including the fallback. Unmatched URLs return 404 with
  `<h1>Not found</h1>` in the rendered shell; the request path is not
  reflected.
- Tables: tree uses `<caption class="muted">Files at {rev}</caption>` and
  scoped column headers (`browse_ui.rs:295-301`); compare uses
  “Commits in this comparison” (`browse_ui.rs:596-598`).
- Breadcrumbs: tree `browse_ui.rs:232-255`, blob `341-347`, commits
  `399-404`, commit `475-481`, compare `584-587`.
- Repository page links “Browse files”, “Commit history” and “Compare” at the
  detected default branch, including `/commits/{rev}` (`ui.rs:711-732`); the
  tree page links `/commits/{rev}` for the revision it is showing
  (`browse_ui.rs:252-255`).
- Empty repositories render a single absolute clone URL built from
  `X-Forwarded-Proto`/`Host` (`ui.rs:626-653`, `735-738`); observed
  `git clone http://127.0.0.1:8080/lab03/r3-empty.git`.
- Anonymous browsing: a public repository is resolved by
  `load_public_anonymous` (`browse_ui.rs:93-98`, `ui.rs:685-690`) and renders
  repo, tree, blob, commits and compare pages; a private repository redirects
  303 to `/login` at every one of those routes. The JSON tree API still
  requires a bearer token (401 anonymously).
- Path safety: `valid_rev`/`valid_path` (`githearth-git/src/browse.rs:28-44`)
  plus the repository-scoped `GIT_LITERAL_PATHSPECS=1` Git invocation reject
  `..`, absolute paths, NUL and `a..b`; encoded and backslash traversal
  attempts return 404 (UI/API tree/blob) or 400 (API validation).
- Blobs: content is capped at `MAX_BLOB_BYTES` = 1 MiB
  (`githearth-git/src/browse.rs:16`, `405-407`); a 2 MiB file reports
  `truncated: true` and the UI shows the size notice instead of content
  (`browse_ui.rs:327-335`). Binary blobs return base64 in the API and a notice
  in the UI. Symlinks are read as blobs containing their target text (e.g.
  `/etc/passwd`), never followed on the server.
- Escaping: every interpolated value goes through `ui::esc`, and links through
  `ui::url_encode`; script tags in file names, file contents, commit
  messages, branch names, compare specs and descriptions are rendered escaped,
  with no raw `<script>` or `<img onerror>` in the response.
- Compare: only `<base>...<head>` with both sides passing `valid_rev` is
  accepted (`browse.rs:161-167`); single-dot, extra separators, empty sides,
  and leading `-` return 400, unknown refs 404. `main...feature` is
  `up_to_date` (0 ahead / 47 behind), `feature...main` clean (47 ahead),
  `main...main` up to date, and `main...orphan` unrelated.
- Commit ids: exactly 40 or 64 hex characters are accepted
  (`browse.rs:196-198`); a short/non-hex id is 400 on the API and a styled 404
  in the UI, a missing 40-hex id is 404 `revision not found`, and uppercase
  hex resolves.
- Observations, not defects: (1) the tree breadcrumb's first crumb points at
  the tree for the revision rather than the repository landing page, unlike
  the other browse breadcrumbs; (2) the UI commits page has no cursor or
  `limit` query support and intentionally stops at 100 with a notice; (3)
  `absolute_clone_url` trusts the `Host` header, so a forged Host is
  reflected into the copyable clone URL (escaped, and only shown to someone
  who can read the repository).
