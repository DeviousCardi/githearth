# License files and third-party notices

GitHearth is distributed under the Business Source License 1.1
(`BUSL-1.1`, declared in `Cargo.toml`). The full text is in `LICENSE` at the
repository root and is copied into the container image at
`/usr/share/doc/githearth/LICENSE`.

The release binaries (`githearth-server`, which the container image ships, and
`githearth-cli`) are built with third-party components, each under its own
license. `THIRD_PARTY_NOTICES` at the repository root lists every third-party
crate in the build graph of those binaries — runtime and build dependencies,
test-only dependencies excluded — with the SPDX license expression each crate
declares.

## Regenerating the notices

```
cargo fetch
scripts/generate-third-party-notices.py
```

The script resolves the dependency graph offline from `Cargo.lock` (via
`cargo metadata --locked --offline`), so it needs no network access and is
deterministic for a given `Cargo.lock`. It reads crate manifests from the
registry cache, so run `cargo fetch` first: the notices list the union over
all target platforms, and `cargo fetch` downloads manifests for every target,
whereas `cargo build` or `cargo check` on Linux only populate the cache for
the current platform's dependencies. Regenerate the file after any dependency
change; keep it committed alongside `Cargo.lock`.

`cargo deny check licenses` (in `scripts/ci.sh`) independently enforces the
allow-list of acceptable licenses in `deny.toml`, so a dependency with an
unapproved license fails CI before it is ever released.

## Stale check

`scripts/ci.sh` runs `scripts/generate-third-party-notices.py --check`, which
regenerates the file in memory and fails with a non-zero exit code if the
committed copy differs. The check is fast and offline.
