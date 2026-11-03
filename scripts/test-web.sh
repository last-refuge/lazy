#!/usr/bin/env bash
# Runs the web client's browser test (crates/lazy-web/tests/browser/flow.mjs): two people in
# headless Chromium against a real server on a fresh database in the *test* Postgres (:54330).
#
#   scripts/test-web.sh
#
# Needs: the built web client (scripts/build-web.sh), a built server (cargo build -p lazy-server
# --profile fast), and Playwright with Chromium in LAZY_PLAYWRIGHT_DIR (default
# ~/.local/lazy-tools/pw): `npm install playwright && npx playwright install chromium` there.
# Chromium's system libraries and fonts must be present; set LD_LIBRARY_PATH and FONTCONFIG_FILE
# if they are installed somewhere private. Screenshots go to $LAZY_WEB_SHOTS (/tmp/lazy-web-shots).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PW="${LAZY_PLAYWRIGHT_DIR:-$HOME/.local/lazy-tools/pw}"
PORT="${LAZY_WEB_TEST_PORT:-18181}"
SERVER="${LAZY_SERVER_BIN:-target/fast/lazy-server}"
[[ -f target/web/index.html ]] || { echo "build the web client first: scripts/build-web.sh" >&2; exit 1; }
[[ -x "$SERVER" ]] || { echo "build the server first: cargo build -p lazy-server --profile fast" >&2; exit 1; }
[[ -d "$PW/node_modules/playwright" ]] || { echo "Playwright is not in $PW (see the top of this script)" >&2; exit 1; }

# A fresh database on the test instance only.
base="$(LAZY_PG_INSTANCE=test scripts/dev-db.sh start)"
db="lazy_web_e2e"
psql "$base" -Atqc "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1
psql "$base" -Atqc "CREATE DATABASE $db"
work="$(mktemp -d)"
trap 'kill "${server_pid:-0}" 2>/dev/null || true; rm -rf "$work"' EXIT

"$SERVER" serve --database-url "${base%/*}/$db" --listen "127.0.0.1:$PORT" --web-dir target/web \
  --attachments-dir "$work/attachments" --open-registration true >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do curl -sf "127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.1; done
for user in ada bob; do
  curl -sf -X POST "127.0.0.1:$PORT/v1/auth/register" -H 'content-type: application/json' \
    -d "{\"username\":\"$user\",\"display_name\":\"${user^} Tester\",\"password\":\"correct horse battery\"}" >/dev/null
done

# The files the test attaches: a small picture and a text file.
python3 - "$work" <<'PY'
import struct, sys, zlib
w, h = 64, 40
raw = b"".join(b"\x00" + b"\xc0\x57\x3e" * w for _ in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(f"{sys.argv[1]}/red.png", "wb").write(png)
open(f"{sys.argv[1]}/notes.txt", "w").write("some notes\n")
PY

cp crates/lazy-web/tests/browser/flow.mjs "$PW/lazy-web-flow.mjs"
LAZY_WEB_URL="http://127.0.0.1:$PORT" LAZY_WEB_FILES="$work" node "$PW/lazy-web-flow.mjs"
