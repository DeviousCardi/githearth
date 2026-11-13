# Git Smart HTTP — M4 lab acceptance report

Tester: t04. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 93 run, 87 pass, 6 fail, 0 skipped.

Repos used (all in `lab04`): `t04-public` (public), `t04-private`, `t04-push`, `t04-auth`, `t04-big`, `t04-arch` (archived), `t04-empty` (public, empty). Read-only cross-checks against `acme/web`, `acme/site` and `lab05/ssh-private`.

Effective limit: `GITHEARTH_GIT_PUSH_LIMIT_BYTES` is not set in `compose.yaml`/container env, so the default `git_push_limit_bytes = 1_073_741_824` (1 GiB) and `git_request_limit_bytes = 10_485_760` (10 MiB) apply (`crates/githearth-server/src/config.rs:75-78`, `githearth.example.toml:17-20`).

`push_credentials` counting note: the SQL in the brief (`WHERE repository_id=...`) fails because the table has no `repository_id` column (`operation_id, credential_kind, credential_id, created_at`). I used the equivalent, read-only:

```sql
SELECT count(*) FROM push_credentials pc
 WHERE pc.operation_id IN (SELECT operation_id FROM repository_pushes WHERE repository_id='<uuid>');
-- plus, for issue #143:
SELECT count(*) FROM push_credentials pc
 WHERE credential_kind='password'
   AND NOT EXISTS (SELECT 1 FROM repository_pushes p WHERE p.operation_id=pc.operation_id);
```

