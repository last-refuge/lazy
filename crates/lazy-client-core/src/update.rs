//! Updates (D28): whether a newer Lazy has been released, which file brings it, and the states the
//! screen shows while it is found, fetched, and installed.
//!
//! Everything here is plain data and rules. Fetching belongs to the runtime
//! (`lazy-client-runtime`, `updates`) and starting an installer belongs to the desktop. A release
//! lists its files and their SHA-256 sums in `SHA256SUMS` (`sha256sum`'s format), and the version
//! of a file is in its name, so that one small file says what the newest release is and what the
//! file for this install must hash to.

use std::cmp::Ordering;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Where the releases are published. The pages under it are `latest/download/<file>`,
/// `download/v<version>/<file>`, and `tag/v<version>`.
pub const RELEASES_URL: &str = "https://github.com/last-refuge/lazy/releases";

/// How often a running Lazy asks whether there is a newer release.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// The most a checksum list may be, in bytes. A release's is a few hundred.
pub const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;

/// A version: `major.minor.patch`, with an optional pre-release (`0.1.0-dogfood.1`), which sorts
/// before the release it leads to. Build metadata (`+abc`) is accepted and ignored.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Identifier>,
}

/// One dot-separated part of a pre-release.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Identifier {
    Number(u64),
    Text(String),
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.cmp(b),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn identifier(text: &str) -> Option<Identifier> {
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    Some(match number(text) {
        Some(value) => Identifier::Number(value),
        None => Identifier::Text(text.to_owned()),
    })
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    /// `1.2.3`, `v1.2.3`, `1.2.3-rc.1`. Anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next().unwrap_or("");
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let major = number(parts.next()?)?;
        let minor = number(parts.next()?)?;
        let patch = number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre.split('.').map(identifier).collect::<Option<Vec<_>>>()?,
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// A pre-release is older than the release it leads to.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (index, part) in self.pre.iter().enumerate() {
            f.write_str(if index == 0 { "-" } else { "." })?;
            match part {
                Identifier::Number(value) => write!(f, "{value}")?,
                Identifier::Text(text) => f.write_str(text)?,
            }
        }
        Ok(())
    }
}

/// A file a release carries, by what it is for. The names are the ones `packaging/` and the
/// release workflow give them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Package {
    /// `Lazy-Setup-<version>-windows-x64.exe`, the Windows installer.
    WindowsSetup,
    /// `Lazy-<version>-macos-universal.dmg`, the macOS disk image.
    MacDisk,
    /// `lazy-chat_<version>_amd64.deb`, for Debian, Ubuntu, and derivatives.
    LinuxDeb,
    /// `lazy-<version>-linux-x86_64.tar.gz`, which holds `install.sh`.
    LinuxArchive,
}

impl Package {
    pub const ALL: [Package; 4] = [
        Package::WindowsSetup,
        Package::MacDisk,
        Package::LinuxDeb,
        Package::LinuxArchive,
    ];

    /// What comes before and after the version in the file's name.
    fn affixes(self) -> (&'static str, &'static str) {
        match self {
            Package::WindowsSetup => ("Lazy-Setup-", "-windows-x64.exe"),
            Package::MacDisk => ("Lazy-", "-macos-universal.dmg"),
            Package::LinuxDeb => ("lazy-chat_", "_amd64.deb"),
            Package::LinuxArchive => ("lazy-", "-linux-x86_64.tar.gz"),
        }
    }

    /// The file's name in a release of `version`.
    pub fn file_name(self, version: &Version) -> String {
        let (prefix, suffix) = self.affixes();
        format!("{prefix}{version}{suffix}")
    }

    /// The version in `file_name`, if it is a file of this kind.
    pub fn version_in(self, file_name: &str) -> Option<Version> {
        let (prefix, suffix) = self.affixes();
        let middle = file_name.strip_prefix(prefix)?.strip_suffix(suffix)?;
        // `Version::parse` allows a leading "v"; a file name does not have one.
        if middle.starts_with('v') {
            return None;
        }
        Version::parse(middle)
    }
}

