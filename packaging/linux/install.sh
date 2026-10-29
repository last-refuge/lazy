#!/usr/bin/env sh
# Installs Lazy from the unpacked tarball. By default for the current user (~/.local); with
# --system (as root) under /usr/local; with --prefix DIR anywhere. --uninstall removes it again.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${HOME}/.local"
ACTION=install
while [ $# -gt 0 ]; do
  case "$1" in
    --system) PREFIX=/usr/local ;;
    --prefix) shift; PREFIX="${1:?--prefix needs a directory}" ;;
    --uninstall) ACTION=uninstall ;;
    -h|--help)
      echo "usage: $0 [--system | --prefix DIR] [--uninstall]"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

BIN="$PREFIX/bin/lazy"
APPS="$PREFIX/share/applications"
ICONS="$PREFIX/share/icons/hicolor"

refresh() {
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" >/dev/null 2>&1 || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$ICONS" >/dev/null 2>&1 || true
}

if [ "$ACTION" = uninstall ]; then
  rm -f "$BIN" "$APPS/lazy.desktop"
  for size in 16 24 32 48 64 128 256 512; do rm -f "$ICONS/${size}x${size}/apps/lazy.png"; done
  refresh
  echo "Lazy removed from $PREFIX (your profile and logs are untouched)."
  exit 0
fi

mkdir -p "$PREFIX/bin" "$APPS"
install -m 0755 "$HERE/lazy" "$BIN"
sed "s|^Exec=.*|Exec=$BIN|" "$HERE/lazy.desktop" > "$APPS/lazy.desktop"
chmod 0644 "$APPS/lazy.desktop"
for png in "$HERE"/icons/lazy-*.png; do
  [ -e "$png" ] || continue
  size="${png##*/lazy-}"; size="${size%.png}"
  mkdir -p "$ICONS/${size}x${size}/apps"
  install -m 0644 "$png" "$ICONS/${size}x${size}/apps/lazy.png"
done
refresh
echo "Lazy installed to $PREFIX. Start it from your application menu, or run: $BIN"
case ":$PATH:" in *":$PREFIX/bin:"*) ;; *) echo "Note: $PREFIX/bin is not on your PATH." ;; esac
