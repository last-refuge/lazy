//! Finding and fetching updates (D28), against an in-process stand-in for the release pages:
//! `latest/download/SHA256SUMS` and `download/v<version>/<file>`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use lazy_client_core::{Package, UpdateFailure, UpdateStatus, Version};
use lazy_client_runtime::async_channel::Receiver;
use lazy_client_runtime::{UpdateConfig, Updater};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

// ----- Stand-in for the release pages -----

#[derive(Default)]
struct Stub {
    /// The checksum list; `None` answers 404, as GitHub does when nothing is published.
    sums: Mutex<Option<String>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    /// Every request for the list is refused with a server error.
    broken: AtomicBool,
    sums_requests: AtomicUsize,
    file_requests: AtomicUsize,
}

async fn sums(State(stub): State<Arc<Stub>>) -> Response {
    stub.sums_requests.fetch_add(1, Ordering::SeqCst);
    if stub.broken.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let text = stub.sums.lock().unwrap().clone();
    match text {
        Some(text) => text.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn file(
    State(stub): State<Arc<Stub>>,
    Path((_tag, name)): Path<(String, String)>,
) -> Response {
    stub.file_requests.fetch_add(1, Ordering::SeqCst);
    let bytes = stub.files.lock().unwrap().get(&name).cloned();
    match bytes {
        Some(bytes) => bytes.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Several chunks' worth of bytes that differ by `seed`.
fn contents(seed: u8) -> Vec<u8> {
    (0..300_000u32).map(|n| (n % 253) as u8 ^ seed).collect()
}

/// Publishes `version` with `bytes` as its `package`; returns the file's name.
fn publish(stub: &Stub, version: &str, package: Package, bytes: &[u8]) -> String {
    let name = package.file_name(&Version::parse(version).unwrap());
    *stub.sums.lock().unwrap() = Some(format!("{}  {name}\n", sha(bytes)));
    stub.files
        .lock()
        .unwrap()
        .insert(name.clone(), bytes.to_vec());
    name
}

struct Rig {
    rt: tokio::runtime::Runtime,
    stub: Arc<Stub>,
    releases_url: String,
    dir: TempDir,
}

impl Rig {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let stub = Arc::new(Stub::default());
        let app = Router::new()
            .route("/releases/latest/download/SHA256SUMS", get(sums))
            .route("/releases/download/{tag}/{name}", get(file))
            .with_state(stub.clone());
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let releases_url = format!("http://{}/releases", listener.local_addr().unwrap());
        rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            rt,
            stub,
            releases_url,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn updates_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("updates")
    }

    /// A configuration that checks every 40 ms, once, 10 ms after starting.
    fn config(&self, current: &str, automatic: bool) -> UpdateConfig {
        let mut config = UpdateConfig::new(
            Version::parse(current).unwrap(),
            Package::LinuxDeb,
            self.updates_dir(),
        );
        config.releases_url = self.releases_url.clone();
        config.first_check = Duration::from_millis(10);
        config.interval = Duration::from_millis(40);
        config.timeout = Duration::from_secs(3);
        config.stall_timeout = Duration::from_secs(3);
        config.automatic = automatic;
        config
    }

    fn start(&self, config: UpdateConfig) -> (Updater, Receiver<UpdateStatus>) {
        Updater::start(self.rt.handle().clone(), config).expect("the updater starts")
    }

    /// The next status, within a deadline.
    fn next(&self, statuses: &Receiver<UpdateStatus>) -> UpdateStatus {
        self.rt
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), statuses.recv()).await
            })
            .expect("a status in time")
            .expect("the channel is open")
    }

    /// Nothing is said for `millis`.
    fn assert_quiet(&self, statuses: &Receiver<UpdateStatus>, millis: u64) {
        std::thread::sleep(Duration::from_millis(millis));
        if let Ok(status) = statuses.try_recv() {
            panic!("expected silence, got {status:?}");
        }
    }
}

fn available(status: UpdateStatus) -> lazy_client_core::AvailableUpdate {
    match status {
        UpdateStatus::Available(update) => update,
        other => panic!("expected an available update, got {other:?}"),
    }
}

