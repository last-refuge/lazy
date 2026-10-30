//! Updating from inside Lazy (D28): which kind of install this is, what to run to replace it with
//! the file the runtime fetched, and the glue between the runtime's updater and the UI's model.
//!
//! The runtime finds and fetches (`lazy_client_runtime::Updater`); the UI asks and shows
//! (`lazy_ui::updates`); this module is the one that starts another program, so the platform
//! rules are here, as plain functions of paths:
//!
//! * **Windows, installed with the setup program** (an uninstaller sits beside `lazy.exe`): run the
//!   new setup program silently. Setup closes Lazy if it is still running, replaces it, and starts
//!   the new one (`/LAZYUPDATE=1`, see `packaging/windows/lazy.iss`).
//! * **macOS, in an app bundle**: a small script waits for Lazy to quit, mounts the disk image,
//!   copies the new `Lazy.app` beside the old one, swaps them, and opens the new one. If the swap
//!   fails (no permission to change `/Applications`), the old app is put back and the disk image
//!   is opened for the person to drag, as on first install. A file the program fetched itself
//!   carries no quarantine mark, so the new copy opens without the first-run question.
//! * **Linux, from the package** (`/usr/bin/lazy`): after Lazy quits, `pkexec apt-get install`
//!   the `.deb`, which asks for a password in the desktop's own dialog; then Lazy starts again.
//! * **Linux, from the tarball into the person's home** (`~/.local/bin/lazy`): unpack the new
//!   tarball and run its `install.sh` into the same prefix, then start Lazy again.
//! * **Anything else** (a portable copy, a build from source, an install for everyone made by
//!   root): Lazy says a release exists and opens its page; it replaces nothing.
//!
//! Every script writes to `update.log` beside the other logs, and every path ends with the old or
//! the new Lazy running again, so a failed update never leaves the person without the program.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{App, Entity};
use lazy_client_core::{Package, UpdateFailure, UpdateStatus, Version};
use lazy_client_runtime::{Runtime, UpdateConfig, Updater};
use lazy_ui::updates::{UpdateHost, UpdateModel};
use serde::{Deserialize, Serialize};

/// The file in the profile folder that holds the person's choice about automatic checks.
pub const SETTINGS_FILE: &str = "updates.json";
const SETTINGS_VERSION: u32 = 1;

/// The environment variable that turns checking on (`1`) or off (`0`) whatever the build:
/// development builds do not check unless asked, and a packager can ship a build that never does.
pub const CHECK_VARIABLE: &str = "LAZY_UPDATE_CHECK";

/// The operating system, as a value so that the rules can be tested for each on any machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// How this copy of Lazy was installed, which decides whether it can replace itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Windows, with the setup program.
    WindowsSetup,
    /// macOS, `app` being the `Lazy.app` that is running.
    MacApp { app: PathBuf },
    /// Linux, the `.deb` (`binary` is `/usr/bin/lazy`).
    LinuxPackage { binary: PathBuf },
    /// Linux, the tarball's `install.sh` into `prefix` (`~/.local`).
    LinuxUser { prefix: PathBuf },
    /// Anything that cannot be replaced from inside.
    Other,
}

