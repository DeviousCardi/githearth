# GitHearth M4 closure — round 4 (final verification)

Date: 2026-11-14. Instance: `main` `cd7946b` (PR #163 merged), deployed with the
README Docker Compose quick start. This round verifies the closure-polish
changes and re-runs each area for regressions before the M4 sign-off is updated.

## Instance

- HTTP <http://127.0.0.1:8080>, SSH `127.0.0.1:2222`, PostgreSQL `127.0.0.1:54329`
- Accounts: `t01`–`t18`, `alice` (owner `acme`), `bob` (write), `carol`
  (maintain), `dave` (admin), `erin` (read), `frank` (triage), `grace`
  (member); password `<test-password>`. Admin `admin` /
  `<redacted>`.
- Do not restart the primary compose project (the ops tester uses a second
  project on ports 18080/12222/54330).

## New in this build

Browser organization/repository creation (`/organizations/new`,
`/organizations/{slug}/repositories/new`), reviewer request/remove on the pull
page, fork parent link, merged-by/time, blocked-merge explanation, JSON errors
for `/api` extractor rejections and unknown paths, merge/auto-merge read
authorization before state, fail-closed code-owner facts, NUL paths → 404,
`new` reserved as a slug/repo name.

## Reporting

Write to
`…/opencode/lab-reports-m4-r4/<NN>-<domain>.md`
with the usual format (verdict, counts, results table, defects with repro,
notes). At least 30 cases per tester, re-run failures once, read the source for
root causes (`file:line`), do not edit the repo or post to GitHub. Read-only SQL
is allowed via `docker compose exec -T postgres psql`.
