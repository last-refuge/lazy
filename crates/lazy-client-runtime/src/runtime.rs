//! The effect executor.
//!
//! # Ownership
//!
//! [`Runtime`] owns a small Tokio runtime and everything spawned on it:
//!
//! * Effect tasks (HTTP calls) live in a `JoinSet` and stop when the runtime's cancellation token
//!   fires. They are never retried by the runtime except reads that are safe to repeat.
//! * One realtime task per workspace, each with its own child token; stopped on sign-out.
//! * Two persistence tasks: a journal actor that applies journal commands in order, and a
//!   debounced saver for drafts and sync positions.
//!
//! [`Runtime::shutdown`] cancels the token, drains the persistence tasks (flushing pending
//! saves), awaits every task, and stops the Tokio runtime. A mutation interrupted by shutdown
//! stays in the journal and is reconciled on the next start.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use lazy_client_core::{ContentCache, Effect, Input, JournalEntry, RequestError, StartupState};
use lazy_domain::{ConversationId, EventSeq, OperationId, WorkspaceId};
use lazy_protocol::{
    BootstrapResponse, HistoryPage, HistoryQuery, OperationStatus, SendMessageRequest,
    SendMessageResponse, SessionResponse, paths,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::api::Api;
use crate::config::{RuntimeConfig, SecureStorageChoice};
use crate::realtime;
use crate::secure::{
    InsecureFileStorage, KeyringStorage, SecretVault, SecureStorage, StorageError,
};
use crate::store::{StateStore, StoreError};

/// Capacity of the channel carrying inputs to the host. A slow host applies backpressure to the
/// realtime connection rather than letting the queue grow.
const INPUT_QUEUE: usize = 4096;
/// How long shutdown waits for tasks before giving up on them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Why the runtime could not start.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("the server URL {0:?} must start with http:// or https://")]
    InvalidServerUrl(String),
    #[error("could not prepare the profile directory {path}: {source}")]
    Profile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not build the HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("could not start the async runtime: {0}")]
    Runtime(std::io::Error),
}

/// Executes the core's [`Effect`]s and reports results as [`Input`]s.
pub struct Runtime {
    inner: Arc<Inner>,
    tokio: Option<tokio::runtime::Runtime>,
}

struct Inner {
    handle: tokio::runtime::Handle,
    config: RuntimeConfig,
    api: Arc<Api>,
    vault: SecretVault,
    profile_dir: PathBuf,
    inputs: async_channel::Sender<Input>,
    cancel: CancellationToken,
    tasks: Mutex<JoinSet<()>>,
    subscriptions: Mutex<HashMap<WorkspaceId, Subscription>>,
    persistence: Mutex<Option<Arc<Persistence>>>,
}

struct Subscription {
    commands: mpsc::UnboundedSender<realtime::Command>,
    cancel: CancellationToken,
}

impl Runtime {
    /// Starts the runtime. Local state is loaded in the background; the first input the receiver
    /// yields is always [`Input::Started`].
    pub fn start(
        config: RuntimeConfig,
    ) -> Result<(Self, async_channel::Receiver<Input>), RuntimeError> {
        let server_url = config.server_url.trim().trim_end_matches('/').to_owned();
        if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
            return Err(RuntimeError::InvalidServerUrl(config.server_url));
        }
        let profile_dir = resolve_profile_dir(&config)?;
        let storage: Arc<dyn SecureStorage> = match &config.secure_storage {
            SecureStorageChoice::Keyring => match KeyringStorage::new() {
                Ok(storage) => Arc::new(storage),
                Err(error) => Arc::new(UnavailableStorage(error.to_string())),
            },
            SecureStorageChoice::InsecureFile => Arc::new(InsecureFileStorage::new(
                profile_dir.join("insecure-secrets"),
            )),
            SecureStorageChoice::Custom(storage) => storage.clone(),
        };
        let vault = SecretVault::new(storage, &profile_dir, &server_url);
        let api = Arc::new(Api::new(&server_url, config.request_timeout)?);

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("lazy-runtime")
            .enable_all()
            .build()
            .map_err(RuntimeError::Runtime)?;
        let (inputs, receiver) = async_channel::bounded(INPUT_QUEUE);
        let inner = Arc::new(Inner {
            handle: tokio.handle().clone(),
            config,
            api,
            vault,
            profile_dir,
            inputs,
            cancel: CancellationToken::new(),
            tasks: Mutex::new(JoinSet::new()),
            subscriptions: Mutex::new(HashMap::new()),
            persistence: Mutex::new(None),
        });

