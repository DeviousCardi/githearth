# M4 lab campaign (V4 acceptance)

Date: 2026-11-14. This is the black-box acceptance campaign behind the M4
sign-off in [m4-signoff.md](../m4-signoff.md). Fifteen independent testers ran
against one live single-node installation, covering every product surface, then
re-ran their cases on the fixed build.

The individual reports are in this directory (`01-…` to `15-…`). Each report has
the per-case table, exact repros, observed vs expected results, and a round-2
section with the re-verification on the fixed image.

## Build under test

| Item | Round 1 | Round 2 |
| --- | --- | --- |
| Source | `main` `b820454` | `fix/m4-lab` `7e1ded3`; review fixes `e638676` (merged as PR #149, main `00607bb`) |
| Deployment | README Docker Compose quick start | same, rebuilt image |
| Image | Debian bookworm, Git 2.39.5 | Debian trixie, Git 2.47.3 |
| Runtime | PostgreSQL 17, server on `127.0.0.1:8080`, SSH `:2222` | same |

The installation was performed from the runbook only (README quick start +
`docs/operations/`), then bootstrapped with `bootstrap-admin`. Fifteen tester
accounts, one isolated `labNN` organization each, plus a shared `acme` fixture
(owner, `write`, `maintain`, `admin`, `read`, `triage`, `member` roles) for
cross-account tests. Git content was pushed over HTTP and SSH.

## Results

| Round | Checks | Pass | Fail | Skipped |
| --- | --- | --- | --- | --- |
| 1 — first build | 1,880 | 1,755 | 98 | 27 |
| 2 — fixed build | 510 | 442 | 68 | 0 |

Round 1 found **1 blocker, 13 major findings and a long tail of minor ones**.
The blocker and every major except two feature gaps — the branch-protection
settings UI (#150) and the browser cross-repo pull-request form (#151) — were
fixed and verified. Round 2's 68 failures are re-confirmations of the remaining
minors (plus one new minor caused by Git 2.47 rejecting an out-of-range cursor),
not regressions. No domain reported a new blocker. Round 2 ran on `7e1ded3`;
the three review fixes in `e638676` (SSH credential cleanup, worker retry
spacing, change-request detail scope) were verified by the full CI and the
affected suites, not by a third tester round.

### Round 2 by domain

| # | Domain | Report | Checks | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Auth, sessions, tokens | [01](01-auth-sessions-tokens.md) | 31/31 | PASS |
| 2 | Organizations and roles | [02](02-organizations-roles.md) | 37/45 | PASS WITH DEFECTS |
| 3 | Repository lifecycle | [03](03-repository-lifecycle.md) | 43/43 | PASS |
| 4 | Git HTTP | [04](04-git-http.md) | 17/23 | PASS WITH DEFECTS |
| 5 | Git SSH, keys, signed commits | [05](05-git-ssh.md) | 23/23 | PASS |
| 6 | Browse and compare UI | [06](06-browse-compare.md) | 24/31 | PASS WITH DEFECTS |
| 7 | Issues and comments | [07](07-issues-comments.md) | 36/44 | PASS WITH DEFECTS |
| 8 | Pull requests and reviews | [08](08-pulls-reviews.md) | 18/26 | PASS WITH DEFECTS |
| 9 | Merge methods, queue, auto-merge | [09](09-merge-queue-automerge.md) | 29/31 | PASS WITH DEFECTS |
| 10 | Branch and tag protection | [10](10-protection-rules.md) | 36/40 | PASS WITH DEFECTS |
| 11 | Forks and cross-repo PRs | [11](11-forks-cross-repo.md) | 31/37 | PASS WITH DEFECTS |
| 12 | Notifications | [12](12-notifications.md) | 12/13 | PASS WITH DEFECTS |
| 13 | UI, accessibility, docs | [13](13-ui-accessibility-docs.md) | 37/47 | PASS WITH DEFECTS |
| 14 | Security and isolation | [14](14-security-tenant-isolation.md) | 30/36 | PASS WITH DEFECTS |
| 15 | Operations drills | [15](15-operations-drills.md) | 38/40 | PASS WITH DEFECTS |

## Blocker and majors found and fixed

| Severity | Defect | Fix (PR #149) | Round-2 evidence |
| --- | --- | --- | --- |
| blocker | Every changed pull-request detail returned 500 (API) / 503 (UI): the shipped Git 2.39.5 rejects `git merge-tree --merge-base`, which the rebase mergeability path always uses | Runtime image moved to Debian trixie (Git 2.47.3); startup and `doctor` now require Git ≥ 2.40 | 08, 09, 10, 11, 13: PR detail 200 everywhere, rebase merge clean/merged |
| major | `require_signed_commits` refused every valid SSH-signed push: the image lacked `ssh-keygen`, so `git verify-commit` failed | Install `openssh-client` in the image | 05, 10: signed commits accepted over HTTP and SSH; unsigned still refused |
| major | `docker compose up` after `down` never started the app: `ssh-keygen` exited 1 when the host key existed | `ssh-keygen` is idempotent and keeps the key | 15: `down` → `up` exit 0 in 9.5 s, host key and clones unchanged |
| major | An outstanding `changes_requested` review did not block a merge | Gate checks outstanding change requests at intent creation, worker revalidation and in the mergeable result; detail uses the same scope | 08, 09: merge 409 while the request stands, fresh approval clears it |
| major | Auto-merge on a conflicting PR accumulated intents, stalled after the enabler's push, and a worker error stranded an intent for the full lease | Dedupe conflicted intents, keep the row enabled, cancel the obsolete queued intent on an enabler push, bound and space retries | 09: one conflicted intent across triggers, resolving push re-raised in 124 ms and merged, failure bounded to 5 attempts |
| major | Forking into a namespace where the actor is a member without create permission returned the masked 404 | Return 403 for a member, 404 for a stranger, matching repository creation | 11: t11→acme 403, unknown/non-member 404, no fork row |
| major | Branch-rule PATCH rejected the documented `pattern` field (only `path` worked) | Accept `pattern` with `path` as an alias | 10: pattern rename 200, alias 200, invalid 400 |
| major | Anonymous visitors were redirected to login when browsing a public repository's tree/blob/commits pages | Anonymous browse uses the same public/active/suspended gate as the Git transport | 06, 13: public pages 200 anonymously, private still 303, no leak |

Minor fixes in the same change: refused SSH pushes no longer leave
`push_credentials` rows (#143); repository descriptions count characters rather
than bytes; zero-width-only titles/comments are refused; a self review request
does not notify the requester; the image ships the `githearth` storage check CLI;
install/ADR/TLS docs no longer describe the pre-2.40 or pre-UI state.

## Remaining known defects

Filed from this campaign; none is a regression from the fix round.

| Issue | Severity | Summary |
| --- | --- | --- |
| [#150](https://github.com/DeviousCardi/githearth/issues/150) | major | Branch protection has no settings UI (API-only); tag protection has one |
| [#151](https://github.com/DeviousCardi/githearth/issues/151) | major | Cross-repository pull requests cannot be created from the browser |
| [#152](https://github.com/DeviousCardi/githearth/issues/152) | minor | NUL byte in user text returns 503 instead of 400 |
| [#153](https://github.com/DeviousCardi/githearth/issues/153) | minor | 404 bodies distinguish missing from unreadable org/repo (existence oracle) |
| [#154](https://github.com/DeviousCardi/githearth/issues/154) | minor | Git HTTP: malformed bodies 200, no 100-continue on fetch, v2 advertisement, read-only push admitted to pack |
| [#155](https://github.com/DeviousCardi/githearth/issues/155) | minor | Browse UI: 100-commit cap, cursor overflow 500, bare 404, captions/breadcrumbs |
| [#156](https://github.com/DeviousCardi/githearth/issues/156) | minor | Fork list/count/parent link missing, fork PR list omits outgoing PRs, system-closed PR can be reopened |
| [#157](https://github.com/DeviousCardi/githearth/issues/157) | minor | PR create/PATCH reports `code_owner_review_satisfied: true` before CODEOWNERS evaluation |
| [#158](https://github.com/DeviousCardi/githearth/issues/158) | minor | UI polish: duplicate id, overflow, stretched checkboxes, unread badge, tag-form 503, queue cancel, ff-only precheck |
| [#159](https://github.com/DeviousCardi/githearth/issues/159) | minor | Commenting on a >30-comment issue redirects to the first page |

Historical note: the primary test database still holds 27 orphan
`push_credentials` rows created by the pre-fix SSH build (21:19–21:24Z); the fix
stops new orphans but does not backfill. The lab instance is disposable.

## Reproducing

1. Deploy with the README Docker Compose quick start on a clean host.
2. Bootstrap an administrator, then create the tester accounts and role
   fixtures (the reports name the accounts and the org roles they used).
3. Run each report's cases with `curl`/`git`/`ssh` against the live instance.
4. For the ops drills, use a second Compose project with
   `GITHEARTH_HTTP_PORT=18080 GITHEARTH_SSH_PORT=12222 GITHEARTH_POSTGRES_PORT=54330`
   (see report 15) so the primary instance is untouched.
