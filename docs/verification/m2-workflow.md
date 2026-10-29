# M2 workflow verification

Date: 2026-10-29. State: **M2 accepted.** The single-node workflow, including the browser issue and comment pages, is verified by an executable harness on main with Smart HTTP push (#22), repository browsing (#21) and the issue pages. M2 is accepted **without** a comparison run against another forge, because none was available (see [Comparison](#comparison)). This report records local runs, not a capacity claim. Items the harness leaves to other tests or later milestones are listed in [Remaining scope](#remaining-scope).

Implementation: delegated OpenCode agents running DeepSeek-V4-Pro-0813 (high reasoning). Review and the recorded run below: Person B's reviewing agent, including an independent harness run separate from the implementing agent's own run. The acceptance re-run below, with the issue-page check added to scenario 3, was made by Person A's agent on the issue-pages branch.

The harness [`scripts/verify-m2.py`](../../scripts/verify-m2.py) implements the M2 exit gate in [05-first-milestone.md](../plan/05-first-milestone.md) (task breakdown items 7–8) and the [M2 acceptance checklist](../plan/08-two-person-roadmap.md#m2-acceptance-checklist).

## Environment

| Item | Value |
| --- | --- |
| Source revision | `efa4f7a` (main with browsing, plus #22 push); this report's commit adds only the harness and documentation |
| Host toolchain | rustc 1.99.1; server built in release mode by the harness |
| Git client and server-side Git | git 2.43.0 |
| PostgreSQL fixture | `postgres:17-bookworm`, server 17.11 (Debian 17.11-1.pgdg12+2) |
| Docker Engine | 29.1.3 |
| OS / kernel | Linux 7.0.0-38-generic x86_64, glibc 2.39 |
| CPU count | 16 |

## Commands

| Command | Result |
| --- | --- |
| `python3 scripts/verify-m2.py --report <path>` | **Passed, 8/8 scenarios** (two independent runs: implementing agent, then reviewer) |
| `scripts/ci.sh` on `efa4f7a` (fmt, clippy, deny, all tests including PostgreSQL, image build) | Passed; workspace line coverage 95.64% |

## Scenario results

Each scenario runs against a disposable PostgreSQL container and the native release binary with a temporary configuration. Secrets are generated per run and never written to the repository. Git credentials reach real `git` clients through a temporary credential helper, never through URLs. Any failure exits non-zero.

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Fresh install | Migrate. Bootstrap the admin via stdin; a second bootstrap is refused. Log in, create an org and a **private** repository (answers `active`). The unrelated user is inserted through SQL with no memberships or grants, because the second bootstrap is refused and there is no registration API yet. | PASS | 0.45 s |
| 2 | Push/clone/browse | Real push of a fixture history (several commits, a branch, a tag, nested dirs, a binary file, a rename). Clone back; `rev-parse` of every ref matches. The browse API (refs, trees, text and binary blobs, paginated commits, commit diff) agrees with `git` in the clone. Browser login and one tree page return 200. | PASS | 0.81 s |
| 3 | Issues/comments | Create an issue and comments, paginate both. Retrying with the same `Idempotency-Key` returns an identical response and adds no issue, comment, outbox or audit row (checked through the API and SQL). **Browser pages:** the issue list shows every issue and redirects to sign-in without a session; a form with a bad CSRF token gets 403; the new-issue form creates an issue (303 to its page, title HTML-escaped); the comment form adds a comment that the page and the API both show. | PASS | 0.56 s |
| 4 | Isolation/archive | The unrelated user gets 404 for clone, push advertisement, API reads, browsing and issues. Unauthenticated Git gets 401. Archiving refuses push while clone still works; unarchive restores push. | PASS | 0.51 s |
| 5 | Crash recovery | (a) `kill -9` receive-pack and the server during a ~50 MB push, then restart: the ref is at either the old or the new commit, and a push row exists exactly when the ref moved. A normal push afterwards succeeds. (b) After an acknowledged push, delete its `repository_pushes`/`ref_updates` rows and reset `journal_applied`, simulating a crash after the acknowledgement but before stage 7. On restart the reconciler reapplies them **exactly once**. | PASS | 6.19 s |
| 6 | Restart | Identity, organization, repository, refs, issues (including the one created in the browser) and `git ls-remote` survive a server restart. | PASS | 0.29 s |
| 7 | Fresh restore | ADR 0003 order: `pg_dump` first, then push one commit and create one repository, then snapshot storage. Restore into a new database and a new storage directory, and start a fresh server. The later push is **reapplied from the journal**. The repository created after the dump is moved to `orphans/` (kept, not served). Clone, browse, issues and comments match, and the unrelated user is still denied. | PASS | 3.29 s |
| 8 | Measurements | Recorded below; nothing asserted. | PASS | 1.29 s |

Acceptance re-run with the browser issue check (main at `b5555af` plus the issue-pages change): **8/8 PASS**. Wall times for that run: 0.45, 0.83, 0.56, 0.52, 6.23, 0.29, 3.25 and 1.29 s. The first attempt failed scenario 6 only because it expected exactly three issues; the check now counts every issue the run created.

## Measurements

These come from one run on a developer laptop, over loopback HTTP, with a small dataset (a handful of commits plus one ~50 MB random blob).

| Measurement | Value |
| --- | --- |
| Server peak RSS (VmHWM) per lifetime | initial 39.1 MiB; after crash 36.2 MiB; after lost-metadata restart 26.9 MiB; after restart 36.4 MiB; restored 36.5 MiB (acceptance re-run: 39.3, 36.1, 26.5, 36.5, 36.5 MiB) |
| PostgreSQL database size | 8.6 MiB (restored copy 8.5 MiB) |
| Storage directory size | 50.1 MiB, mostly the crash-scenario blob (restored copy identical) |
| PostgreSQL container memory (`docker stats`) | 68.6 MiB |
| Push of the fixture history | 0.24 s |
| Clone back | 0.24 s |
| Single-commit pushes (normal, after crash, after lost metadata, after dump) | 0.15–0.18 s |
| `git ls-remote` after restart | 0.16 s |
| Clone of the restored repository (includes the ~50 MB blob) | 1.85 s |

## Comparison

GitLab, Gitea and Forgejo were **not** run in this environment; no comparison system was available. The comparison was optional for M2, which is accepted without it. If one becomes available later, run it against this harness's scenarios and record it here. The numbers above are single-node, loopback-only measurements on one machine. They are neither capacity nor throughput evidence, and must not be compared with whole-platform request rates from other forges.

## Limitations of this run

- In scenario 5a the harness kills receive-pack as soon as the process appears. Which side of the ref commit it lands on depends on timing, so a single run exercises only one of the two outcomes (old or new). The integration tests in `tests/git_push.rs` cover both replay branches (prepared-only at new, and at old) deterministically.
- The resource figures are one sample, not a distribution, and were taken with no concurrent load.

## Remaining scope

Against [05-first-milestone.md](../plan/05-first-milestone.md), the harness does **not** cover the following. Some items are covered by unit or integration tests; the rest are open for later milestones:

- Repository **rename/transfer/delete** and the trash retention window end to end (integration tests cover them; the harness only exercises archive/unarchive).
- **Team- and grant-based** push authorization beyond owner and read-only, branch protection (M3), and the open ADR 0003 item on token freshness for `ref.force_push`/`ref.delete`.
- **SHA-256** repositories end to end. Browsing tests cover SHA-256 reads, but the harness pushes SHA-1.
- Public/internal visibility. Only a private repository is exercised.
- Server-rendered pages beyond login, a tree page and the issue pages. Issue close/reopen and editing are API-only.
- Personal access tokens, SSH transport (before M4), and outbox **delivery**. Outbox rows are durable and deduplicated, but nothing consumes them yet.
- Sustained or concurrent load.
