//! Test harness: real clients driven the way the desktop app drives them.
//!
//! A [`Client`] is a `ClientCore` plus a real `Runtime`, wired exactly as `lazy-desktop` wires
//! them: person actions and runtime reports go into the core, the core's effects go to the
//! runtime. Tests wait on observable conditions with a bounded deadline, never on sleeps.

#![allow(dead_code)]

pub mod proxy;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lazy_client_core::{
    ClientCore, ConnectionState, Delivery, Effect, HistoryStatus, Input, SessionPhase, TimelineItem,
};
use lazy_client_runtime::{
    BackoffPolicy, MemoryStorage, Runtime, RuntimeConfig, SecureStorageChoice,
};
use lazy_server::db;
use lazy_server::testing::{self, TestServer};
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// How long any single wait may take before the test fails.
pub const DEADLINE: Duration = Duration::from_secs(25);
pub const PASSWORD: &str = "correct horse battery";

pub struct World {
    pub server: TestServer,
    pub pool: PgPool,
}

impl World {
    pub async fn start(pool: PgPool) -> World {
        db::seed_default_workspace(&pool)
            .await
            .expect("seed")
            .expect("fresh database has no workspace");
        let server = testing::spawn_test_server(pool.clone()).await;
        World { server, pool }
    }

    /// The position of the newest event in the workspace's log.
    pub async fn event_head(&self) -> i64 {
        sqlx::query_scalar("SELECT max(event_head) FROM workspaces")
            .fetch_one(&self.pool)
            .await
            .expect("event head")
    }

    /// Does what the retention pruner does once events have aged out: removes the whole log so
    /// far and raises the floor, in one transaction. Any client whose cursor is older now has to
    /// start again from a snapshot.
    pub async fn prune_event_log(&self) {
        let mut tx = self.pool.begin().await.expect("begin");
        sqlx::query("UPDATE workspaces SET event_floor = event_head")
            .execute(&mut *tx)
            .await
            .expect("raise the floor");
        sqlx::query(
            "DELETE FROM events e USING workspaces w \
             WHERE e.workspace_id = w.id AND e.seq <= w.event_floor",
        )
        .execute(&mut *tx)
        .await
        .expect("delete pruned events");
        tx.commit().await.expect("commit");
    }

    /// How many committed messages have exactly this plain text.
    pub async fn count_messages(&self, text: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE plain_text = $1")
            .bind(text)
            .fetch_one(&self.pool)
            .await
            .expect("count messages")
    }

    pub async fn wait_for_message_count(&self, text: &str, expected: i64) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.count_messages(text).await == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} x {text:?} in the database"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// State shared between a test and the task that pumps a client's runtime reports.
struct Shared {
    core: Mutex<ClientCore>,
    /// Every effect the core has asked for, oldest first.
    effects: Mutex<Vec<Effect>>,
    changed: Notify,
    saw_unknown_outcome: AtomicBool,
}

impl Shared {
    fn apply(&self, runtime: &Runtime, input: Input) {
        let mut core = self.core.lock().unwrap();
        let output = core.handle(input);
        self.effects
            .lock()
            .unwrap()
            .extend(output.effects.iter().cloned());
        runtime.execute(output.effects);
        if let Some(conversation) = core.selected_conversation()
            && core
                .timeline(conversation)
                .iter()
                .any(|item| matches!(item.delivery, Delivery::UnknownOutcome))
        {
            self.saw_unknown_outcome.store(true, Ordering::SeqCst);
        }
        drop(core);
        self.changed.notify_waiters();
    }
}

/// A `ClientCore` plus a real `Runtime`, pumped continuously in the background exactly as the
/// desktop app pumps them, so every client reacts while a test is busy with another.
pub struct Client {
    pub name: &'static str,
    shared: Arc<Shared>,
    runtime: Option<Arc<Runtime>>,
    pump: Option<JoinHandle<()>>,
    pub profile: PathBuf,
}

impl Client {
    pub fn start(
        name: &'static str,
        server_url: &str,
        profile: &Path,
        storage: Arc<MemoryStorage>,
    ) -> Client {
        let mut config = RuntimeConfig::new(server_url);
        config.profile_dir = Some(profile.to_owned());
        config.secure_storage = SecureStorageChoice::Custom(storage);
        config.request_timeout = Duration::from_secs(4);
        config.reconnect = BackoffPolicy {
            base: Duration::from_millis(40),
            cap: Duration::from_millis(400),
        };
        config.save_debounce = Duration::from_millis(20);
        let (runtime, inputs) = Runtime::start(config).expect("start runtime");
        let runtime = Arc::new(runtime);
        let shared = Arc::new(Shared {
            core: Mutex::new(ClientCore::new()),
            effects: Mutex::new(Vec::new()),
            changed: Notify::new(),
            saw_unknown_outcome: AtomicBool::new(false),
        });
        let pump = tokio::spawn({
            let (shared, runtime) = (shared.clone(), runtime.clone());
            async move {
                while let Ok(input) = inputs.recv().await {
                    shared.apply(&runtime, input);
                }
            }
        });
        Client {
            name,
            shared,
            runtime: Some(runtime),
            pump: Some(pump),
            profile: profile.to_owned(),
        }
    }

    /// Applies a person action to the core and hands its effects to the runtime.
    pub fn act(&self, input: Input) {
        let runtime = self.runtime.as_ref().expect("client is running");
        self.shared.apply(runtime, input);
    }

