# Git Smart HTTP — M4 closure round 3

Tester: t08. Date: 2026-11-14. Instance: main `800cad4` (Debian trixie, Git 2.47.3, `http://127.0.0.1:8080`).
Verdict: PASS.
Cases: 114 run, 114 pass, 0 fail, 0 skipped.

Repos used: `lab08/r3-http` (private; branches `main`, `mainline`, `pc-ff`, `r4-exp`, `r4-exp-expect`; tags `v1.1.0`, `v2.0.0`), `lab08/r3-authz`, `lab08/r3-push`, `lab08/r3-big`, `acme/r3-matrix` (private fixture created as alice for the role matrix), plus read-only checks against `acme/web`, `acme/site`, `lab01/t01-public` and `lab08/r3-http`.
Effective limits: `git_concurrency=8`, `git_request_limit_bytes=10 MiB`, `git_push_limit_bytes=1 GiB`, `git_timeout_seconds=600` (defaults; no `GITHEARTH_GIT_*` overrides in the container). `push_credentials` has no `repository_id` column, so counts are joined through `repository_pushes.operation_id`.

## Results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| 1 | GET upload-pack advertisement (v0) | 200, upload-pack advertisement with # service | 200, exact content type, `001e# service=git-upload-pack\n0000` + refs | PASS |
| 2 | GET upload-pack advertisement with `Git-Protocol: version=2` | 200, v2 capability advertisement | 200, `000eversion 2`, `ls-refs`/`fetch` capabilities | PASS |
| 3 | GET receive-pack advertisement (v0) | 200, receive-pack advertisement | 200, exact content type, `001f# service=git-receive-pack\n0000` | PASS |
| 4 | GET receive-pack advertisement with `Git-Protocol: version=2` (#154) | 200 v0 advertisement including # service | 200, `application/x-git-receive-pack-advertisement`, body starts `001f# service=git-receive-pack\n0000` | PASS |
| 5 | GET info/refs with no `service` | 400 dumb HTTP unsupported | 400 `dumb HTTP protocol is not supported` | PASS |
| 6 | GET info/refs with unknown service (`git-upload-archive`) | 400 | 400 same body | PASS |
| 7 | GET info/refs with `service=Git-Upload-Pack` | 400 (service is case-sensitive) | 400 same body | PASS |
| 8 | GET info/refs with duplicate `service` params | 400 | 400 | PASS |
| 9 | GET info/refs with extra unknown query param | ignored, 200 | 200 advertisement | PASS |
| 10 | GET info/refs for a path without `.git` | 404 | 404 `not found` | PASS |
| 11 | GET info/refs for a missing repository, authenticated | 404 | 404 `{"error":"not found"}` | PASS |
| 12 | GET info/refs with URL-encoded NUL in the repository path | 400 invalid input, no DB query | 400 `{"error":"invalid input"}` | PASS |
| 13 | GET on `/git-upload-pack` | 405 | 405, empty body | PASS |
| 14 | HEAD on `info/refs` | 200, no body | 200, empty body | PASS |
| 15 | Advertisement with a wrong password | 401 + `WWW-Authenticate` | 401, `WWW-Authenticate: Basic realm="githearth"` | PASS |
| 16 | Advertisement for a private repository, anonymous | 401 | 401 `invalid or expired credentials` | PASS |
| 17 | Advertisement with a non-Basic scheme | 401 | 401 | PASS |
| 18 | Advertisement with invalid Basic base64 | 401 | 401 | PASS |
| 19 | Advertisement with Basic credentials lacking a colon | 401 | 401 | PASS |
| 20 | Receive-pack advertisement, anonymous | 401 challenge | 401 + challenge | PASS |
| 21 | Receive-pack POST, anonymous | 401 | 401, no pack read | PASS |
| 22 | Read-only PAT upload-pack advertisement | 200 | 200 advertisement | PASS |
| 23 | Read-only PAT receive-pack advertisement (#154) | 403 | 403 `{"error":"not permitted"}` | PASS |
| 24 | Read-only PAT receive-pack POST (#154) | 403 at admission | 403 `not permitted` | PASS |
| 25 | Read-only PAT receive-pack POST with 50 MB Content-Length and no body (#154) | 403 before reading any body | 403 in 0.005 s; no body uploaded | PASS |
| 26 | Read-only PAT supplied as Basic username, push | 403 | 403, refused at admission | PASS |
| 27 | Receive-pack body with invalid gzip (#154) | 400 | 400 `invalid git request body` | PASS |
| 28 | Receive-pack body with truncated gzip (#154) | 400 | 400 `invalid git request body` | PASS |
| 29 | Receive-pack body with a short pkt-line (#154) | 400 | 400 `invalid git request body` | PASS |
| 30 | Receive-pack empty body | 400 | 400 `invalid git request body` | PASS |
| 31 | Receive-pack flush-only body (`0000`, valid empty command list) | 200, no report-status | 200, empty body | PASS |
| 32 | HTTP body shorter than its Content-Length, half-close (#154) | 400 | 400 | PASS |
| 33 | Truncated chunked body, no terminal chunk (#154) | 400 | 400 | PASS |
| 34 | Gzip body whose decoded pkt-line is short (#154) | 400 | 400 | PASS |
| 35 | pkt-line `ffff` with no payload (#154) | 400 | 400 | PASS |
| 36 | Receive-pack with Content-Length 0 | 400 | 400 | PASS |
| 37 | Gzip-encoded flush-only receive-pack body | 200 | 200, empty body | PASS |
| 38 | Valid command pkt-line followed by `PACKjunkjunk` | 200 with report-status error, no ref | 200, `unpack protocol error (pack version unsupported)` + `ng refs/heads/r5-junk` | PASS |
| 39 | Valid command pkt-line followed by a truncated pack | 200 with protocol report | 200, `early EOF` + `unpack-objects abnormal exit` + `ng` | PASS |
| 40 | Junk and truncated pushes create no refs | refs absent | `r5-junk` and `r5-trunc` absent | PASS |
| 41 | Malformed bodies leave no `push_credentials` row | 0 new orphans | 0 recent orphan rows | PASS |
| 42 | POST upload-pack with `Expect: 100-continue` (#154) | `100 Continue` before the body | `HTTP/1.1 100 Continue` in 0.02 s | PASS |
| 43 | Upload-pack final response after sending the body | 200 + pack result | 200, `NAK` + `PACK` | PASS |
| 44 | POST receive-pack with `Expect: 100-continue` | `100 Continue` before the body | `100 Continue` in 0.03 s | PASS |
| 45 | Receive-pack `Expect` exchange completes | 200 and ref created | 200, `r4-exp-expect` exists | PASS |
| 46 | Client RST during upload-pack (#154) | git killed, permit released | 1 process during request, 0 within 0.4 s of RST | PASS |
| 47 | 12 disconnect cycles, then clone (#154) | clone succeeds promptly | clone rc 0 in 0.26 s | PASS |
| 48 | 8 held git requests, then a 9th | 503 + Retry-After | 503, `Retry-After: 5` | PASS |
| 49 | Held clients close, then a new request | permits released, 200 | 200 | PASS |
| 50 | Gzip-encoded upload-pack body | 200 with result | 200, `NAK` + `PACK` | PASS |
| 51 | Invalid gzip upload-pack body | 400 | 400 `invalid git request body` | PASS |
| 52 | Gzip-encoded real receive-pack push | 200, ref created | 200, `unpack ok` + `ok refs/heads/r4-exp` | PASS |
| 53 | `Content-Encoding: GZIP` (uppercase) on upload-pack | 200 | 200 | PASS |
| 54 | `Content-Encoding: x-gzip` on upload-pack | 200 | 200 | PASS |
| 55 | `Content-Encoding: identity` on upload-pack | 200 | 200 | PASS |
| 56 | `Content-Encoding: deflate` | 415 | 415 `unsupported Content-Encoding` | PASS |
| 57 | Upload-pack Content-Length 20 MB (>10 MiB limit) | 413 before body read | 413 immediately | PASS |
| 58 | Receive-pack Content-Length 1,073,741,825 (>1 GiB) | 413 before body read | 413 immediately | PASS |
| 59 | Chunked upload-pack body of 11 MB (>10 MiB limit) | git killed, no usable result | 200 headers then connection reset, 0-byte body, no PACK | PASS |
| 60 | Chunked 1.15 GB receive-pack push | 413 | 413 after 1,078,984,704 bytes, 19.4 s, `Broken pipe` client-side | PASS |
| 61 | Gzip 1.15 GB decoded receive-pack push | 413 | 413 after 1,078,984,704 bytes, 19.7 s | PASS |
| 62 | No ref and no push row after the two over-limit pushes | none | no `big-raw` ref; `r3-big` 0 push rows at that point | PASS |
| 63 | 300 MB normal push | accepted | rc 0 in 10.2 s, `main` moved | PASS |
| 64 | Password clone of a private repository | success | rc 0 | PASS |
| 65 | Protocol v2 clone | success | rc 0 | PASS |
| 66 | Shallow clone (`--depth 1`) | success | rc 0, shallow | PASS |
| 67 | Protocol v2 fetch after a new commit | new commit fetched | `FETCH_HEAD` equals local HEAD | PASS |
| 68 | Anonymous clone of public `lab01/t01-public` | success | rc 0 | PASS |
| 69 | Anonymous clone of public empty `acme/site` | success with empty warning | rc 0 | PASS |
| 70 | Anonymous clone of a private repository | denied | 401 challenge, no data | PASS |
| 71 | Read matrix: alice (owner), bob (write), carol (maintain), dave (admin), erin (read), frank (triage), t08 (read) | all read `acme/r3-matrix` | 7/7 clones rc 0 | PASS |
| 72 | Read matrix: grace (base member) on the private repo | denied by policy | 404 `not found` (member grant does not include `read`) | PASS |
| 73 | Read by non-member alice on a `lab08` private repository | 404, no existence leak | 404 `not found` | PASS |
| 74 | `repo:read` PAT clone of a private repository | success | rc 0 | PASS |
| 75 | `repo:write` PAT clone of a private repository | success | rc 0 | PASS |
| 76 | Invalid `gth_` token clone | 401 | rc 128, `Authentication failed` | PASS |
| 77 | Revoked PAT clone | 401 | rc 128, `Authentication failed` | PASS |
| 78 | Read-only PAT supplied as Basic username, clone | success | rc 0 | PASS |
| 79 | Wrong-password clone | 401 | rc 128, `Authentication failed` | PASS |
| 80 | Push matrix: alice (owner), bob (write), carol (maintain), dave (admin) | all allowed | 4/4 new branches created | PASS |
| 81 | Push matrix: erin (read) and frank (triage) | denied | HTTP 403 before pack, rc 128 | PASS |
| 82 | Push matrix: grace (base member, cannot read the repo) | denied before upload | 404, indistinguishable from missing repository | PASS |
| 83 | Push as t08 (owner) on `lab08/r3-http` | allowed | rc 0 | PASS |
| 84 | Push as bob (write grant) on `lab08/r3-authz` | allowed | rc 0, `bob-x` created | PASS |
| 85 | Push with `repo:write` PAT | allowed | rc 0, `w-pat` created | PASS |
| 86 | Push with `repo:read` PAT | 403 | 403, rc 128 | PASS |
| 87 | Initial push to an empty repository | success | rc 0 | PASS |
| 88 | Second normal push | success | rc 0 | PASS |
| 89 | Push a new branch | success | rc 0 | PASS |
| 90 | Delete an unprotected branch | success | deleted | PASS |
| 91 | Force push an unprotected branch (non-fast-forward) | accepted | `forced update` | PASS |
| 92 | Fast-forward push to protected `main` | accepted | rc 0 | PASS |
| 93 | Force push to protected `main` | denied | `protected branch: ref.force_push`, pre-receive declined | PASS |
| 94 | Delete protected `main` | denied | `protected branch: ref.delete`, pre-receive declined | PASS |
| 95 | Similarly named unprotected branch `mainline` | unaffected | rc 0 | PASS |
| 96 | Non-fast-forward push without `--force` to `mainline` | rejected client-side; server unchanged | `non-fast-forward`, rc 128 | PASS |
| 97 | `--force-with-lease` with the correct lease | accepted | `forced update` | PASS |
| 98 | `--force-with-lease` with a stale lease | rejected | `stale info`, rc 128 | PASS |
| 99 | Create a lightweight tag | success | `[new tag]` | PASS |
| 100 | Create an annotated tag | success; peeled line advertised | `v1.1.0` and `refs/tags/v1.1.0^{}` present | PASS |
| 101 | Delete an unprotected tag | success | deleted | PASS |
| 102 | Owner creates protected `v2.0.0` (rule `v*`, threshold maintain) | allowed | rc 0 | PASS |
| 103 | Delete protected tag `v2.0.0` | denied | `protected tag: ref.delete` | PASS |
| 104 | Bob (write) creates protected tag `v9.0.0` | denied (write < maintain) | `protected tag: ref.create` | PASS |
| 105 | Bob (write) creates unprotected tag `rel-1` | allowed | `[new tag]` | PASS |
| 106 | Force-move unprotected tag `rel-1` | allowed | `forced update` | PASS |
| 107 | Delete unprotected tag `rel-1` | allowed | deleted | PASS |
| 108 | Push to `refs/notes/probe` | denied | `ref namespace is read-only` | PASS |
| 109 | Empty push with nothing to send | `Everything up-to-date`, rc 0 | same | PASS |
| 110 | Push an empty commit (no new blobs) | accepted | `main` moved | PASS |
| 111 | 25 MB normal push | accepted | rc 0 in 1.06 s | PASS |
| 112 | 2 MB real push through the chunked path | accepted, chunked framing | rc 0; `Transfer-Encoding: chunked` in `GIT_TRACE_CURL` | PASS |
| 113 | Credential accounting after accepted and refused pushes | one credential row per accepted operation; none for refusals | `r3-http` 21/21, `r3-authz` 3/3, `r3-push` 5/5, `r3-matrix` 5/5, `r3-big` 1/1; 0 recent orphans | PASS |
| 114 | No stale ref locks after kills, malformed bodies and 413s | none | no `*.lock` under the three exercised repositories | PASS |

## Defects

None. Issue #154 is closed for all four of its symptoms, and no regression was found in the transport, auth matrix, ref/tag policy or limit handling.

## Notes

- **#154 malformed/truncated receive-pack bodies.** Invalid gzip, truncated gzip, a pkt-line shorter than its declared length, an empty body, an HTTP body shorter than `Content-Length`, a truncated chunked body, a `ffff` pkt-line and a zero-length body all return `400 invalid git request body`. The mapping is in `crates/githearth-server/src/git_http.rs:623-628` (the `decode_failed` flag and the “no stdout + child failed” case), with the gzip flag set in `stream_gzip_body_to_stdin` (`git_http.rs:1105-1165`). A well-formed command followed by a bad pack is still a protocol-level `200` with `report-status` (`unpack …`, `ng …`), which is the correct distinction from an unreadable HTTP body. Flush-only (`0000`) is a valid empty command list and correctly returns `200` with an empty result.
- **#154 `Expect: 100-continue`.** Upload-pack now answers `100 Continue` in 0.02 s without waiting for the body; the handler waits only for the stdin feeder to poll the body before returning the response (`git_http.rs:315-337`, notably `body_polled_rx.await` at line 322). Receive-pack does the same in 0.03 s. A full real push after the interim response succeeds.
- **#154 receive-pack advertisement under `Git-Protocol: version=2`.** The advertisement now starts `001f# service=git-receive-pack\n0000` with `Content-Type: application/x-git-receive-pack-advertisement`. The fix keys the v2 prefix on `is_fetch`/`use_v2` (`git_http.rs:106,151-158`), so a receive-pack request always gets the v0 service header even if a client sends the fetch-only header.
- **#154 read-only PAT admission.** A `repo:read` token is refused with `403` at `info/refs?service=git-receive-pack` and at the POST, before any body is read: a request declaring a 50 MB body with no bytes sent got `403` in 5 ms. The synthetic `RefPush` check is in `git_admission.rs:393-407`; it runs before the semaphore/permit and before `push_credentials` is written, so there is no wasted pack upload and no credential row.
- **#154 disconnect and permit release.** A client RST during upload-pack left one `git upload-pack` process running before the close and zero within 0.4 s; a clone immediately afterwards succeeded in 0.23 s. Twelve disconnect cycles followed by a clone also succeeded (0.26 s). The disconnect drop-guard cancels the supervisor token (`git_http.rs:1225-1227,1287`) and the supervisor drops the permit at `git_http.rs:1271`. Exhausting all eight permits produced `503` with `Retry-After: 5`, and closing the held clients released them.
- **413 paths.** Header-stage `Content-Length` rejection works on both endpoints (20 MB for the 10 MiB request limit; 1,073,741,825 bytes for the 1 GiB push limit). Chunked and gzip pushes of a valid 1,153,785,797-byte pack both returned `413` after 1,078,984,704 bytes (about 5 MiB over the decoded limit), in 19–20 s, with no ref and no push row. A chunked *upload-pack* request over the 10 MiB request limit ends as `200` headers plus a killed/truncated body rather than `413`; that is the documented streaming-kill behaviour for fetches (and the unit test expects it), not a regression. Normal large pushes are unaffected: 25 MB in 1.06 s and 300 MB in 10.2 s.
- **`push_credentials` cleanliness.** Every accepted push on the exercised repositories has exactly one credential row (`r3-http` 21/21, `r3-authz` 3/3, `r3-push` 5/5, `r3-matrix` 5/5, `r3-big` 1/1 after the accepted 300 MB push). Refused pushes (protection, tag rule, read-only PAT, read role, notes namespace), malformed bodies and both 1.15 GB 413s added zero rows; the recent-orphan count stayed 0. The 27 historical `ssh_key` orphans from round 1 are still present and untouched. No stale `*.lock` files remained after the disconnect, malformed-body and 413 cases.
- **Test mistakes, corrected and re-run (not product defects).** (a) Two initial “normal push” cases used a pre-existing remote `main`, so the first push was a non-fast-forward; re-run against a fresh empty repository (`F01/F02`). (b) An anonymous clone first appeared to succeed because the workstation Git credential helper supplied stored credentials; re-run with `GIT_CONFIG_GLOBAL=/dev/null` and `credential.helper=`, it is denied. (c) The first raw Expect/gzip push fixture used a non-zero old OID for a new ref, so receive-pack correctly refused to lock it; corrected to an all-zero old OID and re-run. (d) `v2.0.0` is lightweight, so it has no peeled `^{}` line; the expectation was corrected. (e) The first process-kill probe used an object that does not exist in `r3-http` (git exited immediately) and hit a full permit set from another tester; re-run with the correct HEAD and a free slot.
- **Policy observation.** `grace` is a base `member` in `acme` and `lab08`; the `member` role permits only `member`, so she cannot read private repositories or push. Her `404` is correct policy (`crates/githearth-authz/policies/roles/member.cedar`), not a transport regression.
- **Other observations, not defects.** Malformed *upload-pack* pkt-lines return `200` with an empty result (the #154 work only promised `400` for receive-pack; stock Git reports the protocol error through the result stream). Concurrent testers occasionally held all eight git permits with their own 1.15 GB pushes, producing expected `503`s for unrelated requests; the tests above were re-run once a slot was free.
