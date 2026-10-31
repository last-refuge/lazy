#!/usr/bin/env bash
# Builds the Linux release files from an already built (and stripped) lazy-desktop binary:
#   lazy-<version>-linux-x86_64.tar.gz   unpack and run ./install.sh (user or --system install)
#   lazy-chat_<version>_amd64.deb        for Debian, Ubuntu, and derivatives
# usage: packaging/linux/build-linux.sh <binary> <version> <output-dir>
set -euo pipefail

BIN="${1:?usage: build-linux.sh <binary> <version> <output-dir>}"
VERSION="${2:?version}"
OUT="${3:?output dir}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ICONS="$ROOT/branding/icons"         # the brand kit: lazy-app-<size>.png (16 to 1024)
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

licenses() {  # copies the licenses of what is bundled into $1
  mkdir -p "$1"
  for f in "$ROOT"/crates/lazy-ui/assets/fonts/*/LICENSE.txt; do
    [ -e "$f" ] && cp "$f" "$1/font-$(basename "$(dirname "$f")")-LICENSE.txt"
  done
}

# ----- tarball -----
NAME="lazy-${VERSION}-linux-x86_64"
T="$WORK/$NAME"
mkdir -p "$T/icons"
install -m 0755 "$BIN" "$T/lazy"
cp "$ROOT/packaging/linux/lazy.desktop" "$T/lazy.desktop"
cp "$ROOT/packaging/linux/install.sh" "$T/install.sh"
for size in 16 24 32 48 64 128 256 512; do
  [ -e "$ICONS/lazy-app-$size.png" ] && cp "$ICONS/lazy-app-$size.png" "$T/icons/lazy-$size.png"
done
licenses "$T/LICENSES"
cat > "$T/README.txt" <<TXT
Lazy ${VERSION}

Install for yourself:   ./install.sh
Install for everyone:   sudo ./install.sh --system
Remove again:           ./install.sh --uninstall   (add --system if you installed that way)

Lazy needs a Vulkan driver, a Secret Service keyring (for example GNOME Keyring or KWallet) to keep
your sign-in, and X11 or Wayland. Lazy's first screen asks for your server's address and an
invitation code.
TXT
tar -C "$WORK" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "wrote $OUT/$NAME.tar.gz"

# ----- .deb -----
# Built with ar and tar rather than dpkg-deb, so it works on any build machine (a .deb is an ar
# archive of debian-binary, control.tar.gz, and data.tar.gz, in that order).
DEBVER="${VERSION//-/\~}"          # 0.1.0-dogfood.1 sorts before 0.1.0 as 0.1.0~dogfood.1
D="$WORK/deb/data"
C="$WORK/deb/control"
mkdir -p "$C" "$D/usr/bin" "$D/usr/share/applications" "$D/usr/share/doc/lazy-chat"
install -m 0755 "$BIN" "$D/usr/bin/lazy"
sed 's|^Exec=.*|Exec=lazy|' "$ROOT/packaging/linux/lazy.desktop" > "$D/usr/share/applications/lazy.desktop"
chmod 0644 "$D/usr/share/applications/lazy.desktop"
for size in 16 24 32 48 64 128 256 512; do
  [ -e "$ICONS/lazy-app-$size.png" ] || continue
  mkdir -p "$D/usr/share/icons/hicolor/${size}x${size}/apps"
  install -m 0644 "$ICONS/lazy-app-$size.png" "$D/usr/share/icons/hicolor/${size}x${size}/apps/lazy.png"
done
licenses "$D/usr/share/doc/lazy-chat/licenses"
cat > "$D/usr/share/doc/lazy-chat/copyright" <<TXT
Lazy is (c) its authors; see https://github.com/last-refuge/lazy. This package also contains fonts
under their own licenses, in /usr/share/doc/lazy-chat/licenses.
TXT
find "$D" -type d -exec chmod 0755 {} +
find "$D" -type f ! -perm -u+x -exec chmod 0644 {} +
SIZE_KB="$(du -sk "$D" | cut -f1)"
cat > "$C/control" <<TXT
Package: lazy-chat
Version: ${DEBVER}
Section: net
Priority: optional
Architecture: amd64
Installed-Size: ${SIZE_KB}
Maintainer: Lazy <noreply@users.noreply.github.com>
Homepage: https://github.com/last-refuge/lazy
Depends: libc6 (>= 2.35), libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1, libvulkan1, libegl1
Recommends: mesa-vulkan-drivers | nvidia-vulkan-icd, gnome-keyring | kwalletmanager | keepassxc
Description: A native place for channels, messages and the people you work with
 Lazy is a desktop chat client for a self-hosted Lazy server: channels, direct messages,
 threads, search, files, and notifications. Its first screen asks for your server's address
 and an invitation code.
TXT
( cd "$D" && find . -type f -printf '%P\0' | sort -z | xargs -0 md5sum > "$C/md5sums" )
tar --owner=0 --group=0 --numeric-owner --format=gnu -C "$D" -czf "$WORK/deb/data.tar.gz" .
tar --owner=0 --group=0 --numeric-owner --format=gnu -C "$C" -czf "$WORK/deb/control.tar.gz" .
printf '2.0\n' > "$WORK/deb/debian-binary"
DEB="$OUT/lazy-chat_${DEBVER}_amd64.deb"
rm -f "$DEB"
( cd "$WORK/deb" && ar rcD "$DEB" debian-binary control.tar.gz data.tar.gz )
echo "wrote $DEB"
