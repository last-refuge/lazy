#!/usr/bin/env bash
# Builds the web client (crates/lazy-web, D47) into a directory the server serves under /web/:
#
#   scripts/build-web.sh                 # release build into target/web
#   scripts/build-web.sh --dev out/web   # quick, unoptimized build into out/web
#
# Then run the server with LAZY_WEB_DIR pointing at the directory (scripts/run-server.sh does
# this when target/web exists).
#
# Needs the wasm32-unknown-unknown target (`rustup target add wasm32-unknown-unknown`) and the
# wasm-bindgen CLI at exactly the version in Cargo.lock (`cargo install wasm-bindgen-cli
# --version <it>`, or a release binary from github.com/wasm-bindgen/wasm-bindgen; set
# WASM_BINDGEN to its path if it is not on PATH).
set -euo pipefail
cd "$(dirname "$0")/.."

profile=web
if [[ "${1:-}" == "--dev" ]]; then
  profile=dev
  shift
fi
out="${1:-target/web}"

bindgen="${WASM_BINDGEN:-wasm-bindgen}"
if ! command -v "$bindgen" >/dev/null 2>&1; then
  echo "wasm-bindgen was not found; install it or set WASM_BINDGEN (see the top of this script)." >&2
  exit 1
fi
wanted=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
have=$("$bindgen" --version | awk '{ print $2 }')
if [[ "$wanted" != "$have" ]]; then
  echo "wasm-bindgen is $have but Cargo.lock has $wanted: install that version." >&2
  exit 1
fi

cargo build -p lazy-web --target wasm32-unknown-unknown --profile "$profile"
dir=$profile
[[ "$profile" == dev ]] && dir=debug

rm -rf "$out"
mkdir -p "$out/pkg" "$out/fonts"
"$bindgen" --target web --no-typescript --out-dir "$out/pkg" \
  "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/$dir/lazy_web.wasm"
cp crates/lazy-web/static/* "$out/"
# The icon and the fonts are the desktop app's (the Docker build copies them elsewhere).
cp "${LAZY_WEB_FAVICON:-branding/icons/favicon.svg}" "$out/favicon.svg"
fonts="${LAZY_WEB_FONTS:-crates/lazy-ui/assets/fonts}"
cp "$fonts"/dm-sans/DMSans-{Regular,Italic,SemiBold,SemiBoldItalic}.ttf "$fonts"/nunito/Nunito-Bold.ttf \
  "$fonts"/lilex/Lilex-Regular.ttf "$out/fonts/"
# The fonts' licences travel with them.
for family in dm-sans nunito lilex; do
  for licence in "$fonts/$family"/{OFL.txt,LICENSE.txt,LICENSE}; do
    [[ -f "$licence" ]] && cp "$licence" "$out/fonts/$family-$(basename "$licence")"
  done
done
# Compressed copies, which the server sends to browsers that accept them.
for file in "$out"/pkg/*.wasm "$out"/pkg/*.js "$out"/*.js "$out"/*.css "$out"/*.html "$out"/fonts/*.ttf; do
  gzip -9 -k -n "$file"
done
echo "web client built in $out ($(du -sh "$out/pkg/lazy_web_bg.wasm" | cut -f1) of WebAssembly, $(du -sh "$out/pkg/lazy_web_bg.wasm.gz" | cut -f1) compressed)"
