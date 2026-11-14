# Code owners / issue #157 — M4 closure round 3

Tester: t10. Date: 2026-11-14. Instance: main 800cad4.
Verdict: **PASS WITH DEFECTS**
Cases: 146 run, 144 pass, 2 fail, 0 skipped. Case numbers run C01-C147;
C62 (the first draft-toggle observation) was withdrawn as a duplicate and
re-run as C141, so it has no row.

Scope: issue #157 (create/PATCH must report the live `required_code_owners`
and `code_owner_review_satisfied` for the head/base, matching GET detail and the
merge gate), plus CODEOWNERS resolution, author-as-sole-owner (#137), invalid
files, cross-repo forks, review requests, create/PATCH latency and git-busy
behaviour.

Fixtures (all in `lab10`, created through the API and a plain Git client):
`r3-co` with `main`, `relaxed`, `base2`, `base3` and 40+ scenario branches;
`r3-co2` (base with no CODEOWNERS); `r3-co3` (binary, oversized, malformed,
`docs/CODEOWNERS` and `.githearth/CODEOWNERS` variants); `r3-big` (80 MB blob
used to hold Git permits); fork `r3-fork`; a second fork `r3-fork2` deleted
mid-test. Protection rules were `require_pull_request: true,
required_approvals: 0, require_code_owner_review: true` unless named; the
`relaxed` rule exists to test the flag being off. No app restart, migration or
repository edit.

**Team-owner fixture:** this build exposes no team API, so `lab10/r3-team`
(name `Round 3 owners`, one member `bob`) was inserted with two rows in `teams`
and `team_members`. Nothing else in the database was written; the rest of the
observation used read-only SQL.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C01 | create PR, team owner not approved | 201 [@lab10/r3-team:false] sat=false err=[] | 201 @lab10/r3-team:false sat=false err=[] | PASS |
| C02 | GET detail matches create (team) | same owners/sat/err | 200 {"owners":[{"name":"@lab10/r3-team","approved":false}],"sat":false,"err":[]} | PASS |
| C03 | PATCH title matches create (team) | 200 same owners/sat/err | 200 {"owners":[{"name":"@lab10/r3-team","approved":false}],"sat":false,"err":[]} | PASS |
| C04 | PATCH body only matches create (team) | 200 same owners/sat/err | 200 {"owners":[{"name":"@lab10/r3-team","approved":false}],"sat":false,"err":[]} | PASS |
| C05 | merge gate blocks unapproved team owner | 403 | 403 {"error":"not permitted"} | PASS |
| C06 | non-member (carol) approval leaves team owner false | GET team false sat=false | 200 {"owners":[{"name":"@lab10/r3-team","approved":false}],"sat":false,"err":[]} | PASS |
| C07 | PATCH after non-member approval stays false | team false sat=false | 200 {"owners":[{"name":"@lab10/r3-team","approved":false}],"sat":false,"err":[]} | PASS |
| C08 | merge still blocked after non-member approval | 403 | 403 {"error":"not permitted"} | PASS |
| C09 | team member (bob) approval satisfies team owner | team true sat=true | 200 {"owners":[{"name":"@lab10/r3-team","approved":true}],"sat":true,"err":[]} | PASS |
| C10 | PATCH after team approval matches GET | 200 same | 200 {"owners":[{"name":"@lab10/r3-team","approved":true}],"sat":true,"err":[]} | PASS |
| C11 | merge gate accepts approved team owner | 201 | 201 queued | PASS |
| C12 | create PR with two owners on one file | @carol:false,team:false sat=false | 201 @carol:false,@lab10/r3-team:false sat=false | PASS |
| C13 | any owner approval satisfies the file (GET) | @carol:false,team:true sat=true (any-owner rule) | 200 {"owners":[{"name":"@carol","approved":false},{"name":"@lab10/r3-team","approved":true}],"sat":true,"err":[]} | PASS |
| C14 | PATCH agrees with GET (any-owner rule) | 200 same | 200 {"owners":[{"name":"@carol","approved":false},{"name":"@lab10/r3-team","approved":true}],"sat":true,"err":[]} | PASS |
| C15 | merge gate accepts any-owner satisfaction | 201 | 201 queued | PASS |
| C16 | create PR with unresolvable user owner | @nobody:false sat=false | 201 {"owners":[{"name":"@nobody","approved":false}],"sat":false,"err":[]} | PASS |
| C17 | GET matches create (unknown user) | same | 200 {"owners":[{"name":"@nobody","approved":false}],"sat":false,"err":[]} | PASS |
| C18 | PATCH matches create (unknown user) | same | 200 {"owners":[{"name":"@nobody","approved":false}],"sat":false,"err":[]} | PASS |
| C19 | unresolvable owner never satisfies the gate | 403 | 403 {"error":"not permitted"} | PASS |
| C20 | create PR with unknown team owner | @lab10/nope:false sat=false | 201 {"owners":[{"name":"@lab10/nope","approved":false}],"sat":false,"err":[]} | PASS |
| C21 | GET matches create (unknown team) | same | 200 {"owners":[{"name":"@lab10/nope","approved":false}],"sat":false,"err":[]} | PASS |
| C22 | PATCH matches create (unknown team) | same | 200 {"owners":[{"name":"@lab10/nope","approved":false}],"sat":false,"err":[]} | PASS |
| C23 | unknown team never satisfies the gate | 403 | 403 {"error":"not permitted"} | PASS |
| C24 | create PR changing two files with different owners | @carol:false,@bob:false sat=false | 201 {"owners":[{"name":"@carol","approved":false},{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C25 | GET matches create (two files) | same | 200 {"owners":[{"name":"@carol","approved":false},{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C26 | one file approved is not enough (GET) | @carol:true,@bob:false sat=false | 200 {"owners":[{"name":"@carol","approved":true},{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C27 | PATCH agrees after one of two files approved | 200 same as GET | 200 {"owners":[{"name":"@carol","approved":true},{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C28 | gate blocks while second file unapproved | 403 | 403 {"error":"not permitted"} | PASS |
| C29 | PATCH/GET agree once both files approved | both true sat=true | 200 {"owners":[{"name":"@carol","approved":true},{"name":"@bob","approved":true}],"sat":true,"err":[]} | PASS |
| C30 | gate accepts both-file satisfaction | 201 | 201 queued | PASS |
| C31 | last matching rule wins (special.txt -> @bob) | @bob:false sat=false | 201 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C32 | GET matches create (last rule) | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C33 | PATCH matches create (last rule) | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C34 | gate blocks unapproved last-rule owner | 403 | 403 {"error":"not permitted"} | PASS |
| C35 | create PR for new file under sub/ (dir pattern) | @bob:false sat=false | 201 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C36 | GET matches create (dir pattern) | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C37 | PATCH matches create (dir pattern) | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C38 | create PR changing an unowned file | [] sat=true | 201 {"owners":[],"sat":true,"err":[]} | PASS |
| C39 | GET matches create (unowned file) | same | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C40 | PATCH matches create (unowned file) | same | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C41 | gate allows PR with no owned files | 201 | 201 queued | PASS |
| C42 | delete an owned file requires its owner | @carol:false sat=false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C43 | GET matches create (deletion) | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C44 | PATCH matches create (deletion) | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C45 | gate blocks unapproved owner of a deleted file | 403 | 403 {"error":"not permitted"} | PASS |
| C46 | author-as-sole-owner satisfies create (#137) | @t10:true sat=true | 201 {"owners":[{"name":"@t10","approved":true}],"sat":true,"err":[]} | PASS |
| C47 | GET matches create (author sole owner) | same | 200 {"owners":[{"name":"@t10","approved":true}],"sat":true,"err":[]} | PASS |
| C48 | PATCH matches create (author sole owner) | same | 200 {"owners":[{"name":"@t10","approved":true}],"sat":true,"err":[]} | PASS |
| C49 | gate accepts author-as-sole-owner without a review (#137) | 201 | 201 queued | PASS |
| C50 | author among two owners satisfies file without carol | @t10:true,@carol:false sat=true | 201 {"owners":[{"name":"@t10","approved":true},{"name":"@carol","approved":false}],"sat":true,"err":[]} | PASS |
| C51 | GET matches create (author + other owner) | same | 200 {"owners":[{"name":"@t10","approved":true},{"name":"@carol","approved":false}],"sat":true,"err":[]} | PASS |
| C52 | PATCH matches create (author + other owner) | same | 200 {"owners":[{"name":"@t10","approved":true},{"name":"@carol","approved":false}],"sat":true,"err":[]} | PASS |
| C53 | gate accepts author among multiple owners (#137 breadth) | 201 | 201 queued | PASS |
| C54 | head CODEOWNERS cannot change owners (base wins) | @carol:false sat=false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C55 | GET matches create (head CODEOWNERS ignored) | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C56 | PATCH matches create (head CODEOWNERS ignored) | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C57 | base owner approval satisfies after head CODEOWNERS edit | sat=true | 200 {"owners":[{"name":"@carol","approved":true}],"sat":true,"err":[]} | PASS |
| C58 | gate accepts after base owner approval | 201 | 201 queued | PASS |
| C59 | draft create still reports computed owners | draft=true @carol:false sat=false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C60 | GET matches draft create | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C61 | draft merge blocked before code-owner gate | 409 | 409 {"error":"pull request is a draft"} | PASS |
| C63 | GET matches after draft ready | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C64 | unprotected target still reports owners (sat=false) | @carol:false sat=false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C65 | GET/PATCH match on unprotected target | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C66 | gate ignores code owners when protection flag is off | 201 despite sat=false | 201 queued | PASS |
| C67 | PR adding CODEOWNERS does not gain owners from its head | [] sat=true | 201 {"owners":[],"sat":true,"err":[]} | PASS |
| C68 | GET matches create (no base CODEOWNERS) | same | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C69 | PATCH matches create (no base CODEOWNERS) | same | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C70 | gate allows when base has no CODEOWNERS | 201 | 201 queued | PASS |
| C71 | binary CODEOWNERS fails closed on create | sat=false, errors non-empty | 201 {"owners":[],"sat":false,"err":["CODEOWNERS is not a text file"]} | PASS |
| C72 | GET matches create (binary CODEOWNERS) | same | 200 {"owners":[],"sat":false,"err":["CODEOWNERS is not a text file"]} | PASS |
| C73 | PATCH matches create (binary CODEOWNERS) | same | 200 {"owners":[],"sat":false,"err":["CODEOWNERS is not a text file"]} | PASS |
| C74 | gate blocked by unreadable CODEOWNERS | 403 | 403 {"error":"not permitted"} | PASS |
| C75 | oversized CODEOWNERS fails closed on create | sat=false + error | 201 {"owners":[],"sat":false,"err":["CODEOWNERS exceeds the size limit"]} | PASS |
| C76 | GET/PATCH match create (oversized) | same | 200 {"owners":[],"sat":false,"err":["CODEOWNERS exceeds the size limit"]} | PASS |
| C77 | gate blocked by oversized CODEOWNERS | 403 | 403 {"error":"not permitted"} | PASS |
| C78 | first CODEOWNERS path wins (.githearth over root) | @bob:false | 201 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C79 | GET matches create (path precedence) | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C80 | PATCH matches create (path precedence) | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C81 | precedence owner approval satisfies | sat=true | 200 {"owners":[{"name":"@bob","approved":true}],"sat":true,"err":[]} | PASS |
| C82 | gate accepts precedence owner approval | 201 | 201 queued | PASS |
| C83 | malformed CODEOWNERS errors surfaced on create | errors non-empty, owners [] | 201 {"owners":[],"sat":true,"err":["malformed owner token `carol`"]} | PASS |
| C84 | GET/PATCH match create (malformed) | same | 200 {"owners":[],"sat":true,"err":["malformed owner token `carol`"]} | PASS |
| C85 | malformed CODEOWNERS: gate passes (documented parser behaviour) | 201 (NOTE: errors do not fail closed) | 201 queued | PASS |
| C86 | docs/CODEOWNERS fallback found on create | @carol:false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C87 | GET/PATCH match create (docs fallback) | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C88 | rule with no owners requires nothing | [] sat=true | 201 {"owners":[],"sat":true,"err":[]} | PASS |
| C89 | GET/PATCH match create (no-owner rule) | same | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C90 | gate allows a no-owner rule | 201 | 201 queued | PASS |
| C91 | create before head move (@carol) | @carol:false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C92 | PATCH after head move reports new owner (@bob) | @bob:false sat=false | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C93 | GET after head move matches PATCH | same | 200 {"owners":[{"name":"@bob","approved":false}],"sat":false,"err":[]} | PASS |
| C94 | gate blocks new owner after head move | 403 | 403 {"error":"not permitted"} | PASS |
| C95 | gate accepts new owner approval at new head | 201 | 201 queued | PASS |
| C96 | PATCH a merged PR matches GET (empty diff after merge) | 200 state=merged [] sat=true, same as GET | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C97 | idempotent create replay returns stored code-owner fields | 201, number 23, @nobody:false | 201 {"owners":[{"name":"@nobody","approved":false}],"sat":false,"err":[]} | PASS |
| C98 | create requested review from file owner (@bob) | bob in requested reviewers | 200 [{"reviewer_id":"ebf6bceb-6238-49a4-ac12-a63834ff72ee","username":"bob","requested_by":"65d2fc20-1a8b-479d-a0c7-791cc4548f35","created_at":"2026-11-14T06:46:27Z"}] | PASS |
| C99 | team owner requests its member, not an outsider | bob yes, carol no | 200 ["bob"] | PASS |
| C100 | draft requests no reviewers | 0 requested | 200 [] | PASS |
| C101 | marking ready requests owner review | carol requested | 200 [{"reviewer_id":"efa50857-8c67-437c-b419-993acbc8b27b","username":"carol","requested_by":"65d2fc20-1a8b-479d-a0c7-791cc4 | PASS |
| C102 | add a manual reviewer | 200 | 200 ["carol","dave"] | PASS |
| C103 | list includes the manual reviewer | dave present | 200 ["carol","dave"] | PASS |
| C104 | remove the manual reviewer | 204 | 204 | PASS |
| C105 | review request for unknown user | 422 | 422 {"error":"reviewer not found"} | PASS |
| C106 | review request for the author | 422 | 422 {"error":"the author cannot review their own pull request"} | PASS |
| C107 | owner comment-only review accepted | 201 | 201 | PASS |
| C108 | comment-only review does not satisfy code owner | @carol:false sat=false | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C109 | PATCH matches after comment-only review | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C110 | owner change-request review accepted | 201 | 201 | PASS |
| C111 | change-request does not satisfy code owner | sat=false | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C112 | gate blocks while a change request is outstanding | 403 | 403 {"error":"not permitted"} | PASS |
| C113 | fork CODEOWNERS cannot inject owners (create) | target owners @carol:false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C114 | GET matches create for cross-repo PR | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C115 | PATCH matches create for cross-repo PR | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C116 | gate uses target owners for cross-repo PR | 403 | 403 {"error":"not permitted"} | PASS |
| C117 | target owner approval unlocks cross-repo PR | sat=true | 200 {"owners":[{"name":"@carol","approved":true}],"sat":true,"err":[]} | PASS |
| C118 | merge cross-repo PR after owner approval | 201 | 201 queued | PASS |
| C119 | fork deleting CODEOWNERS changes nothing (create) | @carol:false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C120 | GET/PATCH match create (fork delete CODEOWNERS) | same | 200 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C121 | cross-repo PR with no owned files | [] sat=true | 201 {"owners":[],"sat":true,"err":[]} | PASS |
| C122 | cross-repo source that is not a fork | 400 | 400 {"error":"source repository is not a fork of the target"} | PASS |
| C123 | cross-repo source unknown repo | 404 | 404 {"error":"not found"} | PASS |
| C124 | create latency with CODEOWNERS (6 creates) | all 201; report p50/p95 | n=6 codes=['201'] p50=104ms p95=121ms max=121ms | PASS |
| C125 | create latency without CODEOWNERS baseline (4 creates) | all 201; report p50/p95 | n=4 codes=['201'] p50=105ms p95=106ms max=106ms | PASS |
| C126 | PATCH latency with code-owner evaluation (15 patches) | all 200; report p50/p95 | n=15 codes=['200'] p50=109ms p95=116ms max=123ms | PASS |
| C127 | empty title still rejected | 400 | 400 {"error":"invalid input"} | PASS |
| C128 | unknown source branch still 422 | 422 | 422 {"error":"branch not found"} | PASS |
| C129 | unknown field still 400 | 400 | 400 {"error":"invalid input"} | PASS |
| C130 | unknown PR still 404 | 404 | 404 {"error":"not found"} | PASS |
| C131 | duplicate open PR still 409 | 409 | 409 {"error":"an open pull request already exists for these branches"} | PASS |
| C132 | self-approval still forbidden | 403 | 403 {"error":"not permitted"} | PASS |
| C133 | read grant cannot approve | 403 | 403 {"error":"not permitted"} | PASS |
| C134 | merge without expected_head_oid rejected | 400 | 400 {"error":"invalid input"} | PASS |
| C135 | create during git-busy returns 503 git requests busy | 503 {"error":"git requests busy"} | 503 {"error":"git requests busy"} | PASS |
| C136 | PATCH during git-busy returns 503 git requests busy | 503 {"error":"git requests busy"} | 503 {"error":"git requests busy"} | PASS |
| C137 | failed create left no open pull request | 503 and 0 open PRs for busy5 | 503 open=0 | PASS |
| C138 | failed PATCH left the PR title unchanged | title unchanged | busy patch retry | PASS |
| C139 | retry after busy succeeds with code-owner facts | 201 @carol:false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C140 | retry PATCH after busy succeeds, GET agrees | 200 team:false same as GET | 200 {"owners":[{"name":"@lab10/r3-team","approved":false}],"sat":false,"err":[]} | PASS |
| C141 | draft toggle response reports live code-owner facts | 200 same as GET (team:false) | 200 {"owners":[],"sat":true,"err":[]} | FAIL |
| C142 | PATCH after base moved tracks live owners, matches GET | 200 @nobody:false same as GET | 200 {"owners":[{"name":"@nobody","approved":false}],"sat":false,"err":[]} | PASS |
| C143 | create cross-repo PR before fork deletion | @carol:false | 201 {"owners":[{"name":"@carol","approved":false}],"sat":false,"err":[]} | PASS |
| C144 | deleted-fork PR GET: closed, null head, default owner fields | 200 closed head=null owners=[] sat=true | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C145 | PATCH on deleted-fork PR matches GET | 200 same fields | 200 {"owners":[],"sat":true,"err":[]} | PASS |
| C146 | merge is refused on deleted-fork PR | not 201 | 409 {"error":"pull request is not open"} | PASS |
| C147 | deleted-head detail must not claim code-owner satisfaction | sat=false (gate refuses merge) | GET sat=true, merge 409 | FAIL |

## Defects

### R3-01 (minor) — `POST .../pulls/{n}/draft` still returns the default code-owner fields

- **Severity: minor**
- **Repro** (PR #36, target `base3`, whose `CODEOWNERS` requires
  `@lab10/r3-team` for `team.txt`; the team member has not approved):
  ```bash
  curl -s -X POST -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
    -d '{"draft":true}' \
    http://127.0.0.1:8080/api/v1/organizations/lab10/repositories/r3-co/pulls/36/draft
  # {"draft":true,"required_code_owners":[],"code_owner_review_satisfied":true}
  curl -s -H "Authorization: Bearer $TOK" .../pulls/36
  # {"required_code_owners":[{"name":"@lab10/r3-team","approved":false}],
  #  "code_owner_review_satisfied":false}
  ```
- **Expected / Actual:** the write response either computes the live facts or
  reports an unknown/neutral value; it must not claim satisfaction when GET and
  the merge gate say otherwise. Actual: `[]`/`true` on every draft toggle.
- **Evidence:** `set_draft` builds the response with `detail(...)` and stores it
  directly, without `apply_codeowner_review`
  (`crates/githearth-server/src/pulls.rs:1399-1402`); `detail()` fills
  `required_code_owners: []`, `code_owner_review_satisfied: true`
  (`pulls.rs:571-572`). Create (`pulls.rs:812`), PATCH (`pulls.rs:1223`) and GET
  (`pulls.rs:1038-1049`) apply the real review; the draft route was missed.
  Re-ran twice, deterministic. This is the same false-green #157 described,
  in the remaining write path that returns a `PullDetail`.

### R3-02 (minor) — a headless (system-closed) pull reports `code_owner_review_satisfied: true`

- **Severity: minor**
- **Repro** (cross-repo PR #41; delete its fork `lab10/r3-fork2`, then read or
  PATCH it):
  ```bash
  curl -s -H "Authorization: Bearer $TOK" .../pulls/41
  # {"state":"closed","head_id":null,
  #  "required_code_owners":[],"code_owner_review_satisfied":true}
  curl -s -X PATCH ... -d '{"title":"x"}' .../pulls/41
  # same [] / true
  ```
- **Expected / Actual:** the changed-file set is unknown when the head is gone;
  the code-owner loader is explicitly fail-closed for that case
  (`crates/githearth-server/src/codeowners.rs:583-590`), so the fields should be
  `satisfied: false` (with `required: []`). Actual: the `detail()` defaults are
  returned because `read_pull` only computes the review when both tips resolve
  (`pulls.rs:1038`). The merge endpoint refuses (`409 pull request is not open`),
  so no merge can be authorised on the false value; the impact is a misleading
  display/API field, not an unsafe merge. Re-ran twice, deterministic.

## Notes

- **#157 itself is fixed.** In 60+ create/PATCH/GET combinations the three
  responses carried identical `required_code_owners`, `approved` flags,
  `code_owner_review_satisfied` and `code_owner_errors`, and the merge endpoint
  agreed in every evaluable case: unapproved owner -> 403, owner approval ->
  201, non-owner approval -> 403 (C01-C34, C42-C58, C71-C95, C113-C121).
  Create loads CODEOWNERS before the transaction and evaluates inside it
  (`pulls.rs:752-812`); PATCH does the same (`pulls.rs:1149-1223`); GET uses the
  shared `review` path (`pulls.rs:1038-1049`).
- **Resolution semantics observed.** The last matching rule wins
  (`special.txt` -> `@bob`, C31); a trailing `/` covers a directory
  (`sub/` -> `@bob`, C35); `.githearth/CODEOWNERS` beats root and `docs/`
  (C78); `docs/CODEOWNERS` is the fallback (C86); a rule with no owners requires
  nothing (C88). A file with several owners is satisfied when **any** owner
  approves, so the list can show `@carol:false` next to `satisfied:true`
  (C13, C50); the gate uses the same rule.
- **Author-as-sole-owner (#137) still works.** `@t10` as sole owner is reported
  `approved:true` with no review and the PR merges (C46-C49); the author among
  several owners also satisfies the file (C50-C53). This follows the author
  being inserted into the approver set in `codeowners.rs:520-530`.
- **Owners always come from the target's base.** Editing `CODEOWNERS` on the
  head (C54-C58), adding it on the head where the base had none (C67-C70), and
  editing/deleting it on a fork (C113-C121) changed nothing; the parent's
  target-branch file was used. A non-fork source is 400 and an unknown source
  404 (C122-C123).
- **Invalid/unreadable CODEOWNERS.** Malformed tokens are surfaced in
  `code_owner_errors` but do not fail closed when no owners remain (`* carol`
  -> `[]`/`true`, gate 201; C83-C85) — pre-existing documented parser behaviour,
  not a #157 regression. Binary (`CODEOWNERS is not a text file`) and oversized
  (`CODEOWNERS exceeds the size limit`) files fail closed with `sat:false`, and
  create/PATCH/GET agree; the gate is 403 (C71-C77).
- **Review requests re-tested.** Create requests the changed files' owners
  (direct user and team-expanded member) without requesting an outsider
  (C98-C99); drafts request nobody until marked ready, then request the owner
  (C100-C101); manual add/list/remove works and unknown users or the author are
  422 (C102-C106). A pending request is deleted when that reviewer submits any
  verdict (`pulls.rs:1872-1879`), which is why an owner who had already reviewed
  appeared absent from the list; that was a test mistake, corrected.
- **Latency.** Six creates with CODEOWNERS: p50 104 ms, p95 121 ms, max 121 ms;
  four baseline creates with no CODEOWNERS: p50 105 ms, p95 106 ms. Fifteen
  PATCHes with evaluation: p50 109 ms, p95 116 ms, max 123 ms. The extra Git
  read is within noise of the baseline.
- **Git-busy.** Holding all eight `git_slots` with slow `git-upload-pack`
  streams made create and PATCH return `503 {"error":"git requests busy"}`
  (C135-C136). The failed create left no pull request row (C137), the failed
  PATCH left the title unchanged (C138), and retries returned 201/200 with the
  correct code-owner fields (C139-C140). The 503 is raised before any
  transaction (`browse.rs:78-84`), so durable state cannot be partially written.
- **Merged PRs.** After a merge the head is an ancestor of the base, so the
  changed-file set is empty and PATCH/GET both report `[]`/`true` (C96). That is
  the same computation the detail endpoint does, not a false green.
- **Concurrent merges.** Intents for #11/#12 became `stale: target moved` during
  the rapid merge sequence; the authorization decision at request time was
  correct. Re-merging later returned 403 because `main`'s CODEOWNERS had since
  become `* @nobody` (a previous PR merged it), which the live evaluation
  correctly reported (C142).
- **Test mistakes, corrected and re-run (not product defects):** the cross-repo
  source spec was built without its org (`400`); manual review requests use the
  field `reviewer`, not `username`; one review-request check queried a PR whose
  owner had already reviewed (request correctly consumed); the draft-toggle
  expectation initially assumed `carol` after the base had changed; deleting the
  second fork first returned 403 because the session's sudo age had expired
  (re-login -> 204). Each was corrected and re-run.
- **Re-runs:** the two failing cases (C141 for R3-01, C147 for R3-02) were each
  re-run twice against the live instance; both reproduced exactly. No transient
  500s or flakes were observed in this domain.

## Files read for expected behaviour

- `crates/githearth-server/src/codeowners.rs` — parse/match/resolve, fail-closed
  review (583-590), review request job (642-800).
- `crates/githearth-server/src/pulls.rs` — detail defaults (571-572), create
  evaluation (752-812), GET evaluation (1038-1049), PATCH evaluation
  (1149-1223), draft route (1399-1402).
- `crates/githearth-server/src/merge_intents.rs` — gate facts (439-464).
- `crates/githearth-server/src/browse.rs` — git-busy 503 (78-84).
