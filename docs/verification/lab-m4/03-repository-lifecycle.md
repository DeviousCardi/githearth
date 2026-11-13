# Repository lifecycle, visibility and access grants — M4 lab acceptance report

Tester: t03. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 91 run, 89 pass, 1 fail, 1 skipped.

All scored-suite repositories were created in `lab03` with the `t03x-` prefix;
exploratory checks also used `t03-` names there. `acme` and `beta` were read for
cross-organization checks only; no acme/beta settings, repositories or grants
were changed. Suite: `t03_suite.py` in this directory.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C01 | Create minimal repository, defaults | 201, private, empty description, active, not archived | 201 visibility=private desc='' status=active archived=False | PASS |
| C02 | Create public repository with description | 201, values reflected | 201 public 'hello world' | PASS |
| C03 | Create internal repository | 201, visibility internal | 201 internal | PASS |
| C04 | Invalid visibility values | 400 for secret/empty/Public/number | 'secret'->400, ''->400, 'Public'->400, 1->400 | PASS |
| C05 | visibility null | 201, default private | 201 private | PASS |
| C06 | Create object_format sha1 | 201 | 201 | PASS |
| C07 | Create SHA-256 repository and push/clone | 201; push and clone report sha256 | create=201 push_rc=0 clone_rc=0 clone_format=sha256 | PASS |
| C08 | Invalid object_format values | 400 for sha512/SHA256/sha-1 | sha512->400, SHA256->400, sha-1->400 | PASS |
| C09 | Missing and empty name | 400 both | missing->400, empty->400 | PASS |
| C10 | Name length 100 | 201 | 201 len=100 | PASS |
| C11 | Name length 101 | 400 | 400 | PASS |
| C12 | Name with space/slash/unicode | 400 all | 't03x has space'->400, 't03x/a'->400, 't03x-üñî'->400 | PASS |
| C13 | Uppercase name, case-insensitive lookup | 201 then 200 same id | create=201 read_lower=200 | PASS |
| C14 | Name with dot, underscore, hyphen | 201 | 201 | PASS |
| C15 | Reserved names . and .. | 400 both | .->400, ..->400 | PASS |
| C16 | Leading-dash name | 201, readable | create=201 read=200 | PASS |
| C17 | Reserved .git suffix | lowercase .git 400; uppercase .GIT accepted | lower=.git->400, upper=.GIT->201 | PASS |
| C18 | Duplicate names | 409 exact and case-insensitive | first=201 exact=409 case-variant=409 | PASS |
| C19 | Description length boundary | 1000 chars 201; 1001 chars 400 | 1000->201, 1001->400 | PASS |
| C20 | Description 501 non-ASCII characters (1002 bytes) | 201: schema limit is 1000 characters | 400 | FAIL |
| C21 | Create in unknown organization | 404 | 404 | PASS |
| C22 | Create denied for non-member/read/triage | 404/403/403 | alice(nonmember)=404, erin(read)=403, frank(triage)=403 | PASS |
| C23 | Create allowed for write/maintain/admin/member | 201 for bob, carol, dave, grace | {"bob": 201, "carol": 201, "dave": 201, "grace": 201} | PASS |
| C24 | Create invalid payload | 400 unknown field and malformed JSON | unknown-field=400, malformed-json=400 | PASS |
| C25 | Create idempotency replay | 201 both, same repository id, one row | 201/201 same_id=True | PASS |
| C26 | Idempotency key reused with different body | 422 | first=201, different-body=422 idempotency key reused with different request | PASS |
| C27 | Invalid idempotency keys | 400 for space and >128 characters | space=400, 129-chars=400 | PASS |
| R01 | Read fields | 200 with documented repository fields | 200 keys=['archived', 'description', 'id', 'name', 'organization_id', 'parent_repository_id', 'status', 'visibility'] | PASS |
| R02 | Read case-insensitively; unknown 404 | 200 / 404 | case-variant=200, unknown=404 | PASS |
| R03 | Read private repo by role | read/triage/write 200; member without grant 404; non-member 404 | {"erin": 200, "frank": 200, "bob": 200, "grace": 404, "alice": 404} | PASS |
| R04 | Read internal repo | org member 200; non-member 404 | grace(member)=200, alice(nonmember)=404 | PASS |
| R05 | Public repo read by non-member | public 200; private/internal 404 | public=200, private=404, internal=404 | PASS |
| R06 | Anonymous JSON API | 401 for any repository read (session-required API) | owner-public=200, owner-private=200, anonymous=401 | PASS |
| R07 | Anonymous Git read matrix | public 200; private/internal/nonexistent 401 challenge (no existence leak) | {"t03x-public": 200, "t03x-internal": 401, "t03x-private": 401, "t03x-none": 401} | PASS |
| R08 | Authenticated Git denial | non-member/member-without-grant 404; reader 200 | {"alice-private": 404, "alice-none": 404, "grace-private": 404, "erin-private": 200} | PASS |
| R09 | List fields and UUID ordering | 200, ascending id, full field set | 200 count=55 ordered=True | PASS |
| R10 | List pagination | limit=2 pages concatenate to the full list without duplicates or omissions | all=55 paged=55 nodup=True | PASS |
| R11 | List bounds | 400 for limit 0/101 and malformed after | limit0=400, limit101=400, bad-after=400 | PASS |
| R12 | List filters by read access | member sees public/internal/granted, not ungranted private | 200 count=13 private_visible=False | PASS |
| R13 | List by non-member | 404 | 404 | PASS |
| R14 | Archived repository read and list flag | read 200 archived=true; list includes it with archived=true | read=200 archived=True list_has=True | PASS |
| U01 | PATCH description | 200 reflected | 200 desc='new description' | PASS |
| U02 | PATCH name and redirect | 200; new name reads; old name redirects reads; old-name mutation 404 | rename=200 new=200 old-read=200 old-patch=404 | PASS |
| U03 | Case-only rename | 200, casing changed | 200 T03X-Case | PASS |
| U04 | Rename to existing/invalid name | 409 / 400 | existing=409, invalid=400 | PASS |
| U05 | PATCH description length | 400 for 1001 ASCII and 501 non-ASCII | ascii1001=400, utf8-501-chars=400 | PASS |
| U06 | PATCH visibility | not supported: 400 (visibility is create-only) | SKIP: no documented visibility update; UpdateRepository accepts only name/description | SKIP |
| U07 | PATCH unknown field and empty body | 400 / 200 no-op | unknown-field=400, empty-body=200 | PASS |
| U08 | PATCH actor permissions | maintain/admin 200; write/read/triage 403; member/non-member 404 | {"carol": 200, "dave": 200, "bob": 403, "erin": 403, "frank": 403, "grace": 404, "alice": 404} | PASS |
| U09 | PATCH archived repository | 403 archived-read-only | 403 | PASS |
| U10 | PATCH idempotency replay | 200 both, identical stored response | 200/200 equal=True | PASS |
| U11 | Old-name redirect is cut when the name is reused | redirect until reuse, then resolves to the new repository | before-reuse redirect=200, recreate=201, after-reuse id_is_new=True | PASS |
| A01 | Archive and archive again | 200 archived=true both (idempotent) | 200/200 archived=True/True | PASS |
| A02 | Archive permissions | maintain/write 403, non-member 404, admin/owner 200 | {"carol": 403, "bob": 403, "alice": 404, "dave": 200, "t03": 200} | PASS |
| A03 | Pushes refused while archived | branch create and branch delete rejected | push_rc=1 (remote: permission denied: ref.push        ); delete_rc=1 (remote: permission denied: ref.delete        ) | PASS |
| A04 | Archived repository remains readable | clone succeeds; list shows archived=true | clone_rc=0 list_archived=True | PASS |
| A05 | Unarchive restores writes | 200 archived=false; push succeeds; second unarchive 200 | unarchive=200/200 archived=False push_rc=0 | PASS |
| A06 | Archive unknown/deleted repository | 404 both | unknown=404, deleted-name=404 | PASS |
| A07 | Archived public repository stays publicly readable | anonymous Git 200; non-member JSON 200 | anon_git=200, alice_json=200 | PASS |
| A08 | Archive idempotency replay | 200 both, identical body | 200/200 same=True | PASS |
| D01 | Delete denied for write/maintain/non-member | 403/403/404 | {"bob": 403, "carol": 403, "alice": 404} | PASS |
| D02 | Delete by admin | 204 then 404 | delete=204, read-after=404 | PASS |
| D03 | Delete by owner, replay, second delete | 204; read/list/git 404; key replay 204; new delete 404 | delete=204 read=404 replay=204 second-delete=404 git=404 list=0 | PASS |
| D04 | Name reusable after delete; storage clean | 201 new id; push and clone succeed | delete_recreate=204/201 new_id=True push_rc=0 clone_rc=0 | PASS |
| D05 | Delete-key replay after name reuse | 204 replay; the recreated repository survives | create=201 delete=204 recreate=201 replay-delete=204 new-still-there=200 same_id=True | PASS |
| D06 | Grants do not survive delete/recreate | before 200, after 404, new access list empty | before=200 after=404 grants=[] | PASS |
| D07 | Delete unknown repository/organization | 404 both | unknown-repo=404, unknown-org=404 | PASS |
| D08 | Delete archived repository | 204 then 404 | delete=204, read-after=404 | PASS |
| G01 | Grant read and list access | 201 fields; grantee reads; list shows the grant | grant=201 fields=['created_at', 'expires_at', 'granted_by', 'id', 'role', 'subject_id', 'subject_kind'] read=200 listed=1 | PASS |
| G02 | Re-grant replaces the previous role | one active write grant; push succeeds | regrant=201 active=1 role=write push_rc=0 | PASS |
| G03 | Read-only repository grant enforced on Git push | read grant: push rejected, read 200 | regrant=201 push_rc=1 read=200 | PASS |
| G04 | Revoke grant | 204; access denied; revoke again 404; list empty | revoke=204 read=404 revoke-again=404 nonexistent=404 list=[] | PASS |
| G05 | Invalid grant role and subject kind | 400 all | {"owner": 400, "member": 400, "banana": 400, "empty": 400, "kind-user": 400} | PASS |
| G06 | Grant to a non-member account | 400 invalid grant | 400 | PASS |
| G07 | Grant expiry in the past or malformed | 400 all | {"past": 400, "bad": 400, "epoch": 400} | PASS |
| G08 | Grant expiry in the future | 201; access works then expires to 404; list hides it | grant=201 expires=2026-11-13T21:26:04Z before=200 after=404 list_before=1 list_after=0 | PASS |
| G09 | Grant actor permissions | admin 201; maintain/write/read 403; member 404 | rerun (isolated repo): {"dave": 201, "carol": 403, "bob": 403, "erin": 403, "grace": 404} | PASS |
| G10 | Grant and revoke on an archived repository | admin action allowed while archived; grantee can read | grant=201 read=200 revoke=204 | PASS |
| G11 | Invalid team and unknown grant field | 400 both | invalid-team=400, unknown-field=400 | PASS |
| G12 | Self-grant | 201, listed (no special case) | 201 listed=1 | PASS |
| G13 | Grant idempotency | same id on replay; 422 on different body | 201/201 same=True diff=422 | PASS |
| G14 | Failed re-grant does not revoke the existing grant | 400; previous write grant remains active | first=201 failed-regrant=400 active=write | PASS |
| G15 | List access actor permissions | admin 200; others 403/404 | rerun (isolated repo): {"dave": 200, "carol": 403, "bob": 403, "erin": 403, "grace": 403, "alice": 404} | PASS |
| G16 | List access pagination and bounds | 5 grants, paginated in order; 400 for limit 0/101 | grants=5 paged=5 limit0=400 limit101=400 | PASS |
| G17 | Grant id is scoped to its repository | revoke via another repository 404; original grant unaffected | wrong-repo-revoke=404, original-still-read=200 | PASS |
| G18 | Repository grant elevates above the org role | erin (org read) with repo write can push; after revoke cannot | grant=201 push=0 revoke=204 push-after=1 | PASS |
| G19 | Repository grant does not downgrade an org write grant | read grant to bob; push still succeeds (additive model, ADR 0002) | grant=201 push_rc=0 | PASS |
| G20 | Grants do not leak across repositories | granted 200; other private 404 | granted=200, other-private=404 | PASS |
| G21 | Cross-org same name and grant isolation | same name in two orgs allowed and distinct; grant does not leak to acme | lab03_web=200 acme_web=200 distinct=True grace_acme=404 | PASS |
| G22 | Grant on a deleted repository name | 404 | 404 | PASS |
| G23 | Repository grant maintain/admin ladder | bob(write) PATCH 403; maintain grant PATCH 200 / archive 403; admin grant archive 200 / delete 204 | {"bob_patch_before": 403, "maintain_patch": 200, "maintain_archive": 403, "admin_archive": 200, "admin_delete": 204} | PASS |

