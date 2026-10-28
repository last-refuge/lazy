#!/usr/bin/env bash
# Project-local Postgres for development and tests. Data lives under .dev/ and the server
# listens only on 127.0.0.1, so the system Postgres instance is never touched.
#
# There are two independent instances so that running the tests can never disturb a server you
# are running by hand:
#   dev  (default)  .dev/pgdata       127.0.0.1:54329   for `scripts/run-server.sh`
#   test            .dev/pgdata-test  127.0.0.1:54330   for `scripts/test.sh` and automated runs
# Choose one with LAZY_PG_INSTANCE=test. Never stop or reset the dev instance to run tests.
#
#   scripts/dev-db.sh start    initialize (first run) and start; creates the `lazy` database
#   scripts/dev-db.sh stop     stop the server
#   scripts/dev-db.sh status   report whether it is running
#   scripts/dev-db.sh reset    stop and delete all data for the chosen instance
#   scripts/dev-db.sh url      print the connection URL
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTANCE="${LAZY_PG_INSTANCE:-dev}"
case "$INSTANCE" in
  dev)  SUFFIX="";        DEFAULT_PORT=54329 ;;
  test) SUFFIX="-test";   DEFAULT_PORT=54330 ;;
  *)    SUFFIX="-$INSTANCE"; DEFAULT_PORT="" ;;
esac
PORT="${LAZY_PG_PORT:-$DEFAULT_PORT}"
if [ -z "$PORT" ]; then
  echo "set LAZY_PG_PORT for the custom instance '$INSTANCE'" >&2
  exit 2
fi
DATA="$ROOT/.dev/pgdata$SUFFIX"
SOCK="$ROOT/.dev/pgsock$SUFFIX"
LOG="$ROOT/.dev/postgres$SUFFIX.log"
URL="postgres://lazy@127.0.0.1:${PORT}/lazy"

running() { pg_ctl -D "$DATA" status >/dev/null 2>&1; }

case "${1:-}" in
  start)
    mkdir -p "$ROOT/.dev" "$SOCK"
    if [ ! -f "$DATA/PG_VERSION" ]; then
      initdb -D "$DATA" -U lazy --auth=trust --encoding=UTF8 --locale=C.UTF-8 >/dev/null
    fi
    if ! running; then
      pg_ctl -D "$DATA" -l "$LOG" -w \
        -o "-p $PORT -k $SOCK -c listen_addresses=127.0.0.1 -c fsync=on -c max_connections=300" start >/dev/null
    fi
    if ! psql "$URL" -Atc 'select 1' >/dev/null 2>&1; then
      createdb -h 127.0.0.1 -p "$PORT" -U lazy lazy
    fi
    echo "$URL"
    ;;
  stop)
    if running; then pg_ctl -D "$DATA" -m fast -w stop >/dev/null; fi
    ;;
  status)
    if running; then echo "running: $URL"; else echo "stopped"; exit 1; fi
    ;;
  reset)
    if running; then pg_ctl -D "$DATA" -m immediate -w stop >/dev/null; fi
    rm -rf "$DATA" "$SOCK"
    ;;
  url)
    echo "$URL"
    ;;
  *)
    echo "usage: $0 {start|stop|status|reset|url}" >&2
    exit 2
    ;;
esac
