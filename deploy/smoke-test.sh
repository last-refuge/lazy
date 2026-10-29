#!/usr/bin/env bash
# Checks a running Lazy server from the outside: it is healthy, the first account needs the setup
# code, the person who has it becomes the owner, and everyone after needs an invitation.
# With "--after-restart" it only checks that the owner made by the first run is still there with the
# same password (the data survived a restart).
# usage: deploy/smoke-test.sh <base-url> <setup-code> [--after-restart]        (needs curl and jq)
set -euo pipefail

BASE="${1:?usage: smoke-test.sh <base-url> <setup-code>}"
CODE="${2:?setup code}"
MODE="${3:-}"
PASSWORD="smoke test password 12345"
fail() { echo "FAIL: $*" >&2; exit 1; }
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
register() {  # username [code]
  local body
  body="$(jq -nc --arg u "$1" --arg p "$PASSWORD" --arg c "${2:-}" \
    '{username:$u, display_name:$u, password:$p} + (if $c == "" then {} else {invite_code:$c} end)')"
  curl -s -w '\n%{http_code}' -X POST "$BASE/v1/auth/register" -H 'content-type: application/json' -d "$body"
}

login_owner() {
  local out
  out="$(curl -s -w '\n%{http_code}' -X POST "$BASE/v1/auth/login" -H 'content-type: application/json' \
    -d "$(jq -nc --arg u smokeowner --arg p "$PASSWORD" '{username:$u,password:$p}')")"
  [ "${out##*$'\n'}" = 200 ] || fail "login failed with ${out##*$'\n'}"
  echo "${out%$'\n'*}" | jq -er '.token'
}

echo "health"
health="$(curl -fsS "$BASE/healthz")" || fail "/healthz did not answer 200"
echo "$health" | jq -e '.protocol_version' >/dev/null || fail "/healthz has no protocol_version: $health"

if [ "$MODE" = "--after-restart" ]; then
  echo "the owner is still there"
  token="$(login_owner)"
  role="$(curl -fsS "$BASE/v1/bootstrap" -H "authorization: Bearer $token" | jq -er '.workspaces[0].role')"
  [ "$role" = owner ] || fail "after the restart the owner has role $role"
  echo "OK (after restart)"
  exit 0
fi

echo "the first account needs the setup code"
out="$(register smokeowner)"; [ "${out##*$'\n'}" = 403 ] || fail "no code: expected 403, got ${out##*$'\n'}"
out="$(register smokeowner "wrong-code-wrong-code-wrong")"; [ "${out##*$'\n'}" = 403 ] || fail "wrong code: expected 403, got ${out##*$'\n'}"

echo "the right code creates the owner"
out="$(register smokeowner "$CODE")"; code="${out##*$'\n'}"; body="${out%$'\n'*}"
[ "$code" = 200 ] || fail "right code: expected 200, got $code: $body"
token="$(echo "$body" | jq -er '.token')"
role="$(curl -fsS "$BASE/v1/bootstrap" -H "authorization: Bearer $token" | jq -er '.workspaces[0].role')"
[ "$role" = owner ] || fail "the first account should be the owner, got $role"

echo "the next account needs an invitation, and the setup code is not one"
out="$(register smokeguest)"; [ "${out##*$'\n'}" = 403 ] || fail "second account without a code: expected 403, got ${out##*$'\n'}"
out="$(register smokeguest "$CODE")"; c="${out##*$'\n'}"
[ "$c" = 400 ] || [ "$c" = 403 ] || fail "the setup code must not work as an invitation, got $c"

echo "signing in again works"
login_owner >/dev/null

echo "a WebSocket upgrade is accepted at /v1/realtime"
# Without a valid handshake the server must answer a client error, not hang or 5xx.
s="$(status -H 'Connection: Upgrade' -H 'Upgrade: websocket' "$BASE/v1/realtime")"
case "$s" in 4??) ;; *) fail "/v1/realtime without credentials: expected a 4xx, got $s" ;; esac

echo "OK"
