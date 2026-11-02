#!/usr/bin/env bash
# The media library (media/, the rtc-* crates) must know nothing about Lazy, so it can move to its
# own repository unchanged (llm-docs/DROP_INS.md, D39). Fails if any rtc-* crate depends on a
# lazy-* crate.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
if cargo tree --manifest-path media/Cargo.toml --workspace -e normal,build,dev --prefix none 2>/dev/null \
  | awk '{print $1}' | sort -u | grep -E '^lazy-'; then
  echo "error: the media library must not depend on the lazy-* crates listed above" >&2
  exit 1
fi
echo "ok: the media library depends on no lazy-* crate"