impl InstallKind {
    /// Works out the kind from where the running program is. `home` is the person's home folder;
    /// `exists` says whether a path exists.
    pub fn detect(os: Os, exe: &Path, home: Option<&Path>, exists: &dyn Fn(&Path) -> bool) -> Self {
        match os {
            Os::Windows => match exe.parent() {
                Some(dir) if exists(&dir.join("unins000.exe")) => InstallKind::WindowsSetup,
                _ => InstallKind::Other,
            },
            Os::MacOs => {
                // …/Lazy.app/Contents/MacOS/lazy
                let macos = exe.parent();
                let app = macos.and_then(Path::parent).and_then(Path::parent);
                match app {
                    Some(app)
                        if macos.and_then(Path::file_name) == Some(OsStr::new("MacOS"))
                            && app.extension() == Some(OsStr::new("app"))
                            // Run from the disk image itself: nothing to replace.
                            && !app.starts_with("/Volumes") =>
                    {
                        InstallKind::MacApp {
                            app: app.to_path_buf(),
                        }
                    }
                    _ => InstallKind::Other,
                }
            }
            Os::Linux => {
                if exe == Path::new("/usr/bin/lazy") {
                    let tools = exists(Path::new("/usr/bin/pkexec"))
                        && exists(Path::new("/usr/bin/apt-get"));
                    return if tools {
                        InstallKind::LinuxPackage {
                            binary: exe.to_path_buf(),
                        }
                    } else {
                        InstallKind::Other
                    };
                }
                let bin = exe
                    .parent()
                    .filter(|bin| bin.file_name() == Some(OsStr::new("bin")));
                match (bin.and_then(Path::parent), home) {
                    (Some(prefix), Some(home))
                        if exe.file_name() == Some(OsStr::new("lazy"))
                            && prefix.starts_with(home) =>
                    {
                        InstallKind::LinuxUser {
                            prefix: prefix.to_path_buf(),
                        }
                    }
                    _ => InstallKind::Other,
                }
            }
            Os::Other => InstallKind::Other,
        }
    }

    /// Whether Lazy can replace itself.
    pub fn can_install(&self) -> bool {
        !matches!(self, InstallKind::Other)
    }

    /// The release file that suits this install; for one that cannot be replaced, the one a
    /// person on this system would download, so that it still learns about new releases.
    pub fn package(&self, os: Os) -> Option<Package> {
        match (self, os) {
            (InstallKind::WindowsSetup, _) | (InstallKind::Other, Os::Windows) => {
                Some(Package::WindowsSetup)
            }
            (InstallKind::MacApp { .. }, _) | (InstallKind::Other, Os::MacOs) => {
                Some(Package::MacDisk)
            }
            (InstallKind::LinuxPackage { .. }, _) => Some(Package::LinuxDeb),
            (InstallKind::LinuxUser { .. }, _) | (InstallKind::Other, Os::Linux) => {
                Some(Package::LinuxArchive)
            }
            (InstallKind::Other, Os::Other) => None,
        }
    }

    /// What to run, once the file is ready, to replace this install with it. `pid` is Lazy's own
    /// process, which the scripts wait for.
    pub fn launch(&self, file: &Path, pid: u32, log: &Path) -> Option<Launch> {
        let script = |text: &str, third: &Path| Launch {
            program: OsString::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(text),
                OsString::from("lazy-update"),
                OsString::from(pid.to_string()),
                file.as_os_str().to_owned(),
                third.as_os_str().to_owned(),
                log.as_os_str().to_owned(),
            ],
        };
        match self {
            InstallKind::WindowsSetup => Some(Launch {
                program: file.as_os_str().to_owned(),
                args: SETUP_ARGUMENTS.iter().map(OsString::from).collect(),
            }),
            InstallKind::MacApp { app } => Some(script(MAC_SCRIPT, app)),
            InstallKind::LinuxPackage { binary } => Some(script(DEB_SCRIPT, binary)),
            InstallKind::LinuxUser { prefix } => Some(script(ARCHIVE_SCRIPT, prefix)),
            InstallKind::Other => None,
        }
    }
}

/// A program to start and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl Launch {
    /// Starts it detached from Lazy, so that it outlives Lazy.
    pub fn spawn(&self) -> io::Result<()> {
        use std::process::{Command, Stdio};
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own process group: a Ctrl+C in the terminal Lazy was started from, or the
            // terminal closing, does not reach it.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
            command.creation_flags(0x0000_0008 | 0x0000_0200);
        }
        command.spawn().map(|_| ())
    }
}