## Results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| 1 | GET `/{org}/{repo}/info/refs?service=git-upload-pack` anonymous, public | 200, `application/x-git-upload-pack-advertisement`, service header + refs | 200, exact content type, `001e# service=git-upload-pack\n0000` + HEAD/refs | PASS |
| 2 | Same with `Git-Protocol: version=2` | 200, v2 capability advertisement | 200, `000eversion 2` + `ls-refs`/`fetch` caps | PASS |
| 3 | GET `info/refs?service=git-receive-pack` anonymous, public repo | 401 + challenge (push is never anonymous) | 401, `WWW-Authenticate: Basic realm="githearth"`, JSON error | PASS |
| 4 | GET `info/refs?service=git-receive-pack` with Basic auth | 200, `application/x-git-receive-pack-advertisement` | 200, exact content type, `# service=git-receive-pack` + refs | PASS |
| 5 | GET `info/refs` with no `service` | Dumb HTTP unsupported | 400 `dumb HTTP protocol is not supported` | PASS |
| 6 | `service=git-foo`, `service=GIT-UPLOAD-PACK`, `service=` | 400, unimplemented smart feature | 400 same body | PASS |
| 7 | `service=git-upload-archive` | 400, unimplemented smart feature | 400 same body | PASS |
| 8 | POST `/{org}/{repo}/git-upload-archive` | 404 | 404, empty body | PASS |
| 9 | Dumb paths `.git/HEAD`, `.git/objects/info/packs` | 404 | 404, empty body | PASS |
| 10 | GET `info/refs?service=git-upload-pack` on missing repo, anonymous | 404 | 401 + challenge (same as private; deliberate no-existence-leak, `git_admission.rs:298-310`) | PASS (by design) |
| 11 | Same on missing repo, authenticated | 404 | 404 `{"error":"not found"}` | PASS |
| 12 | Missing org, anonymous / authenticated | 401 / 404 | 401 + challenge / 404 | PASS |
| 13 | Malformed paths: no `.git`, `.git.git`, trailing slash, extra segment, `..` segment, double slash | 404 | 404 (empty body) | PASS |
| 14 | Percent-encoded repo name `%74%30%34-public` | Resolve to real repo | 200 advertisement | PASS |
| 15 | Duplicate `service` param / unknown query param | 400 / ignore | 400 `duplicate field service` / 200 | PASS |
| 16 | Method matrix: HEAD `info/refs`, HEAD/POST/OPTIONS on wrong methods | HEAD 200 empty; others 405 | 200 (no body) / 405 | PASS |
| 17 | POST auth negatives: anonymous, wrong password, Bearer session token, malformed Basic | 401 (+challenge) | 401 + `WWW-Authenticate` for all four | PASS |
| 18 | `Git-Protocol: version=999` / junk | Fall back to v0 | 200 v0 advertisement | PASS |
| 19 | Archived repo advertisement, authenticated / anonymous | 200 / 401 | 200 `# service` + `0000` / 401 | PASS |
| 20 | Upload-pack `want` for an unadvertised oid | Refuse | 200 `ERR upload-pack: not our ref ...` | PASS |
| 21 | Receive-pack advertisement with `Git-Protocol: version=2` | Either v0 with `# service=` prefix or real v2 | 200 but body starts `00b4<oid> refs/heads/...`: v0 ref listing **without** the smart-HTTP service header | **FAIL** (D1) |
| 22 | Wrong (`text/plain`) or absent `Content-Type` on POST upload-pack/receive-pack | 415 | 200, request processed | PASS (lenient; noted) |
| 23 | Anonymous clone public repo | Success | Success, HEAD matches | PASS |
| 24 | Anonymous clone private repo | 401 | `fatal: Authentication failed` (401) | PASS |
| 25 | Anonymous clone `acme/web` (private) | 401 | 401 | PASS |
| 26 | Anonymous clone `acme/site` (public, empty) | Success with empty warning | Success | PASS |
| 27 | Username+password Basic clone private | Success | Success | PASS |
| 28 | PAT as Basic password clone private | Success | Success | PASS |
| 29 | PAT as Basic username (`http://gth_...:x@`) clone private | Success | Success | PASS |
| 30 | Wrong password | 401 + WWW-Authenticate | 401, `WWW-Authenticate: Basic realm="githearth"` | PASS |
| 31 | Revoked PAT clone | 401 | 401 after `DELETE /user/tokens/{id}` (204) | PASS |
| 32 | `repo:read` PAT clone private | Success | Success | PASS |
| 33 | `erin` (read grant) clone `t04-auth` | Success | Success | PASS |
| 34 | `erin` push `t04-auth` | Refused | HTTP 200, pack rejected by pre-receive: `permission denied: ref.push` | PASS (functionally refused) |
| 35 | `bob` (write) push `t04-auth` | Success | Ref moved `1280504..a45907e` | PASS |
| 36 | `t01` (not a lab04 member) clone/push `t04-private`, and a missing lab04 repo | 404, indistinguishable | Both `fatal: repository ... not found` (404) | PASS |
| 37 | `t04` (acme read role) clone `acme/web` | Success | Success, 3 branches + annotated tag `v1.0.0` | PASS |
| 38 | Shallow `--depth=1` clone public | Success | Success, `--is-shallow-repository=true`, 1 commit | PASS |
| 39 | `protocol.version=0` clone | Success | Success | PASS |
| 40 | Fetch after a new push | Success | New commit fetched | PASS |
| 41 | 4 simultaneous clones | All succeed (8 slots) | All 4 succeeded | PASS |
| 42 | Fetch-only repo (`t04-empty`) leaves no push credentials | 0 rows | push rows 0, credential ops 0 | PASS |
| 43 | Gzip-encoded upload-pack request body | Processed | 200, `0008NAK` result | PASS |
| 44 | Chunked upload-pack request body | Processed | 200, result body | PASS |
| 45 | HEAD on private `acme/web.git/info/refs`, anonymous | 401 | 401 | PASS |
| 46 | Initial push creating `main` | Success | `* [new branch]` | PASS |
| 47 | Fast-forward push | Success | Ref moved, server = local | PASS |
| 48 | Non-fast-forward push without force (unprotected) | Refused; ref unchanged | Client refuses (`tip ... behind`); server `main` unchanged | PASS (client-side by design; `receive.denyNonFastForwards=false`) |
| 49 | Force push (unprotected) | Accepted | `+ ... main -> main (forced update)` | PASS |
| 50 | `--force-with-lease`: stale / correct | Stale refused; correct accepted | `! [rejected] (stale info)` / forced update | PASS |
| 51 | Delete non-default branch | Success | `- [deleted] t04-atomic1` | PASS |
| 52 | Delete default branch (`HEAD`) | Refused | `deletion of the current branch prohibited` | PASS |
| 53 | Create lightweight tag | Success | `* [new tag]` | PASS |
| 54 | Create annotated tag | Success, peeled line advertised | Success; `refs/tags/t04-ann^{}` in advertisement | PASS |
| 55 | Move existing tag (`--force`) | Accepted (no tag protection) | `+ c6fcca8...960d332 t04-lw` | PASS |
| 56 | Delete lightweight and annotated tags | Success | Both `- [deleted]` | PASS |
| 57 | Atomic multi-ref push: 2 branches + tag | All applied | 3 refs created | PASS |
| 58 | `--atomic` push including `refs/notes/t04` | All-or-nothing; branch not created | `ref namespace is read-only`; `t04-atomic3` absent | PASS |
| 59 | Non-atomic push including `refs/notes/t04` | Policy denies whole push | Both refs `pre-receive hook declined`; branch absent | PASS |
| 60 | Push commit to new branch | Success | `* [new branch]` | PASS |
| 61 | Empty push (no refs) | `Everything up-to-date`, no change | Same, exit 0 | PASS |
| 62 | Push empty commit (no new objects) | Accepted | Ref created | PASS |
| 63 | Push to archived repo | Refused | HTTP 200, `permission denied: ref.push`, ref absent | PASS |
| 64 | Fetch/clone archived repo | Allowed | Clone succeeds (empty) | PASS |
| 65 | Push to `lab05/ssh-private` (non-member) and `lab05/<missing>` | Refused, no leak | Both 404 `{"error":"not found"}` | PASS |
| 66 | Push `t04-*` branch to `acme/web` as read role | Refused; existing refs untouched | `permission denied: ref.push`; no `t04-*` ref created | PASS |
| 67 | Push with `repo:read` PAT | 403 | **200** advertisement then 200 with `permission denied: ref.push` / `pre-receive hook declined`; ref not created | **FAIL** (D4) |
| 68 | Read-only PAT as username push | Refused | Same 200 + pre-receive denial | PASS |
| 69 | Push with `repo:write` PAT as password | Success | Ref created; `push_credentials` row `kind=token`, id = token id | PASS |
| 70 | Push with `repo:write` PAT as username | Success | Ref created | PASS |
| 71 | Delete nonexistent branch: full refspec `:refs/heads/x` / short `:x` | Full: warn/refuse; short: client error | Full: `remote: warning: deleting a non-existent ref`, exit 0; short: `unable to delete ... does not exist` | PASS |
| 72 | Push to `refs/notes/t04` | Refused | `ref namespace is read-only` | PASS |
| 73 | Push with `Content-Length: 1073741825` (> 1 GiB), no body | 413 before spawn | 413 `request body exceeds limit` in 0.02 s | PASS |
| 74 | Chunked over-limit push (valid 1.15 GB pack) | 413 (closed #142 bypass) | 413 after 1,080,033,280 bytes sent (6,291,456 over limit); no ref | PASS |
| 75 | `Content-Encoding: gzip` over-limit push (1.15 GB decoded) | 413 | 413 after 1,076,887,552 compressed bytes; no ref | PASS |
| 76 | Large but under-limit push (300 MB blob) | Accepted | `* [new branch] main`, 10.1 s | PASS |
| 77 | Real `git push` of 2 MB (chunked path) | Accepted | Probe `Content-Length: 4`, then `Transfer-Encoding: chunked`, 200, ref created | PASS |
| 78 | `Content-Length` exactly at limit (1 GiB), no body | Not 413; bounded by client/timeout | No 413; request waits, disconnect yields no ref and no credential row | PASS |
| 79 | Encodings: `br`, `gzip, identity`, `x-gzip` | 415 / 415 / accepted | 415 / 415 / 200 `unpack ok` | PASS |
| 80 | `Content-Encoding: identity` with over-limit CL | 413 | 413 | PASS |
| 81 | Gzip-encoded small push | Accepted, `unpack ok` | 200, `ok refs/heads/t04-raw-gzip` | PASS |
| 82 | `Expect: 100-continue` on receive-pack | 100 Continue, then 200 | `HTTP/1.1 100 Continue` after 0.05 s; push `unpack ok` | PASS |
| 83 | `Expect: 100-continue` on upload-pack | 100 Continue, then 200 | No 100; `200 OK` headers sent at once; curl stalls 1.0 s then sends body (`no-expect` 0.015 s vs `expect` 1.009 s) | **FAIL** (D3) |
| 84 | HTTP/1.0: GET advertisement, POST fetch, POST push | Works without chunked encoding | 200 in all three; push `unpack ok`; responses close the connection | PASS |
| 85 | Bad `Content-Length`: `abc`, negative, duplicate | 400 | 400 `connection: close`, empty body | PASS |
| 86 | `Content-Length` + `Transfer-Encoding: chunked` both present | TE wins, no smuggling | 200, chunked (CL ignored) | PASS (note) |
| 87 | Short body + half-close (declared 2000, sent 120) | 400 / protocol error | 200 with empty result (`0\r\n\r\n`), no ref | **FAIL** (D2) |
| 88 | Abrupt RST mid-pack (300 of 735 bytes) | No ref, no stale locks, repo usable | No ref; clone and next push succeed; no credential orphan | PASS |
| 89 | `Content-Encoding: gzip` with non-gzip body | 400 | 200, zero-length result | **FAIL** (D2) |
| 90 | Invalid pack bytes after a valid command pkt-line | Error report / 4xx | 200, zero-length result | **FAIL** (D2) |
| 91 | Empty public repo page (signed in) | Clone URL shown | `git clone /lab04/t04-empty.git` in page | PASS |
| 92 | Repo page anonymous | Sign-in redirect | 303 `Location: /login` | PASS (note) |
| 93 | `GET /acme/web.git/info/refs` anonymous on private | 401 | 401 + `WWW-Authenticate` | PASS |

## Defects

### Receive-pack advertisement is malformed when the client asks for protocol v2
- Severity: minor (latent; current Git push does not send the header)
- Repro:
  ```bash
  curl -sS -u t04:<test-password> \
    -H 'Git-Protocol: version=2' \
    'http://127.0.0.1:8080/lab04/t04-push.git/info/refs?service=git-receive-pack' | xxd | head -2
  ```
- Expected: a valid Smart HTTP response — either the v0 advertisement prefixed with
  `001f# service=git-receive-pack\n0000`, or a genuine v2 advertisement starting
  `version 2`.
- Actual: `00b4<oid> refs/heads/feature...` — Git 2.39 `receive-pack` answers v0 (it has no
  push-v2 support), but the HTTP layer suppresses the `# service=` header purely because the
  request header asked for v2. A v2-capable client gets a v0 body it cannot parse; a v0 client
  gets a body without the required service header.
- Evidence: control request without the header starts `001f# service=gi...`; with the header the
  body starts `00b4f8940dd5f759...`. Cause: `git_http.rs:147-158` decides `use_v2` from the
  request header and skips the prefix, instead of from what `receive-pack` actually emits.
  `GIT_TRACE_CURL` on a real `git push` shows no `Git-Protocol` header today, so no current git
  client is affected.
- Scope: systematic for any `git-receive-pack` advertisement carrying `Git-Protocol: version=2`.

### Malformed request bodies are acknowledged `200 OK` with an empty result
- Severity: minor
- Repro:
  ```bash
  curl -sS -u t04:<test-password> \
    -H 'Content-Type: application/x-git-receive-pack-request' \
    -H 'Content-Encoding: gzip' \
    --data-binary 'not-gzip-at-all' \
    -o /dev/null -w '%{http_code} %{size_download}\n' \
    http://127.0.0.1:8080/lab04/t04-push.git/git-receive-pack
  # => 200 0
  ```
  Same for a truncated body (declared 2000, sent 120 then half-close → `200`, `0\r\n\r\n`) and
  for a valid command pkt-line followed by `PACKjunkjunk` (→ `200`, zero-length result).
- Expected: `400 Bad Request` for a body that cannot be decoded, or a protocol error pkt-line.
  Never a success status with no `report-status` for a push that was never applied.
- Actual: HTTP 200, content type `application/x-git-receive-pack-result`, empty body. Git then
  reports `the remote end hung up unexpectedly`.
- Evidence: `stream_gzip_body_to_stdin` logs the decode failure and sends the kill signal
  (`git_http.rs:1079-1083`), but `receive_pack` only maps `limit_exceeded` to 413
  (`git_http.rs:583-585`); body-read errors end the stream with EOF and the handler has already
  produced a 200. No ref is created and no credential row is left, so this is an error-reporting
  defect, not a state-integrity one.
- Scope: systematic for corrupt/truncated gzip, truncated bodies and unparsable packs.

### No `100 Continue` on git-upload-pack; response is sent before the request body is read
- Severity: minor (Git clients disable `Expect`; curl falls back after its 1 s timeout)
- Repro:
  ```bash
  curl -sS -u t04:<test-password> \
    -H 'Content-Type: application/x-git-upload-pack-request' \
    -H 'Expect: 100-continue' --data-binary @/tmp/lab04/upreq.bin \
    -o /dev/null -w 'expect t=%{time_total}\n' \
    http://127.0.0.1:8080/lab04/t04-public.git/git-upload-pack
  # expect t=1.009040   (same body without Expect: 0.015213)
  ```
- Expected: `HTTP/1.1 100 Continue`, then read the body, as the receive-pack path already does
  (raw push with `Expect` gets `100 Continue` in 0.05 s).
- Actual: the handler returns a streaming `200 OK` immediately; no interim response is sent. A
  client that strictly waits for 100 never sends the body; curl waits its 1 s `expect100-timeout`
  and proceeds, so every such fetch pays a 1 s penalty.
- Evidence: raw socket sees `HTTP/1.1 200 OK` before the body is written; `upload_pack` builds the
  stream without peeking (`git_http.rs:289-312`), while `receive_pack` peeks before responding
  (`git_http.rs:579`). Hyper does not send 100 automatically.
- Scope: systematic for `POST /git-upload-pack` with `Expect: 100-continue`.

### Read-only credentials are admitted to receive-pack and refused only after the pack upload (200, not 403)
- Severity: minor (push is refused; wrong status and wasted upload)
- Repro:
  ```bash
  READ=$(cat /tmp/lab04/token-read)   # gth_ token with scopes ["repo:read"]
  curl -sS -o /dev/null -w '%{http_code}\n' -u t04:$READ \
    'http://127.0.0.1:8080/lab04/t04-private.git/info/refs?service=git-receive-pack'
  # 200, advertisement lists all refs
  git -c credential.helper= push "http://t04:$READ@127.0.0.1:8080/lab04/t04-private.git" HEAD:refs/heads/t04-x
  # remote: permission denied: ref.push
  # ! [remote rejected] ... (pre-receive hook declined)
  ```
- Expected (acceptance criterion): 403, refused at admission before the pack is accepted.
- Actual: 200 advertisement, full pack uploaded into quarantine, then HTTP 200 with
  `permission denied: ref.push` / `pre-receive hook declined`; no ref is created. The same
  happens for `erin` (read grant) and for a read-only token supplied as the username.
- Evidence: `admit`/`admit_account` for `Want::Push` only require `repository.read`
  (`git_admission.rs:329-369`; `git_http.rs:83-97`), and the denial comes from `decide_ref` in the
  pre-receive hook (`git_admission.rs:490-505`). ADR 0003 stage 1 documents this read-only
  admission, so this may be intentional; it still misses the acceptance expectation and lets a
  read-only credential spend the full `git_push_limit_bytes` of upload before being refused.
- Scope: systematic for read-only credentials (HTTP and per ADR also SSH).

## Notes / skipped

- Not a defect, worth knowing:
  - Anonymous requests for a *missing* repository return 401, not 404, and a private repo returns
    the identical body. This is the documented anti-enumeration rule (`git_admission.rs:294-310`);
    authenticated requests distinguish 404. The brief's "missing repo 404" applies only once
    authenticated.
  - Non-fast-forward refusal without `--force` is client-side: `receive.denyNonFastForwards` is
    deliberately `false` (ADR 0003) and an unprotected force push is allowed. A server-side
    non-FF refusal only appears via branch protection (M3).
  - Wrong/absent `Content-Type` on both RPC endpoints is accepted (no media-type validation).
    Lenient, not harmful.
  - `Content-Length` is ignored when `Transfer-Encoding: chunked` is present (hyper uses TE);
    that is RFC-correct, but note it if a proxy in front prefers CL.
  - Deleting a nonexistent ref with the *full* refspec is accepted with
    `remote: warning: deleting a non-existent ref`; with the short refspec git refuses locally.
    Harmless idempotency.
  - Non-empty repository pages show no clone URL; only the empty-repo page prints
    `git clone /lab04/<repo>.git`. The clone URL is present where the UI generates it, but a
    "clone" affordance is absent on populated repos.
  - The repo web UI requires a session even for public repositories (anonymous `GET` → 303
    `/login`). Only the Git endpoints allow anonymous public reads.
- Limits and issue #143, exact counts for `t04-big` (`6ae93725-de82-4af9-bee3-fadba12f2115`):
  - Before the three over-limit pushes: `repository_pushes`=0, credential ops=0,
    password orphans (global)=0.
  - After Content-Length 413: unchanged (the check runs before any DB insert).
  - After chunked 413 and after gzip 413 (measured after each): `repository_pushes`=0,
    credential ops=0, password orphans=0. No row was left behind.
  - After the 300 MB accepted push: `repository_pushes`=1, credential ops=1. After the 2 MB
    chunked push: 2/2. The ref moved only for accepted pushes.
  - Same invariant for refused policy pushes: `t04-private` stayed at 3 push rows / 3 credential
    operations after the read-only and read-token-as-username denials; `t04-arch` stayed 0/0
    after the archived-repo denial.
- `service=git-upload-archive` is unimplemented by design (dumb protocol refused, no route);
  reported as the "unimplemented smart feature" case, not as a defect.
- Cross-domain observation (not counted in my cases): `push_credentials` held 27 orphan rows with
  `credential_kind='ssh_key'` created between 21:19:16Z and 21:24:33Z (no matching
  `repository_pushes` row), while the password-orphan count stayed 0. If those came from the SSH
  tester's *refused* pushes, issue #143's cleanup may not cover the SSH transport; worth checking
  in the SSH report. I did not investigate further (outside my domain).
- Not run: exhausting the 8-slot `git_concurrency` budget to force 503/Retry-After (would perturb
  other testers; covered by M4 qualification), tag/branch protection interactions (M3 domains),
  and TLS/proxy behaviour (out of scope for the loopback instance).

## Round 2 — fixed build (2026-11-14)

Build under test: `githearth-app:latest` built 2026-11-13T22:15:54Z (03:45:54 IST), container
`githearth-app-1` started 22:16:22Z. Debian 13 "trixie", `git version 2.47.3` (round 1: Debian
bookworm, Git 2.39). Source branch `fix/m4-lab` (`7e1ded3`); its `git_http.rs` and
`git_admission.rs` are identical to `b820454`, so this build carries no HTTP-transport change.
The image is live: anonymous public browse now returns 200
(`/organizations/lab04/repositories/t04-public`), private still 303 to `/login`.

Verdict: **PASS WITH DEFECTS**. Three of the four round-1 defects reproduce unchanged. The
malformed-body defect is partly fixed: pack-level parse errors now produce a real
`report-status` rejection, but body-decode failures are still acknowledged `200 OK`. The 15-case
smoke is clean.

Cases: 23 run (8 defect re-runs + 15 smoke), 17 pass, 6 fail, 0 skipped.

### Round-2 results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| R1 | `GET info/refs?service=git-receive-pack` with `Git-Protocol: version=2` (round-1 #21) | v0 body with `001f# service=git-receive-pack\n0000`, or real v2 | 200, `application/x-git-receive-pack-advertisement`, body starts `00b4f8940d…` (v0 ref list, **no** service header), 1123 B; control without the header starts `001f# service=gi…`. 2 runs | **FAIL** (D1 remains) |
| R2 | `Content-Encoding: gzip` with non-gzip body (round-1 #89) | 400 or protocol-error pkt-line | 200, content-type receive-pack-result, 0-byte body. 2 runs | **FAIL** (D2 remains) |
| R3 | Truncated body: `Content-Length: 2000`, 120 bytes, half-close (round-1 #87) | 400 / protocol error | 200, body `0\r\n\r\n` (empty report-status). 2 runs | **FAIL** (D2 remains) |
| R4 | Truncated gzip stream (first half of a 24-byte gzip of `0000`) | 400 / protocol error | 200, 0-byte body. 2 runs | **FAIL** (D2 remains) |
| R5 | Valid command pkt-line + `PACKjunkjunk` (round-1 #90) | Error report | 200, `0035unpack protocol error (pack version unsupported)\n002dng refs/heads/...` | PASS (fixed) |
| R5b | Complete body, command + truncated pack header/payload | Error report | 200, `0028unpack unpack-objects abnormal exit\n002fng refs/… unpacker error\n0000` | PASS (fixed) |
| R6 | `Expect: 100-continue` on `POST /git-upload-pack` (round-1 #83) | `HTTP/1.1 100 Continue`, then 200 | No interim response: 200 headers immediately, `time_total` 1.006 s / 1.016 s (2 runs) vs 0.030 s without `Expect` | **FAIL** (D3 remains) |
| R7 | `repo:read` PAT on `GET info/refs?service=git-receive-pack` and push (round-1 #34/#67) | 403 before any pack | Advertisement 200 (357 B, lists `refs/heads/main`); push 200 then `remote: permission denied: ref.push` / `pre-receive hook declined`. 2 runs; no ref, no row | **FAIL** (D4 remains) |
| S1 | Anonymous clone `t04-public` | Success | Success, `HEAD` = `1280504` | PASS |
| S2 | Anonymous clone `t04-private` | 401 | `could not read Username…` (401 challenge), no data | PASS |
| S3 | `t04`+password clone private; wrong password | Success / 401 | Success / `fatal: Authentication failed` (401) | PASS |
| S4 | `repo:read` PAT as Basic password clone private | Success | Success, `HEAD` = `1280504` | PASS |
| S5 | `repo:write` PAT push new branch to `t04-push` | Ref created | `* [new branch] t04-r2-smoke` | PASS |
| S6 | Fetch after that push | Fetches new commit | `FETCH_HEAD` = local `6e05ab7` | PASS |
| S7 | `erin` (read grant) push `t04-auth` | Refused, ref untouched | `pre-receive hook declined`; `main` stays `a45907e`; no new ref | PASS |
| S8 | Archived `t04-arch`: clone + push | Clone works, push refused | Clone succeeds (empty); push `permission denied: ref.push`; no ref | PASS |
| S9 | `t01` (non-member) clone and push `t04-private` | 404 both, no leak | `fatal: repository … not found` for both | PASS |
| S10 | `Content-Length: 1073741825`, no body | 413 before spawn | 413 `request body exceeds limit`, immediate | PASS |
| S11 | Chunked push, valid 1,153,785,777-byte body | 413 | 413 after 1,082,130,432 bytes (8,388,608 over 1 GiB), 22.6 s; no ref | PASS |
| S12 | `Content-Encoding: gzip`, 1,153,785,777 decoded bytes | 413 | 413 at the 1,073,741,824-byte decoded ceiling, 19.0 s; no ref | PASS |
| S13 | No `push_credentials` rows after refused pushes | 0 new rows | `t04-private` 5/5 (3 baseline + SSH smoke; two refused pushes added 0), `t04-auth` 2/2, `t04-arch` 0/0, `t04-big` 2/2 after all 413s; global password orphans 0; no new ssh_key orphans (27 pre-existing rows, max ts 21:24:33Z) | PASS |
| S14 | No ref after 413; repo usable | No ref; clone/push work | `refs/heads/t04-r2-big` absent; clone + small push (`t04-r2-after413`) succeed; `t04-big` 3/3 | PASS |
| S15 | SSH transport unaffected | Clone/push work | `ls-remote`, clone, push, delete over `ssh://127.0.0.1:2222` all succeed; each push journals 1 op / 1 credential | PASS |

### Round-1 defects: status after the fix

- **D1 — receive-pack advertisement malformed under `Git-Protocol: version=2`: REMAINS.**
  Unchanged; `git_http.rs:147-158` still decides the `# service=` prefix from the request header.
  Latent only: `GIT_TRACE_CURL` on a real `git push` (2.50.1) shows no `Git-Protocol` header, so
  no stock client hits it.
- **D2 — malformed bodies acknowledged `200 OK`: PARTLY FIXED.**
  Pack-parse failures now return `unpack …` + `ng …` + `0000` (R5/R5b) — this is Git 2.47's
  receive-pack reporting through the unchanged HTTP layer, not a handler change. Body-decode
  failures (invalid gzip, truncated gzip, short HTTP body) still get `200` with an empty result
  (R2–R4); `receive_pack` still maps only `limit_exceeded` to 413 (`git_http.rs:583-585`).
- **D3 — no `100 Continue` on upload-pack: REMAINS.** Unchanged; `upload_pack` still builds the
  stream without peeking (`git_http.rs:289-312`). Every `Expect: 100-continue` fetch pays the
  client's 1 s fallback.
- **D4 — read-only credentials admitted to receive-pack: REMAINS.** Unchanged; push admission
  still requires only `RepositoryRead` (`git_admission.rs:245-269`), and the denial comes from
  the pre-receive hook (`decide_ref`, `git_admission.rs:428-505`) after the pack upload.

### Round-2 notes

- The 1.15 GB fixtures for S11/S12 were a valid pack (commit `4d38258`) of 1,153,785,777 bytes;
  the gzip variant was the same body deflated. Both over-limit paths refused before any ref
  update and left no journal or credential row.
- The 27 `ssh_key` orphan rows seen in round 1 are still present and untouched (all created
  21:19–21:24Z during round 1); no new orphan of any kind appeared during round 2.
- Scope: HTTP only. The SSH checks were a transport smoke (S15), not a full SSH-domain run.
- No containers were restarted and the repository was not modified during round 2.
