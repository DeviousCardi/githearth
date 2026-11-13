# Repository browsing, diffs and compare — M4 lab acceptance report

Tester: t06. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 95 executed, 86 pass, 9 fail, 6 skipped.

## Fixtures

All work was in `lab06`; `acme`, `beta` and other lab orgs were not touched. No app restart, rebuild or migration.

- `lab06/t06-browse` (private): 6 commits on `main`; branches `feature`, `docs`, `orphan` (unrelated root), `release/v1`, `ünicode-branch`, `longlog` (106 commits), `manycommits` (261 commits), `conflict-a`, `conflict-b`, `hugetree` (20 000 files), and `x<svg/onload=alert(3)>`. Annotated tag `v1.0.0`, lightweight tags `lw-1` and `v/1.0`.
- `lab06/t06-public` (public): `main` only; same content as the `t06-browse` big commit.
- `lab06/t06-empty` (private): no commits.
- Content includes nested dirs; names with spaces, unicode, `%`, and `<script>alert(1).txt`; a symlink to a repo file and one to `/etc/passwd`; empty and no-final-newline files; CRLF; invalid-UTF-8 Latin-1; 6-byte binary; 1.1 MB text file; a 1 MiB+ diff; 310-file and 20 000-file directories; and `<img src=x onerror=alert(1)>` plus `<script>` file content.

