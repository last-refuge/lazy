//! Finding and fetching updates (D28).
//!
//! An [`Updater`] asks shortly after it starts, and then every [`UpdateConfig::interval`] (thirty
//! minutes), what the newest release is. It does that by reading the release's `SHA256SUMS`, a
//! small text file GitHub serves from its download pages, which are not subject to the API's
//! request limit; the file names in it carry the versions
//! ([`lazy_client_core::AvailableUpdate::newest`]).
//!
//! Automatic checks are silent: they never report that a check is under way or that one failed,
//! and the status changes only to "up to date" or to a newer release (the screen shows nothing
//! for the first). A check the person asks for ([`Updater::check_now`]) answers either way,
//! failure included. Fetching a file
//! ([`Updater::download`]) streams it to a temporary file beside its final place, checks its
//! SHA-256 against the list, and renames it into place; a file that does not match is deleted. A
//! copy fetched earlier (Lazy was closed before it was installed) is used again if it still
//! matches.
//!
//! The updater never installs anything: starting an installer is the desktop's (D28).
//!
//! Every status change is sent on the channel [`Updater::start`] returns, in order. Progress is
//! sent at most ten times a second.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lazy_client_core::{
    AvailableUpdate, CHECK_INTERVAL, MAX_CHECKSUMS_BYTES, Package, RELEASES_URL, UpdateFailure,
    UpdateStatus, Version, parse_checksums,
};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// How soon after starting the first automatic check is made, so that opening the app is not
/// slowed by it.
const FIRST_CHECK: Duration = Duration::from_secs(20);

/// The environment variable that points the updater at another place for releases (a mirror, or a
/// test): the address that holds `latest/download/` and `download/v<version>/`.
pub const RELEASES_URL_VARIABLE: &str = "LAZY_RELEASES_URL";

/// Settings for [`Updater::start`].
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    /// Where releases are published: [`RELEASES_URL`], or `LAZY_RELEASES_URL` when that is set.
    pub releases_url: String,
    /// The version running now.
    pub current: Version,
    /// The file of a release that suits this install.
    pub package: Package,
    /// Where files are fetched to. The updater keeps only the newest file here and removes older
    /// ones and leftovers of failed fetches.
    pub download_dir: PathBuf,
    /// Time between automatic checks.
    pub interval: Duration,
    /// Time before the first automatic check.
    pub first_check: Duration,
    /// Whether automatic checks run (the person can turn them off).
    pub automatic: bool,
    /// The time a check may take.
    pub timeout: Duration,
    /// A download that receives nothing for this long fails.
    pub stall_timeout: Duration,
}

impl UpdateConfig {
    pub fn new(current: Version, package: Package, download_dir: PathBuf) -> Self {
        let releases_url = std::env::var(RELEASES_URL_VARIABLE)
            .ok()
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| RELEASES_URL.to_owned());
        Self {
            releases_url,
            current,
            package,
            download_dir,
            interval: CHECK_INTERVAL,
            first_check: FIRST_CHECK,
            automatic: true,
            timeout: Duration::from_secs(20),
            stall_timeout: Duration::from_secs(30),
        }
    }
}

/// Checks for newer releases and fetches their files. Dropping it stops the checks.
pub struct Updater {
    inner: Arc<Inner>,
}

struct Inner {
    handle: tokio::runtime::Handle,
    config: UpdateConfig,
    http: reqwest::Client,
    state: Mutex<State>,
    events: async_channel::Sender<UpdateStatus>,
    cancel: CancellationToken,
}

struct State {
    status: UpdateStatus,
    automatic: bool,
    /// A check is in flight: another is not started beside it.
    checking: bool,
}

impl Updater {
    /// Starts checking on `handle`'s runtime. The receiver yields every status change.
    pub fn start(
        handle: tokio::runtime::Handle,
        config: UpdateConfig,
    ) -> Result<(Self, async_channel::Receiver<UpdateStatus>), reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("lazy-client/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let (events, receiver) = async_channel::unbounded();
        let automatic = config.automatic;
        let inner = Arc::new(Inner {
            handle: handle.clone(),
            config,
            http,
            state: Mutex::new(State {
                status: UpdateStatus::Idle,
                automatic,
                checking: false,
            }),
            events,
            cancel: CancellationToken::new(),
        });
        handle.spawn(inner.clone().run());
        Ok((Self { inner }, receiver))
    }

