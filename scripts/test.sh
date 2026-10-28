#!/usr/bin/env bash
# Run the workspace tests against the separate *test* Postgres instance (port 54330), leaving the
# dev instance and any server running against it untouched.
#   scripts/test.sh                       whole workspace
#   scripts/test.sh -p lazy-e2e           extra arguments go to `cargo test`
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export LAZY_PG_INSTANCE=test
export DATABASE_URL="$("$ROOT/scripts/dev-db.sh" start)"
cd "$ROOT"
exec cargo test --workspace "$@"