## Defects

### 1. Repository description limit is enforced in bytes, not characters

- Severity: minor
- Repro:

```python
import json, urllib.request, urllib.error
base = 'http://127.0.0.1:8080'
def call(method, path, payload=None, token=None):
    headers = {'Content-Type': 'application/json'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    data = None if payload is None else json.dumps(payload, ensure_ascii=False).encode()
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req) as r:
            b = r.read()
            return r.status, (json.loads(b) if b else None)
    except urllib.error.HTTPError as e:
        b = e.read()
        return e.code, (json.loads(b) if b else None)

tok = call('POST', '/api/v1/login',
           {'username': 't03', 'password': '<test-password>'})[1]['token']
print(call('POST', '/api/v1/organizations/lab03/repositories',
           {'name': 't03-defect-desc-500', 'description': 'é' * 500}, tok)[0])   # 201
print(call('POST', '/api/v1/organizations/lab03/repositories',
           {'name': 't03-defect-desc-501', 'description': 'é' * 501}, tok)[0])   # 400
print(call('PATCH', '/api/v1/organizations/lab03/repositories/t03-defect-desc-500',
           {'description': 'é' * 501}, tok)[0])                                  # 400
```

- Expected: `201` for the 501-character description. The persisted schema admits
  up to 1000 characters (`length(description) <= 1000`), and a 500-character
  description is already accepted; PATCH should likewise accept 501 characters.
