#!/usr/bin/env bash
# The shared crates must stay free of UI toolkit, async runtime, and network dependencies so that
# phone clients can reuse them (plan task 0.3). Fails if any of them pulls one in.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

check() { # crate, forbidden-regex
  local crate="$1" forbidden="$2"
  if cargo tree -p "$crate" -e normal --prefix none 2>/dev/null | awk '{print $1}' | sort -u | grep -E "^($forbidden)$"; then
    echo "error: $crate must not depend on the crates listed above" >&2
    return 1
  fi
  echo "ok: $crate"
}

status=0
check lazy-domain   'gpui|gpui_platform|tokio|reqwest|sqlx|axum|keyring|tokio-tungstenite' || status=1
check lazy-protocol 'gpui|gpui_platform|tokio|reqwest|sqlx|axum|keyring|tokio-tungstenite' || status=1
check lazy-client-core 'gpui|gpui_platform|tokio|reqwest|sqlx|axum|keyring|tokio-tungstenite|async-channel' || status=1
exit $status
