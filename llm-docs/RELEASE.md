# Releasing the Lazy client

The desktop client ships as installers for Linux, macOS, and Windows, built by a workflow you start
by hand (like CI, it never runs on a push): **Actions, release, Run workflow**, or

```sh
gh workflow run release -f version=0.5.0 -f publish=true
```

`version` is stamped into the apps (Help, Diagnostics shows it) and into the file names;
`publish=true` also creates a **draft pre-release** on GitHub with every file and a `SHA256SUMS`, for
you to read and publish. Without it the files are only the workflow run's artifacts. The server has
its own workflow, `server` (`DEPLOY.md`).

**Status.** Release 0.5.0 (2026-10-29) was built by this workflow on GitHub for all three platforms,
and it took three runs. Checked: the Linux `.deb` installs on a clean Ubuntu 22.04 inside the job and
every library resolves (`ldd`); the downloaded files were inspected (the `.deb`'s control file and
contents, the Windows zip and the installer's executable type, the disk image's partition table) and
their checksums match `SHA256SUMS`. The first macOS run built both architectures but failed making
the disk image (`hdiutil create -srcfolder`: "No space left on device" with 43 GB free), so the image
is now made at an explicit size (an empty image, mounted, filled, compressed). **Never checked:**
the macOS app and the Windows installer on a real machine (that they start, show the icon, and what
Gatekeeper and SmartScreen say), and the Linux package outside CI. Treat the first install on each
system as their test and fix this page.

## What you get

| Platform | Files | Install |
| --- | --- | --- |
| Linux x86_64 (built on Ubuntu 22.04, glibc 2.35) | `lazy-chat_<version>_amd64.deb`, `lazy-<version>-linux-x86_64.tar.gz` | `.deb`: `sudo apt install ./lazy-chat_*.deb`. Tarball: unpack and run `./install.sh` (for you, in `~/.local`) or `sudo ./install.sh --system`; `./install.sh --uninstall` removes it. On Arch and others use the tarball. |
| macOS 11+, Apple Silicon and Intel | `Lazy-<version>-macos-universal.dmg` | open it, drag Lazy to Applications |
| Windows 10/11 x64 | `Lazy-Setup-<version>-windows-x64.exe`, `lazy-<version>-windows-x64.zip` | run the installer (per user, no administrator needed) or unpack the zip |

Linux needs a Vulkan driver, a Secret Service keyring (GNOME Keyring, KWallet, KeePassXC) for the
stored sign-in, and X11 or Wayland. The client's sign-in screen has the server address (D25); it is
also settable with `LAZY_SERVER_URL`.

## What a running Lazy needs from a release (D28)

Installed copies find new releases by reading the newest release's `SHA256SUMS`, so a release is
only seen if:

- it is **published**, not a draft, and **not a pre-release** (GitHub's "latest" skips both; the
  workflow's draft pre-release must be edited to make it so);
- its tag is `v<version>` (the download address is `releases/download/v<version>/<file>`);
- `SHA256SUMS` is attached and lists the files **named exactly as the packaging scripts name them**
  (`Lazy-Setup-<v>-windows-x64.exe`, `Lazy-<v>-macos-universal.dmg`, `lazy-chat_<v>_amd64.deb`,
  `lazy-<v>-linux-x86_64.tar.gz`). The version is read from the names. Renaming a file means changing
  `Package` in `crates/lazy-client-core/src/update.rs` in the same release, or installed copies will
  not see it.

A release that changes the wire protocol (`PROTOCOL_VERSION`) also needs its notes to say that the
server must be updated first or together with the clients.

## Not signed: what people will see

- **macOS:** the app is signed *ad hoc*, which is enough to run but not to pass Gatekeeper without
  asking. After downloading, open Lazy once; if macOS says it cannot verify it, open **System
  Settings, Privacy & Security** and choose **Open Anyway** (macOS 15 removed the older route; on
  macOS 14 or earlier, **right-click Lazy, choose Open, then Open**). Or, in a terminal:
  `xattr -dr com.apple.quarantine /Applications/Lazy.app`.
- **Windows:** SmartScreen says "Windows protected your PC": **More info, Run anyway**.
- **Linux:** nothing.

To sign and notarize the macOS build, add these repository secrets (Settings, Secrets and variables,
Actions), and the workflow does the rest: `APPLE_CERTIFICATE` (a Developer ID Application
certificate exported as .p12 and base64-encoded), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGN_IDENTITY`
(`Developer ID Application: Name (TEAMID)`), and for notarization `APPLE_ID`, `APPLE_TEAM_ID`, and
`APPLE_APP_PASSWORD` (an app-specific password). That path is written but untested. Windows signing
is not set up (it needs a certificate, or Azure Trusted Signing); add a SignTool step in
`packaging/windows/build-windows.ps1` around the `.exe` and the installer when you have one.

## Where things are

- `.github/workflows/release.yml`: the workflow. Linux builds on `ubuntu-22.04` (the oldest runner, so
  the binary runs on older distributions); macOS on `macos-15`, building `aarch64` and `x86_64`
  separately and joining them with `lipo`; Windows on `windows-2022`.
- `packaging/linux/` (`build-linux.sh`, `install.sh`, `lazy.desktop`), `packaging/macos/`
  (`build-macos.sh`, `Info.plist.in`), `packaging/windows/` (`lazy.iss`, `build-windows.ps1`). Each
  script takes the built binary, the version, and an output directory, and can be run by hand on
  its own platform.
- The release profile: `cargo build --profile dist -p lazy-desktop` (release, stripped). The macOS
  build adds `--features runtime-shaders`, which compiles the Metal shaders when the app starts, so
  the runner needs no Metal toolchain.
- The app's identity: the Linux package is `lazy-chat` and the command `lazy`; the macOS bundle id
  is `io.github.last-refuge.lazy` (set `LAZY_BUNDLE_ID` to change it, before the first release anyone
  installs: it identifies the app to macOS); the Windows `AppId` GUID in `lazy.iss` must never change.

## Icons and brand

The identity is **Soft Signal** (version 1.0.0, in `branding/`): a sloth in a speech bubble, Forest and
Paper, with Moss the mascot. The packages use the kit's own icon files and nothing is generated:
`branding/icons/lazy-app-16.png` to `lazy-app-512.png` (the `.deb` and tarball install them as the
`lazy` icon in the hicolor theme), `lazy-app.icns` (the macOS bundle), and `lazy-app.ico` (the Windows
executable, by `crates/lazy-desktop/build.rs`, and the installer). To change the brand, update the kit
(its own `source/` scripts rebuild the icons and the manifest) and release again. The product
description on every package is the brand's: "A native place for channels, messages and the people
you work with."

## Known gaps

- **Desktop notifications exist only on Linux.** On macOS and Windows the app runs, shows unread
  counts and the window title badge, but sends no operating system notifications (the platform
  notifier there is a stub). This matters for dogfooding on those systems.
- Updating from inside Lazy (D28) exists from the first release after 0.5.0, which has no updater
  and is replaced by hand once. Nothing about it has been run (`VERIFICATION.md`).
- The window position cannot be restored on Wayland (compositors decide).
- Only x86_64 on Linux and Windows; no ARM builds.
- The licenses of bundled fonts (DM Sans, Nunito, Lilex, Noto Color Emoji) are shipped in every package;
  a list of the licenses of the Rust dependencies is not.
