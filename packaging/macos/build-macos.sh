#!/usr/bin/env bash
# Builds Lazy.app and a disk image from the two release binaries (run on macOS):
#   Lazy-<version>-macos-universal.dmg
# The app is signed ad hoc, which lets it run once the person approves it (right-click, Open, the
# first time). With APPLE_SIGN_IDENTITY set (a "Developer ID Application: ..." identity already in
# the keychain) it is signed for real, and with APPLE_ID, APPLE_TEAM_ID, and APPLE_APP_PASSWORD
# also set the image is notarized and stapled, so Gatekeeper opens it without asking.
# usage: packaging/macos/build-macos.sh <arm64-binary> <x86_64-binary> <version> <output-dir>
set -euo pipefail

ARM="${1:?usage: build-macos.sh <arm64-binary> <x86_64-binary> <version> <output-dir>}"
INTEL="${2:?x86_64 binary}"
VERSION="${3:?version}"
OUT="${4:?output dir}"
BUNDLE_ID="${LAZY_BUNDLE_ID:-io.github.last-refuge.lazy}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

SHORT_VERSION="${VERSION%%-*}"          # 0.1.0-dogfood.1 -> 0.1.0 (the bundle wants digits and dots)
APP="$WORK/Lazy.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/lazy" "$ARM" "$INTEL"
chmod 0755 "$APP/Contents/MacOS/lazy"
sed -e "s|@BUNDLE_ID@|$BUNDLE_ID|" -e "s|@SHORT_VERSION@|$SHORT_VERSION|" \
  "$ROOT/packaging/macos/Info.plist.in" > "$APP/Contents/Info.plist"
if [ -e "$ROOT/branding/icons/lazy-app.icns" ]; then
  cp "$ROOT/branding/icons/lazy-app.icns" "$APP/Contents/Resources/Lazy.icns"
else
  # No icon yet: the bundle must not name one that is not there.
  /usr/libexec/PlistBuddy -c "Delete :CFBundleIconFile" "$APP/Contents/Info.plist"
fi
mkdir -p "$APP/Contents/Resources/licenses"
for f in "$ROOT"/crates/lazy-ui/assets/fonts/*/LICENSE.txt; do
  [ -e "$f" ] && cp "$f" "$APP/Contents/Resources/licenses/font-$(basename "$(dirname "$f")")-LICENSE.txt"
done

IDENTITY="${APPLE_SIGN_IDENTITY:--}"
if [ "$IDENTITY" = "-" ]; then
  codesign --force --deep --sign - "$APP"
else
  codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" "$APP"
fi
codesign --verify --deep --strict "$APP"

DMG_DIR="$WORK/dmg"
mkdir -p "$DMG_DIR"
cp -R "$APP" "$DMG_DIR/Lazy.app"
ln -s /Applications "$DMG_DIR/Applications"
DMG="$OUT/Lazy-${VERSION}-macos-universal.dmg"
rm -f "$DMG"
hdiutil create -volname "Lazy" -srcfolder "$DMG_DIR" -ov -format UDZO "$DMG" >/dev/null

if [ "$IDENTITY" != "-" ]; then
  codesign --force --sign "$IDENTITY" --timestamp "$DMG"
  if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
    xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
      --password "$APPLE_APP_PASSWORD" --wait
    xcrun stapler staple "$DMG"
  fi
fi
echo "wrote $DMG"
