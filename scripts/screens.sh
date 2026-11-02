#!/usr/bin/env bash
# Render offscreen screenshots of the UI scenes in crates/lazy-ui/examples/screens.rs.
#
#   scripts/screens.sh 8a-            scenes whose name starts with "8a-"  (writes .dev/screens)
#   scripts/screens.sh 8a- out/dir    ...into another directory
#   scripts/screens.sh --all          every scene (well over 170 PNGs at 2x; slow; rarely wanted)
#
# A prefix is required on purpose. The example given only an output directory renders every
# scene, which costs minutes and buries the screens you meant to look at. Scene names are the
# `name:` fields in screens.rs; list them with:  grep -o 'name: "[^"]*"' crates/lazy-ui/examples/screens.rs
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ $# -lt 1 ]; then
  echo "usage: scripts/screens.sh <scene-name-prefix> [out-dir]   (or --all)" >&2
  exit 2
fi

OUT="${2:-$ROOT/.dev/screens}"
cd "$ROOT"
if [ "$1" = "--all" ]; then
  exec cargo run -q -p lazy-ui --example screens -- "$OUT"
fi

# Count first, so a prefix that matches nothing fails loudly instead of "succeeding" with no output.
if ! grep -q "name: \"$1" crates/lazy-ui/examples/screens.rs; then
  echo "no scene name starts with \"$1\"" >&2
  exit 1
fi
exec cargo run -q -p lazy-ui --example screens -- "$OUT" "$1"