/// One line of `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub file_name: String,
    /// 64 lowercase hexadecimal digits.
    pub sha256: String,
}

/// The lines of a `sha256sum` listing (`<hex>  <name>`, or `<hex> *<name>`). A line that is not
/// one, or whose name is anything but letters, digits, and `. _ - +` (it becomes a file name and
/// part of an address), is left out.
pub fn parse_checksums(text: &str) -> Vec<Checksum> {
    text.lines()
        .filter_map(|line| {
            let (sum, rest) = line.trim_end().split_once(char::is_whitespace)?;
            let name = rest.trim_start();
            let name = name.strip_prefix('*').unwrap_or(name);
            let name = name.strip_prefix("./").unwrap_or(name);
            let sum_ok = sum.len() == 64 && sum.bytes().all(|byte| byte.is_ascii_hexdigit());
            let name_ok = !name.is_empty()
                && !name.starts_with('.')
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+')
                });
            (sum_ok && name_ok).then(|| Checksum {
                file_name: name.to_owned(),
                sha256: sum.to_ascii_lowercase(),
            })
        })
        .collect()
}

/// A release newer than this one, and the file of it that suits this install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: Version,
    pub package: Package,
    pub file_name: String,
    /// What the file must hash to (lowercase hexadecimal).
    pub sha256: String,
    /// Where the file is.
    pub download_url: String,
    /// The release's page, with its notes: where to go when the file cannot be fetched or used.
    pub release_url: String,
}

impl AvailableUpdate {
    /// The newest release listed in `checksums` that has a file of `package` and is newer than
    /// `current`. `releases_url` is where releases live ([`RELEASES_URL`]).
    pub fn newest(
        checksums: &[Checksum],
        package: Package,
        current: &Version,
        releases_url: &str,
    ) -> Option<Self> {
        let releases = releases_url.trim_end_matches('/');
        checksums
            .iter()
            .filter_map(|sum| {
                let version = package.version_in(&sum.file_name)?;
                if version <= *current {
                    return None;
                }
                Some(Self {
                    download_url: format!("{releases}/download/v{version}/{}", sum.file_name),
                    release_url: format!("{releases}/tag/v{version}"),
                    version,
                    package,
                    file_name: sum.file_name.clone(),
                    sha256: sum.sha256.clone(),
                })
            })
            .max_by(|a, b| a.version.cmp(&b.version))
    }
}

/// Why an update could not be checked for, fetched, or started. The reason in words is for the
/// person; the detail (an error from the network or the disk) goes to the log, never to the
/// screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateFailure {
    /// GitHub could not be reached, or answered with an error.
    Unreachable,
    /// What the release lists is not something this build can use.
    Unreadable,
    /// The file arrived but is not the one the release lists.
    Damaged,
    /// The file could not be saved, or the installer could not be started.
    Local,
}

impl UpdateFailure {
    pub fn describe(self) -> &'static str {
        match self {
            UpdateFailure::Unreachable => "Couldn't reach GitHub.",
            UpdateFailure::Unreadable => "The release has no file for this computer.",
            UpdateFailure::Damaged => "The download was damaged, so it was thrown away.",
            UpdateFailure::Local => "Couldn't save or start the update.",
        }
    }
}

/// Where updating stands, as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Nothing has been checked yet (or checking is off).
    Idle,
    /// A check the person asked for is under way. Automatic checks are silent.
    Checking,
    /// The last check found nothing newer.
    UpToDate,
    /// A newer release exists and has not been fetched.
    Available(AvailableUpdate),
    /// Its file is coming. `total` is `None` when the server did not say how big it is.
    Downloading {
        update: AvailableUpdate,
        received: u64,
        total: Option<u64>,
    },
    /// Its file is on disk and matches its checksum: installing is one step.
    Ready {
        update: AvailableUpdate,
        file: PathBuf,
    },
    /// Something failed. `update` is the release it was about, if it got that far.
    Failed {
        update: Option<AvailableUpdate>,
        failure: UpdateFailure,
    },
}

