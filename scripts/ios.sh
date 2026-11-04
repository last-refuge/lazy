#!/usr/bin/env bash
# The iOS app (mobile/ios, D49) on a Mac.
#
#   scripts/ios.sh open   # generates the Xcode project and opens it
#   scripts/ios.sh test   # the whole test on a simulator: its own test server, then mobile/ios/LazyUITests
#
# Needs Xcode with an iOS simulator, and XcodeGen (brew install xcodegen). `test` also needs what
# scripts/mobile-server.sh needs. Screenshots go to target/ios-shots.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PORT="${LAZY_MOBILE_PORT:-18194}"
server_pid=""
stop() { if [[ -n "$server_pid" ]]; then kill "$server_pid" 2>/dev/null || true; fi; }
trap stop EXIT
trap 'exit 143' INT TERM

[[ "$(uname)" == Darwin ]] || { echo "This needs a Mac." >&2; exit 1; }
xcodebuild -version >/dev/null 2>&1 || { echo "Xcode is not ready: install it from the App Store, open it once, then run: sudo xcode-select -s /Applications/Xcode.app" >&2; exit 1; }
command -v xcodegen >/dev/null || { echo "XcodeGen is not installed: brew install xcodegen" >&2; exit 1; }
(cd mobile/ios && xcodegen generate --quiet)

case "${1:-}" in
  open)
    open mobile/ios/Lazy.xcodeproj
    ;;
  test)
    device="$(xcrun simctl list devices available -j | python3 -c '
import json, sys
found = [d["name"] for runtime, ds in json.load(sys.stdin)["devices"].items() if "iOS" in runtime for d in ds if d["name"].startswith("iPhone")]
print(found[-1] if found else "")')"
    [[ -n "$device" ]] || { echo "No iPhone simulator is installed. Add one in Xcode: Settings, Components (or run: xcodebuild -downloadPlatform iOS)." >&2; exit 1; }
    scripts/mobile-server.sh &
    server_pid=$!
    waited=0
    until curl -sf "127.0.0.1:$PORT/healthz" >/dev/null; do
      kill -0 "$server_pid" 2>/dev/null || { echo "The test server did not start; see above." >&2; exit 1; }
      waited=$((waited + 2))
      ((waited < 1800)) || { echo "The test server did not start in 30 minutes." >&2; exit 1; }
      sleep 2
    done
    sleep 2
    shots="$PWD/target/ios-shots"
    rm -rf "$shots"
    mkdir -p "$shots"
    echo "== Testing on: $device"
    (cd mobile/ios && TEST_RUNNER_LAZY_SERVER="http://127.0.0.1:$PORT" TEST_RUNNER_LAZY_SHOTS="$shots" \
      xcodebuild test -project Lazy.xcodeproj -scheme Lazy \
      -destination "platform=iOS Simulator,name=$device" CODE_SIGNING_ALLOWED=NO)
    echo "Screenshots: $shots"
    ;;
  *)
    sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
