# GitHearth M4 closure lab — round 3 brief

Date: 2026-11-14. Instance deployed from `main` `800cad4` (PRs #161 and #162
merged) with the README Docker Compose quick start. This round verifies the
issues the first two rounds filed (#150–#159) and re-runs each domain for
regressions.

## Live instance

- HTTP: <http://127.0.0.1:8080> (`/health/live`, `/health/ready`)
- SSH: `127.0.0.1:2222`
- PostgreSQL: `127.0.0.1:54329` (user `githearth`, db `githearth`; password in
  the repo `.env`)
- Image: Debian trixie, Git 2.47.3, server `800cad4`
- Do **not** restart or stop the primary `githearth` compose project. The ops
  tester uses a second project (see below).

## Accounts

Password for every account: `<test-password>`. Admin: `admin` /
`<redacted>` (provisioning/observation only).

| Account | Role in `acme` and in every `labNN` |
| --- | --- |
| `t01` … `t18` | `owner` of their own `labNN` org |
| `alice` | `owner` of `acme` |
| `bob` | `write` |
| `carol` | `maintain` |
| `dave` | `admin` |
| `erin` | `read` |
| `frank` | `triage` |
| `grace` | `member` (base role) |

`acme` also has `alice` as owner; `beta` is owned by `bob`. Each `labNN` has
the same role ladder as `acme`.

## Fixtures

- `acme/web` (private): branches `main`, `feature`, `docs`, tag `v1.0.0`,
  PR #1, issues.
- `acme/site` (public, empty).
- `lab01/t01-public` (public, one commit) for anonymous browse.
- `beta/app` (private) for isolation checks.
- Earlier rounds left many `tNN-*` repositories and PRs in the lab orgs; reuse
  them or create new ones with a `r3-` prefix. Do not delete other testers'
  data.

## Auth and Git

- Session: `POST /api/v1/login` `{"username","password"}` → bearer token.
- Browser cookie: `githearth_session=<session token>` (the login token works
  as the cookie); UI forms need the session's CSRF token from the page.
- Access token: `POST /api/v1/user/tokens`
  `{"name":"...","scopes":["repo:read"]}` or `["repo:write"]`.
- HTTP Git: `http://<user>:<password>@127.0.0.1:8080/<org>/<repo>.git`
- SSH Git: register a key with `POST /api/v1/user/ssh-keys`; use
  `GIT_SSH_COMMAND='ssh -i <key> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p 2222'`.

## Reporting

Write to
`…/opencode/lab-reports-m4-final/<NN>-<domain>.md`.

Format:

```markdown
# <Domain> — M4 closure round 3

Tester: <account>. Date: 2026-11-14. Instance: main 800cad4.
Verdict: PASS / PASS WITH DEFECTS / FAIL
Cases: N run, N pass, N fail, N skipped.

## Results
| # | Case | Expected | Actual | Verdict |

## Defects
### <title>
- Severity: blocker / major / minor
- Repro:
- Expected / Actual:
- Evidence: status codes, bodies, file:line if known

## Notes
```

Rules: at least 35 cases (aim 50) including the issue(s) assigned and a
regression smoke of the domain; re-run each failure once; separate product
defects from test mistakes; read the source under
`/Users/suniljadav/Aarav/Github/githearth` for expected behaviour and cite
`file:line` for root causes. Do not edit the repository or post to GitHub.
Read-only SQL is allowed:
`cd /Users/suniljadav/Aarav/Github/githearth && docker compose exec -T postgres psql -U githearth -d githearth -Atc "SELECT ..."`.