    /// Where updating stands now.
    pub fn status(&self) -> UpdateStatus {
        self.inner.lock().status.clone()
    }

    /// Whether checks run on their own.
    pub fn automatic(&self) -> bool {
        self.inner.lock().automatic
    }

    /// Turns the regular checks on or off. Turning them on checks at once.
    pub fn set_automatic(&self, automatic: bool) {
        let was = std::mem::replace(&mut self.inner.lock().automatic, automatic);
        if automatic && !was {
            self.spawn_check(false);
        }
    }

    /// Checks now and says what was found, even if it is nothing. Does nothing while a check or
    /// a download is under way; a release already found is shown again.
    pub fn check_now(&self) {
        self.spawn_check(true);
    }

    fn spawn_check(&self, manual: bool) {
        let inner = self.inner.clone();
        self.inner
            .handle
            .spawn(async move { inner.check(manual).await });
    }

    /// Starts fetching the file of the release found (or of the one a fetch failed for). Does
    /// nothing in any other state.
    pub fn download(&self) {
        let inner = self.inner.clone();
        self.inner
            .handle
            .spawn(async move { inner.download().await });
    }
}

impl Drop for Updater {
    fn drop(&mut self) {
        self.inner.cancel.cancel();
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, status: UpdateStatus) {
        // Only a closed channel fails, and then nobody is listening.
        self.events.try_send(status).ok();
    }

    /// Makes `status` the status and tells the listener.
    fn set(&self, status: UpdateStatus) {
        self.lock().status = status.clone();
        self.send(status);
    }

