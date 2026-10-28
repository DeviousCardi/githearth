# Local Git prototype

Status: experimental, operator-only, Linux-oriented. This is a foundation experiment, not a network forge or replicated storage service.

## Implemented

- Shared repository and object ID types.
- A `RepositoryReader` contract for refs and blob reads, implemented by the durable sharded `layout::Storage`.
- Idempotent bare-repository creation under the ADR 0003 sharded storage layout (`repositories/<id[0..2]>/<id>.git`).
- Heads and tags listed through native Git.
- Blob reads by full SHA-1/SHA-256 hexadecimal ID, with an 8 MiB CLI limit.
- Ambient Git configuration/environment isolation for child processes.
- Tests using an actual local Git commit/push, binary blob content, size rejection, missing/non-blob objects, repository symlink rejection, root-escape rejection, and trashed-repository isolation.
- A `layout` module implementing the ADR 0003 storage layout (`repositories/<id[0..2]>/<id>.git`, `tmp/`, `trash/`, `orphans/`): bare repository creation with SHA-1/SHA-256 object formats, the repository config table, idempotent creation, and lifecycle helpers (trash/restore/orphan/purge/scan/clean_tmp), plus root/symlink containment checks.
- A `journal` module implementing the ADR 0003 append-only push journal: CRC-32C-framed JSON records, torn-tail recovery, corruption detection, rotation, and ref/marker recovery classification.

## Try it

Use a fresh directory and repository ID:

```sh
cargo run -p githearth-cli -- init .bench/git-storage 0123456789abcdef0123456789abcdef
cargo run -p githearth-cli -- refs .bench/git-storage 0123456789abcdef0123456789abcdef
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The `init` command creates a bare repository at `repositories/01/0123456789abcdef0123456789abcdef.git` under the storage root and prints the absolute path. Creation is idempotent: repeating the same command succeeds and returns the same path.

The `blob` command takes the storage root, repository ID, and a full object ID from that repository as its fourth argument. It writes raw bytes to standard output. The API accepts full SHA-256 IDs; the initial bare repository creation uses SHA-1 by default.

## Limits before service use

There is no authentication, database mapping, network transport, mutation journal, replication, metrics backend, or distributed acknowledgement. The local `init` command is not a durable product transaction. Repository ownership, creation failure recovery, process timeouts, ref-output limits, byte-oriented ref names, and hostile repository/configuration handling require further design.

The operator must own the storage directory and its contents. The storage layer's path checks do not defend against another local process racing to replace filesystem entries. Native Git operations must gain process deadlines and a stronger containment model before being exposed to remote users.

Native Git integration is intentional at this stage: it provides a conformance baseline for later gitoxide or custom implementations. Any replacement should pass the same compatibility fixtures and demonstrate the intended performance improvement.
