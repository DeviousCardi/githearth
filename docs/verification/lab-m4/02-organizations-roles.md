# Organizations, memberships and roles — M4 lab acceptance report

Tester: t02. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 131 run, 127 pass, 1 fail, 3 skipped.

## Results

### A. Organization creation

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A01 | create org: valid slug/name | 201 with id, slug, name, role=owner | 201 id=3b404575... name=T02 Alpha 7feec4 role=owner | PASS |
| A02 | creator reads the new org immediately | 200, role owner | 200 id=3b404575... name=T02 Alpha 7feec4 role=owner | PASS |
| A03 | creator acts as owner immediately (repository.create) | 201 | 201 id=4ea3238a... name=t02-owner-probe-7feec4 visibility=private | PASS |
| A04 | owner can delete the repository (repository.delete) | 204 | 204 null | PASS |
| A05 | duplicate slug | 409, generic error | 409 {"error": "already exists"} | PASS |
| A06 | duplicate slug owned by another org (global uniqueness) | 409 | 409 {"error": "already exists"} | PASS |
| A07 | empty slug | 400 | 400 {"error": "invalid input"} | PASS |
| A08 | uppercase slug | 400 | 400 {"error": "invalid input"} | PASS |
| A09 | unicode slug | 400 | 400 {"error": "invalid input"} | PASS |
| A10 | leading hyphen | 400 | 400 {"error": "invalid input"} | PASS |
| A11 | trailing hyphen | 400 | 400 {"error": "invalid input"} | PASS |
| A12 | double hyphen | 400 | 400 {"error": "invalid input"} | PASS |
| A13 | slug 65 chars | 400 | 400 {"error": "invalid input"} | PASS |
| A14 | slug 64 chars (boundary) | 201 | 201 id=e31006a5... name=T02 64 char slug role=owner | PASS |
| A15 | slug 1 char (lower boundary) | 201 | 201 id=d0b856c6... name=T02 single char role=owner | PASS |
| A16 | slug with underscore | 400 | 400 {"error": "invalid input"} | PASS |
| A17 | slug with dot | 400 | 400 {"error": "invalid input"} | PASS |
| A18 | slug with space | 400 | 400 {"error": "invalid input"} | PASS |
| A19 | blank name (spaces only) | 400 | 400 {"error": "invalid input"} | PASS |
| A20 | name 200 chars (boundary) | 201 | 201 id=eefd2b5e... name=&lt;200 Ns&gt; slug=t02-n200-7feec4 role=owner | PASS |
| A21 | name 201 chars | 400 | 400 {"error": "invalid input"} | PASS |
| A22 | name 200 Unicode chars (400 bytes) | 201 | 201 id=23bb3822... name=&lt;200 × é&gt; slug=t02-u200-7feec4 role=owner | PASS |
| A23 | name with surrounding spaces | 201 (docs: nonblank, no trim promise) | 201 stored_name=  T02 padded   | PASS |
| A24 | missing name field | 400/422 | 422 Failed to deserialize the JSON body into the target type: missing field `name` at line 1 column 29 | PASS |
| A25 | malformed JSON | 400 | 400 Failed to parse the request body as JSON: key must be a string at line 1 column 2 | PASS |
| A26 | unknown field (deny_unknown_fields) | 400/422 | 422 Failed to deserialize the JSON body into the target type: extra: unknown field `extra`, expected `slug` or `name` at line 1 column 45 | PASS |
| A27 | missing Content-Type | 415 | 415 Expected request with `Content-Type: application/json` | PASS |
| A28 | unauthenticated create | 401 | 401 {"error": "invalid or expired credentials"} | PASS |
| A29 | invalid bearer create | 401 | 401 {"error": "invalid or expired credentials"} | PASS |
| A30 | create repo:read access token for boundary test | 201 + plaintext | 201 token=gth_6c67... | PASS |
| A31 | read-only access token cannot create an org | 403 read-only token | 403 {"error": "read-only token"} | PASS |