    async fn run(self: Arc<Self>) {
        let mut delay = self.config.first_check;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = self.config.interval;
            let automatic = self.lock().automatic;
            if automatic {
                self.check(false).await;
            }
        }
    }

    async fn check(&self, manual: bool) {
        // What to say first, if anything. A fetch in progress or a finished one is never
        // disturbed by a check.
        let first = {
            let mut state = self.lock();
            let settled = matches!(
                state.status,
                UpdateStatus::Downloading { .. } | UpdateStatus::Ready { .. }
            );
            if state.checking || settled {
                return;
            }
            let known = matches!(
                state.status,
                UpdateStatus::Available(_)
                    | UpdateStatus::Failed {
                        update: Some(_),
                        ..
                    }
            );
            if manual && known {
                let status = state.status.clone();
                drop(state);
                self.send(status);
                return;
            }
            state.checking = true;
            if manual {
                state.status = UpdateStatus::Checking;
                Some(UpdateStatus::Checking)
            } else {
                None
            }
        };
        if let Some(status) = first {
            self.send(status);
        }

        let result = self.fetch_latest().await;

        let next = {
            let mut state = self.lock();
            state.checking = false;
            let settled = matches!(
                state.status,
                UpdateStatus::Downloading { .. } | UpdateStatus::Ready { .. }
            );
            let next = if settled {
                None
            } else {
                match result {
                    Ok(Some(found)) => {
                        if state.status.update() == Some(&found) {
                            None
                        } else {
                            Some(UpdateStatus::Available(found))
                        }
                    }
                    Ok(None) => {
                        if state.status == UpdateStatus::UpToDate {
                            None
                        } else {
                            Some(UpdateStatus::UpToDate)
                        }
                    }
                    Err(failure) if manual => Some(UpdateStatus::Failed {
                        update: None,
                        failure,
                    }),
                    // A check nobody asked for that fails is not worth interrupting anyone.
                    Err(_) => None,
                }
            };
            if let Some(status) = &next {
                state.status = status.clone();
            }
            next
        };
        if let Some(status) = next {
            self.send(status);
        }
    }

    /// The newest release's file for this install, if it is newer than the running version.
    async fn fetch_latest(&self) -> Result<Option<AvailableUpdate>, UpdateFailure> {
        let url = format!(
            "{}/latest/download/SHA256SUMS",
            self.config.releases_url.trim_end_matches('/')
        );
        let mut response = self
            .http
            .get(&url)
            .timeout(self.config.timeout)
            .send()
            .await
            .map_err(|error| {
                tracing::debug!(%error, "update check: the request failed");
                UpdateFailure::Unreachable
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            // Nothing has been published.
            return Ok(None);
        }
        if !status.is_success() {
            tracing::debug!(%status, "update check: the release page answered with an error");
            return Err(UpdateFailure::Unreachable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CHECKSUMS_BYTES)
        {
            return Err(UpdateFailure::Unreadable);
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() as u64 + chunk.len() as u64 > MAX_CHECKSUMS_BYTES {
                        return Err(UpdateFailure::Unreadable);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(%error, "update check: the answer was cut off");
                    return Err(UpdateFailure::Unreachable);
                }
            }
        }
        let text = String::from_utf8(body).map_err(|_| UpdateFailure::Unreadable)?;
        let sums = parse_checksums(&text);
        if sums.is_empty() {
            // Not a checksum list: a page from a proxy or a captive portal.
            return Err(UpdateFailure::Unreadable);
        }
        Ok(AvailableUpdate::newest(
            &sums,
            self.config.package,
            &self.config.current,
            &self.config.releases_url,
        ))
    }

    async fn download(&self) {
        let update = {
            let mut state = self.lock();
            let update = match &state.status {
                UpdateStatus::Available(update)
                | UpdateStatus::Failed {
                    update: Some(update),
                    ..
                } => update.clone(),
                _ => return,
            };
            state.status = UpdateStatus::Downloading {
                update: update.clone(),
                received: 0,
                total: None,
            };
            update
        };
        self.send(UpdateStatus::Downloading {
            update: update.clone(),
            received: 0,
            total: None,
        });
        let status = match self.fetch_file(&update).await {
            Ok(file) => {
                tracing::info!(version = %update.version, "update downloaded and checked");
                UpdateStatus::Ready { update, file }
            }
            Err(failure) => UpdateStatus::Failed {
                update: Some(update),
                failure,
            },
        };
        self.set(status);
    }

    /// Fetches the file of `update` into the download folder and returns where it is.
    async fn fetch_file(&self, update: &AvailableUpdate) -> Result<PathBuf, UpdateFailure> {
        let dir = self.config.download_dir.clone();
        fs::create_dir_all(&dir).map_err(|error| local("create the updates folder", &error))?;
        let target = dir.join(&update.file_name);
        let partial = dir.join(format!("{}.part", update.file_name));

        // A copy fetched earlier, if it is still whole.
        let earlier = target.clone();
        let wanted = update.sha256.clone();
        let reusable = tokio::task::spawn_blocking(move || file_matches(&earlier, &wanted))
            .await
            .unwrap_or(false);
        if reusable {
            return Ok(target);
        }

        let result = self.stream_to(&partial, update).await;
        if result.is_err() {
            fs::remove_file(&partial).ok();
        }
        result?;
        // `rename` over an existing file fails on Windows.
        fs::remove_file(&target).ok();
        fs::rename(&partial, &target)
            .map_err(|error| local("move the update into place", &error))?;
        remove_other_updates(&dir, &target);
        Ok(target)
    }

    /// Writes the body of `update`'s address to `partial`, checking its length and its sum.
    async fn stream_to(
        &self,
        partial: &Path,
        update: &AvailableUpdate,
    ) -> Result<(), UpdateFailure> {
        let mut response = self
            .http
            .get(&update.download_url)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(%error, "update download: the request failed");
                UpdateFailure::Unreachable
            })?;
        if !response.status().is_success() {
            tracing::warn!(status = %response.status(), "update download: refused");
            return Err(UpdateFailure::Unreachable);
        }
        let total = response.content_length();
        let mut file =
            File::create(partial).map_err(|error| local("create the download", &error))?;
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let mut last_sent = Instant::now();
        loop {
            let chunk =
                match tokio::time::timeout(self.config.stall_timeout, response.chunk()).await {
                    Ok(Ok(Some(chunk))) => chunk,
                    Ok(Ok(None)) => break,
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "update download: the connection broke");
                        return Err(UpdateFailure::Unreachable);
                    }
                    Err(_) => {
                        tracing::warn!("update download: nothing arrived for too long");
                        return Err(UpdateFailure::Unreachable);
                    }
                };
            hasher.update(&chunk);
            file.write_all(&chunk)
                .map_err(|error| local("write the download", &error))?;
            received += chunk.len() as u64;
            if last_sent.elapsed() >= Duration::from_millis(100) {
                last_sent = Instant::now();
                self.progress(update, received, total);
            }
        }
        if total.is_some_and(|total| total != received) {
            tracing::warn!(received, "update download: ended before its length");
            return Err(UpdateFailure::Unreachable);
        }
        file.sync_all()
            .map_err(|error| local("write the download", &error))?;
        drop(file);
        let digest = hasher.finalize();
        let found: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        if found != update.sha256 {
            tracing::warn!("update download: the checksum does not match the release's");
            return Err(UpdateFailure::Damaged);
        }
        Ok(())
    }

    /// Reports how far a fetch has come, unless it is no longer the status.
    fn progress(&self, update: &AvailableUpdate, received: u64, total: Option<u64>) {
        let status = UpdateStatus::Downloading {
            update: update.clone(),
            received,
            total,
        };
        {
            let mut state = self.lock();
            if !matches!(state.status, UpdateStatus::Downloading { .. }) {
                return;
            }
            state.status = status.clone();
        }
        self.send(status);
    }
}

