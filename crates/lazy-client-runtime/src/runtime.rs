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

use lazy_client_core::{
    ActionKind, ActionOutcome, ContentCache, CreatedWebhook, Effect, EmojiPrefs, Input,
    JournalEntry, RequestError, SecretUrl, ServerInfo, ServerSource, StartupState, ThreadDraft,
    ViewState,
};
use lazy_domain::{ConversationId, EventSeq, OperationId, WorkspaceId};
use lazy_protocol::{
    BootstrapResponse, BrowseChannelsResponse, ConversationMembersResponse, ConversationResponse,
    GroupResponse, HistoryPage, HistoryQuery, InboxPage, InboxReadResponse, MarkReadRequest,
    MessageResponse, NotificationSettingsResponse, OperationStatus, PeoplePage, PersonResponse,
    SavedPage, SavedResponse, SendMessageRequest, SendMessageResponse, SessionResponse,
    SetFavoriteRequest, StatusResponse, UserStateResponse, paths,
};
use lazy_protocol::{MarkThreadReadRequest, SetThreadFollowRequest, ThreadStateResponse};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::api::Api;
use crate::config::{RuntimeConfig, SecureStorageChoice};
use crate::prefs;
use crate::realtime;
use crate::recent_conversations;
use crate::searches;
use crate::secure::{
    InsecureFileStorage, KeyringStorage, SecretVault, SecureStorage, StorageError,
};
use crate::store::{StateStore, StoreError};

mod transfer;
use transfer::{FileServices, Transfers};

/// Capacity of the channel carrying inputs to the host. A slow host applies backpressure to the
/// realtime connection rather than letting the queue grow.
const INPUT_QUEUE: usize = 4096;
/// The one read of a kind that is wanted: its number and the means to hang up on it.
type ReadSlot = Mutex<Option<(u64, CancellationToken)>>;

/// How often the core is told the time when nothing else is happening.
const CLOCK_EVERY: Duration = Duration::from_secs(30);
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
    /// The secrets of this profile on the server in use; replaced when the server is changed.
    vault: Mutex<SecretVault>,
    /// The server in use and where its address came from.
    server: Mutex<(String, ServerSource)>,
    profile_dir: PathBuf,
    inputs: async_channel::Sender<Input>,
    cancel: CancellationToken,
    tasks: Mutex<JoinSet<()>>,
    subscriptions: Mutex<HashMap<WorkspaceId, Subscription>>,
    persistence: Mutex<Option<Arc<Persistence>>>,
    prefs: PrefsSaver,
    /// The outbox and image cache, once the profile's key is known.
    files: Mutex<Option<Arc<FileServices>>>,
    /// Uploads, downloads, and copies in flight.
    transfers: Arc<Mutex<Transfers>>,
    /// The search being waited for. There is one: a newer search or a cancel stops it.
    search: ReadSlot,
    /// The same for the thread inbox.
    inbox: ReadSlot,
    /// And for the saved messages, the people directory, a profile, and the channel directory.
    saved: ReadSlot,
    people: ReadSlot,
    profile: ReadSlot,
    browse: ReadSlot,
    /// And for the workspace's invitations.
    invitations: ReadSlot,
}