impl UpdateStatus {
    /// The release this status is about.
    pub fn update(&self) -> Option<&AvailableUpdate> {
        match self {
            UpdateStatus::Available(update)
            | UpdateStatus::Downloading { update, .. }
            | UpdateStatus::Ready { update, .. } => Some(update),
            UpdateStatus::Failed { update, .. } => update.as_ref(),
            UpdateStatus::Idle | UpdateStatus::Checking | UpdateStatus::UpToDate => None,
        }
    }

    /// How much of the file has arrived, from 0 to 1, when the size is known.
    pub fn fraction(&self) -> Option<f32> {
        match self {
            UpdateStatus::Downloading {
                received,
                total: Some(total),
                ..
            } if *total > 0 => Some((*received as f64 / *total as f64).clamp(0.0, 1.0) as f32),
            _ => None,
        }
    }

    /// There is something to tell the person without being asked: a newer release, its progress,
    /// or its failure. A check that found nothing is not.
    pub fn wants_attention(&self) -> bool {
        self.update().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} parses"))
    }

    const SUMS: &str = "\
052b716e2e9edfb7fb7cd15cfa458f24df58aa9edeb7313cd6a109efa678f93e  Lazy-0.5.0-macos-universal.dmg
4a45b24d4f450cced1fd7e1a6d0f0e6f60c2ae4be89a8864d7efb38c46fcf7c3  Lazy-Setup-0.5.0-windows-x64.exe
24676e6991614b48eab59132db65ccf010b6267e1578d4ec943046f6d3cdff80  lazy-0.5.0-linux-x86_64.tar.gz
3e0cec5b49ae663a31767e124e1e447eb2c4cf26bf39b1d4760b9af755103fbd  lazy-0.5.0-windows-x64.zip
a58e660656604c287d9e266b779babbd2500389fabd1186e2217fd335fdd45fd  lazy-chat_0.5.0_amd64.deb
";

    #[test]
    fn versions_parse_and_print() {
        assert_eq!(version("0.5.0").to_string(), "0.5.0");
        assert_eq!(version("v1.2.3").to_string(), "1.2.3");
        assert_eq!(version(" 1.2.3 ").to_string(), "1.2.3");
        assert_eq!(version("0.1.0-dogfood.1").to_string(), "0.1.0-dogfood.1");
        assert_eq!(
            version("1.0.0+abc").to_string(),
            "1.0.0",
            "metadata is ignored"
        );
        assert!(version("1.0.0-rc.1").is_prerelease());
        assert!(!version("1.0.0").is_prerelease());
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "a.b.c",
            "1.2.x",
            "+1.2.3",
            "1.-2.3",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-é",
            "vv1.2.3",
            "-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            Version::parse("99999999999999999999.0.0"),
            None,
            "too big for a number"
        );
    }

    #[test]
    fn versions_order_as_semantic_versions_do() {
        // The order semver.org gives, lowest first.
        let ordered = [
            "0.4.9",
            "0.5.0-alpha",
            "0.5.0-alpha.1",
            "0.5.0-alpha.beta",
            "0.5.0-beta",
            "0.5.0-beta.2",
            "0.5.0-beta.11",
            "0.5.0-rc.1",
            "0.5.0",
            "0.5.1",
            "0.10.0",
            "1.0.0",
        ];
        for (index, low) in ordered.iter().enumerate() {
            for high in &ordered[index + 1..] {
                assert!(version(low) < version(high), "{low} < {high}");
                assert!(version(high) > version(low), "{high} > {low}");
            }
            assert_eq!(version(low).cmp(&version(low)), Ordering::Equal);
        }
        assert!(version("0.9.0") < version("0.10.0"), "numbers, not text");
        assert_eq!(version("1.0.0+a"), version("1.0.0+b"));
    }

    #[test]
    fn each_package_names_its_file_and_reads_its_version_back() {
        let v = version("0.5.0");
        assert_eq!(
            Package::WindowsSetup.file_name(&v),
            "Lazy-Setup-0.5.0-windows-x64.exe"
        );
        assert_eq!(
            Package::MacDisk.file_name(&v),
            "Lazy-0.5.0-macos-universal.dmg"
        );
        assert_eq!(Package::LinuxDeb.file_name(&v), "lazy-chat_0.5.0_amd64.deb");
        assert_eq!(
            Package::LinuxArchive.file_name(&v),
            "lazy-0.5.0-linux-x86_64.tar.gz"
        );
        for text in ["0.5.0", "12.0.3", "0.1.0-dogfood.1"] {
            let v = version(text);
            for package in Package::ALL {
                assert_eq!(package.version_in(&package.file_name(&v)), Some(v.clone()));
                // Never another package's file.
                for other in Package::ALL.into_iter().filter(|other| *other != package) {
                    assert_eq!(package.version_in(&other.file_name(&v)), None);
                }
            }
        }
        assert_eq!(
            Package::MacDisk.version_in("Lazy-vX-macos-universal.dmg"),
            None
        );
        assert_eq!(
            Package::MacDisk.version_in("Lazy-v1.0.0-macos-universal.dmg"),
            None
        );
        assert_eq!(
            Package::LinuxArchive.version_in("lazy-0.5.0-windows-x64.zip"),
            None
        );
        assert_eq!(Package::LinuxDeb.version_in("SHA256SUMS"), None);
    }

    #[test]
    fn a_checksum_listing_is_read_line_by_line() {
        let sums = parse_checksums(SUMS);
        assert_eq!(sums.len(), 5);
        assert_eq!(sums[0].file_name, "Lazy-0.5.0-macos-universal.dmg");
        assert_eq!(
            sums[0].sha256,
            "052b716e2e9edfb7fb7cd15cfa458f24df58aa9edeb7313cd6a109efa678f93e"
        );

        let hash = "AB".repeat(32);
        let listing = format!(
            "{hash} *binary.bin\n{hash}  ./dotted.txt\n\nnot a line\n{hash}  \n{} x\n",
            "ab".repeat(31)
        );
        let sums = parse_checksums(&listing);
        assert_eq!(
            sums.iter()
                .map(|s| s.file_name.as_str())
                .collect::<Vec<_>>(),
            ["binary.bin", "dotted.txt"],
            "a binary marker and a leading ./ are dropped, the rest is skipped"
        );
        assert_eq!(sums[0].sha256, "ab".repeat(32), "lowercase");
    }

    #[test]
    fn a_name_that_could_leave_the_folder_or_the_address_is_refused() {
        let hash = "cd".repeat(32);
        for name in [
            "../evil.exe",
            "a/b.exe",
            "a\\b.exe",
            ".hidden",
            "a b.exe",
            "a?b.exe",
            "a#b.exe",
            "a%2fb.exe",
            "é.exe",
        ] {
            assert!(
                parse_checksums(&format!("{hash}  {name}\n")).is_empty(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn the_newest_release_with_this_installs_file_is_the_update() {
        let sums = parse_checksums(SUMS);
        let found = AvailableUpdate::newest(
            &sums,
            Package::WindowsSetup,
            &version("0.4.0"),
            RELEASES_URL,
        )
        .expect("0.5.0 is newer than 0.4.0");
        assert_eq!(found.version, version("0.5.0"));
        assert_eq!(found.file_name, "Lazy-Setup-0.5.0-windows-x64.exe");
        assert_eq!(
            found.sha256,
            "4a45b24d4f450cced1fd7e1a6d0f0e6f60c2ae4be89a8864d7efb38c46fcf7c3"
        );
        assert_eq!(
            found.download_url,
            "https://github.com/last-refuge/lazy/releases/download/v0.5.0/Lazy-Setup-0.5.0-windows-x64.exe"
        );
        assert_eq!(
            found.release_url,
            "https://github.com/last-refuge/lazy/releases/tag/v0.5.0"
        );

        // The same version, or a newer one already installed: nothing to do.
        for installed in ["0.5.0", "0.5.1", "1.0.0"] {
            assert_eq!(
                AvailableUpdate::newest(&sums, Package::MacDisk, &version(installed), RELEASES_URL),
                None,
                "{installed}"
            );
        }
        // A pre-release is older than its release, so it is offered the release.
        assert!(
            AvailableUpdate::newest(
                &sums,
                Package::LinuxDeb,
                &version("0.5.0-rc.1"),
                "https://example.test/releases/"
            )
            .is_some_and(|found| found
                .download_url
                .starts_with("https://example.test/releases/download/v0.5.0/")),
            "a trailing slash on the base is dropped"
        );
        // A release without a file for this install offers nothing.
        let only_mac: Vec<Checksum> = sums
            .iter()
            .filter(|sum| Package::MacDisk.version_in(&sum.file_name).is_some())
            .cloned()
            .collect();
        assert_eq!(
            AvailableUpdate::newest(
                &only_mac,
                Package::LinuxArchive,
                &version("0.1.0"),
                RELEASES_URL
            ),
            None
        );
    }

    #[test]
    fn with_several_releases_listed_the_newest_wins() {
        let hash = "01".repeat(32);
        let listing = format!(
            "{hash}  lazy-chat_0.6.0_amd64.deb\n{hash}  lazy-chat_0.10.0_amd64.deb\n{hash}  lazy-chat_0.9.0_amd64.deb\n"
        );
        let found = AvailableUpdate::newest(
            &parse_checksums(&listing),
            Package::LinuxDeb,
            &version("0.5.0"),
            RELEASES_URL,
        )
        .unwrap();
        assert_eq!(found.version, version("0.10.0"));
    }

    #[test]
    fn a_status_knows_its_release_and_how_far_the_file_has_come() {
        let update = AvailableUpdate::newest(
            &parse_checksums(SUMS),
            Package::MacDisk,
            &version("0.1.0"),
            RELEASES_URL,
        )
        .unwrap();
        assert_eq!(UpdateStatus::Idle.update(), None);
        assert_eq!(UpdateStatus::UpToDate.update(), None);
        assert!(!UpdateStatus::UpToDate.wants_attention());
        assert!(!UpdateStatus::Checking.wants_attention());
        assert!(UpdateStatus::Available(update.clone()).wants_attention());
        let downloading = |received, total| UpdateStatus::Downloading {
            update: update.clone(),
            received,
            total,
        };
        assert_eq!(downloading(0, Some(200)).fraction(), Some(0.0));
        assert_eq!(downloading(50, Some(200)).fraction(), Some(0.25));
        assert_eq!(
            downloading(300, Some(200)).fraction(),
            Some(1.0),
            "never past the end"
        );
        assert_eq!(downloading(5, None).fraction(), None);
        assert_eq!(downloading(5, Some(0)).fraction(), None);
        assert_eq!(UpdateStatus::Available(update.clone()).fraction(), None);
        let failed = UpdateStatus::Failed {
            update: Some(update.clone()),
            failure: UpdateFailure::Damaged,
        };
        assert_eq!(failed.update(), Some(&update));
        assert!(failed.wants_attention());
        assert!(
            !UpdateStatus::Failed {
                update: None,
                failure: UpdateFailure::Unreachable
            }
            .wants_attention(),
            "a check that failed is told only to the person who asked"
        );
    }
}