- Actual: `400 {"error":"invalid input"}` for 501 characters (1002 UTF-8 bytes).
  A 1000-character ASCII description is accepted and 1001 is rejected; 500 `é`
  (1000 bytes) is accepted and 501 `é` (1002 bytes) is rejected. The effective
  limit is 1000 UTF-8 bytes, so non-ASCII descriptions are capped at 500
  characters.
- Evidence: `crates/githearth-server/src/repository_api.rs:210` (create) and
  `:498-500` (update) use `str::len()`, which is UTF-8 byte length.
  `crates/githearth-server/migrations/0003_repositories.sql:24` enforces
  `length(description) <= 1000`, which counts characters. The fork path has the
  same byte check (`crates/githearth-server/src/forks.rs:90`). Re-run with fresh
  names reproduced `201` for 500 `é`, `400` for 501 `é`, and `400` for a PATCH
  to 501 `é`.
- Scope: systematic — create and PATCH (and forks). Low user impact, but the
  API and its own schema disagree on the unit of the boundary.

## Notes / skipped

- **U06 skipped — visibility is create-only.** `UpdateRepository`
  (`repository_api.rs:462-469`) accepts only `name` and `description`;
  `PATCH {"visibility":"public"}` returns `400 invalid input`. ADR 0003's
  lifecycle section lists create, rename/transfer, archive/unarchive and delete
  but no visibility transition, so this is recorded as an unsupported capability
  rather than a scored defect. If visibility changes are in M4 scope, this is a
  functional gap: there is no API or UI path to make an existing repository more
  or less visible.
