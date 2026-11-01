//! Shared state and the server entry points.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use lazy_domain::workspace::{RegistrationPolicy, WorkspaceSettings};

use crate::attachments::Attachments;
use crate::config::{
    AttachmentBackend, AttachmentOptions, Config, IntegrationOptions, RetentionPolicy,
    SearchOptions, ServerOptions,
};
use crate::crypto::PasswordService;
use crate::dispatcher::Dispatcher;
use crate::error::AppError;
use crate::webhooks::WebhookLimiter;
use crate::{db, http, logging, pruner};

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
    pub integrations: Arc<Integrations>,
    pub attachments: Arc<Attachments>,
    pub search: SearchOptions,
}

/// What integrations need at run time.
pub struct Integrations {
    /// Where webhook addresses start, without a trailing slash.
    pub public_url: String,
    pub limiter: WebhookLimiter,
    /// See [`crate::SetupCode`].
    pub setup_code: Option<crate::config::SetupCode>,
    /// The keys integration credentials are sealed with (D31).
    pub credentials: crate::credentials::CredentialKeys,
    /// Wakes this process's job worker after an enqueue (does nothing if it runs none).
    pub jobs: crate::jobs::Waker,
}

/// The kinds of job this server knows how to run. Integrations add theirs here.
pub fn job_handlers() -> crate::jobs::Registry {
    crate::jobs::Registry::new()
}

/// Connects to Postgres, optionally migrates and seeds, and serves until `shutdown` completes.
///
/// On shutdown the server stops accepting connections, closes WebSockets with a close frame,
/// waits for them (bounded), stops the dispatcher and the pruner, and closes the pool.
pub async fn run(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AppError> {
    // The effective configuration, without anything secret in it: the database as host, port and
    // name only, the public address without credentials or path.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol_version = lazy_protocol::PROTOCOL_VERSION,
        listen = %config.listen,
        database = %logging::describe_database_url(&config.database_url),
        migrate = config.migrate,
        seed = config.seed,
        open_registration = config.open_registration,
        open_workspace_creation = config.open_workspace_creation,
        event_retention_days = config.event_retention_days,
        max_db_connections = config.max_db_connections,
        max_attachment_bytes = config.max_attachment_bytes,
        public_url = config.public_url.as_deref().map(logging::describe_url),
        seed_registration = match config.seed_registration {
            RegistrationPolicy::Open => "open",
            RegistrationPolicy::InviteOnly => "invite_only",
        },
        setup_code_set = config.setup_code.is_some(),
        credentials_key = config.credential_keys.current_id(),
        run_jobs = config.run_jobs,
        "starting"
    );
    let pool = db::connect(&config.database_url, config.max_db_connections).await?;
    if config.migrate {
        db::migrate(&pool).await?;
    }
    // Whatever applied them, the newest migration the database has.
    let migration_version: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .ok()
            .flatten();
    tracing::info!(migration_version, "database ready");
    if config.seed
        && let Some(seeded) = db::seed_default_workspace_with(
            &pool,
            WorkspaceSettings {
                registration: config.seed_registration,
                ..WorkspaceSettings::default()
            },
        )
        .await?
    {
        tracing::info!(
            workspace_id = %seeded.workspace_id,
            general_id = %seeded.general_id,
            "created the default workspace"
        );
    }
    // Say what the first visitor will find (the text is fixed; no code is ever shown).
    if db::default_workspace_is_empty(&pool).await? == Some(true) {
        if config.setup_code.is_some() {
            tracing::info!("waiting for the first account: registration needs the setup code");
        } else {
            tracing::warn!(
                "no setup code is set: the first person to register will own the workspace"
            );
        }
    }
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| AppError::internal(format!("cannot listen on {}: {e}", config.listen)))?;
    tracing::info!(addr = %config.listen, "lazy-server listening");

    let options = ServerOptions {
        open_registration: config.open_registration,
        open_workspace_creation: config.open_workspace_creation,
        password_cost: config.password_cost,
        retention: RetentionPolicy::from_days(config.event_retention_days),
    };
    let integrations = IntegrationOptions {
        public_url: config.public_url,
        setup_code: config.setup_code,
        credential_keys: config.credential_keys,
        jobs: config.run_jobs.then(crate::jobs::WorkerOptions::default),
        ..IntegrationOptions::default()
    };
    let attachments = AttachmentOptions {
        backend: AttachmentBackend::Directory(config.attachments_dir.clone()),
        max_bytes: config.max_attachment_bytes,
        ..AttachmentOptions::default()
    };
    tracing::info!(dir = %config.attachments_dir.display(), "attachments are stored here");
    let result = serve_full(
        listener,
        pool.clone(),
        options,
        integrations,
        attachments,
        shutdown,
    )
    .await;
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
    serve_with(
        listener,
        pool,
        options,
        IntegrationOptions::default(),
        shutdown,
    )
    .await
}

/// [`serve`] with settings for integrations (the address webhooks are shown under, and how fast
/// they may post).
pub async fn serve_with(
    listener: TcpListener,
    pool: PgPool,
    options: ServerOptions,
    integrations: IntegrationOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AppError> {
    serve_full(
        listener,
        pool,
        options,
        integrations,
        AttachmentOptions::default(),
        shutdown,
    )
    .await
}

/// [`serve_with`] with settings for attachments too (where their bytes are kept, the size limit,
/// and the timings of the cleanup jobs). The default keeps them in memory.
pub async fn serve_full(
    listener: TcpListener,
    pool: PgPool,
    options: ServerOptions,
    integrations: IntegrationOptions,
    attachment_options: AttachmentOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AppError> {
    serve_all(
        listener,
        pool,
        options,
        integrations,
        attachment_options,
        SearchOptions::default(),
        shutdown,
    )
    .await
}

/// [`serve_full`] with settings for search too (its time limit and how many matches it ranks).
pub async fn serve_all(
    listener: TcpListener,
    pool: PgPool,
    options: ServerOptions,
    integrations: IntegrationOptions,
    attachment_options: AttachmentOptions,
    search: SearchOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AppError> {
    let attachments = Arc::new(Attachments::open(attachment_options)?);
    let public_url = match integrations.public_url {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(|e| AppError::internal(format!("no local address: {e}")))?
        ),
    };
    let token = CancellationToken::new();
    let (dispatcher, dispatcher_tasks) = Dispatcher::spawn(pool.clone(), token.child_token());
    let pruner = pruner::spawn(pool.clone(), options.retention, token.child_token());
    let worker = integrations.jobs.map(|worker_options| {
        crate::jobs::spawn(
            pool.clone(),
            job_handlers(),
            worker_options,
            token.child_token(),
        )
    });
    let waker = worker
        .as_ref()
        .map_or_else(crate::jobs::Waker::none, crate::jobs::Worker::waker);
    let state = AppState {
        pool,
        passwords: PasswordService::new(options.password_cost)?,
        dispatcher,
        options,
        shutdown: token.clone(),
        tasks: TaskTracker::new(),
        integrations: Arc::new(Integrations {
            public_url,
            limiter: WebhookLimiter::new(integrations.webhook_rate),
            setup_code: integrations.setup_code,
            credentials: integrations.credential_keys,
            jobs: waker,
        }),
        attachments: Arc::clone(&attachments),
        search,
    };
    let attachment_jobs =
        crate::attachments::spawn_jobs(state.pool.clone(), attachments, token.child_token());
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
    attachment_jobs.join().await;
    if let Some(worker) = worker {
        worker.join().await;
    }
    served.map_err(|e| AppError::internal(format!("server error: {e}")))
}