/// The setup program, silently; it closes Lazy if it is still running and starts the new one.
const SETUP_ARGUMENTS: [&str; 6] = [
    "/SILENT",
    "/SP-",
    "/SUPPRESSMSGBOXES",
    "/NOCANCEL",
    "/CLOSEAPPLICATIONS",
    "/LAZYUPDATE=1",
];

/// A script: the part common to all (`$1` the process to wait for, `$2` the file, `$3` the
/// install, `$4` the log; the log is used if it can be written, and the script never stops because
/// it cannot be), then `$body`. A macro because `concat!` takes only literals.
macro_rules! script {
    ($body:literal) => {
        concat!(
            r#"pid="$1"; file="$2"; target="$3"; log="$4"
if ! : >>"$log" 2>/dev/null; then log=/dev/null; fi
exec >>"$log" 2>&1
echo "update: $(date) waiting for Lazy ($pid) to quit"
n=0
while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 100 ]; do sleep 0.3; n=$((n + 1)); done
"#,
            $body
        )
    };
}

/// macOS: swap the app for the one on the disk image, or open the image if that cannot be done.
const MAC_SCRIPT: &str = script!(
    r#"mnt="$(mktemp -d /tmp/lazy-update.XXXXXX)"
failed=
if hdiutil attach "$file" -nobrowse -noautoopen -readonly -mountpoint "$mnt"; then
  new="$target.update"; old="$target.old"
  rm -rf "$new" "$old"
  if ditto "$mnt/Lazy.app" "$new" && mv "$target" "$old" && mv "$new" "$target"; then
    rm -rf "$old"
    echo "update: replaced $target"
  else
    echo "update: could not replace $target"
    rm -rf "$new"
    if [ -d "$old" ] && [ ! -d "$target" ]; then mv "$old" "$target"; fi
    failed=1
  fi
  hdiutil detach "$mnt" -quiet || hdiutil detach "$mnt" -force -quiet
  rmdir "$mnt"
else
  echo "update: could not open the disk image"
  rmdir "$mnt"
  failed=1
fi
if [ -n "$failed" ]; then open "$file"; fi
open "$target"
"#
);

/// Linux, the package: install it as root through the desktop's own password dialog, then start
/// Lazy again, the new one or (if that failed) the old one. `$3` is `/usr/bin/lazy`.
const DEB_SCRIPT: &str = script!(
    r#"if command -v pkexec >/dev/null 2>&1; then
  pkexec apt-get install -y "$file" || echo "update: the package could not be installed"
else
  echo "update: pkexec is not installed"
fi
exec "$target"
"#
);

/// Linux, the tarball: unpack it and run its installer into the prefix Lazy is in (`$3`), then
/// start Lazy again.
const ARCHIVE_SCRIPT: &str = script!(
    r#"dir="$(mktemp -d)"
name="$(basename "$file" .tar.gz)"
if tar -xzf "$file" -C "$dir" && sh "$dir/$name/install.sh" --prefix "$target"; then
  echo "update: installed into $target"
else
  echo "update: the installer did not finish"
fi
rm -rf "$dir"
exec "$target/bin/lazy"
"#
);

/// What the person chose about automatic checks.
#[derive(Serialize, Deserialize)]
struct Settings {
    version: u32,
    automatic: bool,
}

/// Whether automatic checks are wanted: on, unless the person turned them off.
pub fn load_automatic(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(SETTINGS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .filter(|settings| settings.version == SETTINGS_VERSION)
        .is_none_or(|settings| settings.automatic)
}

/// Remembers the choice, through a temporary file so a crash never leaves half a file.
pub fn save_automatic(dir: &Path, automatic: bool) -> io::Result<()> {
    let text = serde_json::to_string_pretty(&Settings {
        version: SETTINGS_VERSION,
        automatic,
    })
    .map_err(io::Error::other)?;
    let target = dir.join(SETTINGS_FILE);
    let temporary = dir.join(format!("{SETTINGS_FILE}.tmp"));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &target)
}