### B. List and read

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| B01 | list: membership-filtered to own orgs (acme read membership included) | 200; includes lab02/acme/created; excludes beta/lab03 | 200 8 orgs: ["t02-u200-7feec4", "t02-alpha-7feec4", "acme", "lab02", "0", "t02-pad-7feec4", "t02-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "t02-n200-... | PASS |
| B02 | list: display role owner for own org, member elsewhere | lab02=owner, acme=member | lab02=owner acme=member | PASS |
| B03 | list unauthenticated | 401 | 401 {"error": "invalid or expired credentials"} | PASS |
| B04 | list bound: limit=0 | 400 | 400 {"error": "invalid input"} | PASS |
| B05 | list bound: limit=101 | 400 | 400 {"error": "invalid input"} | PASS |
| B06 | list bound: limit=-1 | 400 | 400 {"error": "invalid input"} | PASS |
| B07 | list bound: limit=abc | 400 | 400 Failed to deserialize query string: limit: invalid digit found in string | PASS |
| B08 | list: malformed after cursor | 400 | 400 Failed to deserialize query string: after: UUID parsing failed: invalid character: found `n` at 0 | PASS |
| B09 | cursor pagination limit=1: every visible org once, UUID order | 8 ids in order | 8 ids, equal=True  | PASS |
| B10 | after max UUID | 200 [] | 200 [] | PASS |
| B11 | after zero UUID returns the full visible set | 200, same as unpaged | 200 8 | PASS |
| B12 | read own org (lab02) | 200 role owner | 200 id=b0000000... name=Lab 02 role=owner | PASS |
| B13 | responses are Cache-Control: no-store | no-store | cache-control=no-store | PASS |
| B14 | read acme (member with read grant) | 200 role member | 200 id=a0000000... name=Acme Corporation role=member | PASS |
| B15 | read beta (non-member): 404, no existence leak | 404 organization not found | 404 {"error": "organization not found"} | PASS |
| B16 | read lab03 (non-member): 404, no existence leak | 404 organization not found | 404 {"error": "organization not found"} | PASS |
| B17 | read nonexistent UUID: 404, no existence leak | 404 organization not found | 404 {"error": "organization not found"} | PASS |
| B18 | read malformed UUID | 400 | 400 Invalid URL: Cannot parse `id` with value `not-a-uuid`: UUID parsing failed: invalid character: found `n` at 0 | PASS |
| B19 | read unauthenticated | 401 | 401 {"error": "invalid or expired credentials"} | PASS |
| B20 | platform admin cannot read a non-member org | 404 (no tenant-admin bypass) | 404 {"error": "organization not found"} | PASS |

### C. Role ladder in acme (issue #133 headline)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C01 | bob creates a repository in acme (slug path) | 201 | 201 id=d3dfa9f9... name=t02-acme-bob-4eb09c visibility=private | PASS |
| C02 | carol creates a repository in acme (slug path) | 201 | 201 id=8253c26c... name=t02-acme-carol-4eb09c visibility=private | PASS |
| C03 | dave creates a repository in acme (slug path) | 201 | 201 id=6a50f07a... name=t02-acme-dave-4eb09c visibility=private | PASS |
| C04 | grace creates a repository in acme (slug path) | 201 | 201 id=d296bfd3... name=t02-acme-grace-4eb09c visibility=private | PASS |
| C05 | erin creates a repository in acme (slug path) | 403 | 403 {"error": "not permitted"} | PASS |
| C06 | frank creates a repository in acme (slug path) | 403 | 403 {"error": "not permitted"} | PASS |
| C07 | t03 creates a repository in acme (slug path) | 403 | 403 {"error": "not permitted"} | PASS |
| C08 | admin creates a repository in acme (slug path) | 404 | 404 {"error": "organization not found"} | PASS |
| C09 | alice creates a repository in acme (slug path) | 201 | 201 id=ba7f6fbd... name=t02-acme-alice-4eb09c visibility=private | PASS |
| C10 | 403 bodies are generic and leak no org internals | 403 {error: not permitted} | erin={"error": "not permitted"} frank={"error": "not permitted"} t03={"error": "not permitted"} | PASS |
| C11 | non-member (platform admin) create -> 404, no existence leak | 404 organization not found | 404 {"error": "organization not found"} | PASS |
| C12 | bob reads acme/web (private) across the ladder | 200 | 200 id=83ca9b35... name=web visibility=private | PASS |
| C13 | carol reads acme/web (private) across the ladder | 200 | 200 id=83ca9b35... name=web visibility=private | PASS |
| C14 | dave reads acme/web (private) across the ladder | 200 | 200 id=83ca9b35... name=web visibility=private | PASS |
| C15 | erin reads acme/web (private) across the ladder | 200 | 200 id=83ca9b35... name=web visibility=private | PASS |
| C16 | frank reads acme/web (private) across the ladder | 200 | 200 id=83ca9b35... name=web visibility=private | PASS |
| C17 | grace reads acme/web (private) across the ladder | 404 | 404 {"error": "not found"} | PASS |
| C18-alice | alice deletes the alice test repository | 204 | 204 null | PASS |
| C18-bob | alice deletes the bob test repository | 204 | 204 null | PASS |
| C18-carol | alice deletes the carol test repository | 204 | 204 null | PASS |
| C18-dave | alice deletes the dave test repository | 204 | 204 null | PASS |
| C18-grace | alice deletes the grace test repository | 204 | 204 null | PASS |
| C20 | t02 (acme read grant) cannot create a repository | 403 | 403 {"error": "not permitted"} | PASS |
| C21 | t03 (acme read grant) can read acme/web | 200 | 200 name=web visibility=private | PASS |

### D. Role ladder in lab02 and cumulative permissions

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| D01 | bob creates a repository in lab02 (t02 owns it) | 201 | 201 id=1e99b57e... name=t02-lab02-bob-7feec4 visibility=private | PASS |
| D02 | carol creates a repository in lab02 (t02 owns it) | 201 | 201 id=f0b18928... name=t02-lab02-carol-7feec4 visibility=private | PASS |
| D03 | dave creates a repository in lab02 (t02 owns it) | 201 | 201 id=5873c46d... name=t02-lab02-dave-7feec4 visibility=private | PASS |
| D04 | grace creates a repository in lab02 (t02 owns it) | 201 | 201 id=18fda7f1... name=t02-lab02-grace-7feec4 visibility=private | PASS |
| D05 | erin creates a repository in lab02 (t02 owns it) | 403 | 403 {"error": "not permitted"} | PASS |
| D06 | frank creates a repository in lab02 (t02 owns it) | 403 | 403 {"error": "not permitted"} | PASS |
| D07 | t03 creates a repository in lab02 (t02 owns it) | 404 | 404 {"error": "organization not found"} | PASS |
| D08 | admin creates a repository in lab02 (t02 owns it) | 404 | 404 {"error": "organization not found"} | PASS |
| D09 | t02 creates a repository in lab02 (t02 owns it) | 201 | 201 id=08272f20... name=t02-lab02-t02-7feec4 visibility=private | PASS |
| D10 | write can read a private repository | 200 | 200 id=1e99b57e... name=t02-lab02-bob-7feec4 visibility=private | PASS |
| D11 | read can read/browse a private repository | 200 | 200 id=1e99b57e... name=t02-lab02-bob-7feec4 visibility=private | PASS |
| D12 | triage can read a private repository | 200 | 200 id=1e99b57e... name=t02-lab02-bob-7feec4 visibility=private | PASS |
| D13 | member cannot read a private repository (pinned asymmetry) | 404 | 404 {"error": "not found"} | PASS |
| D14 | owner creates an internal repository | 201 | 201 id=a7df1b96... name=t02-lab02-internal-7feec4 visibility=internal | PASS |
| D15 | member can read an internal repository | 200 | 200 id=a7df1b96... name=t02-lab02-internal-7feec4 visibility=internal | PASS |
| D16 | write cannot maintain (repository.update -> 403) | 403 | 403 {"error": "not permitted"} | PASS |
| D17 | maintain can update a repository | 200 | 200 id=f0b18928... name=t02-lab02-carol-7feec4 visibility=private | PASS |
| D18 | admin can maintain (update description) | 200 | 200 id=5873c46d... name=t02-lab02-dave-7feec4 visibility=private | PASS |
| D19 | maintain cannot delete a repository (admin only) | 403 | 403 {"error": "not permitted"} | PASS |
| D20-0 | t02 (owner) deletes t02-lab02-bob-7feec4 | 204 | 204 null | PASS |
| D20-1 | t02 (owner) deletes t02-lab02-carol-7feec4 | 204 | 204 null | PASS |
| D20-2 | t02 (owner) deletes t02-lab02-dave-7feec4 | 204 | 204 null | PASS |
| D20-3 | t02 (owner) deletes t02-lab02-grace-7feec4 | 204 | 204 null | PASS |
| D20-4 | t02 (owner) deletes t02-lab02-t02-7feec4 | 204 | 204 null | PASS |
| D20-5 | t02 (owner) deletes t02-lab02-internal-7feec4 | 204 | 204 null | PASS |
| D21 | dave (admin) deletes a repository | 204 | 204 null | PASS |

### E. Git write ladder (lab02)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| E01 | bob can push (cumulative write ladder) | push accepted | push accepted | PASS |
| E02 | carol can push (cumulative write ladder) | push accepted | push accepted | PASS |
| E03 | dave can push (cumulative write ladder) | push accepted | push accepted | PASS |
| E04 | t02 can push (cumulative write ladder) | push accepted | push accepted | PASS |
| E05 | erin cannot push (write/triage ladder) | push refused | push refused: remote: permission denied: ref.push         To http://127.0.0.1:8080/lab02/t02-lab02-bob-7feec4.git  ! [remote rejected] HEAD -> t02-erin-7feec4 (pre-receiv... | PASS |
| E06 | frank cannot push (write/triage ladder) | push refused | push refused: remote: permission denied: ref.push         To http://127.0.0.1:8080/lab02/t02-lab02-bob-7feec4.git  ! [remote rejected] HEAD -> t02-frank-7feec4 (pre-recei... | PASS |

### F. Membership display and management surface

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| F01 | owner membership fields are stable across reads | identical id/slug/name/role | 200/200 equal=True | PASS |
| F02 | membership management endpoint is absent | 404 route not found | 404 null | PASS |
| F03 | membership administration (add/remove/change role) | not implemented (docs/foundation.md:111; docs/decisions/0002-authorization.md:80) | no endpoint exists; GET /members -> 404 | SKIP |
| F04 | organization delete endpoint absent (owner role action has no route) | 405 (GET-only route) | 405 null | PASS |
| F05 | non-owner membership displays role=member (grant role is not exposed) | role=member despite write grant | bob acme role=member | PASS |

### G. Tenant isolation

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| G01 | tenant list isolation | excludes beta, lab03 | beta=False lab03=False | PASS |
| G02 | create repository in a foreign org -> 404 (not 403) | 404 organization not found | 404 {"error": "organization not found"} | PASS |
| G03 | create repository in beta -> 404 | 404 | 404 {"error": "organization not found"} | PASS |
| G04 | read a private repo in beta -> 404 | 404 | 404 {"error": "not found"} | PASS |
| G05 | platform admin cannot create in a foreign org | 404 | 404 {"error": "organization not found"} | PASS |
| G06 | foreign-org create denial body leaks no SQL/internal ids | only generic error | {"error": "organization not found"} | PASS |
| G07 | foreign-org read denial body leaks no SQL/internal ids | only generic error | {"error": "organization not found"} | PASS |
| G08 | non-member and nonexistent org are indistinguishable | same 404 body | 404/404 equal=True | PASS |
| G09 | repository 404 body hides foreign organization/repository existence | identical 404 body for denied, missing repository and missing organization (ADR 0002:66) | denied-existing={"error":"not found"} missing-repo={"error":"repository not found"} missing-org={"error":"organization not found"} | FAIL |

### H. Idempotency on organization create

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| H01 | same Idempotency-Key + same payload | replay not implemented: second identical request hits duplicate slug (docs/foundation.md:111) | first=201, second=409 {"error": "already exists"} | SKIP |
| H03 | same Idempotency-Key + different payload | ignored: creates a second org (docs/foundation.md:111) | 201 id=84966431... name=Idem B role=owner | SKIP |
| H04 | no Idempotency-Key: two creates make two orgs | 201 + 201, different ids | 201/201 distinct=True | PASS |

### I. Concurrency and ordering

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| I01 | concurrent duplicate-slug creates: exactly one 201, rest 409 | one 201 + four 409; one org row | statuses=[201, 409, 409, 409, 409] rows=1 | PASS |
| I02 | concurrent distinct-slug creates all succeed | four 201 | statuses=[201, 201, 201, 201] | PASS |
| I03 | organization list is UUID-ordered | strictly ascending ids | count=22 sorted=True | PASS |
| I04 | after a foreign org UUID: only own orgs, no existence leak | 200, no lab03 | 200, own orgs only; lab03 absent | PASS |
| I05 | membership list is identical across two sessions | same rows/roles | 200/200 equal=True | PASS |
| I06 | member cannot delete a repository (admin only; denial masked as 404 because member cannot read private) | 404 not found | 404 {"error": "not found"} | PASS |
| I07 | owner cleanup of the member-created repo | 204 | 204 | PASS |

## Defects

### Repository 404 bodies leak whether a foreign organization and private repository exist

- Severity: minor
- Repro:
  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login -H 'Content-Type: application/json' -d '{"username":"t02","password":"<test-password>"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
  AUTH="Authorization: Bearer $TOKEN"

  curl -s -H "$AUTH" http://127.0.0.1:8080/api/v1/organizations/beta/repositories/app
  # 404 {"error":"not found"}               # org + private repo exist, access denied

  curl -s -H "$AUTH" http://127.0.0.1:8080/api/v1/organizations/beta/repositories/no-such-repo
  # 404 {"error":"repository not found"}    # org exists, repository does not

  curl -s -H "$AUTH" http://127.0.0.1:8080/api/v1/organizations/no-such-org/repositories/app
  # 404 {"error":"organization not found"}  # organization does not exist
  ```
- Expected: every denial and every missing object returns one indistinguishable 404 body.
  ADR 0002 states that services "return 404 when it is missing *or* the decision denies,
  so a denial reveals nothing" (`docs/decisions/0002-authorization.md:66`); an outsider
  must not learn whether a foreign organization or private repository exists.
- Actual: three distinct 404 bodies. `t02` has no membership in `beta` yet can tell
  "beta exists and `app` exists but is denied" from "beta exists, repository absent"
  from "organization absent". The status is uniformly 404; only the body differs, so this
  is an authenticated existence oracle for organization slugs and private repository
  names. Reproduced twice, on both attempts, with the exact bodies above.
- Evidence: `crates/githearth-server/src/repository_api.rs:18-22` defines three different
  messages (`not_found()` = `"not found"`, `org_not_found()` = `"organization not found"`,
  and the repository-missing site `:106-108` = `"repository not found"`). `load_repo`
  (`:96-108`) proceeds for a non-member because `facts::load` returns facts with an empty
  organization list (`crates/githearth-server/src/facts.rs:98-128`), and the denial at
  `:194-197` returns `not_found()` only after the repository lookup succeeded or failed.
- Scope: systematic for the repository routes that use `load_repo`/`load_current`
  (read, update, delete, archive, access); any authenticated non-member can probe.
  No SQL text, internal ids or repository contents leak, only existence/name.


## Notes / skipped

- **Issue #133 (headline fix) is live.** In `acme`, `bob` (write), `carol` (maintain),
  `dave` (admin), `grace` (member/base) and `alice` (owner) each created a repository
  (201) and `erin` (read) / `frank` (triage) got 403, not 404. A non-member gets 404:
  `t03` in `lab02` and the platform administrator in `acme` (the admin is a non-member
  there). The same ladder was reproduced in `lab02` with fresh `t02-` names. All created
  repositories were deleted by an owner/admin account.
- **403 vs 404 is right at the organization boundary.** A member of the organization who
  lacks `repository.create` (`t03` and `t02` in `acme`, both read grants) gets 403
  `{"error":"not permitted"}`. A caller with no membership gets 404
  `{"error":"organization not found"}`, byte-for-byte identical to a nonexistent UUID.
  No foreign UUIDs or SQL fragments appeared in denial bodies. The repository subresource
  bodies do differ; see the defect above.
- **Cumulative ladder observed.** write read a private repo (200); maintain pushed and
  updated a repository (200); admin maintained and deleted; owner did everything.
  `read` and `triage` could read but not push or create. The deliberate non-linearity is
  live and pinned: `member` can `repository.create`, `read`/`triage` cannot, and `read`
  does not carry `member`. Consequently a `member` cannot read a **private** repository
  (404) even one it just created; it can read an **internal** repository. That is asserted
  by `crates/githearth-authz/tests/decisions.rs` ("member cannot read private repo") and
  matches the templates in `crates/githearth-authz/policies/roles/`.
- **Membership display.** `role` comes from the membership row: `owner` for the creator
  of an org, `member` for every other member. It does not expose the grant role
  (bob/carol/dave all display `member` while holding write/maintain/admin). That is the
  M1 mapping described in docs/decisions/0002-authorization.md:38 and what the campaign
  brief expects.
- **No membership administration or organization deletion API.** `GET
  /api/v1/organizations/{id}/members` is an unmatched route (404); `DELETE
  /api/v1/organizations/{id}` is 405 although the engine has an owner-level
  `organization.delete` action. docs/foundation.md:111 lists membership administration
  as later work and M4 promises neither endpoint. Membership administration is marked
  skipped (F03); the 405 is recorded as observed, not a defect.
- **Organization creation has no operation-ID replay.** `Idempotency-Key` is ignored on
  `POST /api/v1/organizations` (documented at docs/foundation.md:111). With the same key
  and payload the second request returns 409 duplicate-slug; with the same key and a new
  slug it creates a second organization. Missing the header creates one org per request.
  Recorded as skipped (H01, H03) with the doc reference, not as a defect. Repository
  creation does use the shared idempotency module (`repository_api.rs:251-392`), so the
  omission is specific to organization creation.
- **Names are not trimmed.** `"  T02 padded  "` is accepted and stored verbatim; the docs
  only require nonblank and at most 200 Unicode characters (docs/foundation.md:107), so
  this is recorded as observation, not a defect. Whitespace-only names are rejected (400).
- **Fixture difference.** The brief says `t02`/`t03` are non-members of `acme`; on this
  deployment both are `acme` members with `read` grants (display role `member`, create ->
  403). The true non-member 404 was therefore verified with `t03` in `lab02` and with the
  platform administrator in `acme`. This is an environment observation, not a product
  defect.
- **Boundaries passed.** slug 1 char and 64 chars accepted, 65 rejected; name 200 chars
  and 200 Unicode chars accepted, 201 rejected; limit 1..100 accepted, 0/101/-1 rejected;
  cursor pages had no duplicates or omissions and stayed UUID-ordered; two concurrent
  duplicate-slug creates produced exactly one 201, four 409 and one row; four concurrent
  distinct creates all succeeded.
- **Test artifacts.** Test organizations use the `t02-` prefix, except the one-character
  boundary slug `0`; about twenty were created for boundary, pagination and concurrency
  coverage and remain because no org-delete route exists. No repository remains in `acme`
  or `lab02` from this run. The acme ladder was re-run once after a harness mistake (UUID
  in the path instead of the slug); the corrected run passed and no product failure was
  recorded.

## Round 2 — fixed build (2026-11-14)

Tester: t02. Instance: http://127.0.0.1:8080, container `githearth-app-1` rebuilt and
restarted from commit `7e1ded3` ("Fix M4 lab defects: Git 2.40 image, signed commits,
merge gate, auto-merge, SSH credentials") in
`…/opencode/wt-m4fix`
(compose working dir confirmed from the container labels; image built 2026-11-14 03:45
IST, container started 22:16Z).

Verdict: **PASS WITH DEFECTS (unchanged)** — 1 of 1 previously reported defect still
reproduces. 45 cases re-run: 37 pass, 8 fail; every failure is the same 404-body oracle.

### Build identity (not a domain test, but it rules out a stale container)

| Check | Expected on the fixed build | Actual | Verdict |
| --- | --- | --- | --- |
| `git --version` in `githearth-app-1` | ≥ 2.40 (fix raises the supported range) | git version 2.47.3 | PASS |
| repository description limit counts characters | 1000 × `é` (2000 bytes) accepted, 1001 rejected | 201 / 400 | PASS |
| `repository_api.rs` diff in `7e1ded3` | only the char-count change | 2 hunks, `desc.len()` → `desc.chars().count()`; `not_found()`/`org_not_found()`/`"repository not found"` untouched | — |

The third row is the cause: the fix does not touch the 404 messages, so the defect could
not have been fixed by this build.

### G09 re-run — repository 404 body oracle

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R-A01 | `GET beta/app` vs `beta/no-such-repo` vs `no-such-org/app` (attempt 1) | one indistinguishable 404 body | 404 ×3: `{"error":"not found"}` / `{"error":"repository not found"}` / `{"error":"organization not found"}` | FAIL |
| R-A02 | same three, attempt 2 (byte compare) | identical bodies | identical outcome to R-A01 | FAIL |
| R-A03 | attempt 3: `beta/app` vs `beta/<random name>` | identical bodies | `not found` vs `repository not found` | FAIL |
| R-A04 | org-existence probe: `beta/<random>` vs `no-such-org/<random>` | identical bodies | `repository not found` vs `organization not found` — a foreign org's existence is confirmed | FAIL |
| R-A05 | `PATCH` on the same three | identical bodies | 404 ×3, three distinct bodies | FAIL |
| R-A06 | `DELETE` on the same three | identical bodies | 404 ×3, three distinct bodies | FAIL |
| R-A07 | `POST .../archive` on the same three | identical bodies | 404 ×3, three distinct bodies | FAIL |
| R-A08 | `GET .../access` on the same three | identical bodies | 404 ×3, three distinct bodies | FAIL |
| R-A09 | anonymous `beta/app` vs `beta/nope` | identical 401, no leak | 401 `{"error":"invalid or expired credentials"}` both | PASS |
| R-A10 | organization read by UUID: `beta` vs a random UUID | identical 404 | 404 `{"error":"organization not found"}` both | PASS |
| R-A11 | own-org missing repo (`lab02/nope`, `acme/nope`) | 404, no leak to a member | 404 `{"error":"repository not found"}` | PASS (observation) |

Unchanged from round 1, byte for byte. The oracle also reads the other way round:
`R-A04` shows an outsider can test whether an organization slug exists at all by probing
one random repository name, because the org-existence answer (`organization not found`)
is distinguishable from the repository answer (`repository not found`). Read, update,
delete, archive and access all expose it; the organization-level routes do not (`R-A10`).

### C. Role ladder in acme — re-run

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R-C01 | bob (write) creates a repository | 201 | 201 | PASS |
| R-C02 | carol (maintain) creates a repository | 201 | 201 | PASS |
| R-C03 | dave (admin) creates a repository | 201 | 201 | PASS |
| R-C04 | grace (member/base) creates a repository | 201 | 201 | PASS |
| R-C05 | alice (owner) creates a repository | 201 | 201 | PASS |
| R-C06 | erin (read) creates a repository | 403 | 403 `{"error":"not permitted"}` | PASS |
| R-C07 | frank (triage) creates a repository | 403 | 403 `{"error":"not permitted"}` | PASS |
| R-C08 | t03 (acme read grant) creates a repository | 403 | 403 `{"error":"not permitted"}` | PASS |
| R-C09 | admin (platform admin, non-member) creates a repository | 404 | 404 `{"error":"organization not found"}` | PASS |

All five created repositories were deleted by alice (owner); `acme` holds only the
fixture repositories afterwards.

### D. Role ladder in lab02 (t02 owner) — re-run

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R-D01 | bob creates a repository | 201 | 201 | PASS |
| R-D02 | carol creates a repository | 201 | 201 | PASS |
| R-D03 | dave creates a repository | 201 | 201 | PASS |
| R-D04 | grace creates a repository | 201 | 201 | PASS |
| R-D05 | t02 (owner) creates a repository | 201 | 201 | PASS |
| R-D06 | erin creates a repository | 403 | 403 `{"error":"not permitted"}` | PASS |
| R-D07 | frank creates a repository | 403 | 403 `{"error":"not permitted"}` | PASS |
| R-D08 | t03 (non-member) creates a repository | 404 | 404 `{"error":"organization not found"}` | PASS |
| R-D09 | admin (platform admin, non-member) creates a repository | 404 | 404 `{"error":"organization not found"}` | PASS |

All created repositories were deleted by t02; `lab02` is empty afterwards.

### Smoke re-run

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R-S01 | create org, valid slug/name | 201, role=owner | 201 id=b310497f... slug=t02-r2-aa96ed role=owner | PASS |
| R-S02 | list includes the new org, excludes beta/lab03 | 200, member-filtered | 200, n=23, new org present, beta=False lab03=False | PASS |
| R-S03 | read the new org | 200, role=owner | 200 role=owner | PASS |
| R-S04 | list/read are `Cache-Control: no-store` | no-store | no-store / no-store | PASS |
| R-S05 | uppercase slug | 400 | 400 `{"error":"invalid input"}` | PASS |
| R-S06 | slug 65 chars | 400 | 400 `{"error":"invalid input"}` | PASS |
| R-S07 | slug 64 chars (boundary) | 201 | 201 slug=t02-l64-aa96ed-bbbb... | PASS |
| R-S08 | duplicate slug | 409 | 409 `{"error":"already exists"}` | PASS |
| R-S09 | unauthenticated create | 401 | 401 `{"error":"invalid or expired credentials"}` | PASS |
| R-S10 | tenant isolation: create a repository in `beta` | 404 | 404 `{"error":"organization not found"}` | PASS |
| R-S11 | tenant isolation: `beta`/`lab03` absent from the list | absent | absent | PASS |
| R-S12 | read `acme` (member), `lab02` (owner), `beta` (non-member) by UUID | 200 member / 200 owner / 404 | 200 member / 200 owner / 404 `{"error":"organization not found"}` | PASS |

### Unchanged surfaces spot-checked

| # | Case | Actual | Verdict |
| --- | --- | --- | --- |
| R-N01 | membership management route absent | `GET /organizations/{id}/members` → 404 | PASS |
| R-N02 | organization delete route absent | `DELETE /organizations/{id}` → 405 | PASS |
| R-N03 | `acme/web` still readable with the acme read grant | 200 name=web | PASS |
| R-N04 | non-member org UUID is indistinguishable from a random UUID | 404/404 equal=True | PASS |

### Remaining defect status

The single defect from round 1, **"Repository 404 bodies leak whether a foreign
organization and private repository exist"**, is **still present** on the fixed build.
`beta/app` returns `{"error":"not found"}`, a non-existent repository in the same org
returns `{"error":"repository not found"}`, and a non-existent organization returns
`{"error":"organization not found"}` — the same three bodies as before, reproduced three
times, and again on the update, delete, archive and access routes. The fixed build's
`repository_api.rs` change was the description char-count fix (`:210`, `:499`); the three
messages at `:18-22`, `:107` and the denial at `:196` are untouched, and
`docs/decisions/0002-authorization.md:66` still states that a denial and a missing object
must return the same 404. Severity stays **minor**: the status is uniformly 404, and no
SQL text, internal id or repository content leaks — only the existence of an organization
slug and of private repository names. Everything else in this domain passed the re-run.

Test artifacts from this round: two organizations, `t02-r2-aa96ed` and
`t02-l64-aa96ed-bbbb...`, remain because no org-delete route exists. All repositories
created for the ladders and the description-length check were deleted; `lab02` is empty
and `acme` holds only `web` and `site`.
