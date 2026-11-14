# Bounded resources & performance — M4 closure round 3

Tester: t18. Date: 2026-11-14. Instance: main 800cad4 (Docker Compose quick start, app up 17+ min, no restart during the run).
Verdict: **PASS WITH DEFECTS**
Cases: 70 total — 69 run (64 pass, 5 fail), 1 skipped. The 5 failures share one root cause (defect P1).

Fixtures: `lab18/r3-big` (73 MiB payload), `lab18/r3-small` (small clone target, later 250 MiB pushes), `lab18/r3-pr` (17,006 files, CODEOWNERS, 2 MiB blob, 1.4 MB text, 400-file branch), `lab18/r3-rebase` (150-commit branch). All created with `r3-` prefix; no existing data deleted. Results were collected while other testers were also driving the shared instance, so log-wide counts include their traffic; per-case numbers below are this run's.

Effective defaults (container env): `request_timeout_seconds=15`, `body_limit_bytes=524288`, `auth_concurrency=2`, `git_concurrency=8`, `git_timeout_seconds=600`, `git_request_limit_bytes=10 MiB`, `git_push_limit_bytes=1 GiB`, `repository_quota_bytes=1 GiB` (`crates/githearth-server/src/config.rs:63-78`).

## Results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| 1 | Create private repo `r3-big` via API | 201 | 201 | PASS |
| 2 | Push 73 MiB single commit over HTTP | Accepted, bounded | exit 0, 3.29 s | PASS |
| 3 | Remote `main` after push | Equals local | `8865b87…` = local | PASS |
| 4 | Full clone of 73 MiB repo | Success, intact | exit 0, 2.27 s; 10/10 objects, `git fsck` 0 fatal | PASS |
| 5 | Shallow clone `--depth=1` | Success | exit 0, 2.26 s, shallow=true | PASS |
| 6 | Warm re-clone | Success | exit 0, 2.30 s | PASS |
| 7 | Server storage delta for the push | ≈ +repo size | +73 MB (shard total 4513→4586 MB) | PASS |
| 8 | DB size before/after a Git-only push | Metadata only, no growth | 17 MB before and after | PASS |
| 9 | Push 17,006-file fixture + 2 MiB blob (r3-pr) | Accepted | exit 0, 0.81 s | PASS |
| 10 | Push base for rebase repo | Accepted | exit 0, 0.17 s | PASS |
| 11 | `git rebase main` of 150 commits after main advanced | Completes, linear | exit 0, 0.51 s; ahead 150 / behind 0 | PASS |
| 12 | Push rebased 150-commit branch | Accepted | exit 0, 0.24 s; remote = local | PASS |
| 13 | 20 concurrent full clones (small repo), run twice | All succeed (modest load) | 20/20 both runs, 0.40–0.77 s | PASS |
| 14 | 8 concurrent full clones | All succeed | 8/8, 0.25–0.37 s | PASS |
| 15 | 10 concurrent API reads, idle (`/refs`×3, `/tree`×3, `/commits`×2, `/blob`, `/compare`) | All 200 | 8/10 first run; 5 repeats 49/50 | **FAIL (P1)** |
| 16 | 4 concurrent clones + 10 API reads | Clones and reads all succeed | clones 4/4; reads 9/10 (one 503) | **FAIL (P1)** |
| 17 | 9 concurrent API reads ×5 rounds | All 200 | 49/50 | **FAIL (P1)** |
| 18 | 12 concurrent API reads ×5 rounds | All 200 | 56/60 | **FAIL (P1)** |
| 19 | 8 concurrent API reads ×5 rounds | All 200 | 40/40 | PASS |
| 20 | Retry an API read that returned 503 | 200 | 200 | PASS |
| 21 | 20 concurrent logins (correct password) | No 429/503, bounded latency | 20×200, max 0.140 s | PASS |
| 22 | 20 concurrent logins (wrong password) | 401, no 5xx | 20×401 | PASS |
| 23 | Login after both bursts | 200 (queue recovers) | 200, 0.014–0.017 s | PASS |
| 24 | 10 concurrent `/api/v1/me` | 200 | 10×200, ≈2 ms | PASS |
| 25 | 20 concurrent `/health/ready` | 200 | 20×200, max 7 ms | PASS |
| 26 | 15 concurrent issue lists (`limit=100`) | 200, DB pool queues | 15×200, 4–6 ms | PASS |
| 27 | 10 concurrent issue creates | 201 | 10×201, max 15 ms | PASS |
| 28 | 10 API reads immediately after an 8×73 MiB clone burst | All 200 | 9/10 (one 503) | **FAIL (P1)** |
| 29 | 8 concurrent 73 MiB clones (at budget) | All succeed | 8/8, 5.11–6.02 s | PASS |
| 30 | 12 concurrent 73 MiB clones (over budget) | Excess refused cleanly, then recover | 8 ok; 4×503 + `Retry-After: 5`; retries 2.24–2.35 s | PASS (bounded) |
| 31 | 5 concurrent 100-commit log reads | 200 | 5×200, 20–25 ms, 27,223 B each | PASS |
| 32 | `GET /tree/main` (root) | 200 | 200, 829 B, 21 ms | PASS |
| 33 | `GET /tree/main?path=big` (12,000 entries) | Bounded | 200, 1,308,061 B, 32 ms | PASS (bounded; see N3) |
| 34 | Same tree at 20,000 entries, twice | 413, not 500 | 413 `{"error":"response too large"}` (30 B), 29–38 ms | PASS |
| 35 | `GET /tree/main?path=mid` (5,000 entries) | 200, bounded | 200, 545,061 B, 24 ms | PASS |
| 36 | `GET /blob` 2 MiB binary | Truncated, bounded | 200, `truncated:true`, 1,398,211 B, 48 ms | PASS |
| 37 | `GET /blob` 1.4 MB text | Truncated at 1 MiB | 200, `truncated:true`, text 1,048,576 B, body 1,064,569 B | PASS |
| 38 | `GET /blob` small file | 200 | 200, 110 B | PASS |
| 39 | `GET /commits?limit=100 / 101 / 0` | 200 / 400 / 400 | 200 / 400 / 400 | PASS |
| 40 | Commit-log cursor paging on 152 commits; bad cursors | 2 pages, unique; invalid → 400 | 100+52 unique; `cursor=abc` and `>i32::MAX` → 400 | PASS |
| 41 | `GET /compare/main...many` (400 files) | 300-file cap, bounded | 200, 300 files, `truncated:true`, 84,163 B, 81 ms | PASS |
| 42 | `GET /compare/main...feat1` | 200 | 200, 1,074 B, 72 ms | PASS |
| 43 | PR create with CODEOWNERS on target (2-file diff) | 201, owners resolved | 201, 106 ms, `required_code_owners:[@t18 approved]` | PASS |
| 44 | 5 sequential PR creates | 201, stable latency | 201 ×5, 98–111 ms | PASS |
| 45 | PR create from a 400-file branch | 201, capped diff pass | 201, 108 ms | PASS |
| 46 | PATCH PR title (re-runs CODEOWNERS pass) | 200 | 200, 113–122 ms | PASS |
| 47 | PATCH PR body 60 KB / 70 KB | 200 / 400 | 200, 103 ms (60,683 B) / 400 | PASS |
| 48 | PATCH PR state=merged; PATCH missing PR | 400; 404 | 400; 404 | PASS |
| 49 | 4 concurrent PR creates | All 201 | 4×201, 95–101 ms | PASS |
| 50 | PR list pagination `limit=2`; `limit=101`; `after=0` | Pages with no dupes; 400; empty | 3 pages, 5 unique, no dupes; 400; `{"items":[],"next":null}` | PASS |
| 51 | 45 sequential issue creates | 201, bounded total | 201 ×45, 0.103 s total, p50 2 ms, max 4 ms | PASS |
| 52 | Issue list `limit=20` pagination | 20/20/5, next cursors | 45 unique, 5,101 / 5,091 / 1,279 B, no dupes | PASS |
| 53 | Issue filters: `state=all/closed/bad`, `limit=0/101`, `after=0` | 200/empty/400/empty | 45 items 11,429 B / empty / 400 / 400 / empty | PASS |
| 54 | Issue PATCH; bad state; missing number | 200; 400; 404 | 200 (2 ms); 400; 404 | PASS |
| 55 | Slow reader aborted mid-response (545 KB tree, 10 KB/s, kill at 1 s) | Server unaffected | 200 started, 20,280 B sent then abort; ready 200; follow-up full 545 KB in 36 ms | PASS |
| 56 | Clone killed mid-transfer (73 MiB) | Slot released, no orphans | next clone 0.286 s; 0 upload-pack/pack-objects children | PASS |
| 57 | 2 stalled `git-upload-pack` POST bodies | Server healthy, slots recover | ready 200; concurrent clone 0.252 s; 0 children after abort | PASS |
| 58 | 250 MiB push aborted with partial body | Ref unchanged, no stale lock, usable | ref unchanged, 0 lock files, ready 200; later full push 10.15 s → ref matches | PASS |
| 59 | 40 MiB push whose client was killed at 0.4 s | Either applied or not, never corrupt | request had completed: ref moved, server healthy | PASS (note N5) |
| 60 | JSON request body trickled past 15 s | 408, connection closed | `HTTP/1.1 408 Request Timeout` at 15.01 s | PASS (by design) |
| 61 | 600 KB JSON body; 11 MiB upload-pack body | 413, fast | 413 at 2 ms (`length limit exceeded`); 413 at 23 ms | PASS |
| 62 | 2 half-header (slowloris-style) sockets for 4 s | Other traffic unaffected | ready 200 during and after; no leftover processes | PASS |
| 63 | Server RSS before load / after large transfers | Bounded, no runaway | 78.6 MB → 252–258 MB, flat across two further 8-clone bursts | PASS (note N4) |
| 64 | Git child RSS and orphan check | ~25 MiB per child, none left | 48 MB for one 73 MiB clone; 0 orphans after aborts | PASS |
| 65 | `docker stats` after run | Healthy | app 350.6 MiB (incl. page cache), postgres 127.2 MiB; cgroup peak 3.23 GB (file cache) | PASS (note N4) |
| 66 | DB size before/after the whole run | Small metadata growth | 17 MB → 21 MB | PASS |
| 67 | Storage volume before/after (shared instance) | Bounded per repo | 4,513 → 7,302 MB; this run added 693 MB in 5 repos (largest 541 MB) | PASS |
| 68 | Health/log scan for the window | No 5xx other than bounded 503 | `ready` 200 at 1.3–2.5 ms; 0×500/502/504; 1×408 (case 60); 133×503 in the shared window, ~15 from this run | PASS (P1) |
| 69 | Git operation hitting the 600 s `git_timeout_seconds` | 503 `git operation timed out` | Not run (would need a 10-min stall); mapping verified in source | SKIPPED |
| 70 | Git transport 503 headers under saturation | 503 + `Retry-After` | 503, `Retry-After: 5`, `text/plain` | PASS |