#[test]
fn the_automatic_check_says_so_when_there_is_a_newer_release_and_then_says_nothing_more() {
    let rig = Rig::new();
    publish(&rig.stub, "0.5.0", Package::LinuxDeb, &contents(1));
    let (updater, statuses) = rig.start(rig.config("0.5.0", true));
    // Nothing newer: the status becomes "up to date", once, however often it checks.
    assert_eq!(rig.next(&statuses), UpdateStatus::UpToDate);
    rig.assert_quiet(&statuses, 300);
    assert!(
        rig.stub.sums_requests.load(Ordering::SeqCst) >= 3,
        "it kept checking"
    );

    publish(&rig.stub, "0.6.0", Package::LinuxDeb, &contents(2));
    let update = available(rig.next(&statuses));
    assert_eq!(update.version, Version::new(0, 6, 0));
    assert_eq!(update.file_name, "lazy-chat_0.6.0_amd64.deb");
    assert_eq!(
        update.download_url,
        format!(
            "{}/download/v0.6.0/lazy-chat_0.6.0_amd64.deb",
            rig.releases_url
        )
    );
    assert_eq!(updater.status(), UpdateStatus::Available(update));
    // The same release found again is not news.
    rig.assert_quiet(&statuses, 300);
}

#[test]
fn a_person_who_asks_is_always_answered() {
    let rig = Rig::new();
    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    assert_eq!(updater.status(), UpdateStatus::Idle);
    assert!(!updater.automatic());

    // Nothing published yet: GitHub answers 404.
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    assert_eq!(rig.next(&statuses), UpdateStatus::UpToDate);

    // A release newer than this one.
    publish(&rig.stub, "0.6.0", Package::LinuxDeb, &contents(1));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    let update = available(rig.next(&statuses));
    assert_eq!(update.version, Version::new(0, 6, 0));

    // Asking again shows what is known without asking GitHub again.
    let requests = rig.stub.sums_requests.load(Ordering::SeqCst);
    updater.check_now();
    assert_eq!(available(rig.next(&statuses)), update);
    assert_eq!(rig.stub.sums_requests.load(Ordering::SeqCst), requests);
    rig.assert_quiet(&statuses, 150);
}

#[test]
fn a_check_that_fails_is_told_only_to_the_person_who_asked() {
    let rig = Rig::new();
    publish(&rig.stub, "0.6.0", Package::LinuxDeb, &contents(1));
    rig.stub.broken.store(true, Ordering::SeqCst);

    // Automatic checks fail quietly.
    let (_quiet, quiet_statuses) = rig.start(rig.config("0.5.0", true));
    rig.assert_quiet(&quiet_statuses, 300);

    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    assert_eq!(
        rig.next(&statuses),
        UpdateStatus::Failed {
            update: None,
            failure: UpdateFailure::Unreachable
        }
    );

    // A page that is not a checksum list (a captive portal) is not "up to date" either.
    rig.stub.broken.store(false, Ordering::SeqCst);
    *rig.stub.sums.lock().unwrap() = Some("<html>Sign in to the network</html>".to_owned());
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    assert_eq!(
        rig.next(&statuses),
        UpdateStatus::Failed {
            update: None,
            failure: UpdateFailure::Unreadable
        }
    );

    // And asking once the network is back works.
    publish(&rig.stub, "0.6.0", Package::LinuxDeb, &contents(1));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    assert_eq!(
        available(rig.next(&statuses)).version,
        Version::new(0, 6, 0)
    );
}

#[test]
fn turning_the_checks_on_checks_at_once() {
    let rig = Rig::new();
    publish(&rig.stub, "0.6.0", Package::LinuxDeb, &contents(1));
    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    rig.assert_quiet(&statuses, 200);
    assert_eq!(
        rig.stub.sums_requests.load(Ordering::SeqCst),
        0,
        "off means off"
    );
    updater.set_automatic(true);
    assert!(updater.automatic());
    assert_eq!(
        available(rig.next(&statuses)).version,
        Version::new(0, 6, 0)
    );
}

#[test]
fn a_download_is_checked_against_the_releases_sum_and_kept() {
    let rig = Rig::new();
    let bytes = contents(7);
    let name = publish(&rig.stub, "0.6.0", Package::LinuxDeb, &bytes);
    // An older update and a leftover from an earlier failed fetch are cleared away.
    std::fs::create_dir_all(rig.updates_dir()).unwrap();
    std::fs::write(rig.updates_dir().join("lazy-chat_0.5.5_amd64.deb"), b"old").unwrap();
    std::fs::write(
        rig.updates_dir().join("lazy-chat_0.5.9_amd64.deb.part"),
        b"x",
    )
    .unwrap();

    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    let update = available(rig.next(&statuses));
    updater.download();
    let ready = loop {
        match rig.next(&statuses) {
            UpdateStatus::Downloading { update: during, .. } => assert_eq!(during, update),
            other => break other,
        }
    };
    let UpdateStatus::Ready { update: done, file } = ready else {
        panic!("expected the update to be ready, got {ready:?}");
    };
    assert_eq!(done, update);
    assert_eq!(file, rig.updates_dir().join(&name));
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    let mut left: Vec<String> = std::fs::read_dir(rig.updates_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, [name.clone()], "only the new file is kept");
    assert_eq!(updater.status(), UpdateStatus::Ready { update, file });

    // Once it is ready, checking does not disturb it.
    updater.check_now();
    rig.assert_quiet(&statuses, 150);
}