- **Object format is not exposed.** `object_format` is accepted on create, but
  `RepositoryJson` (`repository_api.rs:53-63`) has no such field, so create,
  read and list responses never report it. C07 verified SHA-256 end-to-end with
  `git rev-parse --show-object-format` on a clone (`sha256`), so the stored
  format is honoured; clients simply cannot read it from the API.
- **Observed lab03 role grants.** The fixture behaves as the shared brief's
  account table: carol=maintain, dave=admin (the task line said carol=write,
  dave=maintain). Evidence: carol PATCH 200 / archive 403; dave PATCH 200 /
  archive 200 / `GET access` 200 / delete 204. bob=write, erin=read,
  frank=triage, grace=member.
- **Member asymmetry is real and visible.** grace (member) can create a private
  repository (C23) but then gets `404` reading it and `repository not found`
  pushing to it, because the `member` role template grants only the `member`
  group and not `read` (`crates/githearth-authz/policies/roles/member.cedar`).
  This matches the brief's pinned "member is not below read" asymmetry; noted
  for product visibility, not counted as a defect.
- **Anonymous access.** The JSON API requires a session, so anonymous public and
  private reads both return `401` (R06); public reads work anonymously over Git
  and the HTML browse page. Anonymous Git returns `200` for public and `401` +
  `WWW-Authenticate` for private/internal/nonexistent alike (R07); an
  authenticated non-member gets `404` for both private and nonexistent (R08), so
  there is no existence leak. If the brief's "404 not 403" was meant literally for
  anonymous Git, the status is 401 rather than 404, but the property it protects
  (private and nonexistent are indistinguishable) holds.