    /// Reads the core.
    pub fn read<R>(&self, f: impl FnOnce(&ClientCore) -> R) -> R {
        f(&self.shared.core.lock().unwrap())
    }

    /// Every effect the core has asked for since this client started.
    pub fn effects(&self) -> Vec<Effect> {
        self.shared.effects.lock().unwrap().clone()
    }

    /// The positions this client subscribed after, in order.
    pub fn subscribe_positions(&self) -> Vec<i64> {
        self.effects()
            .iter()
            .filter_map(|effect| match effect {
                Effect::Subscribe { after, .. } => Some(after.get()),
                _ => None,
            })
            .collect()
    }

    /// How many times this client asked for a page of history.
    pub fn history_fetches(&self) -> usize {
        self.effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::FetchHistory { .. }))
            .count()
    }

    /// The cursor of the most recent content cache this client asked to save.
    pub fn last_cached_cursor(&self) -> Option<i64> {
        self.effects().iter().rev().find_map(|effect| match effect {
            Effect::PersistCache(cache) => Some(cache.workspaces[0].cursor.get()),
            _ => None,
        })
    }

    /// Whether any row was ever observed in the unknown-outcome state.
    pub fn saw_unknown_outcome(&self) -> bool {
        self.shared.saw_unknown_outcome.load(Ordering::SeqCst)
    }

    /// Waits, up to the deadline, for `condition` to hold.
    pub async fn wait_until(&self, what: &str, condition: impl Fn(&ClientCore) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.read(|core| condition(core)) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(remaining, notified).await.is_err() {
                panic!(
                    "{}: timed out waiting for {what}\n  phase={:?} connection={:?}\n  timeline={:?}",
                    self.name,
                    self.read(|core| core.phase().clone()),
                    self.read(|core| core.connection()),
                    self.texts_with_state(),
                );
            }
        }
    }

    /// Waits for the sign-in screen, registers, and waits until the workspace is loaded and live.
    pub async fn register(&self, username: &str) {
        self.wait_until("sign-in screen", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
        self.act(Input::Register {
            username: username.to_owned(),
            display_name: format!("User {username}"),
            password: PASSWORD.to_owned(),
            invite_code: String::new(),
        });
        self.wait_until_ready().await;
    }

    /// Waits for a loaded workspace, a live connection, and the first page of history.
    pub async fn wait_until_ready(&self) {
        self.wait_until("a ready, live workspace with history", |core| {
            matches!(core.phase(), SessionPhase::Ready)
                && core.connection() == ConnectionState::Live
                && core
                    .selected_conversation()
                    .is_some_and(|c| matches!(core.history_status(c), HistoryStatus::Ready { .. }))
        })
        .await;
    }

    pub fn send(&self, text: &str) {
        let conversation = self
            .read(|core| core.selected_conversation())
            .expect("a selected conversation");
        self.act(Input::SendMessage {
            conversation,
            text: text.to_owned(),
        });
    }

    pub fn items(&self) -> Vec<TimelineItem> {
        self.read(|core| {
            let conversation = core
                .selected_conversation()
                .expect("a selected conversation");
            core.timeline(conversation).to_vec()
        })
    }

    /// Plain text of every row in the selected conversation, oldest first.
    pub fn texts(&self) -> Vec<String> {
        self.items()
            .iter()
            .map(|item| item.document.plain_text())
            .collect()
    }

    pub fn texts_with_state(&self) -> Vec<(String, String)> {
        self.read(|core| {
            core.selected_conversation()
                .map(|c| {
                    core.timeline(c)
                        .iter()
                        .map(|item| (item.document.plain_text(), format!("{:?}", item.delivery)))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    pub async fn wait_for_text(&self, text: &str) {
        let text = text.to_owned();
        self.wait_until(&format!("message {text:?}"), move |core| {
            core.selected_conversation().is_some_and(|c| {
                core.timeline(c)
                    .iter()
                    .any(|i| i.document.plain_text() == text)
            })
        })
        .await;
    }

    /// Waits until every row with this text is confirmed by the server.
    pub async fn wait_confirmed(&self, text: &str) {
        let text = text.to_owned();
        self.wait_until(&format!("{text:?} confirmed"), move |core| {
            core.selected_conversation().is_some_and(|c| {
                let rows: Vec<_> = core
                    .timeline(c)
                    .iter()
                    .filter(|i| i.document.plain_text() == text)
                    .collect();
                !rows.is_empty() && rows.iter().all(|i| i.is_confirmed())
            })
        })
        .await;
    }

    /// Stops the runtime, flushing pending saves, as quitting the app does.
    pub async fn stop(mut self) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
            let _ = pump.await;
        }
        if let Some(runtime) = self.runtime.take() {
            let runtime = Arc::try_unwrap(runtime)
                .unwrap_or_else(|_| panic!("the pump task released the runtime"));
            // `Runtime` blocks while it shuts down, which is not allowed on an async worker.
            tokio::task::spawn_blocking(move || runtime.shutdown())
                .await
                .expect("shutdown task");
        }
    }
}

impl Drop for Client {
    /// A test that fails while a client is alive must not abort the process: dropping a
    /// `Runtime` blocks, which is not allowed on an async worker thread, so do it elsewhere.
    fn drop(&mut self) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        if let Some(runtime) = self.runtime.take() {
            let _ = std::thread::spawn(move || drop(runtime)).join();
        }
    }
}

pub fn count_of(texts: &[String], wanted: &str) -> usize {
    texts.iter().filter(|text| text.as_str() == wanted).count()
}
