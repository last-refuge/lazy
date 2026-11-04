# Lazy's phone apps

The iOS and Android apps are the server's web client in a thin native shell (D49 in
`llm-docs/DECISIONS.md`). A deploy of the web client reaches them with no store release.

**Keep phone behavior in `crates/lazy-web`, never here.** The shells hold only what a page cannot do:

- choosing a server, and the screen for when it cannot be reached;
- file downloads and, on Android, the file chooser and the back button;
- on iOS, JavaScript's `alert`, `confirm`, and `prompt`, which do nothing in a web view unless the
  app shows them.

## How a page and a shell talk

There is no native bridge, so a page gets no native calls.

| Direction | How | Used for |
| --- | --- | --- |
| Shell to page | `LazyShell/1` in the user agent | The page knows it is in the app, and which version |
| Page to shell | Going to a `lazy-shell://` address | `lazy-shell://change-server` |
| Shell to page | Running `window.lazyBack()` | Android's back button: closes the topmost thing open, and answers whether there was one |

A new ability in a shell only reaches people through a store release, so a page must check the
version in the user agent before relying on one.

## Android

On Linux, `scripts/android.sh` installs the JDK, SDK, and emulator it needs under `~/.local/lazy-tools`.
It uses a plugged-in phone (USB debugging on) or an emulator, which needs hardware acceleration.

```sh
scripts/android.sh test             # the whole test, on its own test server
scripts/mobile-server.sh            # or a test server to try the app by hand, in one terminal...
scripts/android.sh try http://127.0.0.1:18194    # ...and the app, in another
```

Debug builds also accept plain HTTP and a starting server, as above. The script makes the
computer's port reachable on the device, so `127.0.0.1` means the computer.

## iOS

Needs a Mac with Xcode and [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install xcodegen`).
The Xcode project is generated from `ios/project.yml` and is not checked in.

```sh
scripts/ios.sh test             # the whole test on a simulator, on its own test server
scripts/ios.sh open             # or generate the project and open it in Xcode
scripts/mobile-server.sh --lan  # a test server a real iPhone can reach
```

To run on a device, choose your team under Signing & Capabilities. Debug builds accept plain HTTP.

`ios/LazyUITests` drives the app with iOS's real keyboard against a real server.

## On GitHub

`gh workflow run mobile` builds the Android APK, and builds and tests the iOS app on a simulator
with a real server, saving screenshots. It runs only when someone starts it, and signs nothing.

## Not built yet

- Push notifications: nothing reaches a phone while the app is closed (D49 records the relay question).
- Release signing and store listings. These need an Apple Developer account and a Google Play account.
- Sharing into Lazy from other apps, and deep links.
- Taking a picture directly on Android (the file chooser picks existing files).
- Drop-ins.
