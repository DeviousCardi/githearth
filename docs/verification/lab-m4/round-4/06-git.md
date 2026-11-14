# Git transport regression — M4 closure round 4 (final verification)

Tester: t06 (owner of `lab06`, read grant in `acme`). Date: 2026-11-14.
Instance: `main` `cd7946b` (PR #163 merged), deployed with the README Docker Compose quick start;
HTTP `http://127.0.0.1:8080`, SSH `127.0.0.1:2222`, PostgreSQL `127.0.0.1:54329`.
Verdict: **PASS**.
Cases: **151 run, 151 pass, 0 fail, 0 skipped** (each initial failure was a test-fixture error; re-run once and listed in the notes).

Repos used: `lab06/r4-http` (private; protected `main` with `require_pull_request`, protected `sgn<pid>` with
`require_signed_commits`, tag rule `v*` at `maintain`), `lab06/r4-authz` (private role matrix),
`lab06/r4-big` (private; large pushes), `lab06/r4-empty<pid>` (fresh empty repo), `lab06/r4-clone`,
`lab06/t06-public` (public), plus read-only checks against the `acme` org grants.
Effective limits (defaults, no `GITHEARTH_GIT_*` overrides in the container): `git_concurrency=8`,
`git_request_limit_bytes=10 MiB`, `git_push_limit_bytes=1 GiB`, `git_timeout_seconds=600`
(`crates/githearth-server/src/config.rs:74-77`).

## Results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| H01 | upload-pack v0 advertisement | 200, service header + refs | 200, application/x-git-upload-pack-advertisement, service=True, refs=True | PASS |
| H02 | upload-pack v2 advertisement | 200, version 2 capabilities | 200, starts=b'000eversion 2\n0015ag', ls-refs=True | PASS |
| H03 | receive-pack v0 advertisement | 200, receive-pack service header | 200, application/x-git-receive-pack-advertisement, header=True | PASS |
| H04 | receive-pack advertisement under Git-Protocol: v2 (#154) | 200 v0 service header | 200, application/x-git-receive-pack-advertisement, starts=b'001f# service=git-receive-pack\n000000b135c821' | PASS |
| H05 | info/refs with no service param | 400 dumb HTTP | 400, b'dumb HTTP protocol is not supported' | PASS |
| H06 | info/refs unknown service | 400 | 400, b'dumb HTTP protocol is not supported' | PASS |
| H07 | service name is case-sensitive | 400 | 400, b'dumb HTTP protocol is not supported' | PASS |
| H08 | duplicate service params | 400, no advertisement | 400, b'Failed to deserialize query string: duplicate fiel' | PASS |
| H09 | extra unknown query param ignored | 200 advertisement | 200 | PASS |
| H10 | path without .git suffix | 404 | 404, b'not found' | PASS |
| H11 | missing repository, authenticated | 404 JSON not found | 404, b'{"error":"not found"}' | PASS |
| H12 | URL-encoded NUL in repository path | 400 invalid input | 400, b'{"error":"invalid input"}' | PASS |
| H13 | GET on git-upload-pack endpoint | 405 empty body | 405, 0 bytes | PASS |
| H14 | HEAD on info/refs | 200, no body | 200, 0 bytes | PASS |
| H15 | wrong password | 401 + WWW-Authenticate | 401, Basic realm="githearth" | PASS |
| H16 | private repository, anonymous | 401 challenge | 401, b'{"error":"invalid or expired credentials' | PASS |
| H17 | non-Basic auth scheme | 401 | 401 | PASS |
| H18 | invalid Basic base64 | 401 | 401 | PASS |
| H19 | Basic credential without colon | 401 | 401 | PASS |
| H20 | receive-pack advertisement, anonymous | 401 challenge | 401 | PASS |
| H21 | receive-pack POST, anonymous | 401, no pack read | 401, b'{"error":"invalid or expired credentials' | PASS |
| H22 | read-only PAT upload-pack advertisement | 200 | 200 | PASS |
| H23 | read-only PAT receive-pack advertisement (#154) | 403 not permitted | 403, b'{"error":"not permitted"}' | PASS |
| H24 | read-only PAT receive-pack POST (#154) | 403 at admission | 403, b'{"error":"not permitted"}' | PASS |
| H25 | read-only PAT POST with 50 MB Content-Length, no body | 403 before body | 403 in 0.019s | PASS |
| H26 | write PAT advertisement | 200 | 200 | PASS |
| H27 | receive-pack body with invalid gzip (#154) | 400 invalid git request body | 400, b'invalid git request body' | PASS |
| H28 | receive-pack body with truncated gzip (#154) | 400 | 400, b'invalid git request body' | PASS |
| H29 | receive-pack body with a short pkt-line (#154) | 400 | 400, b'invalid git request body' | PASS |
| H30 | receive-pack empty body | 400 | 400, b'invalid git request body' | PASS |
| H31 | receive-pack flush-only body | 200, empty result | 200, 5 bytes, dechunked=b'' | PASS |
| H32 | HTTP body shorter than Content-Length, half-close (#154) | 400 | 400, b'invalid git request body' | PASS |
| H33 | Truncated chunked body, no terminal chunk (#154) | 400 | 400, b'invalid git request body' | PASS |
| H34 | gzip body whose decoded pkt-line is short (#154) | 400 | 400, b'invalid git request body' | PASS |
| H35 | pkt-line ffff with no payload (#154) | 400 | 400, b'invalid git request body' | PASS |
| H36 | receive-pack with Content-Length 0 | 400 | 400, b'invalid git request body' | PASS |
| H37 | gzip-encoded flush-only body | 200, empty result | 200, 5 bytes, dechunked=b'' | PASS |
| H38 | valid command followed by PACKjunkjunk | 200 with report-status error | 200, b'62\r\n0035unpack protocol error (pack version unsupported)\n0029ng refs/heads/r4-junk unpacker error\n0000\r\n0\r\n\r\n' | PASS |
| H39 | valid command followed by truncated pack | 200 with protocol report | 200, b'56\r\n0028unpack unpack-objects abnormal exit\n002ang refs/heads/r4-trunc unpacker error\n0000\r\n0\r\n\r\n' | PASS |
| H40 | Junk and truncated pushes create no refs | refs absent | junk=False, trunc=False | PASS |
| H41 | gzip-encoded upload-pack body | 200 with result | 200, NAK=True, PACK=True | PASS |
| H42 | invalid gzip upload-pack body | 400 | 400, b'invalid git request body' | PASS |
| H43 | short pkt-line upload-pack body (documented 200) | 200 empty result | 200, 5 bytes | PASS |
| H44 | gzip-encoded upload-pack request | 200 | 200 | PASS |
| H45 | Content-Encoding: x-gzip on upload-pack | 200 | 200 | PASS |
| H46 | Content-Encoding: identity on upload-pack | 200 | 200 | PASS |
| H47 | Content-Encoding: deflate | 415 | 415, b'unsupported Content-Encoding' | PASS |
| H48 | POST upload-pack with Expect: 100-continue (#154) | 100 Continue before body then 200 | interim=b'HTTP/1.1 100 Continue', final=200, body=b'8\r\n0008NAK\n\r\nDE\r\nPACK\x00\x00\x00\x02\x00\x00\x00\x03\x9e', 0.025s | PASS |
| H49 | POST receive-pack with Expect: 100-continue | 100 Continue then 200, ref created | interim=b'HTTP/1.1 100 Continue', final=200, b'2E\r\n000eunpack ok\n001cok refs/heads/r4-expect\n0000\r\n0\r\n\r\n', 0.033s | PASS |
| H50 | Receive-pack Expect exchange created the ref | r4-expect exists | exists=True | PASS |
| H51 | gzip-encoded real receive-pack push | 200, ref created | 200, b'2C\r\n000eunpack ok\n001aok refs/heads/r4-gzip\n0000\r\n0\r\n\r\n' | PASS |
| H52 | upload-pack Content-Length 20 MB (>10 MiB limit) | 413 before body | 413 in 0.021s | PASS |
| H53 | receive-pack Content-Length 1,073,741,825 (>1 GiB) | 413 before body | 413 in 0.021s | PASS |
| H54 | chunked upload-pack body of 11 MB (>10 MiB limit) | stream killed, no usable result | 200, 5 bytes, PACK=False, 0.02s | PASS |
| H55 | gzip upload-pack body of 11 MB (>10 MiB limit) | stream killed (documented), no usable result | 200 headers, empty result, 0.04 s; no PACK | PASS |
| H56 | 2 MB valid push through the chunked path | accepted, chunked framing | 200, unpack ok, ok refs/heads/r4-chunked | PASS |
| G01 | Password clone of a private repository | success | rc=0 | PASS |
| G02 | Protocol v2 clone | success | rc=0 | PASS |
| G03 | Shallow clone (--depth 1) | success, shallow | rc=0 shallow=yes | PASS |
| G04 | Anonymous clone of public repository | success | rc=0 | PASS |
| G05 | Anonymous clone of private repository | denied | rc=128 | PASS |
| G06-bob | Read matrix: bob on private r4-authz | clone success | rc=0 | PASS |
| G06-carol | Read matrix: carol on private r4-authz | clone success | rc=0 | PASS |
| G06-dave | Read matrix: dave on private r4-authz | clone success | rc=0 | PASS |
| G06-erin | Read matrix: erin on private r4-authz | clone success | rc=0 | PASS |
| G06-frank | Read matrix: frank on private r4-authz | clone success | rc=0 | PASS |
| G07 | Read matrix: grace (base member) | denied, no existence leak | rc=128 | PASS |
| G08 | Read matrix: alice (non-member) | denied, 404 | rc=128 | PASS |
| G09 | repo:read PAT clone | success | rc=0 | PASS |
| G10 | repo:write PAT clone | success | rc=0 | PASS |
| G11 | Invalid gth_ token clone | denied | rc=128 | PASS |
| G12 | Wrong-password clone | denied | rc=128 | PASS |
| G13 | Read-only PAT supplied as Basic username, clone | success | rc=0 | PASS |
| G14 | Revoked PAT clone | denied | rc=128 | PASS |
| G15 | Fetch after a new commit (v2 fetch) | new commit fetched | before=08513d04 remote=f6e2a435 | PASS |
| G16-bob | Push matrix: bob | allowed | rc=0 | PASS |
| G16-carol | Push matrix: carol | allowed | rc=0 | PASS |
| G16-dave | Push matrix: dave | allowed | rc=0 | PASS |
| G17 | Push matrix: t06 owner | allowed | rc=0 | PASS |
| G18-erin | Push matrix: erin denied | denied | rc=128 403 | PASS |
| G18-frank | Push matrix: frank denied | denied | rc=128 403 | PASS |
| G18-grace | Push matrix: grace denied | denied | rc=128 not found | PASS |
| G18-alice | Push matrix: alice denied | denied | rc=128 not found | PASS |
| G19 | Push with repo:write PAT | allowed | rc=0 | PASS |
| G20 | Push with repo:read PAT | denied | rc=128 403 | PASS |
| G21 | Initial push to an empty repository | success | rc=0 | PASS |
| G22 | Second normal push | success | rc=0 | PASS |
| G23 | Push a new branch | success | rc=0 | PASS |
| G24 | Delete an unprotected branch | success | create=0 delete=0 | PASS |
| G25 | Force push an unprotected branch | accepted, forced update | rc=0  | PASS |
| G26 | Push to protected main (PR required) | denied | rc=1 protected branch: ref.push | PASS |
| G27 | Force push to protected main | denied | rc=1 protected branch: ref.push | PASS |
| G28 | Delete protected main | denied | rc=1 protected branch: ref.delete | PASS |
| G29 | Similarly named unprotected branch mainline | force push allowed | rc=0 | PASS |
| G30 | Non-fast-forward push without --force | rejected client-side | rc=1 non-fast-forward | PASS |
| G31 | --force-with-lease with correct lease | accepted | rc=0 | PASS |
| G32 | --force-with-lease with stale lease | rejected | rc=1 stale info | PASS |
| G33 | Create a lightweight tag | success | rc=0 | PASS |
| G34 | Create an annotated tag | success, peeled line advertised | rc=0 peeled=1 | PASS |
| G35 | Delete an unprotected tag | success | rc=0 | PASS |
| G36 | Owner creates protected tag v4.0.0 | allowed | rc=0 | PASS |
| G37 | Delete protected tag v4.0.0 | denied | rc=1 protected tag: ref.delete | PASS |
| G38 | Bob (write) creates protected tag v5.0.0 | denied | rc=1 protected tag: ref.create | PASS |
| G39 | Bob (write) creates unprotected tag rel-1 | allowed | rc=0 | PASS |
| G40 | Delete unprotected tag rel-1 | allowed | rc=0 | PASS |
| G41 | Push to refs/notes/probe | denied (read-only namespace) | rc=1 read-only | PASS |
| G42 | Empty push with nothing to send | up-to-date, rc 0 | rc=0  | PASS |
| G43 | Push an empty commit | accepted | rc=0 | PASS |
| G44 | 25 MB normal push | accepted | rc=0 in 0.15s | PASS |
| S01 | SSH clone of a private repository | success | rc=0 | PASS |
| S02 | SSH clone of a public repository with a registered key | success | rc=0 | PASS |
| S03 | SSH clone with an unregistered key | denied | rc=128 | PASS |
| S04 | scp-style SSH URL clone | success | rc=0 | PASS |
| S05 | Shell command over SSH | refused | rc=128 githearth: repository not found | PASS |
| S06 | Password authentication over SSH | refused | rc=255 | PASS |
| S07 | SFTP subsystem | refused | rc=255 | PASS |
| S08 | git-upload-archive command | refused | rc=128 githearth: repository not found | PASS |
| S09 | Path traversal in the SSH command | refused | rc=128 githearth: repository not found | PASS |
| S10 | SSH push of a new branch | accepted | rc=0 | PASS |
| S11 | SSH push to protected main | denied | rc=1 protected branch: ref.push | PASS |
| S12 | SSH force push to protected main | denied | rc=1 protected branch: ref.push | PASS |
| S13 | SSH delete protected main | denied | rc=1 protected branch: ref.delete | PASS |
| S14 | SSH push to an unprotected branch | accepted | rc=0 | PASS |
| S15 | SSH clone as erin (read role) | success | rc=0 | PASS |
| S16 | SSH push as erin (read role) | denied at admission | rc=128 not permitted | PASS |
| S17 | Create protected signed branch with a signed root commit | accepted | rc=0  | PASS |
| S18 | Unsigned commit on a require_signed_commits branch | denied | rc=1 not signed by a registered committer | PASS |
| S19 | Commit signed with an unregistered key | denied | rc=1 not signed by a registered committer | PASS |
| S20 | Signed by t06's key with erin as committer | denied | rc=1 not signed by a registered committer | PASS |
| S21 | Signed with a second registered key | accepted | rc=0 | PASS |
| S22 | Advertised SSH host key matches the container file | same fingerprint | scan=SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE file=SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE | PASS |
| S23 | Host key file exists with mode 0600 | 0600 | mode/size=600 387 | PASS |
| S24 | 8 parallel HTTP clones | all succeed | 8/8 | PASS |
| S25 | 6 parallel SSH clones | all succeed | 6/6 | PASS |
| S26 | 4 parallel SSH pushes | all accepted | 4/4 | PASS |
| J01 | GET unknown /api path | 404 JSON envelope | 404 application/json {"error":"not found"} | PASS |
| J02 | GET /api with malformed query (page=abc) | 400 JSON envelope | 400 application/json {"error":"invalid input"} | PASS |
| J03 | GET /api/v1/organizations/not-a-uuid | 400 JSON envelope | 400 application/json {"error":"invalid input"} | PASS |
| J04 | GET /…git/info/refs with no service | 400 stays text/plain (middleware must not rewrite) | 400 text/plain "dumb HTTP protocol is not supported" | PASS |
| J05 | GET /…git/info/refs for a path without .git | 404 stays text/plain | 404 text/plain "not found" | PASS |
| J06 | GET /…git path with URL-encoded NUL | 400 JSON invalid input | 400 application/json {"error":"invalid input"} | PASS |
| J07 | GET UI path with URL-encoded NUL | 404 styled HTML, not 503 | 404 text/html styled 404 page | PASS |
| P01 | Refused pushes leave no push_credentials row | 0 new rows | 1234 -> 1234 after read-PAT push, protected-main push and malformed body | PASS |
| P02 | Accepted push records exactly one credential row | 1 row | 1234 -> 1235; operation d8e1aea3 has 1 row | PASS |
| P03 | repository_pushes to push_credentials is 1:1 per exercised repo | 1:1 | r4-http 61/61, r4-authz 38/38, r4-big 2/2, r4-empty 3/3, r4-empty* 3/3 each | PASS |
| P04 | Dangling credential rows | none recent | 27 historical rows (2026-11-13), 0 created in the last 3 hours | PASS |
| P05 | Stale ref locks after kills, malformed bodies and 413s | none | 0 *.lock files under /var/lib/githearth/repositories | PASS |
| P06 | Chunked 1.15 GB receive-pack push | 413 | 413 after 1,080,033,404 decoded bytes, 18.8 s | PASS |
| P07 | Gzip 1.15 GB receive-pack push | 413 | 413 after 1,082,130,555 decoded bytes, 19.3 s | PASS |
| P08 | No ref and no push row after the two over-limit pushes | none | no big-chunk/big-gzip ref; r4-big still 2 push rows (initial + 300 MB) | PASS |
| P09 | 300 MB normal push | accepted | rc 0 in 10.26 s; big-ok created | PASS |


## Defects

None. The transport, authentication matrix, malformed-body handling, limits, ref/tag policy and
credential bookkeeping all match the documented behaviour, and the JSON/UI changes in this build do
not touch the `/…git` routes.

## Notes

1. **JSON middleware does not reach the Git routes.** `json_rejections` is layered globally but acts
   only when `request.uri().path().starts_with("/api/")` **and** the response is `400` with a
   `text/plain` content type (`crates/githearth-server/src/lib.rs:396-420`, layer at `:320`). Probes
   show `/api` unknown paths and extractor rejections keep `{"error":...}` JSON (`J01`-`J03`), while
   `/…git/info/refs` with no `service` still answers `400 text/plain "dumb HTTP protocol is not
   supported"` (`J04`), a `.git`-less path still `404 text/plain "not found"` (`J05`), and the Git
   NUL path still `400 {"error":"invalid input"}` (`J06`). The UI NUL path renders the styled `404`
   HTML page, not a `503` (`J07`). The read-only boundary layer is applied only to the JSON router
   (`lib.rs:305-310`); Git keeps its own transport admission (`git_admission.rs:382-411`).

2. **Auth matrix.** Private advertisement without credentials, with a wrong password, a non-Basic
   scheme, invalid Basic base64 or a colon-less credential all return `401` with
   `WWW-Authenticate: Basic realm="githearth"` (`H15`-`H19`). Missing and unauthorised repos are
   indistinguishable (`404 not found`). Read matrix: owner, write, maintain, admin, read and triage
   can clone `r4-authz`; the base `member` (grace) and a non-member (alice) get `404`
   (`G06`-`G08`). Push matrix: owner/write/maintain/admin accepted, read/triage `403` at admission,
   member/non-member `404` (`G16`-`G18`). `repo:write` PAT pushes; `repo:read` is `403` before any
   body is read (`H23`-`H25`, `G19`-`G20`). Invalid and revoked `gth_` tokens fail (`G11`, `G14`).

3. **Malformed receive-pack bodies.** Invalid/truncated gzip, a short pkt-line, an empty body, a body
   shorter than `Content-Length` (half-close), a truncated chunked body, a gzip body whose decoded
   pkt-line is short, `ffff` with no payload and `Content-Length: 0` all return
   `400 invalid git request body` (`H27`-`H36`). The mapping is the `decode_failed` flag and the
   “no stdout + child failed” case in `git_http.rs:620-627`; the gzip flag is set in
   `stream_gzip_body_to_stdin` (`git_http.rs:1113-1165`). A well-formed command list followed by a
   junk or truncated pack is still `200` with `unpack protocol error`/`early EOF` and `ng …`
   (`H38`-`H39`), which is the correct distinction. A flush-only (`0000`) body is a valid empty
   command list and returns `200` with an empty result (`H31`, `H37`, dechunked to zero bytes).

4. **`Expect: 100-continue`.** Both endpoints answer `HTTP/1.1 100 Continue` before the body and then
   `200`; a real gzip/Expect push creates its ref (`H48`-`H51`). The handler waits only for the
   stdin feeder to poll the body before returning the response (`git_http.rs:315-337`, notably
   `body_polled_rx.await` at `:322`).

5. **Limits and 413.** Header-stage `Content-Length` rejection works on both endpoints: 20 MiB on
   upload-pack and 1,073,741,825 bytes on receive-pack both return `413` in ~20 ms without reading a
   body (`H52`-`H53`). Streaming chunked and gzip pushes of a valid 1.175 GB pack return `413` after
   1,080,033,404 and 1,082,130,555 decoded bytes respectively (18.8 s and 19.3 s), with no ref and
   no push row (`P06`-`P08`). A chunked or gzip **upload-pack** body over the 10 MiB request limit
   ends as `200` headers plus a killed/truncated result rather than `413` (`H54`, `H55`); that is the
   documented streaming-kill behaviour for fetches, not a regression. Normal large pushes are
   unaffected: 2 MiB chunked (`H56`), 25 MB (`G44`) and 300 MB (`P09`) all succeed.

6. **`push_credentials`.** Every exercised repo has one credential row per accepted push
   (`r4-http` 61/61, `r4-authz` 38/38, `r4-big` 2/2, `r4-empty*` 3/3 each). Refused pushes
   (read-only PAT, protected branch, malformed body) added zero rows (`P01`), an accepted push added
   exactly one (`P02`), and the recent-orphan count is 0 (`P04`). The 27 historical `ssh_key`
   orphans from 2026-11-13 are untouched. No `*.lock` files remain after the kill/malformed/413
   cases (`P05`). The cleanup is `git_http.rs:564-570`.

7. **SSH.** Ed25519 keys authenticate the transport; an unregistered key, password auth, a shell
   command, SFTP, `git-upload-archive` and a `../../` path are all refused (`S01`-`S09`). Protected
   refs deny push/force/delete over SSH (`S11`-`S13`); `read` (erin) clones but is refused at push
   admission (`S15`-`S16`). Signed commits on a `require_signed_commits` branch: a signed root is
   accepted; unsigned, an unregistered signer, and a signer whose key belongs to a different
   committer are denied; a second registered key is accepted (`S17`-`S21`, checks at
   `git_http.rs:931-1002`). The advertised host key matches the container file
   (`SHA256:+YjB0CqMF7SETWcQk8NZBwK06sTURXH6rhXOuXZZzHE`) and the file is mode `600` (`S22`-`S23`;
   generation at `ssh.rs:46-67`, load at `:646-651`).

8. **Concurrency.** 8 parallel HTTP clones, 6 parallel SSH clones and 4 parallel SSH pushes all
   succeed (`S24`-`S26`). No `503` was seen in this round; slots were free at the time of each batch.

9. **Test mistakes, corrected and re-run (not product defects).** (a) The first junk/truncated-pack
   fixtures omitted the `0000` flush pkt-line, so receive-pack correctly answered `400`; re-run with
   the flush gave `200` + `report-status`. (b) The first initial-push case reused a non-empty
   `r4-empty`, so it was a non-fast-forward; re-run against a fresh repo per run. (c) Tag cases
   `v0.1.0`/`v0.2.0` matched the protected `v*` rule (delete denied, create conflicted); re-run with
   unprotected tag names. (d) A zsh history modifier mangled `$1:test…` into `t06est…` in the
   parallel-clone URL; fixed with `${1}:…`. (e) Unquoted `$SSHOPT` is not word-split by zsh, and an
   unmatched glob aborted the cleanup `rm`; fixed with `setopt shwordsplit nomatch`. (f) The
   signed-branch name was reused across runs (non-fast-forward); each run now creates a unique
   branch and its matching rule. (g) The first raw Expect/gzip push bodies had no pack or a stale
   ref; re-run with a real `git pack-objects` pack and deleted refs.

10. **Observations, not defects.** A duplicated `service` query parameter returns `400` with axum's
    `text/plain` deserialisation message (“duplicate field `service`”) rather than the handler's
    “dumb HTTP protocol” body (`H08`). The status is correct and the path is not under `/api/`, so
    the JSON middleware does not touch it; `git_http.rs` is unchanged since round 3. `grace`'s `404`
    is the `member` role policy (it grants only `member`), not a transport fault. Older round-3 test
    rules/branches remain in `lab06`; they do not affect this run. No repository files were edited
    and nothing was posted to GitHub; SQL was read-only.