- **Grants are additive, not overriding.** A repository grant elevates above the
  organization role (G18: erin org-read + repo-write can push) but does not
  downgrade it (G19: bob org-write + repo-read still pushes). This matches ADR
  0002's permit-only model. If the product intends a repository grant to
  override/downgrade an organization grant, that is not implemented. Granting or
  revoking access on an archived repository is allowed (G10): `manage_access` is
  an admin action outside the `archived-read-only` forbid.
- **Minor observations (no verdict impact).** `{"visibility":null}` is accepted
  as `private` (C05); `PATCH {}` returns `200` as a no-op (U07); a malformed
  `after` cursor returns a plain-text 400 rather than JSON (R11); the repository
  list is a bare array with `limit`/`after` and no `next` cursor; `PATCH` of a
  501-character non-ASCII description is also rejected (U05), consistent with the
  byte cap.
- **First-run failures G09/G15 were test mistakes, not product defects.** G09
  granted grace read on the same repository before checking her grant permission
  (so her subsequent 403 was correct), and G15 likewise. Re-running both with an
  isolated repository produced the tabulated PASS results. C20 was re-run with
  fresh names and failed identically, so it is recorded as the defect above.
- **Not tested / skipped by scope:** team grants with a real team (no team
  management API in M4), suspended accounts/organizations, concurrent
  same-name create/rename races, crash mid-create/mid-delete reconciler paths,
  SSH transport (HTTP Git used for all push/clone checks), and quota/disk-full.

## Round 2 — fixed build (2026-11-14)

Tester: t03. Instance: http://127.0.0.1:8080; `githearth-app-1` was rebuilt at
03:45 IST from the fix branch (`fix/m4-lab`, 7e1ded3) instead of `main` b820454.
Updated verdict: **PASS** (round 1: PASS WITH DEFECTS).
Round 1: 91 cases, 89 pass, 1 fail (C20), 1 skipped. Round 2: **43 cases, 43
pass, 0 fail, 0 skip.** The single round-1 defect is fixed and no regression was
found on the re-run surface.

Every round-2 repository used the `t03r2-` prefix in `lab03`; all 21 were deleted
afterwards and `lab03` is back to its round-1 set of 100 repositories (verified by
paging the list). Suite: `t03_round2.py`; results: `t03-round2-results.json`.

### Fix verification

**C20 — description limit now counts characters: FIXED.** The exact round-1
repro, unchanged, now gives:

```text
create 500 e-acute  : 201
create 501 e-acute  : 201   (round 1: 400)
create 1000 e-acute : 201
create 1001 e-acute : 400
PATCH  501 e-acute  : 200   (round 1: 400)
```

Boundaries verified on create, PATCH and fork (stored value read back and
compared character-for-character):

