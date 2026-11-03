#!/usr/bin/env bash
# Run the workspace tests against the separate *test* Postgres instance (port 54330), leaving the
# dev instance and any server running against it untouched.
#   scripts/test.sh                       whole workspace
#   scripts/test.sh -p lazy-e2e           extra arguments go to the test runner
#
# With cargo-nextest (https://nexte.st; `cargo install cargo-nextest --locked`, or its prebuilt
# binary) every test runs in its own process, in parallel across all the test binaries, with the
# tests that use Postgres capped as `.config/nextest.toml` says: a few minutes instead of a quarter
# of an hour. Without it, `cargo test` runs the binaries one after another.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export LAZY_PG_INSTANCE=test
export DATABASE_URL="$("$ROOT/scripts/dev-db.sh" start)"
cd "$ROOT"
if cargo nextest --version >/dev/null 2>&1; then
  exec cargo nextest run --workspace --no-fail-fast "$@"
fi
echo "cargo-nextest is not installed; running the tests one binary at a time (slow)" >&2
exec cargo test --workspace "$@"