Repros below assume `TOKEN` is the t06 bearer from `POST /api/v1/login` and `COOKIE` is the t06 browser session cookie from the `/login` form.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A01 | refs lists branches, tags and default branch | 200 JSON; default_branch main; feature/release/v1/unicode branches; v1.0.0 and lw-1 tags | 200 default=main branches=12 tags=['lw-1', 'v/1.0', 'v1.0.0'] | Pass |
| A02 | anonymous refs rejected | 401 JSON | 401 application/json {"error":"invalid or expired credentials"} | Pass |
| A03 | non-member refs hidden | 404 JSON | 404 {"error":"not found"} | Pass |
| A04 | empty repository refs | 200; default main; no branches/tags | 200 {'default_branch': 'main', 'branches': [], 'tags': []} | Pass |
| A05 | root tree lists directories first with kinds and sizes | 200; directories before files; docs tree; bin.dat blob size 6; symlink kind | 200 n=22 first=['dir with space', 'docs', 'many'] firstKind=tree bin=6 link=symlink | Pass |
| A06 | subdirectory and nested tree | 200; docs has nested+guide.md; docs/nested has inner.txt | 200 ['nested', 'guide.md']; nested 200 ['inner.txt'] | Pass |
| A07 | branch, tag, commit-sha, HEAD and short-sha refs | all 200; annotated tag shows old tree; lightweight tag/C2 show renamed.md | feature=200; v1.0.0=200; lw-1=200; 71670b5=200; HEAD=200; 5ea6722=200; 7a2e192=200 tagOld=True lightNew=True | Pass |
| A08 | slashed and unicode branch/tag names | release/v1, unicode branch and v/1.0 tree all 200 | {'release/v1': (200, 17), 'ünicode-branch': (200, 17), 'v/1.0': (200, 16)} | Pass |
| A09 | space, unicode, nested and 310-entry paths | all 200; many has 310 entries; no pagination | {'dir with space': (200, 2), 'ünicode-dir': (200, 1), 'docs/nested': (200, 1), 'many': (200, 310)} | Pass |
| A10 | path that is a file, and unknown path | 404 for both | file=404; missing=404 | Pass |
| A11 | unknown revision | 404 with error revision not found | 404 {"error":"revision not found"} | Pass |
| A12 | invalid revision syntax | 400 for a..b and -foo | {'a..b': 400, '-foo': 400} | Pass |
| A13 | path traversal and invalid paths rejected | 400 for .., encoded .., a/../b, absolute, NUL, trailing/empty | {'..': (400, '{"error":"invalid inpu'), '%2e%2e': (400, '{"error":"invalid inpu'), 'a/../b': (400, '{"error":"invalid inpu'), '/etc/passwd': (400, '{"error":"invalid inpu'), 'a%... | Pass |
| A14 | Windows-style traversal is a literal missing path | 404 not 500 and no escape | 404 {"error":"not found"} | Pass |
| A15 | overlong path rejected | 400 | 400 {"error":"invalid input"} | Pass |
| A16 | 20 000-entry tree is bounded | 413 response too large (bounded, no pagination) | 413 {"error":"response too large"} | Pass |
| A17 | tree of empty repository | 404 revision not found | 404 {"error":"revision not found"} | Pass |
| A18 | tree access control | non-member 404; anonymous 401 | nonmember=404; anon=401 | Pass |
| B01 | text blob | 200 JSON; renamed.md text 'hello world\n'; size 12 | 200 size=12 text='hello world\n' | Pass |
| B02 | empty file and no trailing newline | size 0 text ""; no-newline preserved | empty=0/'' nonewline='no newline at end' | Pass |
| B03 | space and unicode paths | all 200 with exact text | {'space name.txt': (200, 'space file\n'), 'ünicode-dir/ünicode-ü.txt': (200, 'unicode file\n'), 'dir with space/space.txt': (200, 'space dir file\n')} | Pass |
| B04 | binary blob | 200; binary true; base64 x\0y\0z\0; no text | 200 binary=True size=6 decoded=b'x\x00y\x00z\x00' | Pass |
| B05 | large text blob truncated at 1 MiB | 200; size 1100000; truncated true; 1 MiB content | 200 size=1100000 truncated=True textlen=1048576 | Pass |
| B06 | invalid UTF-8 text is base64, not binary | 200; binary false; no text; base64 preserves bytes | 200 binary=False text=None b64bytes=b'\xff\xfe latin1 \xe9\n' | Pass |
| B07 | HTML file content is returned as JSON data | 200; raw markup in JSON text (inert) | 200 text='<img src=x onerror=alert(1)>\n<script>alert(2)</script>\n' | Pass |
| B08 | symlink blobs return the target path, never file contents | text is target path; no /etc/passwd contents | guide='docs/guide.md' passwd='/etc/passwd' | Pass |
| B09 | directory and missing blob path | 404 for both | dir=404; missing=404 | Pass |
| B10 | missing query, invalid revision, unknown revision | 400; 400; 404 | {'/blob/main': 400, '/blob/a..b?path=x': 400, '/blob/nope?path=x': 404} | Pass |
| B11 | blob of empty repository | 404 revision not found | 404 {"error":"revision not found"} | Pass |
| C01 | main commit log newest first | 200; 6 commits; tip main; next null | 200 n=6 first=1eda07ff last=7a2e1929 next=None | Pass |
| C02 | cursor pagination without duplicates or omissions | 2 per page; 3 pages; 6 unique; next advances | pages=[(2, '2'), (2, '4'), (2, None)] unique=6 | Pass |
| C03 | limit bounds | 1 and 100 accepted; 0, 101, -1 -> 400 | {'1': 200, '100': 200, '0': 400, '101': 400, '-1': 400} | Pass |
| C04 | cursor syntax and past-end cursor | bad cursors 400; past end 200 empty next null | {'x': 400, '-1': 400, '18446744073709551616': 400}; pastEnd=200/0 | Pass |
| C05 | cursor above 2^31 wraps in git --skip | out-of-range cursor returns empty page, not the first page | 200 items=6 first=1eda07ff | **Fail** |
| C06 | path-filtered log | 2 commits touching docs/guide.md; unknown path empty | guide=['4e2c95e3', '7a2e1929'] missing=0 | Pass |
| C07 | 261-commit history paginates completely | 3 pages; 261 unique commits | pages=3 seen=261 unique=261 | Pass |
| C08 | root commit diff against empty tree | 200; no parents; 17 files; A statuses; binary flag; raw markup in JSON patch | 200 parents=0 files=17 xss=True bin=True | Pass |
| C09 | rename detection | R100 README.md -> renamed.md | 200 R100 README.md->renamed.md | Pass |
| C10 | merge commit detail and first-parent diff | 2 parents; diff shows docs/guide.md + feature.txt | 200 parents=['71670b54', '4e2c95e3'] files=['docs/guide.md', 'feature.txt'] | Pass |
| C11 | 1 MiB patch and 300-file cap | 300 files; truncated true; patch <=1 MiB; message raw in JSON | 200 files=300 truncated=True patchlen=1048577 msglen=47 | Pass |
| C12 | author and committer metadata | name/email/time populated and equal | {'name': 'T06 Tester', 'email': 't06@example.test', 'time': 1767225600} / {'name': 'T06 Tester', 'email': 't06@example.test', 'time': 1767225600} | Pass |
| C13 | unknown, short and non-hex commit ids | 404; 400; 400 | {'00000000': 404, '7a2e192': 400, 'xyz': 400} | Pass |
| C14 | annotated tag object id peels to its commit | 200; returned id is the commit | 200 id=7a2e1929 | Pass |
| C15 | commit detail of empty repository | 404 | 404 {"error":"revision not found"} | Pass |
| D01 | main...feature where feature is an ancestor | up_to_date; 0 ahead 3 behind; empty commits/files | 200 mb=4e2c95e3 ahead=0 behind=3 {'status': 'up_to_date', 'conflicts': []} files=0 | Pass |
| D02 | feature...main fast-forward with huge diff | clean; 3 ahead; 300 files truncated; 3 commits | 200 ahead=3 behind=0 {'status': 'clean', 'conflicts': []} files=300 trunc=True commits=3 | Pass |
| D03 | identical refs | up_to_date; zero ahead/behind; empty diff | 200 {'status': 'up_to_date', 'conflicts': []} files=0 commits=0 | Pass |
| D04 | unrelated histories | 200; merge_base null; mergeable unrelated; bounded diff | 200 mb=None {'status': 'unrelated', 'conflicts': []} files=300 trunc=True | Pass |
| D05 | conflicting branches | conflicts ["conflict.txt"] | 200 {'status': 'conflicts', 'conflicts': ['conflict.txt']} | Pass |
| D06 | tag, HEAD, slashed and unicode refs | all 200 | {'v1.0.0...main': (200, 'clean'), 'HEAD...main': (200, 'up_to_date'), 'release%2Fv1...main': (200, 'clean'), '%C3%BCnicode-branch...main': (200, 'clean')} | Pass |
| D07 | unknown head revision | 404 revision not found | 404 {"error":"revision not found"} | Pass |
| D08 | malformed specs | 400 for no/partial/multiple separators and two-dot | {'main': (400, '{"error":"invalid input"}'), 'main..feature': (400, '{"error":"invalid input"}'), 'main...': (400, '{"error":"invalid input"}'), '...main': (400, '{"error":"inva... | Pass |
| D09 | four-dot spec boundary | 400 or 404; never 500 (returns 404 revision not found) | 404 {"error":"revision not found"} | Pass |
| D10 | commit list capped at 250 | 250 commits; commits_truncated true | 200 ahead=260 commits=250 truncated=True | Pass |
| D11 | huge diff response bounded | 200; <2 MB; truncated true | 200 bytes=1103724 | Pass |
| D12 | compare on empty repo and access control | empty 404; non-member 404; anonymous 401 | empty=404 nonmember=404 anon=401 | Pass |
| E01 | repository landing page | 200 HTML; title Repository; one h1; tree and compare links | 200 title=Repository h1=['t06-browse'] treeLink=True compareLink=True | Pass |
| E02 | repo -> tree -> blob links work | tree 200; blob link followed 200 with file text | tree=200 blobLinks=18 followed=200 pre=True | Pass |
| E03 | compare -> commit link works | compare 200; commit link followed 200 (title Commit) | compare=200 commitLinks=3 followed=200 title=Commit | Pass |
| E04 | tree page structure, landmark and escaping | 200; Tree; one h1; scoped headers; breadcrumb; lang; no script/img | 200 title=Tree h1=['t06-browse'] th=3 breadcrumb=True lang=True scripts=0 | Pass |
| E05 | tree UI for branch/tag/sha/HEAD/slashed/unicode refs | all 200 | {'feature': 200, 'v1.0.0': 200, 'lw-1': 200, '71670b54928a494d6968c5ae28602f663ef2059c': 200, 'HEAD': 200, 'release/v1': 200, 'ünicode-branch': 200, 'v/1.0': 200} | Pass |
| E06 | space/unicode directories and encoded XSS filename round-trip | all 200; script filename link followed and escaped | {'dir with space': (200, 2263), 'ünicode-dir': (200, 2133), 'docs/nested': (200, 2180)}; scriptLinkFollowed=200 escaped=True | Pass |
| E07 | blob text page | 200; title File; one h1; escaped pre; byte size | 200 title=File h1=['renamed.md'] pre=True | Pass |
| E08 | HTML file content escaped in blob view | 200; escaped img/script; no raw tags | 200 escapedImg=True rawImg=False scripts=0 | Pass |
| E09 | binary and truncated blob notices | both 200 with byte-size notice; no content pre | bin=200 notice=True large=200 notice=True | Pass |
| E10 | empty and no-newline files | empty pre; exact text without added newline | empty=200/True nonewline=200/True | Pass |
| E11 | commit list page | 200; title Commits; 6 commit links | 200 title=Commits commitLinks=6 | Pass |
| E12 | 106-commit history has no pagination control | all 106 reachable or a next link; no silent cap | 200 links=100 total=106 pagination=[] | **Fail** |
| E13 | commit page escapes filename and file content | 200; one h1; no raw script; escaped markup; 17 file blocks | 200 title=Commit scripts=0 escaped=True pre=17 | Pass |
| E14 | commit message XSS and huge diff notice | 200; message escaped; Diff output truncated.; bounded | 200 bytes=1053711 scripts=0 escapedMsg=True truncated=True | Pass |
| E15 | merge commit page | 200; first-parent diff shown | 200 title=Commit feature=True | Pass |
| E16 | compare page structure | 200; title Compare; one h1; Files changed h2; scoped headers | 200 title=Compare h1=['Compare main...feature'] h2=True th=True | Pass |
| E17 | compare form redirect and validation | 303 to canonical; missing head -> 404 | form=303 loc=/organizations/lab06/repositories/t06-browse/compare/main...feature missingHead=404 | Pass |
| E18 | compare states in HTML | identical Up to date; unrelated; conflict path listed | {'main...main': (200, True, False, False), 'main...orphan': (200, False, True, False), 'conflict-a...conflict-b': (200, False, False, True)} | Pass |
| E19 | branch name reflected into compare heading is escaped | 200; &lt;svg... in h1; no raw <svg | 200 escaped=True raw=False | Pass |
| E20 | huge compare page bounded | 200; <2 MB; truncation notice | 200 bytes=1054773 notice=True | Pass |
| E21 | styled 404 for missing org/repo/path/ref/commit | all 404 HTML with title and h1 Not found | nope=404/Not found; nope=404/Not found; nope=404/Not found; 0000000000000000000000000000000000000000=404/Not found; main...nope=404/Not found; main=404/Not found; nope=404/Not f... | Pass |
| E22 | trailing slash / missing rev gives bare 404 | 404 styled page with security headers | {'/tree/main/': (404, '', 0, None), '/blob/main/': (404, '', 0, None), '/commits/main/': (404, '', 0, None), '/tree': (404, '', 0, None), '/compare/main...main/': (404, '', 0, N... | **Fail** |
| E23a | anonymous browsing of a public repository | tree/blob/commits/compare all 200 | tree=303 blob=303 commits=303 compare=303 | **Fail** |
| E23b | anonymous private repository browse (brief expects 404) | 404 for private tree/blob | privateTree=303 privateBlob=303 | **Fail** |
| E24 | public repository landing page for anonymous | 200; public name; Browse files link | 200 h1=['t06-public'] treeLink=True | Pass |
| E25 | session role matrix for private repo | anonymous 303 login; non-member 404; read member 200; owner 200 | {('/tree/main', 'anon'): 303, ('/', 'anon'): 303, ('/tree/main', 't01'): 404, ('/tree/main', 'erin'): 200, ('/tree/main', 't06'): 200} | Pass |
| E26 | non-member can browse a public repository when signed in | UI and API 200 | ui=200 api=200 | Pass |
| E27 | empty repository page | 200; empty notice and clone instructions | 200 emptyNotice=True clone=True | Pass |
| E28 | path traversal attempts in UI paths | all styled 404; no 500; no file contents | {'/tree/main/%2e%2e': (404, 'Not found', False), '/tree/main/%2e%2e%2f%2e%2e%2fetc%2fpasswd': (404, 'Not found', False), '/blob/main/%2e%2e%2fetc%2fpasswd': (404, 'Not found', F... | Pass |
| E29 | file as tree, directory as blob, trailing slash on subpath | all styled 404 | {'/tree/main/renamed.md': (404, 'Not found', 1320), '/blob/main/docs': (404, 'Not found', 1320), '/tree/main/docs/': (404, 'Not found', 1320), '/blob/main/renamed.md/': (404, 'N... | Pass |
| F01 | lang, single h1, main landmark and skip link on every browse page | all pages: lang en; exactly one h1; main; skip link | {'/': (200, 1, True, True, True), '/tree/main': (200, 1, True, True, True), '/blob/main/renamed.md': (200, 1, True, True, True), '/commits/main': (200, 1, True, True, True), '/c... | Pass |
| F02 | table column headers are scoped | tree 3 and compare 3 scoped headers | {'/tree/main': (3, True), '/compare/main...feature': (3, True)} | Pass |
| F03 | tables have captions (accessibility review claims all do) | <caption> present on tree and compare tables | {'/tree/main': (False, 1), '/compare/main...feature': (False, 1)} | **Fail** |
| F04 | drill-down pages carry breadcrumb navigation | blob/commits/commit/compare have nav aria-label Breadcrumb | {'/blob/main/renamed.md': False, '/commits/main': False, '/commit/7a2e1929b3896d0c8c53df72cbd2b33fb5d8e50d': False, '/compare/main...feature': False} | **Fail** |
| F05 | browse pages carry no-store, CSP, frame and sniffing headers | no-store; CSP; DENY; nosniff | {'cache-control': 'no-store', 'content-security-policy': "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'no... | Pass |
| F06 | no raw script or img tags survive in browse HTML | 0 raw <script>, 0 <img> across tree/blob/commit/compare | {'/tree/main': (0, 0), '/blob/main/xss.html': (0, 0), '/commit/7a2e1929b3896d0c8c53df': (0, 0), '/compare/x%3Csvg%2Fonload%3Dal': (0, 0)} | Pass |
| F07 | compare form inputs labelled | label for base and head | labels=(True, True) | Pass |
| F08 | repository page links to commit history | a /commits/{rev} link on the repository page | commitsLink=False links=[] | **Fail** |
| F09 | empty repository clone command is usable | host-qualified clone URL | clone command='/lab06/t06-empty.git' | **Fail** |
| S01 | Raw file/download endpoint | HTML/raw file response or documented absence | No route: `/raw/...` 404 (UI/API); browse API returns JSON/base64 | Skipped |
| S02 | Tag detail page | Tag metadata page | No route: `/tags/v1.0.0` 404; tags only in `/refs` and as revision names | Skipped |
| S03 | Blame / full history views | Blame or file-history page | No route: `/blame/...` 404; capability inventory puts blame/history in M9 | Skipped |
| S04 | Graft / shallow commit browsing | Grafted commit visible in log/diff | Not pushable through HTTP; merge commit covered (C10/E15) | Skipped |
| S05 | Line numbers / syntax highlighting | Line numbering or highlighted markup | Blob renders escaped plain `<pre>`; no line numbers or highlighting, no doc claim | Skipped |
| S06 | Branch/tag selector in UI | Ref picker on repo/tree page | No picker; non-default refs reachable by URL only (not promised in M2 docs) | Skipped |

## Defects

### Public repository browse pages redirect anonymous users to sign-in
- Severity: major
- Repro:
  ```sh
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-public/tree/main
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-public/blob/main/renamed.md
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-public/commits/main
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-public/compare/main...main
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-public  # 200 landing page
  GIT_TERMINAL_PROMPT=0 git ls-remote http://127.0.0.1:8080/lab06/t06-public.git  # works anonymously
  ```
- Expected: anonymous `GET` of a public repository's tree, blob, commits and compare pages returns `200` HTML, as the public landing page already does.
- Actual: every browse subpage returns `303` with `Location: /login`; only the repository landing page returns `200`. The public landing page's own "Browse files" link therefore leads to sign-in. Anonymous JSON browse (`/api/v1/.../tree/main`, `/refs`) is `401`.
- Evidence: `crates/githearth-server/src/browse_ui.rs:77-81` authenticates first and never attempts the anonymous public path; contrast `crates/githearth-server/src/ui.rs:631-637`, which does `load_public_anonymous` for the landing page. `docs/verification/m4-qualification.md:49` claims public UI reads for anonymous visitors, but the harness only checks the landing page (`scripts/verify-m4.py:1952-1954`). Private browse pages also return `303`; the brief expected `404`, and `scripts/verify-m4.py:1955-1958` asserts `303` for the private landing, so I treat the private redirect as intended design and the public failure as the defect. No existence leak: public, private and missing repositories all redirect identically when anonymous.
- Scope: systematic, all public repositories and all five browse page kinds for anonymous users.

### Commit history UI silently caps at 100 commits
- Severity: minor
- Repro:
  ```sh
  curl -s -b "githearth_session=$COOKIE" http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/commits/longlog | grep -o '/commit/[0-9a-f]\{40\}' | sort -u | wc -l
  ```
  `longlog` has 106 commits. The API paginates all of them:
  `GET /api/v1/organizations/lab06/repositories/t06-browse/commits/manycommits?limit=100` (261 commits) returns `next` cursors and three pages.
- Expected: all commits reachable, or a "next/older" link or an explicit "showing 100 of 106" notice.
- Actual: exactly 100 commit links, no pagination link and no truncation notice; commits 101-106 are unreachable from the UI.
- Evidence: `crates/githearth-server/src/browse_ui.rs:365-374` fetches `browse::MAX_LOG_LIMIT` (100) and renders the list; it never consumes a `next` cursor.
- Scope: every repository with more than 100 commits.

### Log cursor above 2^31 wraps and replays the first page
- Severity: minor
- Repro:
  ```sh
  for c in 2147483647 2147483648 4294967296 4294967297; do
    curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:8080/api/v1/organizations/lab06/repositories/t06-browse/commits/main?cursor=$c" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(len(d["items"]), d["items"][0]["summary"] if d["items"] else "-")'
  done
  ```
  Observed: `2147483647 -> 0 -`; `2147483648 -> 6 add space directory and percent file`; `4294967296 -> 6 ...`; `4294967297 -> 5 big commit ...` (skip 1).
- Expected: a cursor past the end returns an empty page (as `cursor=1000` does) or `400`; it must not return commits already seen.
- Actual: values at or above 2^31 are parsed as a negative 32-bit `--skip` and treated as zero, so the response restarts at the tip. A client that supplies a large cursor can receive duplicates.
- Evidence: `crates/githearth-server/src/browse.rs:420-423` parses an arbitrary `u64`; `crates/githearth-git/src/browse.rs:485-487` passes it as `--skip={after}`, and this Git build truncates it to a signed 32-bit value. Server-issued cursors stay small, so normal paging is unaffected.
- Scope: systematic for externally supplied cursors >= 2^31.

### Unmatched browse URLs return an empty bare 404 without the styled page or security headers
- Severity: minor
- Repro:
  ```sh
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/tree/main/
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/blob/main/
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/commits/main/
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/tree
  curl -si http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/compare/main...main/
  ```
- Expected: a styled HTML `404` page (like a missing repository or path), with `Content-Type: text/html`, CSP and `X-Frame-Options: DENY`; for API URLs a JSON error.
- Actual: `404`, `content-length: 0`, no `Content-Type`, no CSP, no `X-Frame-Options`. Defined missing-repository/missing-path routes return the 1 320-byte styled "Not found" page with all headers, so the failure is only for URLs that do not match a route, including common trailing slashes and `/tree` without a revision. API equivalents are also empty `404`s rather than JSON.
- Evidence: no catch-all fallback in `crates/githearth-server/src/ui.rs:924-943`, `crates/githearth-server/src/browse_ui.rs:24-54` or the top-level router at `crates/githearth-server/src/lib.rs:248-270`.
- Scope: every unmatched path under the UI/API routers (trailing slash, missing rev, typo, unimplemented subpage).

### Tree and compare tables have no `<caption>` although the accessibility review says all tables do
- Severity: minor
- Repro:
  ```sh
  curl -s -b "githearth_session=$COOKIE" http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/tree/main | grep -o '<table>\|<caption[^>]*>'
  curl -s -b "githearth_session=$COOKIE" http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/compare/main...feature | grep -o '<table>\|<caption[^>]*>'
  ```
  Both print only `<table>`.
- Expected: `docs/verification/accessibility.md:15` states "Tables add a `<caption>`"; both tables should carry an accessible caption.
- Actual: no caption on the tree table or the compare commit table. Column headers do have `scope="col"`, and the compare page has an `<h2>Files changed</h2>` before the table, so this is a documentation/accessibility gap rather than a data problem.
- Evidence: `crates/githearth-server/src/browse_ui.rs:286-291` and `:566-569`.
- Scope: both tables rendered by the browse UI.

### Blob, commits, commit and compare pages have no breadcrumb navigation
- Severity: minor
- Repro:
  ```sh
  for p in blob/main/renamed.md commits/main commit/7a2e1929b3896d0c8c53df72cbd2b33fb5d8e50d compare/main...feature; do
    curl -s -b "githearth_session=$COOKIE" "http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse/$p" | grep -o 'aria-label="[^"]*"'
  done
  ```
  Each prints only `aria-label="Primary"`; the tree page prints `Primary` and `Breadcrumb`.
- Expected: `docs/verification/accessibility.md:15` claims `<nav aria-label="Breadcrumb">` on drill-down pages.
- Actual: breadcrumbs are emitted only by `render_tree`; blob, commits, commit and compare pages have none.
- Evidence: `crates/githearth-server/src/browse_ui.rs:246` (tree only), handlers `:304-349`, `:351-407`, `:431-468`, `:502-589`.
- Scope: four browse page kinds.

### Repository and tree pages do not link to commit history
- Severity: minor
- Repro:
  ```sh
  curl -s -b "githearth_session=$COOKIE" http://127.0.0.1:8080/organizations/lab06/repositories/t06-browse | grep -o 'href="[^"]*"'
  ```
  Links are `/tree/main`, `/compare/main...main`, `/issues`, `/pulls`, `/settings/tags`; there is no `/commits/...` link. The tree page links only blobs and subdirectories.
- Expected: a visible "Commits"/"History" link to `/commits/{default-branch}` from the repository or tree page, so the full commit log is discoverable.
- Actual: the functional `/commits/{rev}` page is reachable only by typing the URL (or via changed-commit links on the compare page); the full history is not linked.
- Evidence: `crates/githearth-server/src/ui.rs:664-674` builds the browse section with only tree and compare; `crates/githearth-server/src/browse_ui.rs:377-390` adds commit links only in the compare page.
- Scope: all repositories.

### Empty repository's clone instruction is not a usable URL
- Severity: minor
- Repro:
  ```sh
  curl -s -b "githearth_session=$COOKIE" http://127.0.0.1:8080/organizations/lab06/repositories/t06-empty | grep -o 'git clone [^<]*'
  # git clone /lab06/t06-empty.git
  git clone /lab06/t06-empty.git   # fails: no such path/host
  ```
- Expected: a runnable clone URL, e.g. `git clone http://127.0.0.1:8080/lab06/t06-empty.git`, or a clearly labelled relative path.
- Actual: `git clone /lab06/t06-empty.git` — no scheme or host, and not the server's storage path, so it cannot work as shown.
- Evidence: `crates/githearth-server/src/ui.rs:676-681`; `crates/githearth-server/tests/browse_ui.rs:529-530` pins the same string.
- Scope: every empty repository page.

## Notes / skipped

- **Skipped (no implementation / out of M2 scope):**
  - Raw file/download endpoint: no route (`/raw/...` is `404` in UI and API); browse serves JSON/base64. `docs/plan/06-capability-inventory.md:14` puts archives in M9.
  - Tag detail page: no route (`/tags/v1.0.0` `404`); tags are exposed in `/refs` and usable as revisions.
  - Blame / full file history: no route (`/blame/...` `404`); `docs/plan/06-capability-inventory.md:14` puts blame/history in M9.
  - Graft/shallow commit browsing: no HTTP surface to create grafts; a real merge commit was tested instead (C10/E15).
  - Line numbers / syntax highlighting: blob pages render escaped plain `<pre>` (`browse_ui.rs:331`); no line numbers or highlighting, and no doc claim.
  - Branch/tag selector UI: no picker; non-default refs work by URL but are not linked from the repo page (not promised in M2 docs).
- **Private-repository 404 expectation:** the brief expected anonymous private browse to return `404`; it returns `303 -> /login` (E23b). This matches the existing acceptance harness (`scripts/verify-m4.py:1955-1958`) and does not leak existence; counted as a fail against the brief, recorded under the first defect rather than as a separate security finding.
- **Public API:** anonymous `/api/v1/.../tree/main` and `/refs` return `401`; anonymous Git HTTP clone of `t06-public` works and its landing page is `200`. The public read gap is in the browser browse pages (and the JSON browse API, which always requires a bearer token), not in Git transport or visibility storage.
- **Large tree:** a 20 000-entry directory is bounded, not paginated: API `413 {"error":"response too large"}` and UI `503` "Unavailable" (A16). The API message is clear; the UI hides the reason. Not counted as a separate defect because bounding is deliberate and the API surfaces it.
- **Encoding:** invalid-UTF-8 Latin-1 is returned exactly as base64 by the API (`binary:false`, no `text`); the UI uses `from_utf8_lossy` and shows replacement characters (B06). No encoding control/notice is offered.
- **Tags:** `/refs` reports the annotated tag `v1.0.0` with the tag-object id (`5ea672...`), while lightweight `lw-1` reports its commit id; tree by tag name and by tag-object id both resolves the commit (A01/A07/C14). No tag detail view exists.
- **Compare semantics:** only three-dot `base...head` is implemented; `base..head` is `400` (D08). `main....feature` returns `404 revision not found`, not `400`, but never `500` (D09). Unrelated histories return `200` with `mergeable.status: "unrelated"` and a bounded diff, not `409/422` (D04).
- **SHAs:** tree accepts a 7-character short SHA; `/commit/{id}` requires exactly 40 or 64 hex characters and returns `400` for a short SHA (A07/C13). Intentional per `is_commit_id`, but the UI shows a 404 for the same short SHA.
- **Safety checks:** every traversal attempt (`..`, `%2e%2e`, `a/../b`, absolute, NUL, backslash, encoded slash) ended in `400`/`404`, never `500`, and never exposed storage paths or `/etc/passwd` contents (A13/A14/E28). Symlink blobs return target text only (B08). A 1.1 MB blob and a 1 MiB patch are truncated with flags; a 300-file diff/compare is capped; a 1 103 724-byte compare JSON and 1 054 773-byte compare HTML stayed bounded (B05/C11/D11/E20).
- **XSS:** `<img src=x onerror=alert(1)>`, `<script>alert(2)</script>`, a `<script>` filename, a `<script>` commit message and an `<svg/onload=...>` branch name are all escaped in HTML (`&lt;...`), no raw `<script>`/`<img>` tags appear, and every styled browse response carries `Content-Security-Policy: default-src 'none'; ...` (F06/E08/E13/E14/E19). CSP also blocks inline script if markup ever slipped through.
- **Other pass items:** `lang="en"`, one `<h1>`, skip link, `<main id="main">`, scoped `<th>`, `Cache-Control: no-store`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` on all styled browse pages; styled 404 for missing org/repo/path/ref/commit; public landing page and public authenticated browsing; role matrix owner/read/non-member; empty-repository refs and 404 browse pages; commit/merge/rename/binary/root diffs; annotated vs lightweight tag refs; branch/tag/sha/HEAD refs; log/compare pagination and truncation flags.
- No `500` responses were observed during the run. All failing cases were re-run once (9/9 reproduced) before being recorded.

## Round 2 — fixed build (2026-11-14)

Re-verification on the rebuilt app image (`githearth-app`, built 2026-11-13T22:15:49Z),
which corresponds to `fix/m4-lab` 7e1ded3 ("Public repositories are browsable
anonymously; private ones still redirect."); round 1 ran `main` b820454. Account
`t06`, org `lab06`; fixtures unchanged (`t06-public` 5 commits / 20 entries on
`main`, `t06-browse` 6 commits on `main` plus all test refs, `t06-empty`).
No repository edits, restarts or migrations; the working checkout was only read.

Verdict: **PASS WITH DEFECTS** — the major defect is fixed; 7 minor defects remain.
Cases: 31 re-run, 24 pass, 7 fail, 0 skipped.

### Round 2 results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R2-01 | anonymous public repository landing + tree | 200 HTML; 20 entries, directories first | landing=200/1833 b; tree=200/4271 b, 20 entries, first 3 all `tree` | Pass |
| R2-02 | anonymous public blob | 200; exact text | 200/1396 b; `<pre>hello world\n</pre>` | Pass |
| R2-03 | anonymous public commit log | 200; 5 commit links | 200/2316 b; 5 unique `/commit/<40-hex>` links | Pass |
| R2-04 | anonymous public commit detail | 200; title Commit | 200/1053430 b; title `Commit`; message escaped | Pass |
| R2-05 | anonymous public compare | 200; Up to date | 200/1866 b; "Up to date" | Pass |
| R2-06 | anonymous public subpaths and refs | all 200 | docs=200, docs/nested=200, guide.md=200, space/unicode blobs=200, sha ref=200, HEAD=200, `main...<sha>`=200 | Pass |
| R2-07 | anonymous public bad path/ref | styled 404 with headers | 404 `text/html`, 1324 b, CSP/frame/nosniff present, 4 shapes | Pass |
| R2-08 | anonymous private tree/blob/commits/commit/compare (E23b) | 303 to login (brief text said 404) | all five 303 `Location: /login`, 0 b | Pass |
| R2-09 | anonymous missing repository | 303, indistinguishable from private | `/lab06/t06-nosuchrepo` 303 `/login`, 0 b — identical to private | Pass |
| R2-10 | private role matrix | anon 303; non-member 404; read 200; owner 200 | t01=404/1320 b; erin=200; t06=200; anon=303 | Pass |
| R2-11 | non-member signed-in public browse | UI and API 200 | t01 UI=200, API=200 | Pass |
| R2-12 | anonymous public security headers | no-store, CSP, DENY, nosniff | all four present on tree and blob | Pass |
| R2-13 | anonymous public commit-message XSS | escaped; no raw script/img | `&lt;script&gt;alert(&#x27;commit-msg&#x27;)`; raw script=0 img=0 | Pass |
| R2-14 | anonymous public big tree and truncated blob | 200; 310 entries; truncation notice | `many`=200, 310 entries/42588 b; `large.txt`=200, "Binary or truncated file (1100000 bytes)" | Pass |
| R2-15 | E12 log cap: `longlog` (106) and `manycommits` (261) | all reachable or next link / notice | 100 unique commit links each; no cursor, next/older, or "showing N of M" text | **Fail** |
| R2-16 | C05 cursor above 2^31 | empty page or 400 | `cursor=2147483647` 200/0 items; `2147483648`..`2^64-1` → **500** `{"error":"storage operation failed"}` | **Fail** |
| R2-17 | E22 unmatched browse URLs | styled 404 + security headers | 404, 0 b, no `Content-Type`, no CSP (7 URL shapes; UI and API; also anonymous) | **Fail** |
| R2-18 | F03 table captions | `<caption>` on tree and compare | 1 `<table>`, 0 `<caption>`, 3 `scope="col"` on both | **Fail** |
| R2-19 | F04 breadcrumbs | `nav aria-label="Breadcrumb"` on drill-down pages | blob/commits/commit/compare carry only `aria-label="Primary"`; tree has Breadcrumb | **Fail** |
| R2-20 | F08 link to commit history | `/commits/{rev}` link on repo or tree page | 0 `/commits/` links on either page (repo: tree, compare, issues, pulls, settings/tags) | **Fail** |
| R2-21 | F09 empty-repo clone instruction | host-qualified clone URL | `git clone /lab06/t06-empty.git` | **Fail** |
| S1 | anon public tree root | 200; dirs first; ≥20 entries | 200; 21 rows (20 entries); kinds [:3]=tree,tree,tree | Pass |
| S2 | anon public blob text exact | 200; `hello world\n` | 200; exact | Pass |
| S3 | anon public binary blob | 200; notice, no `<pre>` | 200; binary notice; pre=False | Pass |
| S4 | anon public HTML file escaped | 200; escaped img/script | escaped=True; raw script=0/img=0 | Pass |
| S5 | anon public commits + commit | 200; 5 links; commit 200 | 200/5/200 | Pass |
| S6 | anon public compare | 200; Up to date | 200; up to date | Pass |
| S7 | auth tree XSS filename | escaped name; link follows 200 | `&lt;script&gt;alert(1).txt`; href `%3Cscript%3Ealert%281%29.txt` → 200 | Pass |
| S8 | auth tree `<svg/onload>` branch | 200; no raw svg; encoded ref | 200; no raw `<svg>`; links carry `x%3Csvg%2Fonload%3Dalert%283%29%3E` | Pass |
| S9 | auth compare XSS + conflict state | escaped heading; conflict listed | h1 `Compare x&lt;svg/onload=alert(3)&gt;...main`; raw 0/0; `conflict.txt` present | Pass |
| S10 | auth commit page XSS | escaped message/content; no raw tags | 200; `&lt;script&gt;`; raw 0/0; 17 `<pre>` blocks | Pass |

### Major defect cleared

`crates/githearth-server/src/browse_ui.rs` `load_repo_ui` now falls back to
`repository_api::load_public_anonymous` when `browser_auth` returns 401, and the
public path reuses `git_admission::anonymous_readable_repository`
(`git_admission.rs:318-327`), which admits only `RepositoryStatus::Active`,
visible-to-anonymous repositories in a non-suspended org — the same gate as the
public landing page and anonymous Git transport. `username` is threaded as
`Option<&str>` so anonymous pages render without account chrome.

Evidence: all five page kinds for `t06-public` are 200 anonymously and render
real content (tree rows, `hello world\n`, 5 commit links, `Up to date`); the
"Browse files" link on the anonymous public landing page now leads to a 200.
Private and missing repositories both return the same `303 -> /login` with a
0-byte body, so no private-repository existence leak was introduced. Signed-in
behaviour is unchanged: non-member 404, read member and owner 200 on the private
repo, and non-member 200 on the public repo.

### Minor defects — current status

- **E12 commit log cap — still present.** `longlog` (106 commits) and
  `manycommits` (261) each render exactly 100 unique commit links with no
  pagination control or truncation notice; the handler still calls
  `browse::log(..., Some(0), browse::MAX_LOG_LIMIT, ...)` and never consumes a
  `next` cursor (`browse_ui.rs:355-366` in 7e1ded3).
- **C05 cursor wrap — still present, symptom worsened to HTTP 500.** With Git
  2.47.3 in the fixed image, `git rev-list --skip=2147483648` now fails with
  `fatal: '2147483648': not an integer` (verified in the container: `--skip`
  works to 2^31-1 and fails at 2^31), and the app maps that to
  `500 {"error":"storage operation failed"}` plus an
  `ERROR repository read failed category=git` log line. Boundary sweep:
  `abc`/`-1` 400; `0` 200/6 items; `2^31-2` and `2^31-1` 200/0 items;
  `2^31`, `3e9`, `u64::MAX` 500; `u64::MAX+1` 400. Root cause unchanged:
  an externally supplied `u64` cursor is passed to `--skip` unbounded
  (`githearth-git/src/browse.rs` untouched by the fix commit). 21 such 500s
  were produced by this sweep.
- **E22 bare 404 — still present.** `/tree/main/`, `/blob/main/`,
  `/commits/main/`, `/tree`, `/compare/main...main/`, `/commit/`, `/blame/main/x`
  are 404 with `content-length: 0`, no `Content-Type`, no CSP, no
  `X-Frame-Options`; API equivalents (e.g. `/tree/main/`) are also empty 404s.
  Defined missing-path/missing-repo URLs still return the styled 1 320-1 324 b
  page with all headers.
- **F03 captions — still present.** Tree and compare each render one `<table>`
  with three `scope="col"` headers and no `<caption>`.
- **F04 breadcrumbs — still present.** Blob, commits, commit and compare pages
  expose only `aria-label="Primary"`; breadcrumbs are still tree-only.
- **F08 commits link — still present.** Neither the repository page nor the tree
  page links to `/commits/{rev}`; the log remains URL-only.
- **F09 clone instruction — still present.** `t06-empty` still prints
  `git clone /lab06/t06-empty.git` (no scheme/host; not a storage path).
- **E23b — now passes as designed.** Anonymous private browse returns `303` to
  `/login`, which the fix commit message and `scripts/verify-m4.py` pin as
  intended; the brief's `404` wording is not met, but this is unchanged design,
  not a regression. Recorded as Pass with that caveat.

### Round 2 notes

- **Anonymous JSON API is still 401.** `/api/v1/.../t06-public/tree/main` and
  `/refs` return `401 {"error":"invalid or expired credentials"}` without a
  bearer token, while the UI pages are 200 and anonymous Git HTTP clone works.
  The fix scoped public anonymous access to the browser pages plus Git
  transport; the JSON browse API remains token-only. Not counted as a defect
  (round 1 treated it as a note), but the UI/API asymmetry remains.
- **No new defects found.** 10-case smoke of tree/blob/commit/compare and XSS
  escaping passed 10/10, including anonymous public XSS content, the
  `<script>alert(1).txt` filename, the `<svg/onload=alert(3)>` ref (encoded in
  every link, escaped in the compare heading), the `<script>` commit message,
  binary/truncated notices and a 310-entry directory.
- **No regressions in access control.** Private role matrix (anon 303,
  non-member 404, read 200, owner 200) and non-member public access (UI/API 200)
  are unchanged; private and missing repos remain indistinguishable anonymously.
- **500s.** 22 `status:500` log records in the 25-minute window: 21 from the
  cursor sweep above, 1 on `GET .../pulls/{number}` that belongs to another
  tester's traffic and is outside this domain.
- **Smoke harness corrections (test mistakes, not product defects).** The first
  smoke run had three wrong assertions: it expected one commit in `t06-public`
  (it has 5), searched for a literal `alert(1)` in a percent-encoded `href`, and
  expected the branch name to appear as text on the tree page (the tree page
  shows only the repository name; the ref appears percent-encoded in links and
  escaped in the compare heading). The assertions were corrected and all 10
  smoke cases then passed; nothing in the product changed between runs.
