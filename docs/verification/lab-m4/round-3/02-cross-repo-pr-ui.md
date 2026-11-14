# Cross-repository pull requests from the browser — M4 closure round 3

Tester: t02. Date: 2026-11-14. Instance: main 800cad4.
Verdict: **PASS WITH DEFECTS** (#151 closed; one minor residual from #156).
Cases: 95 run, 94 pass, 1 fail, 0 skipped.

## Setup

- Created `lab02/r3-upstream` (public, `main`/`feature` plus contribution
  branches), `lab02/r3-priv` (private) and `lab02/r3-nonfork` (public, not a
  fork) through `POST /api/v1/organizations/lab02/repositories`.
- Forked `r3-upstream` to `lab02/r3-fork`, `lab02/r3-fork2`,
  `r3-scratch/r3-cross` (separate org), `lab03/r3-t03-fork` (owned by t03) and
  the now-deleted `lab02/r3-gone`; forked `r3-priv` to `lab02/r3-priv-fork`
  and `r3-scratch/r3-priv-cross` (private, unreadable by erin/t03/dave).
- Pushed contribution branches to the forks over HTTP Git, then drove the
  browser form (`GET /organizations/lab02/repositories/r3-upstream/pulls/new`)
  and its `POST .../pulls` with the session cookie and that session's CSRF
  token. Every API detail was cross-checked with `GET .../pulls/{n}` and Git
  `ls-remote`/`show`.
- Accounts used: t02 (owner lab02), erin (read grant lab02/acme), t03
  (owner lab03, no grant in lab02), dave (admin lab02, maintain beta).
- 17 initial failures were re-run once: 16 were my test data/expectations
  (a missing `docs` branch, hard-coded PR numbers, a key that is in fact
  valid, and the documented read-group behaviour of `pull_request.create`);
  one is the real defect below.

## Results

### A. Form and source-repository selector

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | New-PR form for the upstream as t02 | 200 | 200 | PASS |
| 2 | Selector defaults to the current repository | `value="" selected`, "This repository (r3-upstream)" | Same | PASS |
| 3 | Lists fork `lab02/r3-fork` | Option present | Present | PASS |
| 4 | Lists second fork `lab02/r3-fork2` | Option present | Present | PASS |
| 5 | Lists cross-org fork `r3-scratch/r3-cross` | Option present | Present | PASS |
| 6 | Lists another user's fork `lab03/r3-t03-fork` | Option present | Present | PASS |
| 7 | Deleted fork `lab02/r3-gone` omitted | Absent | Absent | PASS |
| 8 | Non-fork `lab02/r3-nonfork` omitted | Absent | Absent | PASS |
| 9 | Fork of another parent (`r3-priv-fork`) omitted | Absent | Absent | PASS |
| 10 | Option values are safe `org/name` specs | No HTML metacharacters | 5 values, all safe | PASS |
| 11 | Reader sees a readable private fork | `lab02/r3-priv-fork` present | Present | PASS |
| 12 | Reader does not see a private fork in an org she cannot read | `r3-scratch/r3-priv-cross` absent | Absent | PASS |
| 13 | New-PR page for that unreadable private fork | 404 | 404 | PASS |
| 14 | Non-member views a public repo's new-PR form | 200 | 200 | PASS |
| 15 | Unreadable private repo's form (`beta/app`) | 404 | 404 | PASS |
| 16 | Anonymous GET new-PR form | 303 `/login` | 303 `/login` | PASS |
| 17 | Read-grant target form (`acme/web`) still renders | 200, only "This repository" | 200 | PASS |
| 18 | Form for a nonexistent repository | 404 | 404 | PASS |

### B. Creating and merging cross-repository PRs from the form

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 19 | POST fork → upstream creates PR #2 | 303 `/pulls/2` | 303 `/pulls/2` | PASS |
| 20 | Cross-repo PR page renders | 200 | 200 | PASS |
| 21 | Detail names the fork | `source_repository_id` = `lab02/r3-fork` id | `fb2eacdb-…` | PASS |
| 22 | Head from fork, base from upstream | Fork tip / upstream main | `c3ff0de…` / `b7bd8de…` | PASS |
| 23 | Upstream list includes the PR | #2 present | #2 present | PASS |
| 24 | Fork page links to the outgoing PR | Link to upstream `/pulls/2` | Present | PASS |
| 25 | Fork page labels outgoing PRs | "Pull requests opened from this fork" | Present | PASS |
| 26 | PR from a fork in another org | 303 `/pulls/3` | 303 `/pulls/3` | PASS |
| 27 | Cross-org PR source id | `r3-cross` id | `76ad64b7-…` | PASS |
| 28 | PR from another user's readable fork | 303 `/pulls/4` | 303 `/pulls/4` | PASS |
| 29 | Other user's fork recorded as source | `r3-t03-fork` id | `e86e1ed5-…` | PASS |
| 30 | Same branch name on both sides (`feature`→`feature`) | 303 `/pulls/13` | 303 `/pulls/13` | PASS |
| 31 | Same-name detail | source=fork, `feature`→`feature` | Same | PASS |
| 32 | Draft cross-repo PR (#5) stores `draft` | `draft=true`, source=fork | Same | PASS |
| 33 | Draft PR page shows a Draft badge | Badge | Present | PASS |
| 34 | Duplicate open branch pair | 409 Conflict page | 409 | PASS |
| 35 | Missing source branch | 422 "Branch not found" | 422 | PASS |
| 36 | Missing target branch | 422 | 422 | PASS |
| 37 | Empty title | 400 + form error | 400 | PASS |
| 38 | 257-character title | 400 | 400 | PASS |
| 39 | 65,537-character body | 400 | 400 | PASS |
| 40 | Idempotent replay (same key/body) | 303 same `/pulls/2` | 303 `/pulls/2` | PASS |
| 41 | Same key, different body | 422 reuse notice | 422 | PASS |
| 42 | Key with a space (invalid) | 400 | 400 | PASS |
| 43 | Merge cross-repo PR from the browser | 303 back to PR | 303 | PASS |
| 44 | PR becomes merged | `merged` | `merged` | PASS |
| 45 | Fork file reaches target main | `origin/main:b.txt` = `b` | `b` | PASS |
| 46 | Merged cross-repo PR page | 200 | 200 | PASS |
| 47 | Cross-repo PR targets a non-default branch | 303 `/pulls/7` | 303 `/pulls/7` | PASS |
| 48 | Target branch stored | `feature` | `feature` | PASS |
| 49 | Empty `source_repository` = same-repo PR | 303 `/pulls/8` | 303 `/pulls/8` | PASS |
| 50 | Same-repo PR source is the target | `r3-upstream` id | `287953a1-…` | PASS |
| 51 | API same-repo create without the field | 201 | 201 (#9) | PASS |
| 52 | Same-repo PR with identical branches | 400 | 400 | PASS |
| 53 | API cross-repo create (control) | 201 | 201 (#16) | PASS |

### C. Authorization and existence-oracle edges

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 54 | Source = private repo t02 cannot read (`beta/app`) | 404 | 404 | PASS |
| 55 | Nonexistent source org | 404 | 404 | PASS |
| 56 | Nonexistent source repo | 404 | 404 | PASS |
| 57 | Source is not a fork of the target | 400 | 400 | PASS |
| 58 | Source is the target itself | 400 | 400 | PASS |
| 59 | Malformed `org/repo/extra` | 400 | 400 | PASS |
| 60 | Empty source org | 400 | 400 | PASS |
| 61 | Empty source name | 400 | 400 | PASS |
| 62 | NUL byte in source spec | 400 | 400 | PASS |
| 63 | Invalid title on an unreadable target | 404, not 400 | 404 | PASS |
| 64 | Valid body on an unreadable target | 404 | 404 | PASS |
| 65 | POST to a nonexistent target | 404 | 404 | PASS |
| 66 | Read-grant member creates a cross-repo PR | 303 (read group owns create) | 303; PR #10, author erin | PASS |
| 67 | Read-grant member API same-repo create | 201 | 201 (#15) | PASS |
| 68 | Non-member of a public repo creates | 403 | 403 | PASS |
| 69 | Non-member API create | 403 | 403 | PASS |
| 70 | Unreadable source fork, writable target (dave) | 404 | 404 | PASS |
| 71 | Readable private non-fork source (dave) | 400 | 400 | PASS |
| 72 | Anonymous POST | 303 `/login` | 303 `/login` | PASS |
| 73 | Missing CSRF | 403 "Invalid CSRF token" | 403 | PASS |
| 74 | Wrong CSRF | 403 | 403 | PASS |
| 75 | Read-grant target (`acme/web`) create reaches fork check | 400 (create permitted) | 400 | PASS |
| 76 | Source spec is case-insensitive | Resolves `lab02/R3-FORK` | 303 (#11) | PASS |
| 77 | Deleted fork as source | 404 | 404 | PASS |
| 78 | Fork-of-a-fork as source | 400 | 400 | PASS |
| 79 | Invalid title with unreadable vs nonexistent source | Identical 400 bodies, no name leak | Bodies byte-identical | PASS |

### D. Escaping

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 80 | Create with `<script>` title and `<img>` body | 303 to new PR | 303 `/pulls/14` | PASS |
| 81 | Title HTML-escaped on the PR page | `&lt;script&gt;`, no raw tag | Escaped | PASS |
| 82 | Body HTML-escaped on the PR page | `&lt;img …&gt;`, no raw tag | Escaped | PASS |
| 83 | `&` and unicode survive escaping | `&amp; café` | Correct | PASS |
| 84 | Invalid-title re-render escapes the echoed title | 400, no raw `<script>` | 400, escaped | PASS |
| 85 | Re-render keeps the selected fork | `lab02/r3-fork` selected | Selected | PASS |
| 86 | Re-rendered title input quotes escaped | `value="&quot;&gt;&lt;script&gt;` | Present | PASS |
| 87 | Same-repo form with `<b>` title | 303 | 303 | PASS |
| 88 | Same-repo PR page escapes `<b>` | `&lt;b&gt;` only | Escaped | PASS |

### E. Fork page and regression smoke

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 89 | Fork repository page renders | 200 | 200 | PASS |
| 90 | Fork page links back to its parent | "Forked from r3-upstream" link | **No parent link (0 occurrences of `r3-upstream`)** | **FAIL** |
| 91 | Fork form still offered | "Fork repository" button | Present | PASS |
| 92 | First cross-repo PR (merged) page | 200 | 200 | PASS |
| 93 | Upstream repository page | 200 | 200 | PASS |
| 94 | All created PRs present in the target list | #1–#12 present | Present | PASS |
| 95 | Fork API parent/children (regression #156) | Parent `r3-upstream`, child count | Parent present, children 1 | PASS |

## Defects

### D-01 (minor) The fork repository page still has no link back to its parent

- Severity: minor
- Scope: residual of #156, not a regression from the #151 fix.
- Repro:
  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
    -H 'Content-Type: application/json' \
    -d '{"username":"t02","password":"<test-password>"}' \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
  curl -s -H "Cookie: githearth_session=$TOKEN" \
    http://127.0.0.1:8080/organizations/lab02/repositories/r3-fork \
    | grep -c 'r3-upstream'    # 0
  # the page shows name, organization, visibility, status and the Fork card,
  # but no "forked from" line or parent href
  ```
- Expected: a "forked from `lab02/r3-upstream`" link on the fork's page (round 2
  case C10). The JSON API now does return the parent link.
- Actual: no parent link is rendered. `GET /api/v1/organizations/lab02/repositories/r3-fork/forks`
  returns `parent: r3-upstream` and `count: 1`, so the data exists; the browser
  page simply does not render it. `crates/githearth-server/src/ui.rs:852-861`
  builds the repository page from only name/organization/visibility/status/
  description plus the browse and fork cards, and `forks.rs:42-110` is
  API-only. #156 fixed the API half, not the page.
- Evidence: HTTP 200, zero occurrences of `r3-upstream` in the page body;
  `grep -c` = 0.

## Notes

- **#151 is closed.** A fork contributor can open a cross-repository pull
  request entirely in the browser: the selector lists only forks of the target
  that the viewer can read (`pulls_ui.rs:443-509`, including the per-fork
  `RepositoryRead` check at 491-498 and the `status = 'active'` filter), the
  selected `org/name` is forwarded as `source_repository`
  (`pulls_ui.rs:607-617`), and the PR page, list and merge all work
  (`pulls.rs:719-723`, `pulls.rs:480-507`). Verified merge commit `21a7ded`
  contains the fork's file.
- The authorization edges behave as specified: unreadable target or source is
  a generic 404, non-members get 403, a source that is not a direct fork is
  400, and an invalid title on an unreadable target is 404 rather than the
  readable-target 400 (`pulls_ui.rs:453-458`). Source resolution authorizes
  read before checking the fork relationship or status
  (`pulls.rs:639-660`), so private existence and fork status are not
  enumerable.
- `pull_request.create` is deliberately part of the `read` action group
  (`crates/githearth-authz/policies/githearth.cedarschema:153`,
  `builtin.cedar:80-92`), so a `read` grant can open a PR; the round 2 matrix
  expected this too. The "lacks write" 403 therefore comes from a viewer with
  no grant on the target (t03), which is what case 68/69 exercises.
- `source_repository` resolution is case-insensitive (`lab02/R3-FORK` was
  accepted); repository lookup is case-insensitive elsewhere, so this is
  consistent.
- Observation, not filed as a defect: the cross-repo PR page shows only the
  head SHA and the source branch name, not the fork (`.../pulls/2` contains no
  `r3-fork`). The API returns `source_repository_id`, and the fork page lists
  outgoing PRs, so the information is available elsewhere; a "from
  org/repo:branch" label would still help upstream reviewers.
- No other regression surfaced: same-repo PRs still default to the current
  repository (cases 2, 49-52), the fork form still renders, and the fork API
  parent/children listing still works.