/// Logs a failure on this computer and names it for the person in general terms.
fn local(what: &str, error: &std::io::Error) -> UpdateFailure {
    tracing::warn!(%error, "update: could not {what}");
    UpdateFailure::Local
}

/// Whether the file at `path` is there and hashes to `sha256`.
fn file_matches(path: &Path, sha256: &str) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return false,
        }
    }
    let digest = hasher.finalize();
    let found: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    found == sha256
}

/// Deletes the files in `dir` that are older updates or leftovers of failed fetches, except
/// `keep`. Anything else in the folder is left alone.
fn remove_other_updates(dir: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let ours = name.ends_with(".part")
            || Package::ALL
                .iter()
                .any(|package| package.version_in(name).is_some());
        if ours {
            fs::remove_file(&path).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn a_file_matches_only_its_own_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        assert!(!file_matches(&path, &sha(b"")), "a file that is not there");
        // Larger than the read buffer, so more than one read is hashed.
        let bytes: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        fs::write(&path, &bytes).unwrap();
        assert!(file_matches(&path, &sha(&bytes)));
        assert!(!file_matches(&path, &sha(&bytes[1..])));
        assert!(!file_matches(&path, "nope"));
    }

    #[test]
    fn older_updates_and_leftovers_are_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("lazy-chat_0.7.0_amd64.deb");
        for name in [
            "lazy-chat_0.7.0_amd64.deb",
            "lazy-chat_0.6.0_amd64.deb",
            "Lazy-Setup-0.6.0-windows-x64.exe",
            "lazy-chat_0.7.0_amd64.deb.part",
            "notes.txt",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        fs::create_dir(dir.path().join("lazy-chat_0.5.0_amd64.deb")).unwrap();
        remove_other_updates(dir.path(), &keep);
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "lazy-chat_0.5.0_amd64.deb",
                "lazy-chat_0.7.0_amd64.deb",
                "notes.txt"
            ],
            "the kept file, a folder, and a file that is not an update stay"
        );
    }

    #[test]
    fn a_configuration_starts_with_the_thirty_minute_rule() {
        let config = UpdateConfig::new(
            Version::new(0, 5, 0),
            Package::LinuxArchive,
            PathBuf::from("updates"),
        );
        assert_eq!(config.interval, Duration::from_secs(30 * 60));
        assert!(config.automatic);
        assert!(config.first_check < config.interval);
    }
}
