# GitHearth

An open home for your code.

GitHearth is a planned open-source development platform for repositories, code review, project planning, CI/CD, packages, security, and collaboration. The long-term goal is broad GitHub/GitLab-style capability with customer control over hosting and data.

**Current status: engineering experiments.** The repository contains a Rust local Git read prototype and an executable PostgreSQL/FoundationDB comparison. It does not yet contain a network server, product installer, replicated Git engine, or CI runner. The repository remains private during initial development.

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
| [Metadata contract](docs/plan/01-metadata-contract.md) | Data ownership, transactions, workload definitions, and account-scale profiles |
| [Decision gates](docs/plan/02-decision-gates.md) | Agreed requirements, unresolved decisions, and the implementation sequence |
| [Deployment contract](docs/plan/03-deployment-contract.md) | Single-node/cluster behavior, failure semantics, expansion, and merge requirements |
| [Database experiment](experiments/metadata/README.md) | Reproducible lab setup, workload runner, and result interpretation |
| [Database decision](docs/decisions/0001-metadata-database.md) | Provisional PostgreSQL selection, measured evidence, and remaining qualification gates |
| [Local results](experiments/metadata/results/local-screening-2026-10-27/REPORT.md) | 84 measured runs, errors, resource observations, and process recovery |
| [Git prototype](docs/git-prototype.md) | Working local commands, tests, and current limitations |

The full product direction remains broad; detailed subsystem designs follow the initial database and storage experiments. These documents do not claim feature parity, production capacity, or distributed correctness.

## Verify the Rust prototype

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
