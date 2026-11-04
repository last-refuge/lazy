#!/usr/bin/env bash
# A throwaway server for trying the phone apps (D49), on Linux or a Mac: its own Postgres in a
# temporary folder (never the dev or test instance), a fresh database, the web client from this
# checkout, and two people, ada and bob, with the password "correct horse battery". Ctrl-C stops
# it and deletes everything.
#
#   scripts/mobile-server.sh          # on this computer only (an emulator or simulator can reach it)
#   scripts/mobile-server.sh --lan    # also on the local network, for a real phone
#
# With --lan, anyone on your network can reach it and create an account while it runs, so use it
# only on a network you trust.
#
# Needs Rust (rustup), and Postgres's initdb and pg_ctl (pacman -S postgresql, brew install postgresql@16).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PORT="${LAZY_MOBILE_PORT:-18194}"
PG_PORT="${LAZY_MOBILE_PG_PORT:-54331}"
LISTEN="127.0.0.1"
[[ "${1:-}" == "--lan" ]] && LISTEN="0.0.0.0"

command -v cargo >/dev/null || { echo "Rust is not installed: see https://rustup.rs" >&2; exit 1; }
pg_bin=""
if command -v initdb >/dev/null; then
  pg_bin="$(dirname "$(command -v initdb)")"
elif command -v brew >/dev/null && [[ -x "$(brew --prefix postgresql@16 2>/dev/null)/bin/initdb" ]]; then
  pg_bin="$(brew --prefix postgresql@16)/bin"
else
  for dir in /usr/lib/postgresql/*/bin; do [[ -x "$dir/initdb" ]] && pg_bin="$dir"; done
fi
[[ -n "$pg_bin" ]] || { echo "Postgres is not installed (pacman -S postgresql, or brew install postgresql@16)." >&2; exit 1; }

echo "== Building the web client and the server (minutes the first time)"
command -v rustup >/dev/null && rustup target add wasm32-unknown-unknown >/dev/null 2>&1
wanted=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
bindgen="${WASM_BINDGEN:-wasm-bindgen}"
if ! command -v "$bindgen" >/dev/null || [[ "$("$bindgen" --version | awk '{print $2}')" != "$wanted" ]]; then
  echo "Installing wasm-bindgen $wanted"
  cargo install wasm-bindgen-cli --version "$wanted" --locked
  bindgen="$(dirname "$(command -v cargo)")/wasm-bindgen"
  [[ -x "$bindgen" ]] || bindgen="$HOME/.cargo/bin/wasm-bindgen"
fi
WASM_BINDGEN="$bindgen" scripts/build-web.sh --dev target/mobile-web
cargo build -p lazy-server --profile fast

work="$(mktemp -d)"
server_pid=""
cleanup() {
  [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
  "$pg_bin/pg_ctl" -D "$work/pg" -m fast -w stop >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 143' INT TERM

"$pg_bin/initdb" -D "$work/pg" -U lazy --auth=trust --encoding=UTF8 --locale=C >/dev/null
"$pg_bin/pg_ctl" -D "$work/pg" -l "$work/pg.log" -w \
  -o "-p $PG_PORT -k $work -c listen_addresses=127.0.0.1" start >/dev/null
"$pg_bin/createdb" -h 127.0.0.1 -p "$PG_PORT" -U lazy lazy_mobile

target/fast/lazy-server serve --database-url "postgres://lazy@127.0.0.1:$PG_PORT/lazy_mobile" \
  --listen "$LISTEN:$PORT" --web-dir target/mobile-web --attachments-dir "$work/attachments" \
  --open-registration true >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do curl -sf "127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.2; done
curl -sf "127.0.0.1:$PORT/healthz" >/dev/null || { tail -20 "$work/server.log" >&2; exit 1; }
for name in ada:Ada bob:Bob; do
  curl -sf -X POST "127.0.0.1:$PORT/v1/auth/register" -H 'content-type: application/json' \
    -d "{\"username\":\"${name%%:*}\",\"display_name\":\"${name##*:} Tester\",\"password\":\"correct horse battery\"}" >/dev/null
done

echo
echo "== Lazy is running. Sign in as ada or bob, password: correct horse battery"
echo "   iOS simulator, or an Android device through scripts/android.sh:  http://127.0.0.1:$PORT"
if [[ "$LISTEN" == "0.0.0.0" ]]; then
  ip="$(ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}' || true)"
  echo "   A real phone on this network:  http://${ip:-this-computer}:$PORT"
fi
echo "   Ctrl-C stops it and deletes its data."
wait "$server_pid"
