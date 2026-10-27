# GitHearth

An open home for your code.

GitHearth is a planned open-source development platform for repositories, code review, project planning, CI/CD, packages, security, and collaboration. The long-term goal is broad GitHub/GitLab-style capability with customer control over hosting and data.

**Current status: verified experimental application foundation.** The repository contains a Rust HTTP server with PostgreSQL migrations, administrator bootstrap, bearer sessions and membership-scoped organizations, alongside the local Git prototype and database comparison. Git HTTP/SSH hosting, issues/reviews, production installation, replication and CI remain planned. The repository remains private during initial development.

## Product commitments

- One product supports both a single-machine installation and a distributed deployment.
- The same APIs, permission rules, identifiers, and durable formats apply in both modes.
- Rust is the primary implementation language; performance claims require measurements.
- Users can self-host, export their data, and use standard Git clients.
- The proposed licensing is `MIT OR Apache-2.0`; final license files are a release prerequisite.
- Completed features are intended to be available without paid feature gates.

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
| [Database experiment](experiments/metadata/README.md) | Reproducible lab setup, workload runner, and result interpretation |
| [Database decision](docs/decisions/0001-metadata-database.md) | Provisional PostgreSQL selection, measured evidence, and remaining qualification gates |
| [Authorization decision](docs/decisions/0002-authorization.md) | Cedar policy-based authorization: roles, built-in forbids, failing closed |
| [Local results](experiments/metadata/results/local-screening-2026-10-27/REPORT.md) | 84 measured runs, errors, resource observations, and process recovery |
| [Git prototype](docs/git-prototype.md) | Working local commands, tests, and current limitations |

The full product scope now has a milestone roadmap and capability inventory; detailed subsystem ADRs are resolved before their implementation gates. These documents do not claim feature parity, production capacity, or distributed correctness.

## Verify the Rust prototype

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
