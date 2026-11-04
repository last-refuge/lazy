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

Needs JDK 17 or later and the Android SDK (`ANDROID_HOME`), API 36.

```sh
cd mobile/android
./gradlew assembleDebug    # app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Debug builds also accept plain HTTP and a starting server, for testing against a machine's own
server (from an emulator, that machine is `10.0.2.2`):

```sh
adb shell am start -n io.github.last_refuge.lazy/.MainActivity --es server http://10.0.2.2:8080
```

## iOS

Needs a Mac with Xcode and [XcodeGen](https://github.com/yonaskolb/XcodeGen). The Xcode project is
generated from `ios/project.yml` and is not checked in.

```sh
cd mobile/ios
brew install xcodegen && xcodegen generate
open Lazy.xcodeproj
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
