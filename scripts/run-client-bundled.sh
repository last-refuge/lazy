#!/usr/bin/env bash
# Run a desktop client as a real app bundle with its own disposable profile (macOS only).
#   scripts/run-client-bundled.sh [profile-name]      (default profile: alice)
#
# Desktop notifications need an installed app: the system's notification centre refuses (and the
# client turns itself off) for a bare `cargo run` binary. This builds .dev/bundles/<name>/Lazy.app
# from the release binary, signs it ad hoc with the bundle id as its identifier, and starts it.
# Each profile gets its own bundle id, so each has its own notification permission (and its own
# entry in System Settings, named "Lazy"). Delete .dev/bundles/<name> and the profile to start over.
set -euo pipefail
[ "$(uname -s)" = Darwin ] || { echo "macOS only" >&2; exit 1; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME="${1:-alice}"
BUNDLE_ID="io.github.last-refuge.lazy.dev.${NAME}"
APP="$ROOT/.dev/bundles/$NAME/Lazy.app"
export LAZY_PROFILE_DIR="$ROOT/.dev/profiles/$NAME"
export LAZY_SERVER_URL="${LAZY_SERVER_URL:-http://127.0.0.1:8080}"
export RUST_LOG="${RUST_LOG:-lazy=info}"
mkdir -p "$LAZY_PROFILE_DIR"
cd "$ROOT"
cargo build -p lazy-desktop --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/lazy-desktop "$APP/Contents/MacOS/lazy"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
sed -e "s|@BUNDLE_ID@|$BUNDLE_ID|" -e "s|@SHORT_VERSION@|${VERSION:-0.0.0}|" \
  packaging/macos/Info.plist.in > "$APP/Contents/Info.plist"
if [ -e branding/icons/lazy-app.icns ]; then
  cp branding/icons/lazy-app.icns "$APP/Contents/Resources/Lazy.icns"
else
  /usr/libexec/PlistBuddy -c "Delete :CFBundleIconFile" "$APP/Contents/Info.plist"
fi
codesign --force --deep --sign - --identifier "$BUNDLE_ID" "$APP"
codesign --verify --deep --strict "$APP"

# `open` starts the app through LaunchServices, which is what gives it a bundle for the
# notification centre; --env passes the profile. -W waits, so Ctrl-C here ends the wait only:
# stop the app by quitting it (Cmd-Q).
exec open -n -W "$APP" \
  --env "LAZY_PROFILE_DIR=$LAZY_PROFILE_DIR" \
  --env "LAZY_SERVER_URL=$LAZY_SERVER_URL" \
  --env "RUST_LOG=$RUST_LOG"