| Unit | 1000 chars | 1001 chars |
| --- | --- | --- |
| `é` (2 bytes, create) | 201, 2000-byte value stored intact | 400 |
| `漢` (3 bytes, create) | 201, 3000-byte value stored intact | 400 |
| `👍` (4 bytes, create) | 201, 4000-byte value stored intact | 400 |
| `é` (PATCH) | 200, value updated | 400 |
| `é` (fork description) | 201, value stored intact | 400 |
| ASCII `a` (create + PATCH) | 201 / 200 | 400 (unchanged) |

Code: `crates/githearth-server/src/repository_api.rs:210` (create) and `:499`
(update) now use `chars().count() > 1000`; the fork path at
`crates/githearth-server/src/forks.rs:93` does the same. This matches
`migrations/0003_repositories.sql:24` (`length(description) <= 1000`).

**403 namespace authorization: no regression.**

- Create in `lab03`: `erin` (read) 403, `frank` (triage) 403, `alice`
  (non-member) 404, unknown org 404; owner/write/maintain/admin/member all 201
  (t03, bob, carol, dave, grace).
- Fork into `lab03` (public parent): `erin` (read) **403**, `frank` (triage)
  **403**, `alice` (non-member) 404; `bob` (write) 201, `grace` (member) 201;
  unknown target org 404; fork of a private parent the actor cannot read 404.
  This is the issue #133 change (`forks.rs:127-128` returns 403 only when the
  actor is an organization member) working as intended: members who lack create
  get 403, strangers still get 404, so namespace existence is not leaked.

**Grant / revoke: still works.** Grant elevates an organization-read user to
write (push denied before 201 grant, allowed after, denied again after 204
revoke); expiry hides the grant and denies access at the boundary; invalid role,
subject kind and unknown subject all 400; re-grant replaces the role with one
active grant.

### Re-run table

