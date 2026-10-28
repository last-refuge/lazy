//! Helpers for running a real server in-process, for end-to-end tests in this and other crates.
//!
//! Enabled by the `testing` feature.

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::app::serve;
use crate::config::{PasswordCost, RetentionPolicy, ServerOptions};
use crate::error::AppError;

/// A server bound to an ephemeral loopback port.
pub struct TestServer {
    /// `http://127.0.0.1:<port>`; append the paths from `lazy_protocol::api::paths`. For the
    /// WebSocket use the same address with a `ws://` scheme.
    pub base_url: String,
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), AppError>>>,
}

impl TestServer {
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
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await
}

/// Like [`spawn_test_server`] with explicit options.
pub async fn spawn_test_server_with(pool: PgPool, options: ServerOptions) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("listener has a local address");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(serve(listener, pool, options, async move {
        let _ = stopped.await;
    }));
    TestServer {
        base_url: format!("http://{addr}"),
        addr,
        stop: Some(stop),
        task: Some(task),
    }
}
