#!/usr/bin/env bash
# The Android app (mobile/android, D49) on Linux: installs what it needs, then builds and runs it on
# a plugged-in phone (USB debugging on) or, if there is none, an emulator.
#
#   scripts/android.sh setup          # JDK, Android SDK, and emulator under ~/.local/lazy-tools; accepts Google's SDK licenses
#   scripts/android.sh build          # the debug APK
#   scripts/android.sh try [address]  # installs and opens it, with a window; a test server: scripts/mobile-server.sh
#   scripts/android.sh test           # the whole test: its own test server, then mobile/android/test/e2e.mjs
#
# `try` and `test` run the steps before them. An emulator needs hardware acceleration, because a web
# view crashes without it (VERIFICATION.md); LAZY_ANDROID_NO_KVM=1 skips the check, to test this script.
# Needs curl, python3, and, for `test`, Node 22 or later plus what scripts/mobile-server.sh needs.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
TOOLS="$HOME/.local/lazy-tools"
IMAGE="${LAZY_ANDROID_IMAGE:-system-images;android-34;google_apis;x86_64}"
AVD="${LAZY_ANDROID_AVD:-lazy-android}"
PORT="${LAZY_MOBILE_PORT:-18194}"
APP=io.github.last_refuge.lazy
APK=mobile/android/app/build/outputs/apk/debug/app-debug.apk
export ANDROID_HOME="${ANDROID_HOME:-$TOOLS/android-sdk}"
export ANDROID_AVD_HOME="$TOOLS/avd"
[[ -x "$TOOLS/jdk/bin/java" ]] && export JAVA_HOME="${JAVA_HOME:-$TOOLS/jdk}"
[[ -n "${JAVA_HOME:-}" ]] && export PATH="$JAVA_HOME/bin:$PATH"
export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"
emulator_pid=""
server_pid=""
stop() {
  [[ -n "$emulator_pid" ]] && kill "$emulator_pid" 2>/dev/null || true
  [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
}
trap stop EXIT
trap 'exit 143' INT TERM

setup() {
  mkdir -p "$TOOLS"
  if ! java -version 2>&1 | grep -qE 'version "(1[7-9]|[2-9][0-9])'; then
    echo "== Installing a JDK"
    rm -rf "$TOOLS/jdk"
    mkdir -p "$TOOLS/jdk"
    curl -fsSL "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jdk/hotspot/normal/eclipse" |
      tar -xz -C "$TOOLS/jdk" --strip-components=1
    export JAVA_HOME="$TOOLS/jdk" PATH="$TOOLS/jdk/bin:$PATH"
  fi
  if ! command -v sdkmanager >/dev/null; then
    echo "== Installing the Android command-line tools"
    local download
    download="$(mktemp -d)"
    curl -fsSL -o "$download/tools.zip" "https://dl.google.com/android/repository/commandlinetools-linux-13114758_latest.zip"
    python3 -c "import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$download/tools.zip" "$download"
    mkdir -p "$ANDROID_HOME/cmdline-tools"
    rm -rf "$ANDROID_HOME/cmdline-tools/latest"
    mv "$download/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest"
    chmod -R u+x "$ANDROID_HOME/cmdline-tools/latest/bin"
    rm -rf "$download"
  fi
  echo "== Installing the SDK and the emulator (some GB the first time); accepting Google's SDK licenses"
  (yes || true) | sdkmanager --licenses >/dev/null 2>&1 || true
  sdkmanager --install "platform-tools" "platforms;android-36" "build-tools;36.0.0" "emulator" "$IMAGE" 2>&1 | tail -1
}

build() {
  echo "== Building the app"
  (cd mobile/android && ./gradlew assembleDebug --no-daemon -q)
  echo "Built $APK"
}

# The attached device, if any is ready.
device() { adb devices | awk 'NR > 1 && $2 == "device" { print $1; exit }'; }

start_emulator() { # with a window ("window") or without
  if [[ -z "${LAZY_ANDROID_NO_KVM:-}" ]] && ! [[ -r /dev/kvm && -w /dev/kvm ]]; then
    echo "The emulator needs hardware acceleration, and this account cannot use /dev/kvm." >&2
    echo "Fix it with: sudo usermod -aG kvm \"\$USER\", then log in again. Or plug in a phone with USB debugging on." >&2
    exit 1
  fi
  # Without this folder, avdmanager succeeds but puts the device where the emulator will not look.
  mkdir -p "$ANDROID_AVD_HOME"
  avdmanager list avd 2>/dev/null | grep -q "Name: $AVD\$" ||
    echo no | avdmanager create avd -n "$AVD" -k "$IMAGE" -d pixel_6 --force >/dev/null
  local flags=(-avd "$AVD" -no-audio -no-boot-anim -no-snapshot)
  if [[ "$1" == window ]]; then
    # The emulator's window toolkit needs X11; on Wayland (Omarchy) that is XWayland.
    [[ -n "${WAYLAND_DISPLAY:-}" ]] && export QT_QPA_PLATFORM=xcb
  else
    flags+=(-no-window -gpu swiftshader_indirect)
  fi
  [[ -n "${LAZY_ANDROID_NO_KVM:-}" ]] && flags+=(-accel off)
  echo "== Starting the emulator (a few minutes the first time)"
  emulator "${flags[@]}" >"${TMPDIR:-/tmp}/lazy-android-emulator.log" 2>&1 &
  emulator_pid=$!
  local waited=0
  until [[ "$(timeout 15 adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == 1 ]]; do
    if ! kill -0 "$emulator_pid" 2>/dev/null; then
      tail -20 "${TMPDIR:-/tmp}/lazy-android-emulator.log" >&2
      echo "The emulator stopped; its log is above." >&2
      exit 1
    fi
    waited=$((waited + 5))
    if ((waited > 1200)); then
      echo "The emulator did not finish starting in 20 minutes." >&2
      exit 1
    fi
    sleep 5
  done
  for setting in window_animation_scale transition_animation_scale animator_duration_scale; do
    adb shell settings put global "$setting" 0
  done
}

case "${1:-}" in
  setup) setup ;;
  build)
    setup
    build
    ;;
  try)
    setup
    build
    [[ -n "$(device)" ]] || start_emulator window
    adb install -r "$APK"
    adb reverse "tcp:$PORT" "tcp:$PORT"
    extra=()
    [[ -n "${2:-}" ]] && extra=(--es server "$2")
    adb shell am start -n "$APP/.MainActivity" "${extra[@]}" >/dev/null
    echo "The app is open. A test server from scripts/mobile-server.sh is at http://127.0.0.1:$PORT on the device."
    if [[ -n "$emulator_pid" ]]; then
      echo "Close the emulator's window, or press Ctrl-C, when you are done."
      wait "$emulator_pid"
    fi
    ;;
  test)
    command -v node >/dev/null || { echo "The test needs Node 22 or later." >&2; exit 1; }
    setup
    build
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
    [[ -n "$(device)" ]] || start_emulator headless
    adb install -r "$APK"
    adb reverse "tcp:$PORT" "tcp:$PORT"
    LAZY_SERVER="http://127.0.0.1:$PORT" LAZY_SHOTS="${LAZY_SHOTS:-$PWD/target/android-shots}" node mobile/android/test/e2e.mjs
    echo "Screenshots: ${LAZY_SHOTS:-$PWD/target/android-shots}"
    ;;
  *)
    sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