/// What the UI's buttons do, on this computer.
pub struct DesktopUpdates {
    updater: Updater,
    kind: InstallKind,
    profile_dir: PathBuf,
    log: PathBuf,
}

impl UpdateHost for DesktopUpdates {
    fn can_install(&self) -> bool {
        self.kind.can_install()
    }

    fn automatic(&self) -> bool {
        self.updater.automatic()
    }

    fn set_automatic(&self, automatic: bool) {
        self.updater.set_automatic(automatic);
        if let Err(error) = save_automatic(&self.profile_dir, automatic) {
            tracing::warn!(%error, "could not save the choice about update checks");
        }
    }

    fn check_now(&self) {
        self.updater.check_now();
    }

    fn download(&self) {
        self.updater.download();
    }

    fn install(&self) -> Result<(), UpdateFailure> {
        let UpdateStatus::Ready { file, .. } = self.updater.status() else {
            return Err(UpdateFailure::Local);
        };
        let launch = self
            .kind
            .launch(&file, std::process::id(), &self.log)
            .ok_or(UpdateFailure::Local)?;
        tracing::info!(program = ?launch.program, "starting the installer");
        launch.spawn().map_err(|error| {
            tracing::error!(%error, "could not start the installer");
            UpdateFailure::Local
        })
    }
}

/// Whether this run checks for updates: always when `LAZY_UPDATE_CHECK=1`, never when it is `0`,
/// else in release builds only.
fn checks_enabled() -> bool {
    match std::env::var(CHECK_VARIABLE).as_deref() {
        Ok("0") => false,
        Ok(_) => true,
        Err(_) => !cfg!(debug_assertions),
    }
}