| # | Case | Round-1 ref | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- | --- |
| R2-D01 | Create, 501 non-ASCII chars (1002 bytes) | C20 re-run | 201, stored intact | 201 stored_len=501 | PASS |
| R2-D02 | Create, 1000 non-ASCII chars | new boundary | 201 | 201 stored_len=1000 | PASS |
| R2-D03 | Create, 1001 non-ASCII chars | new boundary | 400 | 400 invalid input | PASS |
| R2-D04 | ASCII boundary | C19 | 1000→201, 1001→400 | 1000→201, 1001→400 | PASS |
| R2-D05 | 3-byte CJK boundary | C19 variant | 1000→201, 1001→400 | 1000→201 stored 3000 B, 1001→400 | PASS |
| R2-D06 | 1000 emoji (4000 bytes) | new | 201 | 201 stored_len=1000 | PASS |
| R2-D07 | PATCH 501 non-ASCII chars | U05 re-run | 200 reflected | 200 stored_len=501 | PASS |
| R2-D08 | PATCH non-ASCII boundary | U05 boundary | 1000→200, 1001→400 | 1000→200, 1001→400 | PASS |
| R2-D09 | PATCH ASCII 1001 | U05 | 400 | 400 | PASS |
| R2-D10 | Fork 501 non-ASCII chars | new (fork path) | 201 reflected | 201 stored_len=501 | PASS |
| R2-D11 | Fork description boundary | new (fork path) | 1000→201, 1001→400 | 1000→201, 1001→400 | PASS |
| R2-D12 | Empty description | C01 | 201 create, 200 patch | 201 / 200 | PASS |
| R2-S01 | Create defaults | C01 | 201 private, empty desc, active, not archived | 201 private desc='' status=active archived=False | PASS |
| R2-S02 | Create public with description | C02 | 201 reflected | 201 public 'hello world' | PASS |
| R2-S03 | Read fields + case-insensitive lookup | R01/R02 | 200 full field set, same id | 200 keys match, case variant same id | PASS |
| R2-S04 | Read unknown | R02 | 404 | 404 | PASS |
| R2-S05 | List includes new repos in UUID order | R09/R10 | full list via paging, ascending, no dupes | 200 n=110 ordered=True | PASS |
| R2-S06 | PATCH description | U01 | 200 reflected | 200 | PASS |
| R2-S07 | Rename + old-name redirect | U02 | old name reads 200, mutates 404 | rename=200 old-read=200 old-patch=404 | PASS |
| R2-S08 | PATCH permissions | U08 | maintain 200, write 403, non-member 404 | 200 / 403 / 404 | PASS |
| R2-S09 | Archive blocks pushes, unarchive restores | A01/A03/A05 | push refused while archived, ok after | archive=200 push_blocked=1 unarchive=200 push=0 | PASS |
| R2-S10 | Delete by owner | D03 | 204, read 404, second 404 | 204 / 404 / 404 | PASS |
| R2-S11 | Grant read | G01 | 201, grantee reads, listed, push refused | grant=201 read=200 listed=1 push_rc=1 | PASS |
| R2-S12 | Revoke grant | G04 | 204, read 404, again 404, list empty | 204 / 404 / 404 list=[] | PASS |
| R2-S13 | Create idempotency replay | C25 | 201 twice, same id | 201/201 same_id=True | PASS |
| R2-N01 | Create by org read | C22 regression | 403 | 403 not permitted | PASS |
| R2-N02 | Create by org triage | C22 regression | 403 | 403 not permitted | PASS |
| R2-N03 | Create by non-member | C22 regression | 404 | 404 | PASS |
| R2-N04 | Create in unknown org | C21 | 404 | 404 | PASS |
| R2-N05 | Create by allowed roles | C23 | 201 for five roles | t03/bob/carol/dave/grace all 201 | PASS |
| R2-N06 | Fork into namespace as read role | new (403 fix) | 403 | 403 not permitted | PASS |
| R2-N07 | Fork into namespace as triage role | new (403 fix) | 403 | 403 not permitted | PASS |
| R2-N08 | Fork into namespace as non-member | new | 404 | 404 | PASS |
| R2-N09 | Fork into namespace as write role | new | 201 | 201 t03r2-fork-bob | PASS |
| R2-N10 | Fork into namespace as member role | new | 201 | 201 t03r2-fork-grace | PASS |
| R2-N11 | Fork into unknown organization | new | 404 | 404 | PASS |
| R2-N12 | Fork of unreadable private parent | new | 404 | 404 | PASS |
| R2-G01 | Grant elevates org read, revoke restores | G18 re-run | denied → allowed → denied | push 1 → 0 after grant → 1 after revoke | PASS |
| R2-G02 | Grant expiry | G08 | 200 before, 404 after, hidden | 200 before, 404 after, list=0 | PASS |
| R2-G03 | Invalid grant payloads | G05/G06 | 400 all | role=400 kind=400 subject=400 | PASS |
| R2-G04 | Re-grant replaces role | G02 | one active write grant | 201/201 active=1 role=write | PASS |
| R2-X01 | Anonymous Git matrix | R07 spot check | public 200, private/nonexistent 401 | 200 / 401 / 401 | PASS |
| R2-X02 | Anonymous JSON read | R06 spot check | 401 | 401 | PASS |

### Remaining defects

None. C20 is fixed on create, PATCH and fork, and no regression appeared in the
re-run surface (lifecycle smoke, namespace authorization for create and fork,
grants and revocation, anonymous read behaviour).

### Round-2 notes

- **U06 remains skipped, unchanged.** Updating `visibility` is still not
  supported by `UpdateRepository`; this is a create-only property, as recorded in
  round 1, not a regression.
- **First attempt at this round had four test mistakes**, all corrected before
  the table above and none product defects: listing with `limit=200` is a
  documented 400 (max 100; switched to `limit=100` + `after` paging); the fork
  source repository was not created before the fork cases; the expiry test used
  `frank`, who has effective read via triage, so "after expiry" was still 200
  (rerun with `grace`, as in round 1); the anonymous-public check pointed at a
  private repository. Final run: 43/43 PASS.
- **Minor observations from round 1 were not re-tested** beyond list pagination
  (null visibility, `PATCH {}` no-op, plain-text malformed-cursor 400, bare-array
  list response); nothing in the fixed commit touches those paths.
- **Out of scope this round:** the anonymous browse-UI change and other domains'
  surfaces are covered by their own reports; no interaction with repository
  lifecycle was observed.
