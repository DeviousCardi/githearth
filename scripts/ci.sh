#!/usr/bin/env bash
# Local equivalent of .github/workflows/ci.yml. Run before every merge.
# Starts a disposable PostgreSQL container unless GITHEARTH_TEST_DATABASE_URL is set.
set -euo pipefail
cd "$(dirname "$0")/.."

export RUSTFLAGS="-D warnings"
min_lines="${GITHEARTH_MIN_LINE_COVERAGE:-95}"

container=""
# Not `[ -n ] &&`: with no container that test fails and becomes the exit status.
cleanup() { [ -z "$container" ] || docker rm -f -v "$container" >/dev/null; }
trap cleanup EXIT

if [ -z "${GITHEARTH_TEST_DATABASE_URL:-}" ]; then
    container="githearth-ci-$$"
    password="$(openssl rand -hex 16)"
    docker run -d --name "$container" -e POSTGRES_USER=githearth -e POSTGRES_PASSWORD="$password" \
        -e POSTGRES_DB=githearth_test -p 127.0.0.1::5432 postgres:17-bookworm >/dev/null
    for _ in $(seq 60); do
        docker exec "$container" pg_isready -U githearth -d githearth_test >/dev/null 2>&1 && break
        sleep 1
    done
    port="$(docker port "$container" 5432/tcp | head -1 | cut -d: -f2)"
    export GITHEARTH_TEST_DATABASE_URL="postgres://githearth:${password}@127.0.0.1:${port}/githearth_test"
fi

step() { printf '\n==> %s\n' "$*"; }
step fmt
cargo fmt --all -- --check
step clippy
cargo clippy --workspace --all-targets --locked -- -D warnings
step deny
cargo deny check advisories bans licenses sources
step "tests with coverage (including PostgreSQL)"
cargo llvm-cov --workspace --locked --summary-only --fail-under-lines "$min_lines" -- --include-ignored
step "container image"
if [ "${GITHEARTH_CI_SKIP_IMAGE:-0}" != 1 ]; then
    docker build -q -t githearth-server:ci . >/dev/null
fi
step "all gates passed"
