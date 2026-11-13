# GitHearth

A home for your code.

GitHearth is a planned source-available development platform for repositories, code review, project planning, CI/CD, packages, security, and collaboration. The long-term goal is broad GitHub/GitLab-style capability with customer control over hosting and data.

**Current status: verified experimental single-node application.** The repository contains a Rust server with PostgreSQL migrations, administrator bootstrap, bearer sessions and membership-scoped organizations, Git hosting over Smart HTTP and SSH, repository browsing, issues and pull requests (threaded inline reviews, suggestions, drafts, reviewers and code owners, commit checks, branch and tag protection, signed-commit policy, merge methods, conflict handling, merge queue and auto-merge), forks, notifications, a server-rendered UI, and installation, backup/restore, upgrade and uninstall runbooks with executable drills. Replication, CI integration, packages, security scanning and production hardening remain planned. The repository remains private during initial development.

## Product commitments

- One product supports both a single-machine installation and a distributed deployment.
- The same APIs, permission rules, identifiers, and durable formats apply in both modes.
- Rust is the primary implementation language; performance claims require measurements.
- Users can self-host, export their data, and use standard Git clients.
- GitHearth is licensed under the Business Source License 1.1; see [Licensing](#licensing).
- Completed features are intended to be available without feature tiers: one commercial license covers the whole product.

## Licensing

GitHearth is source-available under the [Business Source License 1.1](LICENSE). It is not open source.

- **Free:** reading, copying and modifying the code, and any non-production use such as development, testing and evaluation.
- **Needs a commercial license:** production use. [Contact the maintainers](https://github.com/DeviousCardi) to arrange one.
- **Later:** each version becomes available under the Apache License 2.0 four years after it is published.

Contributions are covered by [CONTRIBUTING.md](CONTRIBUTING.md). To report a vulnerability, see [SECURITY.md](SECURITY.md).

## Quick start (Docker Compose)

Requirements: Docker Engine with Compose, Python 3, and a terminal. Ports 8080,
2222 and 54329 must be free. Run these from the repository root. The database and
repository storage are persistent Docker volumes, and the generated `.env` is a
secret (it is git-ignored).

```bash
# 1. Generate a unique local database password
umask 077
python3 - <<'PY'
import pathlib, secrets
with pathlib.Path('.env').open('x') as f:
    f.write('POSTGRES_PASSWORD=' + secrets.token_hex(32) + '\n')
PY

# 2. Build and start PostgreSQL, migrate, generate the SSH host key, then the app
docker compose up --build --wait

# 3. Confirm the server is ready
curl --fail http://127.0.0.1:8080/health/ready

# 4. Create the first administrator (password on stdin, 12+ bytes; runs once)
read -r -s -p 'New administrator password (12+ bytes): ' ADMIN_PASSWORD; echo
printf '%s' "$ADMIN_PASSWORD" | docker compose exec -T app githearth-server bootstrap-admin --username admin
unset ADMIN_PASSWORD
```

Open <http://127.0.0.1:8080/> and sign in as `admin`. Git clients use
`http://127.0.0.1:8080/<org>/<repo>.git` or `ssh://127.0.0.1:2222/<org>/<repo>.git`.

Stop while keeping data with `docker compose down`. **`docker compose down --volumes`
permanently deletes this installation's database.** Plain HTTP is for loopback
development only; terminate TLS at a trusted proxy before any nonlocal use
(see [tls.md](docs/operations/tls.md)).

For the direct-binary path, the full configuration reference, the API exercise
and the real-PostgreSQL test procedure, see [foundation.md](docs/foundation.md).
For a production installation, the preflight `doctor` check and upgrades, see
[install.md](docs/operations/install.md), [upgrade.md](docs/operations/upgrade.md)
and [uninstall.md](docs/operations/uninstall.md).

## Start here

Start with the [master plan](docs/plan/00-master-plan.md).

| Document | Purpose |
| --- | --- |
| [Master plan](docs/plan/00-master-plan.md) | Vision, constraints, deployment promise, scope, and design priorities |
| [Full forge roadmap](docs/plan/04-forge-roadmap.md) | Milestones M0–M14, dependencies and completion gates through the complete baseline |
| [First milestone](docs/plan/05-first-milestone.md) | M1 scope, acceptance criteria and M2 task breakdown |
| [Capability inventory](docs/plan/06-capability-inventory.md) | Required feature families and their acceptance obligations |
| [Qualification and decisions](docs/plan/07-qualification-and-decisions.md) | Capacity, faults, deployment profiles, pending choices and release evidence |
| [M1 verification](docs/verification/m1-foundation.md) | Reviewer evidence, criterion outcomes and limits |
| [Foundation runbook](docs/foundation.md) | Start the server, migrate, bootstrap, exercise API and run real PostgreSQL tests |
| [Metadata contract](docs/plan/01-metadata-contract.md) | Data ownership, transactions, workload definitions, and account-scale profiles |
| [Decision gates](docs/plan/02-decision-gates.md) | Agreed requirements, unresolved decisions, and the implementation sequence |
| [Deployment contract](docs/plan/03-deployment-contract.md) | Single-node/cluster behavior, failure semantics, expansion, and merge requirements |
| [Backup and restore](docs/operations/backup-restore.md) | Concrete backup/restore procedure and storage verification |
| [Database experiment](experiments/metadata/README.md) | Reproducible lab setup, workload runner, and result interpretation |
| [Database decision](docs/decisions/0001-metadata-database.md) | Provisional PostgreSQL selection, measured evidence, and remaining qualification gates |
| [Authorization decision](docs/decisions/0002-authorization.md) | Cedar policy-based authorization: roles, built-in forbids, failing closed |
| [Merge execution decision](docs/decisions/0004-merge-execution.md) | A/B merge contract: intents, methods, expected-ref execution, recovery (proposed for M3) |
| [Local results](experiments/metadata/results/local-screening-2026-10-27/REPORT.md) | 84 measured runs, errors, resource observations, and process recovery |
| [Git prototype](docs/git-prototype.md) | Working local commands, tests, and current limitations |

The full product scope now has a milestone roadmap and capability inventory; detailed subsystem ADRs are resolved before their implementation gates. These documents do not claim feature parity, production capacity, or distributed correctness.

## Verify the Rust prototype

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