#[test]
fn a_file_that_does_not_match_its_sum_is_thrown_away_and_can_be_fetched_again() {
    let rig = Rig::new();
    let good = contents(3);
    let name = publish(&rig.stub, "0.6.0", Package::LinuxDeb, &good);
    // The release lists the sum of `good`, but the file served is something else.
    rig.stub
        .files
        .lock()
        .unwrap()
        .insert(name.clone(), contents(4));

    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    let update = available(rig.next(&statuses));
    updater.download();
    let failed = loop {
        match rig.next(&statuses) {
            UpdateStatus::Downloading { .. } => continue,
            other => break other,
        }
    };
    assert_eq!(
        failed,
        UpdateStatus::Failed {
            update: Some(update.clone()),
            failure: UpdateFailure::Damaged
        }
    );
    let entries = std::fs::read_dir(rig.updates_dir()).unwrap().count();
    assert_eq!(entries, 0, "neither the file nor a partial one is left");

    // The file is put right; trying again from the failure works.
    rig.stub.files.lock().unwrap().insert(name, good.clone());
    updater.download();
    let ready = loop {
        match rig.next(&statuses) {
            UpdateStatus::Downloading { .. } => continue,
            other => break other,
        }
    };
    let UpdateStatus::Ready { file, .. } = ready else {
        panic!("expected the update to be ready, got {ready:?}");
    };
    assert_eq!(std::fs::read(file).unwrap(), good);
}

#[test]
fn a_file_the_release_page_does_not_have_fails_as_unreachable() {
    let rig = Rig::new();
    let name = publish(&rig.stub, "0.6.0", Package::LinuxDeb, &contents(1));
    rig.stub.files.lock().unwrap().remove(&name);

    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    let update = available(rig.next(&statuses));
    updater.download();
    let failed = loop {
        match rig.next(&statuses) {
            UpdateStatus::Downloading { .. } => continue,
            other => break other,
        }
    };
    assert_eq!(
        failed,
        UpdateStatus::Failed {
            update: Some(update),
            failure: UpdateFailure::Unreachable
        }
    );
    // Asking again after a failure about a release shows that failure; it does not forget it.
    updater.check_now();
    assert!(matches!(
        rig.next(&statuses),
        UpdateStatus::Failed {
            update: Some(_),
            ..
        }
    ));
}

#[test]
fn a_copy_fetched_before_lazy_was_closed_is_used_again_without_fetching() {
    let rig = Rig::new();
    let bytes = contents(9);
    let name = publish(&rig.stub, "0.6.0", Package::LinuxDeb, &bytes);
    std::fs::create_dir_all(rig.updates_dir()).unwrap();
    std::fs::write(rig.updates_dir().join(&name), &bytes).unwrap();

    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    available(rig.next(&statuses));
    updater.download();
    let ready = loop {
        match rig.next(&statuses) {
            UpdateStatus::Downloading { .. } => continue,
            other => break other,
        }
    };
    assert!(matches!(ready, UpdateStatus::Ready { .. }), "{ready:?}");
    assert_eq!(rig.stub.file_requests.load(Ordering::SeqCst), 0);

    // A copy that is damaged is not trusted: it is fetched again.
    let rig = Rig::new();
    let name = publish(&rig.stub, "0.6.0", Package::LinuxDeb, &bytes);
    std::fs::create_dir_all(rig.updates_dir()).unwrap();
    std::fs::write(rig.updates_dir().join(&name), &bytes[1..]).unwrap();
    let (updater, statuses) = rig.start(rig.config("0.5.0", false));
    updater.check_now();
    assert_eq!(rig.next(&statuses), UpdateStatus::Checking);
    available(rig.next(&statuses));
    updater.download();
    let ready = loop {
        match rig.next(&statuses) {
            UpdateStatus::Downloading { .. } => continue,
            other => break other,
        }
    };
    let UpdateStatus::Ready { file, .. } = ready else {
        panic!("expected the update to be ready, got {ready:?}");
    };
    assert_eq!(std::fs::read(file).unwrap(), bytes);
    assert_eq!(rig.stub.file_requests.load(Ordering::SeqCst), 1);
}