/// The emoji preferences waiting to be written. Kept apart from [`Persistence`] because they
/// need no key: they are saved even where the encrypted state cannot be.
#[derive(Default)]
struct PrefsSaver {
    /// The newest preferences not yet written; a newer one replaces an older.
    pending: Mutex<Option<EmojiPrefs>>,
    /// The same for the recent searches.
    pending_searches: Mutex<Option<Vec<String>>>,
    /// And for the recently visited conversations.
    pending_conversations: Mutex<Option<Vec<ConversationId>>>,
    /// The active workspace (deleted at sign-out) and the device's appearance (kept).
    pending_active: Mutex<Option<Option<WorkspaceId>>>,
    pending_appearance: Mutex<Option<lazy_client_core::Appearance>>,
    dirty: Notify,
    /// Incremented by sign-out so a write in flight cannot bring the file back.
    epoch: Arc<std::sync::atomic::AtomicU64>,
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
        let configured = config.server_url.trim().trim_end_matches('/').to_owned();
        if !(configured.starts_with("http://") || configured.starts_with("https://")) {
            return Err(RuntimeError::InvalidServerUrl(config.server_url));
        }
        let profile_dir = resolve_profile_dir(&config)?;
        // The environment (or whoever started the runtime) wins, then what the person chose in
        // the application, then the default.
        let (server_url, source) = if config.server_locked {
            (configured, ServerSource::Environment)
        } else if let Some(saved) = crate::saved_server::load(&profile_dir) {
            (saved, ServerSource::Saved)
        } else {
            (configured, ServerSource::Default)
        };
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
            vault: Mutex::new(vault),
            server: Mutex::new((server_url, source)),
            profile_dir,
            inputs,
            cancel: CancellationToken::new(),
            tasks: Mutex::new(JoinSet::new()),
            subscriptions: Mutex::new(HashMap::new()),
            persistence: Mutex::new(None),
            prefs: PrefsSaver::default(),
            files: Mutex::new(None),
            transfers: Arc::new(Mutex::new(Transfers::default())),
            search: Mutex::new(None),
            inbox: Mutex::new(None),
            saved: Mutex::new(None),
            people: Mutex::new(None),
            profile: Mutex::new(None),
            browse: Mutex::new(None),
            invitations: Mutex::new(None),
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
                        tracing::warn!(reason = task_failure(&error), "a runtime task failed");
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
                tracing::error!(reason = task_failure(&error), "a runtime task failed");
            }
        }
    }

    async fn emit(&self, input: Input) {
        // A closed channel means the host is gone; there is nobody to tell.
        let _ = self.inputs.send(input).await;
    }

    /// The secrets of this profile on the server in use.
    fn vault(&self) -> SecretVault {
        self.vault.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The server in use, as `Started` and `ServerSwitched` carry it.
    fn server_info(&self) -> ServerInfo {
        let (url, source) = self
            .server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        ServerInfo {
            url,
            source,
            ..ServerInfo::default()
        }
    }

    fn persistence(&self) -> Option<Arc<Persistence>> {
        self.persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Writes the emoji preferences a moment after the last change, and once more on shutdown.
    async fn run_prefs_saver(self: Arc<Self>) {
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    self.save_prefs().await;
                    return;
                }
                () = self.prefs.dirty.notified() => {
                    tokio::select! {
                        () = self.cancel.cancelled() => {
                            self.save_prefs().await;
                            return;
                        }
                        () = tokio::time::sleep(self.config.save_debounce) => self.save_prefs().await,
                    }
                }
            }
        }
    }

    /// Tells the core the time every 30 seconds (and with realtime frames, in `realtime.rs`).
    async fn run_clock(self: Arc<Self>) {
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return,
                () = tokio::time::sleep(CLOCK_EVERY) => {
                    self.emit(Input::Clock(lazy_domain::Timestamp::now())).await;
                }
            }
        }
    }

    async fn save_prefs(&self) {
        let saved = self
            .prefs
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let searches = self
            .prefs
            .pending_searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let conversations = self
            .prefs
            .pending_conversations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let active = self
            .prefs
            .pending_active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let appearance = self
            .prefs
            .pending_appearance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if saved.is_none()
            && searches.is_none()
            && conversations.is_none()
            && active.is_none()
            && appearance.is_none()
        {
            return;
        }
        let epoch = self.prefs.epoch.load(std::sync::atomic::Ordering::SeqCst);
        let current = self.prefs.epoch.clone();
        let dir = self.profile_dir.clone();
        let written = tokio::task::spawn_blocking(move || {
            // Signing out since this was taken means the files were deleted on purpose.
            let signed_out = || current.load(std::sync::atomic::Ordering::SeqCst) != epoch;
            // The device's appearance is not the account's: sign-out leaves it, and so does a
            // save that was waiting.
            if let Some(appearance) = appearance
                && let Err(error) = crate::appearance::save(&dir, appearance)
            {
                tracing::warn!(%error, "could not save the appearance");
            }
            if signed_out() {
                return;
            }
            if let Some(active) = active
                && let Err(error) = crate::active_workspace::save(&dir, active)
            {
                tracing::warn!(%error, "could not save the active workspace");
            }
            // A convenience, not worth an alarm: the next change tries again.
            if let Some(saved) = saved
                && let Err(error) = prefs::save(&dir, &saved)
            {
                tracing::warn!(%error, "could not save the emoji preferences");
            }
            if let Some(searches) = searches
                && !signed_out()
                && let Err(error) = searches::save(&dir, &searches)
            {
                tracing::warn!(%error, "could not save the recent searches");
            }
            if let Some(conversations) = conversations
                && !signed_out()
                && let Err(error) = recent_conversations::save(&dir, &conversations)
            {
                tracing::warn!(%error, "could not save the recent conversations");
            }
        })
        .await;
        if let Err(error) = written {
            tracing::warn!(
                reason = task_failure(&error),
                "saving the preferences failed"
            );
        }
    }

    /// Deletes the saved emoji preferences and recent searches (sign-out) and drops any save
    /// still waiting.
    async fn forget_prefs(&self) {
        self.prefs
            .epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.prefs
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.prefs
            .pending_searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.prefs
            .pending_conversations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.prefs
            .pending_active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let dir = self.profile_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            prefs::remove(&dir);
            searches::remove(&dir);
            recent_conversations::remove(&dir);
            crate::active_workspace::remove(&dir);
        })
        .await;
    }

    async fn start_up(self: Arc<Self>) {
        let loader = self.clone();
        let loaded = tokio::task::spawn_blocking(move || loader.load_local_state()).await;
        let (state, store, files) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                let state = StartupState {
                    problems: vec![format!(
                        "Local state could not be loaded ({}).",
                        task_failure(&error)
                    )],
                    ..StartupState::default()
                };
                (state, None, None)
            }
        };
        self.install_local_state(files, store);
        self.spawn_unguarded(self.clone().run_prefs_saver());
        self.spawn_unguarded(self.clone().run_clock());
        self.emit(Input::Started(state)).await;
    }

    /// Starts using the opened state: the files' services and the journal.
    fn install_local_state(
        self: &Arc<Self>,
        files: Option<Arc<FileServices>>,
        store: Option<StateStore>,
    ) {
        *self.files.lock().unwrap_or_else(|e| e.into_inner()) = files;
        let persistence = store.map(|store| Persistence::start(self, store));
        *self.persistence.lock().unwrap_or_else(|e| e.into_inner()) = persistence;
    }

    /// `Effect::UseServer`: saves the address, deletes the local state and the secrets of the
    /// server left (a session and a profile's state belong to one server), points requests and
    /// realtime connections at the new one, and loads what its profile holds.
    async fn switch_server(self: &Arc<Self>, url: String) -> Result<Box<StartupState>, String> {
        if self.config.server_locked {
            return Err("the server is set by the environment".to_owned());
        }
        let url = lazy_client_core::normalize_server_url(&url)
            .map_err(|_| "that isn't a server address".to_owned())?;
        let dir = self.profile_dir.clone();
        let saving = url.clone();
        let saved =
            tokio::task::spawn_blocking(move || crate::saved_server::save(&dir, &saving)).await;
        if !matches!(saved, Ok(Ok(()))) {
            return Err("the choice could not be saved".to_owned());
        }
        // Nothing of the old server goes to the new one: not its session, not its messages.
        self.api.set_token(None);
        self.stop_subscriptions();
        self.cancel_search();
        self.cancel_slot(&self.inbox);
        self.cancel_slot(&self.saved);
        self.cancel_slot(&self.people);
        self.cancel_slot(&self.profile);
        self.cancel_slot(&self.browse);
        self.cancel_slot(&self.invitations);
        self.forget_files().await;
        if let Some(persistence) = self.persistence() {
            persistence.wipe().await;
        }
        self.forget_prefs().await;
        let old = self.vault();
        let deleted = tokio::task::spawn_blocking(move || old.delete_all()).await;
        if !matches!(deleted, Ok(Ok(()))) {
            tracing::warn!("the secrets of the server left could not all be deleted");
        }
        self.api.set_base(&url);
        let vault = self.vault().for_server(&self.profile_dir, &url);
        *self.vault.lock().unwrap_or_else(|e| e.into_inner()) = vault;
        *self.server.lock().unwrap_or_else(|e| e.into_inner()) = (url, ServerSource::Saved);
        tracing::info!("now using another server");
        self.reopen_local_state().await.map(Box::new)
    }

    /// Opens the local state again under the vault's current key, which is a new one after the
    /// old key was deleted (sign-out, or a change of server): what is saved from then on has to be
    /// readable at the next start, and the old key is gone. The journal still open is closed.
    async fn reopen_local_state(self: &Arc<Self>) -> Result<StartupState, String> {
        if let Some(persistence) = self.persistence() {
            persistence.close().await;
        }
        let loader = self.clone();
        let loaded = tokio::task::spawn_blocking(move || loader.load_local_state()).await;
        let (state, store, files) = loaded.map_err(|error| {
            format!("local state could not be loaded ({})", task_failure(&error))
        })?;
        self.install_local_state(files, store);
        Ok(state)
    }

    /// Reads the stored session and opens the encrypted state. Blocking.
    fn load_local_state(&self) -> (StartupState, Option<StateStore>, Option<Arc<FileServices>>) {
        let mut state = StartupState {
            server: self.server_info(),
            emoji_prefs: prefs::load(&self.profile_dir),
            recent_searches: searches::load(&self.profile_dir),
            recent_conversations: recent_conversations::load(&self.profile_dir),
            appearance: crate::appearance::load(&self.profile_dir),
            active_workspace: crate::active_workspace::load(&self.profile_dir),
            ..StartupState::default()
        };
        match self.vault().session_token() {
            Ok(Some(token)) => {
                self.api.set_token(Some(token));
                state.session_restored = true;
            }
            Ok(None) => {}
            Err(error) => state.problems.push(storage_problem(&error)),
        }
        let mut files = None;
        let store = match self.vault().state_key() {
            Ok(key) => {
                let (store, recovered) = StateStore::open(self.profile_dir.clone(), &key);
                // Copies of files the journal does not name are left over from an interrupted
                // run. If the journal could not be read, whatever it named is kept.
                let keep: std::collections::HashSet<_> = recovered
                    .journal
                    .iter()
                    .flat_map(|entry| entry.attachments.iter().map(|file| file.id))
                    .collect();
                files = Some(FileServices::open(
                    &self.profile_dir,
                    &key,
                    store.journal_is_trusted().then_some(&keep),
                    self.inputs.clone(),
                    self.transfers.clone(),
                    self.cancel.clone(),
                ));
                state.journal = recovered.journal;
                state.drafts = recovered.drafts;
                state.thread_drafts = recovered.thread_drafts;
                state.view = recovered.view;
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
        (state, store, files)
    }

    fn execute_one(self: Arc<Self>, effect: Effect) {
        // The effect's `Debug` keeps names and sizes and drops what was typed.
        tracing::trace!(?effect, "executing an effect");
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
                self.cancel_search();
                self.cancel_slot(&self.inbox);
                self.cancel_slot(&self.saved);
                self.cancel_slot(&self.people);
                self.cancel_slot(&self.profile);
                self.cancel_slot(&self.browse);
                self.cancel_slot(&self.invitations);
                let inner = self.clone();
                self.spawn_unguarded(async move {
                    if let Some(token) = token {
                        // Best effort: the server expires the session anyway.
                        let _ =
                            tokio::time::timeout(Duration::from_secs(3), inner.api.logout(&token))
                                .await;
                    }
                    inner.forget_files().await;
                    if let Some(persistence) = inner.persistence() {
                        persistence.wipe().await;
                    }
                    inner.forget_prefs().await;
                    let vault = inner.vault();
                    let deleted = tokio::task::spawn_blocking(move || vault.delete_all()).await;
                    if let Ok(Err(error)) = deleted {
                        inner
                            .emit(Input::StorageProblem(storage_problem(&error)))
                            .await;
                    }
                    // The key the state was encrypted with went with the secrets: whoever signs
                    // in next needs a new one before anything is saved, or what is saved cannot
                    // be read at the next start.
                    let _ = inner.reopen_local_state().await;
                });
            }
            Effect::CheckServer { url, generation } => {
                let inner = self.clone();
                let timeout = self.config.request_timeout.min(Duration::from_secs(8));
                self.spawn(async move {
                    let result = crate::api::probe(&url, timeout).await;
                    inner
                        .emit(Input::ServerChecked { generation, result })
                        .await;
                });
            }
            Effect::UseServer { url, generation } => {
                let inner = self.clone();
                self.spawn_unguarded(async move {
                    let result = inner.switch_server(url).await;
                    inner
                        .emit(Input::ServerSwitched { generation, result })
                        .await;
                });
            }
            Effect::ForgetSession => {
                self.api.set_token(None);
                self.stop_subscriptions();
                let vault = self.vault();
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
            Effect::Unsubscribe { workspace } => self.unsubscribe(workspace),
            Effect::CreateWorkspace { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: the core repeats the (idempotent) request with the same id.
                    let result = inner
                        .api
                        .put::<lazy_protocol::JoinedWorkspace, _>(
                            &paths::workspace(workspace),
                            &request,
                        )
                        .await
                        .map(|joined| ActionOutcome::Joined(Box::new(joined.workspace)));
                    let kind = ActionKind::CreateWorkspace;
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::AcceptInvitation { request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: an invitation's use is counted.
                    let result = inner
                        .api
                        .post::<lazy_protocol::JoinedWorkspace, _>(
                            paths::ACCEPT_INVITATION_ROUTE,
                            &request,
                        )
                        .await
                        .map(|joined| ActionOutcome::Joined(Box::new(joined.workspace)));
                    let kind = ActionKind::AcceptInvitation;
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::UpdateProfile { request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Idempotent: a lost answer is simply asked again.
                    let result = inner
                        .retry_read(|| {
                            inner.api.patch::<lazy_protocol::ProfileResponse, _>(
                                paths::ME_ROUTE,
                                &request,
                            )
                        })
                        .await
                        .map(|response| ActionOutcome::Profile(response.user));
                    let kind = ActionKind::UpdateProfile;
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::SetAvatar { source } => self.set_avatar(source),
            Effect::RemoveAvatar => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .retry_read(|| {
                            inner
                                .api
                                .delete::<lazy_protocol::ProfileResponse>(paths::ME_AVATAR_ROUTE)
                        })
                        .await
                        .map(|response| ActionOutcome::Profile(response.user));
                    let kind = ActionKind::SetAvatar;
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::FetchAvatar { user, version } => self.fetch_avatar(user, version),
            Effect::ForgetAvatars { users } => self.forget_avatars(users),
            Effect::PersistActiveWorkspace(workspace) => {
                *self
                    .prefs
                    .pending_active
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(workspace);
                self.prefs.dirty.notify_one();
            }
            Effect::PersistAppearance(appearance) => {
                *self
                    .prefs
                    .pending_appearance
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(appearance);
                self.prefs.dirty.notify_one();
            }
            Effect::UpdateWorkspace { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Idempotent: a lost answer is simply asked again.
                    let path = paths::workspace(workspace);
                    let result = inner
                        .retry_read(|| {
                            inner
                                .api
                                .patch::<lazy_protocol::WorkspaceResponse, _>(&path, &request)
                        })
                        .await
                        .map(|response| ActionOutcome::Workspace(response.workspace));
                    let kind = ActionKind::UpdateWorkspace;
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::SetMemberRole {
                workspace,
                user,
                request,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Idempotent: setting the role someone has changes nothing.
                    let path = paths::person_role(workspace, user);
                    let role = request.role;
                    let result = inner
                        .retry_read(|| inner.api.put::<serde::de::IgnoredAny, _>(&path, &request))
                        .await
                        .map(|_| ActionOutcome::Role { user, role });
                    let kind = ActionKind::SetMemberRole(user);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::SetMemberDeactivated {
                workspace,
                user,
                request,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::person_deactivated(workspace, user);
                    let deactivated = request.deactivated;
                    let result = inner
                        .retry_read(|| inner.api.put::<serde::de::IgnoredAny, _>(&path, &request))
                        .await
                        .map(|_| ActionOutcome::Deactivated { user, deactivated });
                    let kind = ActionKind::SetMemberDeactivated(user);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::LoadInvitations {
                workspace,
                generation,
            } => {
                let inner = self.clone();
                self.single_read(
                    |inner| &inner.invitations,
                    generation,
                    async move {
                        let path = paths::invitations(workspace);
                        inner
                            .retry_read(|| inner.api.get::<lazy_protocol::InvitationList>(&path))
                            .await
                    },
                    move |result| Input::InvitationsLoaded { generation, result },
                );
            }
            Effect::CancelInvitations => self.cancel_slot(&self.invitations),
            Effect::CreateInvitation { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: creating carries no operation id, so repeating it could make a
                    // second invitation. The core resolves an unknown outcome by listing them.
                    let result = inner
                        .api
                        .post::<lazy_protocol::CreatedInvitation, _>(
                            &paths::invitations(workspace),
                            &request,
                        )
                        .await
                        .map(ActionOutcome::InvitationCreated);
                    let kind = ActionKind::CreateInvitation;
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::RevokeInvitation { invitation } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Revoking is idempotent, so a lost answer is simply asked again.
                    let path = paths::invitation(invitation);
                    let result = inner
                        .retry_read(|| inner.api.delete::<lazy_protocol::InvitationResponse>(&path))
                        .await
                        .map(|response| ActionOutcome::Invitation(response.invitation));
                    let kind = ActionKind::RevokeInvitation(invitation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
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
            Effect::FetchReplies {
                root,
                before,
                limit,
                generation,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let query = HistoryQuery { before, limit };
                    let path = paths::replies(root);
                    let result = inner
                        .retry_read(|| inner.api.get_query::<HistoryPage, _>(&path, &query))
                        .await;
                    inner
                        .emit(Input::RepliesLoaded {
                            root,
                            generation,
                            result,
                        })
                        .await;
                });
            }
            Effect::SetThreadFollow { root, following } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: the core repeats an unknown outcome itself, since the request
                    // sets a state and repeating it is harmless.
                    let path = paths::thread_action(root, "follow");
                    let result = inner
                        .api
                        .put::<ThreadStateResponse, _>(&path, &SetThreadFollowRequest { following })
                        .await;
                    inner.emit(Input::ThreadFollowResult { root, result }).await;
                });
            }
            Effect::MarkThreadRead { root, up_to } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::thread_action(root, "read");
                    let result = inner
                        .api
                        .post::<ThreadStateResponse, _>(&path, &MarkThreadReadRequest { up_to })
                        .await;
                    inner
                        .emit(Input::ThreadReadResult {
                            root,
                            up_to,
                            result,
                        })
                        .await;
                });
            }
            Effect::PersistThreadDrafts(drafts) => {
                if let Some(persistence) = self.persistence() {
                    persistence.queue_thread_drafts(drafts);
                }
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
                thread_root,
                also_in_channel,
                attachments,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let request = SendMessageRequest {
                        operation_id: operation,
                        document,
                        thread_root,
                        also_in_channel,
                        attachment_ids: attachments,
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
            Effect::EditMessage { message, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    let operation = request.operation_id;
                    // Exactly one attempt, like a send: the core reconciles an unknown outcome
                    // by asking the server about the operation.
                    let result = inner
                        .api
                        .patch::<MessageResponse, _>(&paths::message(message), &request)
                        .await;
                    inner.emit(Input::EditResult { operation, result }).await;
                });
            }
            Effect::CreateGroup { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt only: an unknown outcome is reconciled by the core, never
                    // repeated here.
                    let operation = request.operation_id;
                    let result = inner
                        .api
                        .post::<GroupResponse, _>(&paths::groups(workspace), &request)
                        .await;
                    inner
                        .emit(Input::GroupCommandResult { operation, result })
                        .await;
                });
            }
            Effect::CreateWebhook {
                conversation,
                request,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: creating carries no operation id, so repeating it could make a
                    // second webhook. The core resolves an unknown outcome by listing them.
                    let result = inner
                        .api
                        .post::<lazy_protocol::CreatedWebhook, _>(
                            &paths::webhooks(conversation),
                            &request,
                        )
                        .await
                        .map(|created| {
                            // The address is shown once and goes into a wrapper that no debug
                            // output can print.
                            ActionOutcome::WebhookCreated(CreatedWebhook {
                                webhook: created.webhook,
                                url: SecretUrl::new(created.url),
                            })
                        });
                    let kind = ActionKind::CreateWebhook(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::DeleteWebhook { webhook } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Deleting is idempotent, so a lost answer is simply asked again.
                    let path = paths::webhook(webhook);
                    let result = inner
                        .retry_read(|| inner.api.delete_unit(&path))
                        .await
                        .map(|()| ActionOutcome::Done);
                    let kind = ActionKind::DeleteWebhook(webhook);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::UploadEmoji {
                workspace,
                emoji,
                name,
                source,
            } => self.upload_emoji(workspace, emoji, name, source),
            Effect::RenameEmoji { emoji, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: a rename to a name someone else has taken meanwhile must not
                    // be answered as if it had worked.
                    let result = inner
                        .api
                        .patch::<lazy_protocol::CustomEmojiResponse, _>(
                            &paths::emoji(emoji),
                            &request,
                        )
                        .await
                        .map(|response| ActionOutcome::CustomEmoji(response.emoji));
                    let kind = ActionKind::RenameCustomEmoji(emoji);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::DeleteEmoji { emoji } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Deleting is idempotent, so a lost answer is simply asked again.
                    let path = paths::emoji(emoji);
                    let result = inner
                        .retry_read(|| inner.api.delete_unit(&path))
                        .await
                        .map(|()| ActionOutcome::Done);
                    let kind = ActionKind::DeleteCustomEmoji(emoji);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::FetchEmojiImage { emoji } => self.fetch_emoji_image(emoji),
            Effect::ForgetEmojiImages { emojis } => self.forget_emoji_images(emojis),
            Effect::LoadWebhooks { conversation } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::webhooks(conversation);
                    let result = inner
                        .retry_read(|| inner.api.get::<lazy_protocol::WebhooksResponse>(&path))
                        .await
                        .map(|response| ActionOutcome::Webhooks(response.webhooks));
                    let kind = ActionKind::LoadWebhooks(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::PersistView(view) => {
                if let Some(persistence) = self.persistence() {
                    persistence.queue_view(view);
                }
            }
            Effect::UpdateGroup { group, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .api
                        .patch::<GroupResponse, _>(&paths::group(group), &request)
                        .await
                        .map(|r| ActionOutcome::Group(r.group));
                    let kind = ActionKind::UpdateGroup(group);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::DeleteGroup { group } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Deleting is idempotent, so a lost answer is simply asked again. Any 2xx
                    // status counts.
                    let path = paths::group(group);
                    let result = inner
                        .retry_read(|| inner.api.delete_unit(&path))
                        .await
                        .map(|()| ActionOutcome::Done);
                    let kind = ActionKind::DeleteGroup(group);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::FetchMembers {
                conversation,
                generation,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "members");
                    let result = inner
                        .retry_read(|| inner.api.get::<ConversationMembersResponse>(&path))
                        .await;
                    inner
                        .emit(Input::MembersFetched {
                            conversation,
                            generation,
                            result,
                        })
                        .await;
                });
            }
            Effect::SetReaction { message, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt: the core repeats an unknown outcome itself, since the request
                    // sets a state and repeating it is harmless.
                    let emoji = request.emoji;
                    let result = inner
                        .api
                        .put::<MessageResponse, _>(&paths::reactions(message), &request)
                        .await;
                    inner
                        .emit(Input::ReactionResult {
                            message,
                            emoji,
                            result,
                        })
                        .await;
                });
            }
            Effect::PersistEmojiPrefs(prefs) => {
                *self.prefs.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(prefs);
                self.prefs.dirty.notify_one();
            }
            Effect::PersistRecentSearches(queries) => {
                *self
                    .prefs
                    .pending_searches
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(queries);
                self.prefs.dirty.notify_one();
            }
            Effect::Search {
                workspace,
                generation,
                params,
            } => self.search(workspace, generation, params),
            Effect::CancelSearch => self.cancel_search(),
            Effect::SetNotificationSettings {
                workspace,
                settings,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::notification_settings(workspace);
                    let result = inner
                        .api
                        .put::<NotificationSettingsResponse, _>(&path, &settings)
                        .await;
                    inner
                        .emit(Input::NotificationSettingsResult { workspace, result })
                        .await;
                });
            }
            Effect::SetConversationNotificationPrefs {
                conversation,
                request,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_notification_prefs(conversation);
                    let result = inner
                        .api
                        .put::<lazy_protocol::ConversationNotificationPrefsResponse, _>(
                            &path, &request,
                        )
                        .await;
                    inner
                        .emit(Input::ConversationPrefsResult {
                            conversation,
                            result,
                        })
                        .await;
                });
            }
            Effect::SetStatus { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::status(workspace);
                    let result = inner.api.put::<StatusResponse, _>(&path, &request).await;
                    inner.emit(Input::StatusResult { workspace, result }).await;
                });
            }
            Effect::LoadInbox {
                workspace,
                generation,
                query,
            } => self.load_inbox(workspace, generation, query),
            Effect::CancelInbox => self.cancel_slot(&self.inbox),
            Effect::SetMessageSaved { message, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::message_saved(message);
                    let result = inner.api.put::<SavedResponse, _>(&path, &request).await;
                    inner
                        .emit(Input::MessageSavedResult {
                            message_id: message,
                            result,
                        })
                        .await;
                });
            }
            Effect::LoadSaved {
                workspace,
                generation,
                query,
            } => {
                let inner = self.clone();
                self.single_read(
                    |inner| &inner.saved,
                    generation,
                    async move {
                        let path = paths::saved(workspace);
                        inner
                            .retry_read(|| inner.api.get_query::<SavedPage, _>(&path, &query))
                            .await
                    },
                    move |result| Input::SavedLoaded { generation, result },
                );
            }
            Effect::CancelSaved => self.cancel_slot(&self.saved),
            Effect::LoadPeople {
                workspace,
                generation,
                query,
            } => {
                let inner = self.clone();
                self.single_read(
                    |inner| &inner.people,
                    generation,
                    async move {
                        let path = paths::people(workspace);
                        inner
                            .retry_read(|| inner.api.get_query::<PeoplePage, _>(&path, &query))
                            .await
                    },
                    move |result| Input::PeopleLoaded { generation, result },
                );
            }
            Effect::CancelPeople => self.cancel_slot(&self.people),
            Effect::LoadProfile {
                workspace,
                generation,
                user,
            } => {
                let inner = self.clone();
                self.single_read(
                    |inner| &inner.profile,
                    generation,
                    async move {
                        let path = paths::person(workspace, user);
                        inner
                            .retry_read(|| inner.api.get::<PersonResponse>(&path))
                            .await
                    },
                    move |result| Input::ProfileLoaded { generation, result },
                );
            }
            Effect::CancelProfile => self.cancel_slot(&self.profile),
            Effect::LoadBrowse {
                workspace,
                generation,
                query,
            } => {
                let inner = self.clone();
                self.single_read(
                    |inner| &inner.browse,
                    generation,
                    async move {
                        let path = paths::browse_channels(workspace);
                        inner
                            .retry_read(|| {
                                inner
                                    .api
                                    .get_query::<BrowseChannelsResponse, _>(&path, &query)
                            })
                            .await
                    },
                    move |result| Input::BrowseLoaded { generation, result },
                );
            }
            Effect::CancelBrowse => self.cancel_slot(&self.browse),
            Effect::PersistRecentConversations(ids) => {
                *self
                    .prefs
                    .pending_conversations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(ids);
                self.prefs.dirty.notify_one();
            }
            Effect::MarkInboxRead { workspace } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::inbox_read(workspace);
                    let result = inner.api.post_empty::<InboxReadResponse>(&path).await;
                    inner.emit(Input::InboxReadResult { result }).await;
                });
            }
            // Shown by the platform part of the host; nothing to run here.
            Effect::Notify(_)
            | Effect::ClearNotifications { .. }
            | Effect::RetractNotification { .. }
            | Effect::SetBadge(_) => {}
            Effect::FetchMessage { message } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::message(message);
                    let result = inner
                        .retry_read(|| inner.api.get::<MessageResponse>(&path))
                        .await;
                    inner.emit(Input::MessageFetched { message, result }).await;
                });
            }
            Effect::DeleteMessage { message } => {
                let inner = self.clone();
                self.spawn(async move {
                    // Deleting is idempotent (a repeat returns the tombstone), so a lost
                    // answer is simply asked again.
                    let path = paths::message(message);
                    let result = inner
                        .retry_read(|| inner.api.delete::<MessageResponse>(&path))
                        .await;
                    inner.emit(Input::DeleteResult { message, result }).await;
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
            Effect::CreateChannel { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    // One attempt only: an unknown outcome is reconciled by the core, never
                    // repeated here.
                    let operation = request.operation_id;
                    let result = inner
                        .api
                        .post::<ConversationResponse, _>(&paths::channels(workspace), &request)
                        .await;
                    inner
                        .emit(Input::ConversationCommandResult { operation, result })
                        .await;
                });
            }
            Effect::OpenConversation { workspace, request } => {
                let inner = self.clone();
                self.spawn(async move {
                    let operation = request.operation_id;
                    let result = inner
                        .api
                        .post::<ConversationResponse, _>(
                            &paths::open_conversation(workspace),
                            &request,
                        )
                        .await;
                    inner
                        .emit(Input::ConversationCommandResult { operation, result })
                        .await;
                });
            }
            Effect::JoinChannel { conversation } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "join");
                    let result = inner
                        .api
                        .post_empty::<ConversationResponse>(&path)
                        .await
                        .map(|r| ActionOutcome::Conversation(r.conversation));
                    let kind = ActionKind::Join(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::LeaveChannel { conversation } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "leave");
                    let result = inner
                        .api
                        .post_unit(&path)
                        .await
                        .map(|()| ActionOutcome::Done);
                    let kind = ActionKind::Leave(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::AddMembers {
                conversation,
                request,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "members");
                    let result = inner
                        .api
                        .post::<ConversationResponse, _>(&path, &request)
                        .await
                        .map(|r| ActionOutcome::Conversation(r.conversation));
                    let kind = ActionKind::AddMembers(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::RemoveMember { conversation, user } => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .api
                        .delete_unit(&paths::member(conversation, user))
                        .await
                        .map(|()| ActionOutcome::Done);
                    let kind = ActionKind::RemoveMember(conversation, user);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::UpdateConversation {
                conversation,
                request,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let result = inner
                        .api
                        .patch::<ConversationResponse, _>(
                            &paths::conversation(conversation),
                            &request,
                        )
                        .await
                        .map(|r| ActionOutcome::Conversation(r.conversation));
                    let kind = ActionKind::UpdateConversation(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::ArchiveConversation { conversation } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "archive");
                    let result = inner
                        .api
                        .post_empty::<ConversationResponse>(&path)
                        .await
                        .map(|r| ActionOutcome::Conversation(r.conversation));
                    let kind = ActionKind::Archive(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::UnarchiveConversation { conversation } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "unarchive");
                    let result = inner
                        .api
                        .post_empty::<ConversationResponse>(&path)
                        .await
                        .map(|r| ActionOutcome::Conversation(r.conversation));
                    let kind = ActionKind::Unarchive(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::SetFavorite {
                conversation,
                favorite,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "favorite");
                    let result = inner
                        .api
                        .put::<UserStateResponse, _>(&path, &SetFavoriteRequest { favorite })
                        .await
                        .map(|r| ActionOutcome::UserState(r.state));
                    let kind = ActionKind::SetFavorite(conversation);
                    inner.emit(Input::ActionResult { kind, result }).await;
                });
            }
            Effect::MarkRead {
                conversation,
                up_to,
            } => {
                let inner = self.clone();
                self.spawn(async move {
                    let path = paths::conversation_action(conversation, "read");
                    let result = inner
                        .api
                        .post::<UserStateResponse, _>(&path, &MarkReadRequest { up_to })
                        .await
                        .map(|r| r.state);
                    inner
                        .emit(Input::MarkReadResult {
                            conversation,
                            up_to,
                            result,
                        })
                        .await;
                });
            }
            Effect::PersistCache(cache) => {
                if let Some(persistence) = self.persistence() {
                    persistence.queue_cache(*cache);
                }
            }
            Effect::DiscardCache => {
                tracing::info!("discarding the saved message cache");
                if let Some(persistence) = self.persistence() {
                    persistence.discard_cache();
                }
            }
            Effect::StageFile {
                attachment,
                generation,
                source,
            } => self.stage_file(attachment, generation, source),
            Effect::UploadFile {
                attachment,
                conversation,
                generation,
                name,
                media_type,
                size: _,
            } => self.upload_file(attachment, conversation, generation, name, media_type),
            Effect::CancelUpload { attachment } => self.cancel_upload(attachment),
            Effect::ReleaseFiles { attachments } => self.release_files(attachments),
            Effect::DownloadAttachment {
                attachment,
                destination,
            } => self.download_attachment(attachment, destination),
            Effect::CancelDownload { attachment } => self.cancel_download(attachment),
            Effect::FetchImage { attachment, full } => self.fetch_image(attachment, full),
            Effect::ForgetAttachments { attachments } => self.forget_attachments(attachments),
        }
    }

    async fn finish_authentication(&self, result: Result<SessionResponse, RequestError>) {
        match result {
            Ok(session) => {
                self.api.set_token(Some(session.token.clone()));
                let vault = self.vault();
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
                tracing::info!(user = %session.user.id, "signed in");
                self.emit(Input::Authenticated { user: session.user }).await;
            }
            Err(error) => {
                tracing::info!(reason = error.code(), "sign-in failed");
                self.emit(Input::AuthFailed(error)).await;
            }
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
                    tracing::debug!(
                        attempt,
                        attempts,
                        "repeating a read after an unknown outcome"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// Asks the server for a page of search results. A search already waiting for an answer is
    /// stopped first: nobody wants it, and it emits nothing.
    fn search(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        generation: u64,
        params: lazy_protocol::SearchParams,
    ) {
        let inner = self.clone();
        self.single_read(
            |inner| &inner.search,
            generation,
            async move {
                let path = paths::search(workspace);
                inner
                    .retry_read(|| {
                        inner
                            .api
                            .search::<lazy_protocol::SearchResponse, _>(&path, &params)
                    })
                    .await
            },
            move |result| Input::SearchResult { generation, result },
        );
    }

    /// Runs a read of which one is wanted at a time: it replaces the one waiting in `slot`, which
    /// is hung up on, and reports nothing if it is itself replaced or cancelled before it ends.
    fn single_read<T: Send + 'static>(
        self: &Arc<Self>,
        slot: fn(&Inner) -> &ReadSlot,
        generation: u64,
        call: impl Future<Output = Result<T, RequestError>> + Send + 'static,
        report: impl FnOnce(Result<T, RequestError>) -> Input + Send + 'static,
    ) {
        let cancel = self.cancel.child_token();
        let previous = slot(self)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace((generation, cancel.clone()));
        if let Some((_, previous)) = previous {
            previous.cancel();
        }
        let inner = self.clone();
        self.spawn(async move {
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                result = call => result,
            };
            {
                // Only the request still current reports; the slot is free for the next.
                let mut slot = slot(&inner).lock().unwrap_or_else(|e| e.into_inner());
                if cancel.is_cancelled() || slot.as_ref().is_none_or(|(g, _)| *g != generation) {
                    return;
                }
                *slot = None;
            }
            inner.emit(report(result)).await;
        });
    }

    /// Asks the server for a page of the thread inbox.
    fn load_inbox(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        generation: u64,
        query: lazy_protocol::InboxQuery,
    ) {
        let inner = self.clone();
        self.single_read(
            |inner| &inner.inbox,
            generation,
            async move {
                let path = paths::inbox(workspace);
                inner
                    .retry_read(|| inner.api.get_query::<InboxPage, _>(&path, &query))
                    .await
            },
            move |result| Input::InboxLoaded { generation, result },
        );
    }

    /// Stops the search and the inbox page being waited for, if any, without reporting them.
    fn cancel_search(&self) {
        self.cancel_slot(&self.search);
    }

    fn cancel_slot(&self, slot: &ReadSlot) {
        let previous = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((_, cancel)) = previous {
            cancel.cancel();
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

    /// Stops reading a workspace the person may not read any more.
    fn unsubscribe(&self, workspace: WorkspaceId) {
        let removed = self
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&workspace);
        if let Some(subscription) = removed {
            subscription.cancel.cancel();
        }
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

/// Why a task did not finish, in a word. A [`tokio::task::JoinError`]'s own text repeats the
/// panic message, which can be anything the panicking code was holding.
fn task_failure(error: &tokio::task::JoinError) -> &'static str {
    if error.is_panic() {
        "panicked"
    } else {
        "cancelled"
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
    thread_drafts: Option<Vec<ThreadDraft>>,
    view: Option<ViewState>,
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

    fn queue_view(&self, view: ViewState) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).view = Some(view);
        self.dirty.notify_one();
    }

    fn queue_thread_drafts(&self, drafts: Vec<ThreadDraft>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .thread_drafts = Some(drafts);
        self.dirty.notify_one();
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
                            message: format!("storage task failed ({})", task_failure(&error)),
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
        let (drafts, thread_drafts, view, cache) = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            (
                pending.drafts.take(),
                pending.thread_drafts.take(),
                pending.view.take(),
                pending.cache.take(),
            )
        };
        if drafts.is_none() && thread_drafts.is_none() && view.is_none() && cache.is_none() {
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
            if let Some(thread_drafts) = thread_drafts {
                store.save_thread_drafts(&thread_drafts)?;
            }
            if let Some(view) = view {
                store.save_view(&view)?;
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
        thread_drafts: recovered.thread_drafts,
        view: recovered.view,
        cache: recovered.cache,
        emoji_prefs: prefs::load(profile_dir),
        recent_searches: searches::load(profile_dir),
        recent_conversations: recent_conversations::load(profile_dir),
        appearance: crate::appearance::load(profile_dir),
        active_workspace: crate::active_workspace::load(profile_dir),
        problems: recovered.problems,
        server: ServerInfo::default(),
    }
}