## Defects

### P1. JSON Git-backed reads fail fast at 9–12 concurrency with 503 and no `Retry-After`
- Severity: **minor** (transient, retry succeeds; bounded by design, but user-visible and silent about backoff)
- Repro: with a bearer token, issue 10 concurrent `GET`s mixing `/refs`, `/tree/main`, `/commits/main`, `/blob`, `/compare` on `lab18/r3-small`; repeat 5 times. Repeat while 4 clones or 8×73 MiB clones run. Also observed immediately after a clone burst.
- Expected: at the brief's normal-load bar, 10 concurrent reads should all succeed, or a refusal should tell the client when to retry (as the Git transport does).
- Actual: 1–2 of 10 return `503 {"error":"git requests busy"}` (29 B) with **no** `Retry-After` header. 9 concurrent: 1/50; 12 concurrent: 4/60; 10 reads right after an 8-clone burst: 1/10. A retry immediately afterwards returns 200. The same 8-slot budget also refuses unrelated `POST /pulls` (PR create) while clones run; 9 + 10 such 503s appear in the shared log from concurrent testers.
- Evidence:
  - `crates/githearth-server/src/browse.rs:78-84` — `acquire_permit` uses `try_acquire_owned()`, so the 9th concurrent Git-backed read is refused immediately, not queued.
  - `crates/githearth-server/src/git_http.rs:1012-1018` — the Git transport takes the same `state.git_slots`; `lib.rs:152-153` and `config.rs:74` give one 8-permit budget for both.
  - `crates/githearth-server/src/git_http.rs:1403-1409` — Git transport 503 sets `Retry-After: 5`; `crates/githearth-server/src/lib.rs:418-423` — the JSON `ApiError` path sets no headers, so API 503s have no retry hint.
  - Log routes carrying 503 in the window: `/blob/{rev}`, `/commits/{rev}`, `/compare/{spec}`, `/refs`, `/tree/{rev}`, `/pulls`, `/info/refs`, `/git-upload-pack`.
