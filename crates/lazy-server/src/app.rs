//! Shared state and the server entry points.

use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{Config, RetentionPolicy, ServerOptions};
use crate::crypto::PasswordService;
use crate::dispatcher::Dispatcher;
use crate::error::AppError;
use crate::{db, http, pruner};

/// Longest shutdown waits for open connections to finish after telling them to close.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

/// State shared by every request.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub passwords: PasswordService,
    pub dispatcher: Dispatcher,
    pub options: ServerOptions,
    /// Cancelled when the server begins shutting down; connections close in response.
    pub shutdown: CancellationToken,
    /// Tracks WebSocket connection tasks so shutdown can wait for them.
    pub tasks: TaskTracker,
}

/// Connects to Postgres, optionally migrates and seeds, and serves until `shutdown` completes.
///
/// On shutdown the server stops accepting connections, closes WebSockets with a close frame,
/// waits for them (bounded), stops the dispatcher and the pruner, and closes the pool.
pub async fn run(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AppError> {
    let pool = db::connect(&config.database_url, config.max_db_connections).await?;
    if config.migrate {
        db::migrate(&pool).await?;
    }
    if config.seed
        && let Some(seeded) = db::seed_default_workspace(&pool).await?
    {
        tracing::info!(
            workspace_id = %seeded.workspace_id,
            general_id = %seeded.general_id,
            "created the default workspace"
        );
    }
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| AppError::internal(format!("cannot listen on {}: {e}", config.listen)))?;
    tracing::info!(addr = %config.listen, "lazy-server listening");

    let options = ServerOptions {
        open_registration: config.open_registration,
        password_cost: config.password_cost,
        retention: RetentionPolicy::from_days(config.event_retention_days),
    };
    let result = serve(listener, pool.clone(), options, shutdown).await;
    pool.close().await;
    result
}

/// Serves on an already-bound listener against an existing pool, which is left open.
pub async fn serve(
    listener: TcpListener,
    pool: PgPool,
    options: ServerOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AppError> {
    let token = CancellationToken::new();
    let (dispatcher, dispatcher_tasks) = Dispatcher::spawn(pool.clone(), token.child_token());
    let pruner = pruner::spawn(pool.clone(), options.retention, token.child_token());
    let state = AppState {
        pool,
        passwords: PasswordService::new(options.password_cost)?,
        dispatcher,
        options,
        shutdown: token.clone(),
        tasks: TaskTracker::new(),
    };
    let tasks = state.tasks.clone();
    let app = http::router(state);

    let stop = {
        let token = token.clone();
        async move {
            shutdown.await;
            // Tell WebSocket connections to close before HTTP draining begins.
            token.cancel();
        }
    };
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(stop)
        .await;

    token.cancel();
    tasks.close();
    if tokio::time::timeout(SHUTDOWN_DRAIN, tasks.wait())
        .await
        .is_err()
    {
        tracing::warn!("connections did not close within the shutdown window");
    }
    dispatcher_tasks.join().await;
    pruner.join().await;
    served.map_err(|e| AppError::internal(format!("server error: {e}")))
}
