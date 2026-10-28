#!/usr/bin/env bash
# Start the dev database (if needed) and run the Lazy server on 127.0.0.1:8080.
#   scripts/run-server.sh [extra lazy-server args]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export DATABASE_URL="$("$ROOT/scripts/dev-db.sh" start)"
export RUST_LOG="${RUST_LOG:-lazy_server=info,tower_http=warn}"
cd "$ROOT"
exec cargo run -p lazy-server --release -- serve "$@"