- Suggestion: queue briefly (like the 2 s auth queue) or emit `Retry-After` on JSON 503s and document the shared budget. Not a data-integrity issue; server stayed ready throughout.

## Notes

- **N1. Bounded, not unbounded.** All large payloads were bounded: tree 413 at 20k entries (30 B), blob truncation at 1 MiB (text 1.06 MB; base64 1.40 MB), compare capped at 300 files (84 KB), log capped at 100 commits (27 KB), issue pages 5 KB, PR pages 0.6–60 KB (the 60 KB page is the 60 KB PR body from case 47). 12 concurrent 73 MiB clones exceeded the budget exactly as documented (`docs/verification/m4-qualification.md:47`): 8 ran, 4 got 503+`Retry-After`, and all succeeded on retry.
- **N2. No hidden timeouts under normal load.** No 500/502/504 in the window; every normal request finished well inside 15 s (p50 browse 18–36 ms, PR create/PATCH ≈0.1 s, auth burst max 0.14 s). The only 408 was the deliberate 15 s slow-body case (60). The 600 s Git timeout was not reachable without a 10-minute stall; `browse.rs:169-180` maps `StorageError::Timeout` to 503 `git operation timed out`, and `git_http.rs` enforces the same deadline on transport operations.
- **N3. 12k tree is 1.31 MB, not 1 MiB.** The 1 MiB cap is on `git ls-tree` output (`crates/githearth-git/src/browse.rs:20,297`); JSON encoding adds ~30 %, so the response can reach ~1.4 MB before the 20k-entry 413. Bounded, but worth knowing for proxies/logging limits.
- **N4. Server RSS settles at ~252 MB after large transfers.** Idle after start: 78.6 MB. After 73 MB pushes, two 250 MB pushes and clone bursts: 252–258 MB, then exactly flat across two further 8×73 MiB clone bursts (no growth), so this is retained allocator high-water, not an unbounded leak. It is above the ~100 MB server figure in `docs/operations/hardware.md:18-33`, but far below the 1 GiB recommendation. `docker stats` reports 350 MB because it includes page cache (cgroup `file` 2.8 GB, peak 3.23 GB); `VmRSS`/`VmHWM` of PID 1 was 257,920 kB, one 73 MiB clone child added 48 MB. No orphaned git children after any abort.
- **N5. Client-killed pushes are ambiguous, not corrupting.** A 40 MiB push killed at 0.4 s had already arrived in full, so the ref moved (the server finished a complete request). A 250 MiB push killed at 0.15 s left the ref untouched and no lock. This matches at-most-once client visibility; the server side was consistent in both cases.
- **N6. Auth burst is healthy.** 20 simultaneous logins all succeeded (max 140 ms) despite `auth_concurrency=2`; the 2 s queue is sufficient. 20 bad-password logins returned 401 with no 5xx.
- **N7. Shared instance.** Other testers were active throughout (log traffic, `r3-*` repos in other orgs, 503s on `/pulls` and `/info/refs` not caused by this run). Baseline storage/DB size and log-wide 503 counts therefore include their data; per-case measurements above were taken from this run's responses.
- **N8. Test mistakes (not product defects).** First log-pagination attempt used `after=` instead of `cursor=` (400 unknown field); the 400-file PR/issue PATCH initially used issue #1 when PR numbers 1–5 had consumed the shared number space (issues started at 6), giving 404; the first slow-read test hit the 20k tree (already 413) instead of `mid`. All were corrected and re-run; the results above are the corrected runs.
- No repository files were edited, no application restarts were performed, and no data other than `lab18/r3-*` was created by this run.
