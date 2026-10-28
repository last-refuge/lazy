#!/usr/bin/env bash
# Run a desktop client with its own disposable profile, so several clients can share a machine.
#   scripts/run-client.sh [profile-name]      (default profile: alice)
# Profiles live in .dev/profiles/<name>; delete the directory to start fresh.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME="${1:-alice}"
export LAZY_PROFILE_DIR="$ROOT/.dev/profiles/$NAME"
export LAZY_SERVER_URL="${LAZY_SERVER_URL:-http://127.0.0.1:8080}"
export RUST_LOG="${RUST_LOG:-lazy=info}"
mkdir -p "$LAZY_PROFILE_DIR"
cd "$ROOT"
exec cargo run -p lazy-desktop --release
