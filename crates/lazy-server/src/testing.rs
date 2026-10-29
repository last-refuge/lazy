//! Helpers for running a real server in-process, for end-to-end tests in this and other crates.
//!
//! Enabled by the `testing` feature.

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::app::serve_all;
use crate::config::{
    AttachmentBackend, AttachmentOptions, IntegrationOptions, PasswordCost, RetentionPolicy,
    SearchOptions, ServerOptions,
};
use crate::error::AppError;

/// A server bound to an ephemeral loopback port.
pub struct TestServer {
    /// `http://127.0.0.1:<port>`; append the paths from `lazy_protocol::api::paths`. For the
    /// WebSocket use the same address with a `ws://` scheme.
    pub base_url: String,
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), AppError>>>,
    /// Where this server keeps attachment files; removed with the server.
    attachments_dir: std::path::PathBuf,
    _scratch: Option<tempfile::TempDir>,
}

impl TestServer {
    /// The directory this server keeps attachment files in (a temporary one, unless the test
    /// gave one), for tests that look at the disk.
    pub fn attachments_dir(&self) -> &std::path::Path {
        &self.attachments_dir
    }

    /// Stops the server gracefully and waits for it to finish.
    pub async fn shutdown(mut self) -> Result<(), AppError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.task.take() {
            Some(task) => task
                .await
                .map_err(|e| AppError::internal(format!("server task failed: {e}")))?,
            None => Ok(()),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Best effort when a test forgets to call `shutdown`.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Starts a server on `pool` with cheap password hashing, open registration, and event pruning
/// off (so tests are not affected by a background pruner unless they ask for one).
///
/// The database must already be migrated; the default workspace is not seeded (call
/// [`crate::db::seed_default_workspace`] first, or create your own).
pub async fn spawn_test_server(pool: PgPool) -> TestServer {
    spawn_test_server_with(
        pool,
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await
}

/// Like [`spawn_test_server`] with explicit options.
pub async fn spawn_test_server_with(pool: PgPool, options: ServerOptions) -> TestServer {
    spawn_test_server_configured(pool, options, IntegrationOptions::default()).await
}

/// Like [`spawn_test_server_with`], also setting how integrations are addressed and limited.
pub async fn spawn_test_server_configured(
    pool: PgPool,
    options: ServerOptions,
    integrations: IntegrationOptions,
) -> TestServer {
    spawn_test_server_full(pool, options, integrations, AttachmentOptions::default()).await
}

/// Like [`spawn_test_server_configured`], also setting how attachments are stored and cleaned up.
/// A server whose options leave the backend as [`AttachmentBackend::Memory`] (the default) gets a
/// temporary directory of its own instead, so that tests exercise the real storage; give a
/// [`AttachmentBackend::Directory`] to choose one.
pub async fn spawn_test_server_full(
    pool: PgPool,
    options: ServerOptions,
    integrations: IntegrationOptions,
    attachments: AttachmentOptions,
) -> TestServer {
    spawn_test_server_all(
        pool,
        options,
        integrations,
        attachments,
        SearchOptions::default(),
    )
    .await
}

/// Like [`spawn_test_server_full`], also setting search's time limit and how many matches it ranks.
pub async fn spawn_test_server_all(
    pool: PgPool,
    options: ServerOptions,
    integrations: IntegrationOptions,
    mut attachments: AttachmentOptions,
    search: SearchOptions,
) -> TestServer {
    let scratch = match &attachments.backend {
        AttachmentBackend::Memory => {
            let dir = tempfile::tempdir().expect("a temporary attachments directory");
            attachments.backend = AttachmentBackend::Directory(dir.path().to_owned());
            Some(dir)
        }
        AttachmentBackend::Directory(_) => None,
    };
    let attachments_dir = match &attachments.backend {
        AttachmentBackend::Directory(dir) => dir.clone(),
        AttachmentBackend::Memory => std::path::PathBuf::new(),
    };
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("listener has a local address");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_all(
        listener,
        pool,
        options,
        integrations,
        attachments,
        search,
        async move {
            let _ = stopped.await;
        },
    ));
    TestServer {
        base_url: format!("http://{addr}"),
        addr,
        stop: Some(stop),
        task: Some(task),
        attachments_dir,
        _scratch: scratch,
    }
}