/// Starts the updater on `runtime` and gives the UI its model, if this run checks for updates.
/// Call before the window opens.
pub fn start_updates(
    runtime: &Runtime,
    profile_dir: &Path,
    log_dir: &Path,
    cx: &mut App,
) -> Option<Entity<UpdateModel>> {
    if !checks_enabled() {
        tracing::info!("update checks are off in this run");
        return None;
    }
    let os = Os::current();
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let exe = std::env::current_exe().ok();
    // Symbolic links are followed (an app in a linked folder), but not on Windows, where that
    // gives paths in a form nothing else here uses.
    let exe = if cfg!(windows) {
        exe
    } else {
        exe.and_then(|exe| exe.canonicalize().ok())
    };
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_owned());
    let kind = exe.map_or(InstallKind::Other, |exe| {
        InstallKind::detect(os, &exe, home.as_deref(), &|path| path.exists())
    });
    let package = kind.package(os)?;
    let mut config = UpdateConfig::new(current.clone(), package, profile_dir.join("updates"));
    config.automatic = load_automatic(profile_dir);
    let (updater, statuses) = match runtime.start_updater(config) {
        Ok(started) => started,
        Err(error) => {
            tracing::warn!(%error, "could not start the update checks");
            return None;
        }
    };
    tracing::info!(?kind, %current, "checking for updates");
    let host = Rc::new(DesktopUpdates {
        updater,
        kind,
        profile_dir: profile_dir.to_owned(),
        log: log_dir.join("update.log"),
    });
    let model = lazy_ui::updates::install(cx, host, current.to_string());
    let weak = model.downgrade();
    cx.spawn(async move |cx| {
        while let Ok(status) = statuses.recv().await {
            if weak
                .update(cx, |model, cx| model.set_status(status, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    Some(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing<'a>(paths: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |path| paths.iter().any(|known| Path::new(known) == path)
    }

    fn detect(os: Os, exe: &str, home: Option<&str>, present: &[&str]) -> InstallKind {
        InstallKind::detect(os, Path::new(exe), home.map(Path::new), &existing(present))
    }

    #[test]
    fn windows_replaces_itself_only_when_the_setup_program_installed_it() {
        assert_eq!(
            detect(
                Os::Windows,
                "/apps/Lazy/lazy.exe",
                None,
                &["/apps/Lazy/unins000.exe"]
            ),
            InstallKind::WindowsSetup
        );
        assert_eq!(
            detect(Os::Windows, "/apps/Lazy/lazy.exe", None, &[]),
            InstallKind::Other,
            "the portable copy"
        );
    }

    #[test]
    fn a_mac_app_is_replaced_in_place_unless_it_runs_from_the_disk_image() {
        assert_eq!(
            detect(
                Os::MacOs,
                "/Applications/Lazy.app/Contents/MacOS/lazy",
                None,
                &[]
            ),
            InstallKind::MacApp {
                app: PathBuf::from("/Applications/Lazy.app")
            }
        );
        assert_eq!(
            detect(
                Os::MacOs,
                "/Users/a/Applications/Lazy.app/Contents/MacOS/lazy",
                None,
                &[]
            ),
            InstallKind::MacApp {
                app: PathBuf::from("/Users/a/Applications/Lazy.app")
            }
        );
        for exe in [
            "/Volumes/Lazy/Lazy.app/Contents/MacOS/lazy",
            "/Users/a/code/target/debug/lazy-desktop",
            "/Applications/Lazy/Contents/MacOS/lazy",
            "/lazy",
        ] {
            assert_eq!(
                detect(Os::MacOs, exe, None, &[]),
                InstallKind::Other,
                "{exe}"
            );
        }
    }

    #[test]
    fn linux_replaces_the_package_or_a_tarball_install_in_the_home_folder() {
        let tools = ["/usr/bin/pkexec", "/usr/bin/apt-get"];
        assert_eq!(
            detect(Os::Linux, "/usr/bin/lazy", Some("/home/a"), &tools),
            InstallKind::LinuxPackage {
                binary: PathBuf::from("/usr/bin/lazy")
            }
        );
        assert_eq!(
            detect(
                Os::Linux,
                "/usr/bin/lazy",
                Some("/home/a"),
                &["/usr/bin/apt-get"]
            ),
            InstallKind::Other,
            "no way to ask for the password"
        );
        assert_eq!(
            detect(Os::Linux, "/usr/bin/lazy", Some("/home/a"), &[]),
            InstallKind::Other,
            "a package that is not a .deb"
        );
        assert_eq!(
            detect(Os::Linux, "/home/a/.local/bin/lazy", Some("/home/a"), &[]),
            InstallKind::LinuxUser {
                prefix: PathBuf::from("/home/a/.local")
            }
        );
        for exe in [
            // installed for everyone by root: not ours to change
            "/usr/local/bin/lazy",
            "/home/a/code/lazy/target/debug/lazy-desktop",
            "/home/a/Downloads/lazy",
            "/home/b/.local/bin/lazy",
        ] {
            assert_eq!(
                detect(Os::Linux, exe, Some("/home/a"), &[]),
                InstallKind::Other,
                "{exe}"
            );
        }
        assert_eq!(
            detect(Os::Linux, "/home/a/.local/bin/lazy", None, &[]),
            InstallKind::Other,
            "no home folder known"
        );
    }

    #[test]
    fn every_install_names_the_file_that_suits_it() {
        let user = InstallKind::LinuxUser {
            prefix: PathBuf::from("/home/a/.local"),
        };
        assert_eq!(
            InstallKind::WindowsSetup.package(Os::Windows),
            Some(Package::WindowsSetup)
        );
        assert_eq!(
            InstallKind::MacApp {
                app: PathBuf::from("/Applications/Lazy.app")
            }
            .package(Os::MacOs),
            Some(Package::MacDisk)
        );
        assert_eq!(user.package(Os::Linux), Some(Package::LinuxArchive));
        assert_eq!(
            InstallKind::LinuxPackage {
                binary: PathBuf::from("/usr/bin/lazy")
            }
            .package(Os::Linux),
            Some(Package::LinuxDeb)
        );
        // Copies that cannot be replaced still learn that a release exists.
        assert_eq!(
            InstallKind::Other.package(Os::Windows),
            Some(Package::WindowsSetup)
        );
        assert_eq!(
            InstallKind::Other.package(Os::MacOs),
            Some(Package::MacDisk)
        );
        assert_eq!(
            InstallKind::Other.package(Os::Linux),
            Some(Package::LinuxArchive)
        );
        assert_eq!(InstallKind::Other.package(Os::Other), None);
        assert!(user.can_install());
        assert!(!InstallKind::Other.can_install());
    }

    #[test]
    fn what_is_run_is_the_setup_program_or_a_script_with_the_files_it_needs() {
        let file = Path::new("/profile/updates/Lazy-Setup-0.6.0-windows-x64.exe");
        let log = Path::new("/logs/update.log");
        let setup = InstallKind::WindowsSetup.launch(file, 42, log).unwrap();
        assert_eq!(setup.program, file.as_os_str());
        assert!(setup.args.iter().any(|arg| arg == "/SILENT"));
        assert!(setup.args.iter().any(|arg| arg == "/LAZYUPDATE=1"));

        let dmg = Path::new("/profile/updates/Lazy-0.6.0-macos-universal.dmg");
        let mac = InstallKind::MacApp {
            app: PathBuf::from("/Applications/Lazy.app"),
        }
        .launch(dmg, 42, log)
        .unwrap();
        assert_eq!(mac.program, "/bin/sh");
        assert_eq!(mac.args[0], "-c");
        assert!(mac.args[1].to_string_lossy().contains("hdiutil attach"));
        assert_eq!(
            mac.args[2..],
            [
                OsString::from("lazy-update"),
                OsString::from("42"),
                dmg.as_os_str().to_owned(),
                OsString::from("/Applications/Lazy.app"),
                log.as_os_str().to_owned(),
            ]
        );

        let deb = Path::new("/profile/updates/lazy-chat_0.6.0_amd64.deb");
        let package = InstallKind::LinuxPackage {
            binary: PathBuf::from("/usr/bin/lazy"),
        }
        .launch(deb, 7, log)
        .unwrap();
        assert!(
            package.args[1]
                .to_string_lossy()
                .contains("pkexec apt-get install")
        );

        let tarball = Path::new("/profile/updates/lazy-0.6.0-linux-x86_64.tar.gz");
        let user = InstallKind::LinuxUser {
            prefix: PathBuf::from("/home/a/.local"),
        }
        .launch(tarball, 7, log)
        .unwrap();
        assert!(
            user.args[1]
                .to_string_lossy()
                .contains("install.sh\" --prefix")
        );

        assert_eq!(InstallKind::Other.launch(file, 1, log), None);
    }

    /// The scripts are the one part of an update that nothing else exercises, so at least their
    /// syntax is checked: `sh -n` reads a script and runs none of it.
    #[cfg(unix)]
    #[test]
    fn the_scripts_are_valid_shell() {
        for (name, script) in [
            ("mac", MAC_SCRIPT),
            ("deb", DEB_SCRIPT),
            ("archive", ARCHIVE_SCRIPT),
        ] {
            let output = std::process::Command::new("sh")
                .args(["-n", "-c", script])
                .output()
                .expect("sh runs");
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn the_choice_about_automatic_checks_is_kept_and_defaults_to_on() {
        let dir = std::env::temp_dir().join(format!("lazy-updates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_automatic(&dir), "no file: on");
        save_automatic(&dir, false).unwrap();
        assert!(!load_automatic(&dir));
        save_automatic(&dir, true).unwrap();
        assert!(load_automatic(&dir));
        for damaged in [
            "{ half",
            "[]",
            r#"{"version": 2, "automatic": false}"#,
            r#"{"automatic": false}"#,
        ] {
            std::fs::write(dir.join(SETTINGS_FILE), damaged).unwrap();
            assert!(load_automatic(&dir), "{damaged}: on");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
