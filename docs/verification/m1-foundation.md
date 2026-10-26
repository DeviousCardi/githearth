# M1 foundation verification

Date: 2026-10-27. State: **verified experimental foundation**. All C01–C12 criteria in the [M1 specification](../plan/05-first-milestone.md) passed. This completes M1, not the Git-and-issues workflow or a production release.

Implementation: delegated GPT-6.1 Sol, medium reasoning. Review: primary agent, including independent real PostgreSQL, executable HTTP and Compose verification. Work began from e9866ca. The final repository commit containing this report identifies the reviewed source; the executable-input fingerprint below permits independent comparison without a self-referential commit hash.

## Environment

- Host toolchain: rustc 1.99.1 / cargo 1.99.1.
- Application release container: built with Rust 1.85-bookworm, confirming the declared minimum toolchain for the locked server build.
- Docker Engine: 29.1.3.
- PostgreSQL fixture: postgres:17-bookworm, image digest `sha256:858bb5e98eed02eb8cafbcdc007f9a65d1b613972028944a8ec2133ccaf80d27`.
- Final tested application image: `sha256:909d26581d295006762b726b8f2582c8913064cd29cbaf291c45692dcb7093fe`.
- This M1 fixture uses PostgreSQL 17; the earlier metadata screening used 18.6. M1 validates application behavior, not a new cross-version performance comparison.

## Commands and results

| Command/scenario | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed |
| `cargo test --workspace` | Eight tests passed: three existing core/Git plus five server; DB test ignored by default |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `python3 scripts/verify-foundation.py` | Passed; explicitly runs real PostgreSQL contract and native executable workflow |
| Real PostgreSQL contract, explicit ignored invocation inside script | One integration contract passed, 3.49 seconds on final independent run |
| Dockerfile locked release build using Rust 1.85 | Passed, implementing agent; final image independently smoke-tested |
| Compose config/migration/bootstrap/login/me/org/restart/logout | Passed independently with final image and repository Compose definition |
| Credential log check | Generated password, DB password and bearer token absent from checked native logs; password/token absent from Compose app/migrate logs |
| `git diff --check`, documentation relative links | Passed |

Container reproduction uses the runbook's `docker compose up --build --wait`. For the independent final smoke, the already-built final image was tagged as `githearth-review-final-app` and `githearth-review-final-migrate`, then `docker compose --env-file <temporary-secret-file> -p githearth-review-final up --no-build --wait` was exercised. The supervisor bootstrapped through stdin, logged in, created an org, restarted app, verified persisted identity/org, logged out and checked logs. Only that temporary project's containers, network and volume were removed. No installation secrets were checked in.

## Criterion outcomes

| ID | Outcome | Evidence |
| --- | --- | --- |
| C01 | Pass | Workspace checks, existing native Git push/read/symlink tests, Rust 1.85 release build |
| C02 | Pass | Real empty DB migration twice; failed history, checksum mismatch, unknown migration and missing required relation make readiness fail |
| C03 | Pass | Missing/bounded config unit tests; unknown/type/syntax TOML errors sanitized; sample and Compose used successfully; env override reviewed |
| C04 | Pass | Concurrent DB bootstrap has one success; duplicate bootstrap does not change credentials; actual CLI duplicate rejected; stdin input |
| C05 | Pass | Argon2id v19 m19456/t2/p1/output32 pinned and tested; wrong/unknown login; digest-only random session persistence; actual login/me |
| C06 | Pass | Absent/malformed/unknown/expired/revoked tokens denied; actual logout followed by denied admission |
| C07 | Pass | Two-account tenant isolation, no platform-admin bypass, invalid/duplicate slug checks, three-page cursor completeness without foreign data |
| C08 | Pass | Forced final outbox insert failure leaves organization/membership/audit/outbox counts unchanged; successful linked creation |
| C09 | Pass | Live200/ready503 during real DB pause; ready200 after restart with stable address; incompatible schema rejected |
| C10 | Pass | Oversized body rejected; deterministic saturation429; cancelled caller retains blocking worker permit until actual work ends; graceful SIGTERM observed |
| C11 | Pass | Native and container executable workflow, DB restart and HTTP process restart retain session/org; Compose migration and health dependencies work |
| C12 | Pass | Runbook/sample/Compose and cleanup reviewed; limits, HTTP/TLS, secrets and absent backup automation stated |

## Review findings and corrections

The supervisor requested and received explicit password cost parameters, deterministic auth overload/cancellation tests, malformed credentials, invalid orgs, multi-page isolation tests, unknown migration handling and secret-safe config tests.

The initial independent restart run failed because Docker changed an automatically allocated host port on container restart. Minimal reproduction confirmed the mapping changed; the app still targeted the old address. The harness now publishes an explicit loopback port and asserts it remains unchanged. Recovery then passed without a runtime reconnection change.

A fresh-fixture run also caught PostgreSQL's temporary initialization Unix socket as ready before the final TCP listener accepted clients. Both the harness and Compose readiness check now use TCP localhost. The corrected isolated run and final Compose smoke passed. Failed runs were not treated as product acceptance evidence.

## Scope limits and next gate

Experimental local HTTP/API foundation. Not implemented: Git HTTP/SSH hosting, UI, issues/reviews, organization operation-ID replay, account lifecycle/reset, session cleanup/quotas, durable outbox consumer, distributed abuse controls, backup automation or HA. No throughput, 100,000-active-user, power-loss durability or production-security claim follows from these tests.

Unique slugs help resolve an unknown organization result manually but are not replay semantics. M2 begins with the Git receive journal/acknowledgement ADR, repository lifecycle/authorization, durable operation IDs, Git smart HTTP, browsing and issues/comments. See [M2 tasks](../plan/05-first-milestone.md#m2-first-task-breakdown).

All supervisor temporary containers/volumes/secret files were removed. Built images and build cache remain reusable. The private repository remains private.

## Executable-input fingerprint

SHA-256 over sorted relative path, NUL, file bytes, NUL for workspace Cargo files, Dockerfile, Compose, example configuration, verification script, and every file under `crates/`:

`601cd23741752e45bb3dc04c194ae8838f3280174cf1b6c8896315b11fe3b16f`