        let startup = inner.clone();
        inner.spawn_unguarded(async move { startup.start_up().await });
        Ok((
            Self {
                inner,
                tokio: Some(tokio),
            },
            receiver,
        ))
    }

    /// The directory holding this client's encrypted local state.
    pub fn profile_dir(&self) -> &Path {
        &self.inner.profile_dir
    }

    /// Starts the given effects. Never blocks; results arrive as inputs.
    pub fn execute(&self, effects: Vec<Effect>) {
        self.inner.reap();
        for effect in effects {
            self.inner.clone().execute_one(effect);
        }
    }

    /// Stops all work, flushes pending saves, and waits for tasks to finish. Call from outside
    /// any async context.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let Some(tokio) = self.tokio.take() else {
            return;
        };
        self.inner.cancel.cancel();
        let inner = self.inner.clone();
        tokio.block_on(async move {
            // Closing the journal channel lets its actor finish the commands already queued.
            let persistence = inner
                .persistence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(persistence) = persistence {
                persistence.close().await;
            }
            let mut tasks =
                std::mem::take(&mut *inner.tasks.lock().unwrap_or_else(|e| e.into_inner()));
            let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
                while let Some(joined) = tasks.join_next().await {
                    if let Err(error) = joined {
                        tracing::warn!(%error, "a runtime task failed");
                    }
                }
            })
            .await;
            if drained.is_err() {
                tracing::warn!("runtime tasks did not stop in time; aborting them");
                tasks.abort_all();
            }
        });
        tokio.shutdown_timeout(Duration::from_secs(2));
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    /// Spawns a task that stops when the runtime shuts down.
    fn spawn(self: &Arc<Self>, future: impl Future<Output = ()> + Send + 'static) {
        let cancel = self.cancel.clone();
        self.spawn_unguarded(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = future => {}
            }
        });
    }

    /// Spawns a task that manages its own shutdown.
    fn spawn_unguarded(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawn_on(future, &self.handle);
    }

    /// Observes finished tasks so a panic is logged rather than lost.
    fn reap(&self) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(joined) = tasks.try_join_next() {
            if let Err(error) = joined {
                tracing::error!(%error, "a runtime task failed");
            }
        }
    }

    async fn emit(&self, input: Input) {
        // A closed channel means the host is gone; there is nobody to tell.
        let _ = self.inputs.send(input).await;
    }

    fn persistence(&self) -> Option<Arc<Persistence>> {
        self.persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn start_up(self: Arc<Self>) {
        let loader = self.clone();
        let loaded = tokio::task::spawn_blocking(move || loader.load_local_state()).await;
        let (state, store) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                let state = StartupState {
                    problems: vec![format!("Local state could not be loaded: {error}")],
                    ..StartupState::default()
                };
                (state, None)
            }
        };
        if let Some(store) = store {
            let persistence = Persistence::start(&self, store);
            *self.persistence.lock().unwrap_or_else(|e| e.into_inner()) = Some(persistence);
        }
        self.emit(Input::Started(state)).await;
    }

    /// Reads the stored session and opens the encrypted state. Blocking.
    fn load_local_state(&self) -> (StartupState, Option<StateStore>) {
        let mut state = StartupState::default();
        match self.vault.session_token() {
            Ok(Some(token)) => {
                self.api.set_token(Some(token));
                state.session_restored = true;
            }
            Ok(None) => {}
            Err(error) => state.problems.push(storage_problem(&error)),
        }
        let store = match self.vault.state_key() {
            Ok(key) => {
                let (store, recovered) = StateStore::open(self.profile_dir.clone(), &key);
                state.journal = recovered.journal;
                state.drafts = recovered.drafts;
                state.cache = recovered.cache;
                state.problems.extend(recovered.problems);
                Some(store)
            }
            Err(error) => {
                if state.problems.is_empty() {
                    state.problems.push(storage_problem(&error));
                }
                None
            }
        };
        (state, store)
    }

    fn execute_one(self: Arc<Self>, effect: Effect) {
        match effect {
            Effect::Register(request) => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .api
                        .post::<SessionResponse, _>(paths::REGISTER, &request)
                        .await;
                    inner.finish_authentication(result).await;
                });
            }
            Effect::Login(request) => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .api
                        .post::<SessionResponse, _>(paths::LOGIN, &request)
                        .await;
                    inner.finish_authentication(result).await;
                });
            }
            Effect::SignOut => {
                let token = self.api.token();
                self.api.set_token(None);
                self.stop_subscriptions();
                let inner = self.clone();
                self.spawn_unguarded(async move {
                    if let Some(token) = token {
                        // Best effort: the server expires the session anyway.
                        let _ =
                            tokio::time::timeout(Duration::from_secs(3), inner.api.logout(&token))
                                .await;
                    }
                    if let Some(persistence) = inner.persistence() {
                        persistence.wipe().await;
                    }
                    let vault = inner.vault.clone();
                    let deleted = tokio::task::spawn_blocking(move || vault.delete_all()).await;
                    if let Ok(Err(error)) = deleted {
                        inner
                            .emit(Input::StorageProblem(storage_problem(&error)))
                            .await;
                    }
                });
            }
            Effect::ForgetSession => {
                self.api.set_token(None);
                self.stop_subscriptions();
                let vault = self.vault.clone();
                self.spawn_unguarded(async move {
                    let _ = tokio::task::spawn_blocking(move || vault.delete_session_token()).await;
                });
            }
            Effect::LoadBootstrap => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .retry_read(|| inner.api.get::<BootstrapResponse>(paths::BOOTSTRAP))
                        .await;
                    inner
                        .emit(match result {
                            Ok(response) => Input::BootstrapLoaded(Box::new(response)),
                            Err(error) => Input::BootstrapFailed(error),
                        })
                        .await;
                });
            }
            Effect::Subscribe { workspace, after } => self.subscribe(workspace, after),
            Effect::FetchHistory {
                conversation,
                before,
                limit,
                generation,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let query = HistoryQuery { before, limit };
                    let path = paths::messages(conversation);
                    let result = inner
                        .retry_read(|| inner.api.get_query::<HistoryPage, _>(&path, &query))
                        .await;
                    inner
                        .emit(Input::HistoryLoaded {
                            conversation,
                            generation,
                            result,
                        })
                        .await;
                });
            }
            Effect::PersistSubmission(entry) => {
                let operation = entry.operation_id;
                match self.persistence() {
                    Some(persistence) => persistence.submit(entry),
                    None => {
                        let inner = self.clone();
                        self.spawn(async move {
                            let message = "local storage is unavailable".to_owned();
                            inner
                                .emit(Input::SubmissionPersistFailed { operation, message })
                                .await;
                        });
                    }
                }
            }
            Effect::SendMessage {
                operation,
                conversation,
                document,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let request = SendMessageRequest {
                        operation_id: operation,
                        document,
                    };
                    // Exactly one attempt: a failure here is reconciled by the core, which asks
                    // the server whether the operation committed.
                    let result = inner
                        .api
                        .post::<SendMessageResponse, _>(&paths::messages(conversation), &request)
                        .await;
                    inner.emit(Input::SendResult { operation, result }).await;
                });
            }
            Effect::QueryOperationStatus { operation } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::operation(operation);
                    let result = inner
                        .retry_read(|| inner.api.get::<OperationStatus>(&path))
                        .await;
                    inner
                        .emit(Input::OperationStatusResult { operation, result })
                        .await;
                });
            }
            Effect::SettleSubmission { operation } => {
                if let Some(persistence) = self.persistence() {
                    persistence.settle(operation);
                }
            }
            Effect::PersistDrafts(drafts) => {
                if let Some(persistence) = self.persistence() {
                    persistence.queue_drafts(drafts);
                }
            }
            Effect::PersistCache(cache) => {
                if let Some(persistence) = self.persistence() {
                    persistence.queue_cache(*cache);
                }
            }
            Effect::DiscardCache => {
                if let Some(persistence) = self.persistence() {
                    persistence.discard_cache();
                }
            }
        }
    }

    async fn finish_authentication(&self, result: Result<SessionResponse, RequestError>) {
        match result {
            Ok(session) => {
                self.api.set_token(Some(session.token.clone()));
                let vault = self.vault.clone();
                let token = session.token.clone();
                let saved =
                    tokio::task::spawn_blocking(move || vault.set_session_token(&token)).await;
                if let Ok(Err(error)) = saved {
                    let message = format!(
                        "{} You will need to sign in again next time.",
                        storage_problem(&error)
                    );
                    self.emit(Input::StorageProblem(message)).await;
                }
                self.emit(Input::Authenticated { user: session.user }).await;
            }
            Err(error) => self.emit(Input::AuthFailed(error)).await,
        }
    }

    /// Repeats a read that is safe to repeat, with backoff, until it succeeds, is rejected, or
    /// attempts run out.
    async fn retry_read<T, F, Fut>(&self, mut call: F) -> Result<T, RequestError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, RequestError>>,
    {
        let attempts = self.config.read_attempts.max(1);
        let mut attempt = 1;
        loop {
            match call().await {
                Err(RequestError::Unknown(_)) if attempt < attempts => {
                    let delay = self.config.reconnect.delay(attempt, jitter());
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    fn subscribe(self: &Arc<Self>, workspace: WorkspaceId, after: EventSeq) {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = subscriptions.get(&workspace)
            && existing
                .commands
                .send(realtime::Command::Resubscribe(after))
                .is_ok()
        {
            return;
        }
        let (commands, receiver) = mpsc::unbounded_channel();
        let cancel = self.cancel.child_token();
        subscriptions.insert(
            workspace,
            Subscription {
                commands,
                cancel: cancel.clone(),
            },
        );
        let context = realtime::Context {
            api: self.api.clone(),
            inputs: self.inputs.clone(),
            backoff: self.config.reconnect,
            idle_timeout: self.config.idle_timeout,
        };
        self.spawn_unguarded(realtime::run(context, workspace, after, receiver, cancel));
    }

    fn stop_subscriptions(&self) {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        for (_, subscription) in subscriptions.drain() {
            subscription.cancel.cancel();
        }
    }
}

fn jitter() -> u32 {
    let mut bytes = [0u8; 4];
    match getrandom::fill(&mut bytes) {
        Ok(()) => u32::from_le_bytes(bytes),
        Err(_) => u32::MAX / 2,
    }
}

fn storage_problem(error: &StorageError) -> String {
    format!(
        "Secure storage problem: {error}. Drafts and recovery of unsent messages are unavailable; \
         set LAZY_INSECURE_STORAGE=1 to use plain files for development."
    )
}

fn resolve_profile_dir(config: &RuntimeConfig) -> Result<PathBuf, RuntimeError> {
    let dir = config
        .profile_dir
        .clone()
        .or_else(|| std::env::var_os("LAZY_PROFILE_DIR").map(PathBuf::from))
        .or_else(|| {
            directories::ProjectDirs::from("app", "lazy", "Lazy")
                .map(|dirs| dirs.data_local_dir().to_owned())
        })
        .unwrap_or_else(|| std::env::temp_dir().join("lazy-profile"));
    std::fs::create_dir_all(&dir).map_err(|source| RuntimeError::Profile {
        path: dir.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    std::fs::canonicalize(&dir).map_err(|source| RuntimeError::Profile { path: dir, source })
}

/// Stand-in used when the credential store cannot be initialized; every operation reports why.
struct UnavailableStorage(String);

impl SecureStorage for UnavailableStorage {
    fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::Unavailable(self.0.clone()))
    }
    fn set(&self, _key: &str, _value: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unavailable(self.0.clone()))
    }
    fn delete(&self, _key: &str) -> Result<(), StorageError> {
        Err(StorageError::Unavailable(self.0.clone()))
    }
}

// ----- Persistence -----

enum JournalCommand {
    Submit(JournalEntry),
    Settle(OperationId),
    DiscardCache,
    Wipe(oneshot::Sender<()>),
}

/// Pending drafts and cache, replaced rather than queued so they cannot grow.
#[derive(Default)]
struct PendingSaves {
    drafts: Option<Vec<(ConversationId, String)>>,
    cache: Option<ContentCache>,
}

/// Owns the on-disk state and the tasks that write it.
struct Persistence {
    store: Arc<Mutex<StateStore>>,
    journal: Mutex<Option<mpsc::UnboundedSender<JournalCommand>>>,
    pending: Mutex<PendingSaves>,
    dirty: Notify,
    /// Incremented by a wipe so an in-flight save cannot resurrect deleted files.
    epoch: std::sync::atomic::AtomicU64,
    inputs: async_channel::Sender<Input>,
    debounce: Duration,
    cancel: CancellationToken,
}

impl Persistence {
    fn start(inner: &Arc<Inner>, store: StateStore) -> Arc<Self> {
        let (journal_tx, journal_rx) = mpsc::unbounded_channel();
        let persistence = Arc::new(Self {
            store: Arc::new(Mutex::new(store)),
            journal: Mutex::new(Some(journal_tx)),
            pending: Mutex::new(PendingSaves::default()),
            dirty: Notify::new(),
            epoch: std::sync::atomic::AtomicU64::new(0),
            inputs: inner.inputs.clone(),
            debounce: inner.config.save_debounce,
            cancel: inner.cancel.clone(),
        });
        inner.spawn_unguarded(persistence.clone().run_journal(journal_rx));
        inner.spawn_unguarded(persistence.clone().run_saver());
        persistence
    }

    fn send(&self, command: JournalCommand) -> bool {
        self.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|tx| tx.send(command).is_ok())
    }

    fn submit(&self, entry: JournalEntry) {
        let operation = entry.operation_id;
        if !self.send(JournalCommand::Submit(entry)) {
            let _ = self.inputs.try_send(Input::SubmissionPersistFailed {
                operation,
                message: "the client is shutting down".into(),
            });
        }
    }

    fn settle(&self, operation: OperationId) {
        self.send(JournalCommand::Settle(operation));
    }

    fn queue_drafts(&self, drafts: Vec<(ConversationId, String)>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drafts = Some(drafts);
        self.dirty.notify_one();
    }

    /// Queues the content cache for a debounced write. A newer cache replaces one still waiting,
    /// so a burst of changes costs one write, and what is written is always a whole cache.
    fn queue_cache(&self, cache: ContentCache) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).cache = Some(cache);
        self.dirty.notify_one();
    }

    /// Deletes the saved cache and drops any write of it that is still waiting.
    fn discard_cache(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).cache = None;
        self.send(JournalCommand::DiscardCache);
    }

    fn lock_store(&self) -> MutexGuard<'_, StateStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Applies journal commands in order. The acknowledgment for a submission is sent only after
    /// its data has been flushed to disk.
    async fn run_journal(self: Arc<Self>, mut commands: mpsc::UnboundedReceiver<JournalCommand>) {
        while let Some(command) = commands.recv().await {
            let this = self.clone();
            match command {
                JournalCommand::Submit(entry) => {
                    let operation = entry.operation_id;
                    let result =
                        tokio::task::spawn_blocking(move || this.lock_store().add_journal(entry))
                            .await;
                    let input = match result {
                        Ok(Ok(())) => Input::SubmissionPersisted { operation },
                        Ok(Err(error)) => Input::SubmissionPersistFailed {
                            operation,
                            message: error.to_string(),
                        },
                        Err(error) => Input::SubmissionPersistFailed {
                            operation,
                            message: format!("storage task failed: {error}"),
                        },
                    };
                    let _ = self.inputs.send(input).await;
                }
                JournalCommand::Settle(operation) => {
                    let result =
                        tokio::task::spawn_blocking(move || this.lock_store().settle(operation))
                            .await;
                    if let Ok(Err(error)) = result {
                        let message =
                            format!("Couldn't update the unsent-message journal: {error}");
                        let _ = self.inputs.send(Input::StorageProblem(message)).await;
                    }
                }
                JournalCommand::DiscardCache => {
                    let _ = tokio::task::spawn_blocking(move || this.lock_store().discard_cache())
                        .await;
                }
                JournalCommand::Wipe(done) => {
                    self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    *self.pending.lock().unwrap_or_else(|e| e.into_inner()) =
                        PendingSaves::default();
                    let _ = tokio::task::spawn_blocking(move || this.lock_store().wipe()).await;
                    let _ = done.send(());
                }
            }
        }
    }

    /// Writes drafts and the cache a short while after they change, coalescing bursts.
    async fn run_saver(self: Arc<Self>) {
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    self.flush_pending().await;
                    return;
                }
                () = self.dirty.notified() => {
                    tokio::select! {
                        () = self.cancel.cancelled() => {
                            self.flush_pending().await;
                            return;
                        }
                        () = tokio::time::sleep(self.debounce) => self.flush_pending().await,
                    }
                }
            }
        }
    }

    async fn flush_pending(self: &Arc<Self>) {
        let epoch = self.epoch.load(std::sync::atomic::Ordering::SeqCst);
        let (drafts, cache) = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            (pending.drafts.take(), pending.cache.take())
        };
        if drafts.is_none() && cache.is_none() {
            return;
        }
        let this = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut store = this.lock_store();
            // A wipe since this batch was taken means the files were deleted on purpose.
            if this.epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch {
                return Ok(());
            }
            if let Some(drafts) = drafts {
                store.save_drafts(&drafts)?;
            }
            if let Some(cache) = cache {
                store.save_cache(cache)?;
            }
            Ok::<(), StoreError>(())
        })
        .await;
        if let Ok(Err(error)) = result {
            let message = format!("Couldn't save local state: {error}");
            let _ = self.inputs.send(Input::StorageProblem(message)).await;
        }
    }

    async fn wipe(&self) {
        let (done, wait) = oneshot::channel();
        if self.send(JournalCommand::Wipe(done)) {
            let _ = wait.await;
        }
    }

    /// Stops accepting journal commands; the journal actor finishes those already queued.
    async fn close(&self) {
        self.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

/// Reads a profile's saved state directly from disk. For tests that verify what is durable.
#[doc(hidden)]
pub fn inspect_state(profile_dir: &Path, key: &[u8; 32]) -> StartupState {
    let (_, recovered) = StateStore::open(profile_dir.to_owned(), key);
    StartupState {
        session_restored: false,
        journal: recovered.journal,
        drafts: recovered.drafts,
        cache: recovered.cache,
        problems: recovered.problems,
    }
}
