//! The Phase 1 gate on the real UI: two clients, each a real `RootView` with the real runtime
//! pump behind it, exchange messages through a real server and Postgres. Everything a person
//! does is a simulated key press delivered to the focused control: registering, typing, sending.
//!
//! The windows are GPUI's headless windows (no display, no compositor), rendered by the same wgpu
//! renderer and text system the desktop app uses. Set `LAZY_SCREENSHOT_DIR` to keep an image of
//! each window at the end.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext as _, Entity, HeadlessAppContext, Keystroke, WindowAppearance, WindowHandle, px,
    size,
};
use lazy_client_core::{ConnectionState, Delivery, HistoryStatus, SessionPhase};
use lazy_client_runtime::{MemoryStorage, Runtime, RuntimeConfig, SecureStorageChoice};
use lazy_server::{db, testing};
use lazy_ui::app_model::AppModel;
use lazy_ui::root_view::RootView;
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;

const DEADLINE: Duration = Duration::from_secs(30);
const PASSWORD: &str = "correct horse battery";

/// A throwaway database on the dev Postgres, dropped at the end.
struct TempDatabase {
    admin: PgPool,
    name: String,
    pool: PgPool,
}

impl TempDatabase {
    async fn create() -> TempDatabase {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://lazy@127.0.0.1:54330/lazy".to_owned());
        let admin = PgPool::connect(&url)
            .await
            .expect("connect to Postgres (scripts/dev-db.sh start)");
        let name = format!("lazy_gui_{}", uuid_hex());
        // The name is generated from hex digits, so it cannot carry SQL.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .expect("create database");
        let options: PgConnectOptions = url.parse().expect("database URL");
        let pool = PgPool::connect_with(options.database(&name))
            .await
            .expect("connect to the new database");
        sqlx::migrate!("../lazy-server/migrations")
            .run(&pool)
            .await
            .expect("migrate");
        db::seed_default_workspace(&pool)
            .await
            .expect("seed")
            .expect("fresh database");
        TempDatabase { admin, name, pool }
    }

    async fn drop(self) {
        self.pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
    }
}

fn uuid_hex() -> String {
    lazy_domain::OperationId::generate()
        .as_uuid()
        .simple()
        .to_string()
}

/// One client: a headless window running the real views, the model, and the real runtime.
struct GuiClient {
    name: &'static str,
    cx: HeadlessAppContext,
    window: WindowHandle<RootView>,
    model: Entity<AppModel>,
    runtime: Option<Rc<Runtime>>,
}

impl GuiClient {
    fn start(name: &'static str, server_url: &str, profile: &Path) -> GuiClient {
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("IBM Plex Sans")),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        // The runtime reports from other threads, so the executor may wait for them.
        cx.allow_parking();

        let mut config = RuntimeConfig::new(server_url);
        config.profile_dir = Some(profile.to_owned());
        config.secure_storage = SecureStorageChoice::Custom(MemoryStorage::new());
        config.request_timeout = Duration::from_secs(5);
        let (runtime, inputs) = Runtime::start(config).expect("start the runtime");
        let runtime = Rc::new(runtime);

        cx.update(|app| lazy_ui::init(app, WindowAppearance::Light));
        let model = cx.update(|app| lazy_desktop::start_model(runtime.clone(), inputs, app));
        let window = cx
            .open_window(size(px(1100.), px(720.)), {
                let model = model.clone();
                move |window, app| app.new(|cx| RootView::new(model, window, cx))
            })
            .expect("open the window");
        let mut client = GuiClient {
            name,
            cx,
            window,
            model,
            runtime: Some(runtime),
        };
        client.settle();
        client
    }

    /// Runs pending work and draws a frame, so focus and key bindings reflect the latest state.
    fn settle(&mut self) {
        self.cx.run_until_parked();
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                window.draw(cx).clear(cx)
            })
            .expect("draw");
        self.cx.run_until_parked();
    }

    fn key(&mut self, keystroke: &str) {
        let os = self.cx.update(|app| app.platform_info().os);
        let keystroke = Keystroke::parse_with_os(keystroke, os).expect("a valid keystroke");
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                window.dispatch_keystroke(keystroke, cx)
            })
            .expect("dispatch key");
        self.settle();
    }

    fn keys(&mut self, keystrokes: &str) {
        for keystroke in keystrokes.split(' ') {
            self.key(keystroke);
        }
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(&if ch == ' ' {
                "space".to_owned()
            } else {
                ch.to_string()
            });
        }
    }

    fn read<R>(&mut self, f: impl FnOnce(&lazy_client_core::ClientCore) -> R) -> R {
        let model = self.model.clone();
        self.cx.update(|app| f(model.read(app).core()))
    }

    /// Lets the runtime and the UI run until `condition` holds or the deadline passes.
    fn wait_until(
        &mut self,
        what: &str,
        condition: impl Fn(&lazy_client_core::ClientCore) -> bool,
    ) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            self.settle();
            if self.read(|core| condition(core)) {
                return;
            }
            if Instant::now() >= deadline {
                let (phase, connection) =
                    self.read(|core| (core.phase().clone(), core.connection()));
                panic!(
                    "{}: timed out waiting for {what}; phase={phase:?}, connection={connection:?}",
                    self.name
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn texts(&mut self) -> Vec<String> {
        self.read(|core| {
            let conversation = core
                .selected_conversation()
                .expect("a selected conversation");
            core.timeline(conversation)
                .iter()
                .map(|item| item.document.plain_text())
                .collect()
        })
    }

    /// Registers entirely from the keyboard: Tab to "Create an account", Enter, then fill the form.
    fn register_with_the_keyboard(&mut self, username: &str, display_name: &str) {
        self.wait_until("the sign-in form", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        });
        // Sign-in order is: username, password, Sign in, "New here? Create an account".
        self.keys("tab tab tab enter");
        // Create-account order is: username, display name, password, invitation code, button, link.
        // Back to the top.
        self.keys("shift-tab shift-tab shift-tab shift-tab shift-tab");
        self.type_text(username);
        self.key("tab");
        self.type_text(display_name);
        self.key("tab");
        self.type_text(PASSWORD);
        self.key("enter");
        self.wait_until("a ready, live workspace", |core| {
            matches!(core.phase(), SessionPhase::Ready)
                && core.connection() == ConnectionState::Live
                && core
                    .selected_conversation()
                    .is_some_and(|c| matches!(core.history_status(c), HistoryStatus::Ready { .. }))
        });
    }

    fn screenshot(&mut self, dir: &Path) {
        self.screenshot_as(dir, "");
    }

    fn screenshot_as(&mut self, dir: &Path, label: &str) {
        std::fs::create_dir_all(dir).expect("create screenshot directory");
        self.settle();
        let suffix = if label.is_empty() {
            String::new()
        } else {
            format!("-{label}")
        };
        match self.cx.capture_screenshot(self.window.into()) {
            Ok(image) => image
                .save(dir.join(format!("gui-{}{suffix}.png", self.name)))
                .expect("write png"),
            Err(error) => eprintln!("{}: could not capture a screenshot: {error:#}", self.name),
        }
    }

    fn stop(mut self) {
        // Drop the UI first so nothing holds the runtime, then the runtime flushes and stops.
        drop(self.cx);
        if let Some(runtime) = self.runtime.take() {
            drop(runtime);
        }
    }
}

#[test]
fn two_gui_clients_exchange_messages_through_a_real_server() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let database = rt.block_on(TempDatabase::create());
    let server = rt.block_on(async { testing::spawn_test_server(database.pool.clone()).await });
    let dir = tempfile::tempdir().unwrap();

    let mut alice = GuiClient::start("alice", &server.base_url, &dir.path().join("alice"));
    let mut bob = GuiClient::start("bob", &server.base_url, &dir.path().join("bob"));
    alice.register_with_the_keyboard("alice", "Alice Nguyen");
    bob.register_with_the_keyboard("bob", "Bob Okafor");

    // Alice writes with the keyboard and presses Enter; Bob sees it arrive.
    alice.type_text("Hello Bob, welcome to Lazy!");
    alice.key("enter");
    bob.wait_until("Alice's message", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c)
                .iter()
                .any(|i| i.document.plain_text() == "Hello Bob, welcome to Lazy!")
        })
    });

    // Bob answers over two lines (Shift+Enter breaks the line; Enter sends).
    bob.type_text("Thanks Alice!");
    bob.key("shift-enter");
    bob.type_text("Glad to be here.");
    bob.key("enter");
    alice.wait_until("Bob's reply", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c)
                .iter()
                .any(|i| i.document.plain_text() == "Thanks Alice!\nGlad to be here.")
        })
    });

    // Both settle to the same confirmed history, each message once, authors correct.
    for client in [&mut alice, &mut bob] {
        client.wait_until("both messages confirmed", |core| {
            core.selected_conversation().is_some_and(|c| {
                let rows = core.timeline(c);
                rows.len() == 2
                    && rows
                        .iter()
                        .all(|r| matches!(r.delivery, Delivery::Confirmed { .. }))
            })
        });
    }
    let expected = [
        "Hello Bob, welcome to Lazy!",
        "Thanks Alice!\nGlad to be here.",
    ];
    assert_eq!(alice.texts(), expected);
    assert_eq!(bob.texts(), expected);
    let authors = bob.read(|core| {
        let c = core.selected_conversation().unwrap();
        core.timeline(c)
            .iter()
            .map(|i| core.display_name(i.author_id).to_owned())
            .collect::<Vec<_>>()
    });
    assert_eq!(authors, ["Alice Nguyen", "Bob Okafor"]);

    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        alice.screenshot(&dir);
        bob.screenshot(&dir);
    }

    alice.stop();
    bob.stop();
    rt.block_on(async {
        server.shutdown().await.unwrap();
        database.drop().await;
    });
}

/// Formatting, editing, and deleting through the real views: everything is a key press, and the
/// other window sees each change arrive.
#[test]
fn two_gui_clients_format_edit_and_delete_through_the_real_views() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let database = rt.block_on(TempDatabase::create());
    let server = rt.block_on(async { testing::spawn_test_server(database.pool.clone()).await });
    let dir = tempfile::tempdir().unwrap();

    let mut alice = GuiClient::start("alice", &server.base_url, &dir.path().join("alice"));
    let mut bob = GuiClient::start("bob", &server.base_url, &dir.path().join("bob"));
    alice.register_with_the_keyboard("alice", "Alice Nguyen");
    bob.register_with_the_keyboard("bob", "Bob Okafor");

    // Typed formatting arrives as a structured document in the other window.
    alice.type_text("*hello* world");
    alice.key("enter");
    bob.wait_until("the formatted message", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c).iter().any(|i| {
                i.document.plain_text() == "hello world"
                    && matches!(
                        &i.document.blocks()[0],
                        lazy_domain::Block::Paragraph { inlines } if inlines.iter().any(|inline| matches!(
                            inline,
                            lazy_domain::Inline::Text { text, marks } if text == "hello" && marks.bold
                        ))
                    )
            })
        })
    });
    alice.wait_until("the message confirmed", |core| {
        core.selected_conversation()
            .is_some_and(|c| core.timeline(c).iter().all(|r| r.is_confirmed()))
    });

    // Up in the empty composer edits the latest message, starting from its source text. Select
    // all, type the replacement, and Enter saves.
    alice.key("up");
    alice.keys("ctrl-a");
    alice.type_text("*hello* there");
    alice.key("enter");
    for client in [&mut alice, &mut bob] {
        client.wait_until("the edit on both screens", |core| {
            core.selected_conversation().is_some_and(|c| {
                core.timeline(c)
                    .iter()
                    .any(|i| i.document.plain_text() == "hello there" && i.is_edited())
            })
        });
    }

    // Bob reacts from the keyboard: the timeline, the actions menu (Add reaction is first), the
    // picker's search field, a word, Enter for the first result. Alice sees the chip.
    bob.keys("shift-tab enter enter");
    bob.type_text("rocket");
    bob.key("enter");
    for client in [&mut alice, &mut bob] {
        client.wait_until("the rocket reaction on both screens", |core| {
            core.selected_conversation().is_some_and(|c| {
                core.timeline(c)
                    .iter()
                    .any(|i| i.reactions.len() == 1 && i.reactions[0].emoji.to_string() == "🚀")
            })
        });
    }

    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        alice.screenshot_as(&dir, "reacted");
        bob.screenshot_as(&dir, "reacted");
    }

    // After an Up-arrow edit the keyboard is back in the composer. The keyboard route to Delete:
    // Shift+Tab to the timeline, Enter for the actions menu (Add reaction, one toggle for the
    // rocket, Reply in thread, Edit, Delete, ...), down to Delete, Enter; the dialog starts on
    // Cancel, so Tab then Enter confirms.
    let shots = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from);
    alice.keys("shift-tab enter down down down down");
    if let Some(dir) = &shots {
        alice.screenshot_as(dir, "menu");
    }
    alice.keys("enter");
    if let Some(dir) = &shots {
        alice.screenshot_as(dir, "dialog");
    }
    alice.keys("tab enter");
    for client in [&mut alice, &mut bob] {
        client.wait_until("the deletion on both screens", |core| {
            core.selected_conversation()
                .is_some_and(|c| core.timeline(c).is_empty())
        });
    }

    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        alice.screenshot(&dir);
        bob.screenshot(&dir);
    }

    alice.stop();
    bob.stop();
    rt.block_on(async {
        server.shutdown().await.unwrap();
        database.drop().await;
    });
}

/// Mentions through the real views: Alice types the start of a name, accepts the suggestion with
/// the keyboard, and sends; Bob's window shows a message whose mention is of him.
#[test]
fn two_gui_clients_mention_through_the_autocomplete() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let database = rt.block_on(TempDatabase::create());
    let server = rt.block_on(async { testing::spawn_test_server(database.pool.clone()).await });
    let dir = tempfile::tempdir().unwrap();

    let mut alice = GuiClient::start("alice", &server.base_url, &dir.path().join("alice"));
    let mut bob = GuiClient::start("bob", &server.base_url, &dir.path().join("bob"));
    alice.register_with_the_keyboard("alice", "Alice Nguyen");
    bob.register_with_the_keyboard("bob", "Bob Okafor");
    // Alice's client has to know who is in the channel before it can suggest anyone.
    alice.wait_until("the member list", |core| {
        core.selected_conversation()
            .and_then(|c| core.conversation_members(c))
            .is_some_and(|members| members.len() == 2)
    });

    alice.type_text("hey @bo");
    alice.key("enter");
    alice.type_text("look at this");
    alice.key("enter");
    let bob_id = bob.read(|core| core.me().unwrap().id);
    bob.wait_until("the mention", move |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c).iter().any(|i| {
                i.document.plain_text() == "hey @bob look at this"
                    && i.document.mentions() == [lazy_domain::MentionTarget::User { id: bob_id }]
            })
        })
    });
    alice.wait_until("the message confirmed", |core| {
        core.selected_conversation()
            .is_some_and(|c| core.timeline(c).iter().all(|r| r.is_confirmed()))
    });

    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        alice.screenshot_as(&dir, "mention");
        bob.screenshot_as(&dir, "mention");
    }

    alice.stop();
    bob.stop();
    rt.block_on(async {
        server.shutdown().await.unwrap();
        database.drop().await;
    });
}

/// Threads through the real views: Bob opens a thread from the actions menu, replies from the
/// pane's composer, and Alice's window shows the summary under her message; she opens the pane
/// from the keyboard and sees the reply.
#[test]
fn two_gui_clients_hold_a_thread_through_the_real_views() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let database = rt.block_on(TempDatabase::create());
    let server = rt.block_on(async { testing::spawn_test_server(database.pool.clone()).await });
    let dir = tempfile::tempdir().unwrap();

    let mut alice = GuiClient::start("alice", &server.base_url, &dir.path().join("alice"));
    let mut bob = GuiClient::start("bob", &server.base_url, &dir.path().join("bob"));
    alice.register_with_the_keyboard("alice", "Alice Nguyen");
    bob.register_with_the_keyboard("bob", "Bob Okafor");

    alice.type_text("shall we ship on friday");
    alice.key("enter");
    bob.wait_until("Alice's message", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c)
                .iter()
                .any(|i| i.document.plain_text() == "shall we ship on friday" && i.is_confirmed())
        })
    });

    // Bob: Shift+Tab to the timeline, Enter for the menu (Add reaction, Reply in thread, ...),
    // down to Reply in thread, Enter. Focus moves to the pane's reply box.
    bob.keys("shift-tab enter down enter");
    bob.wait_until("the thread pane", |core| core.open_thread().is_some());
    bob.type_text("friday works for me");
    bob.key("enter");

    // Alice's channel shows the root with its summary, not the reply itself.
    alice.wait_until("the summary under her message", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c).iter().any(|i| {
                i.document.plain_text() == "shall we ship on friday"
                    && i.thread.as_ref().is_some_and(|t| t.reply_count == 1)
            }) && core.timeline(c).len() == 1
        })
    });

    // Alice opens it from the keyboard: the cursor is on the message, Down reaches the summary
    // line, Enter opens the pane.
    alice.keys("shift-tab down enter");
    alice.wait_until("her thread pane with the reply", |core| {
        core.open_thread().is_some_and(|t| {
            core.thread(t.root).is_some_and(|v| {
                v.replies
                    .iter()
                    .any(|r| r.document.plain_text() == "friday works for me")
            })
        })
    });

    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        alice.screenshot_as(&dir, "thread");
        bob.screenshot_as(&dir, "thread");
    }

    alice.stop();
    bob.stop();
    rt.block_on(async {
        server.shutdown().await.unwrap();
        database.drop().await;
    });
}

/// Search through the real views: Ctrl+F, a word, Enter, and the arrow keys; Enter opens the hit
/// and the conversation shows the message highlighted (see the screenshot).
#[test]
fn a_gui_client_searches_and_opens_a_result_with_the_keyboard() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let database = rt.block_on(TempDatabase::create());
    let server = rt.block_on(async { testing::spawn_test_server(database.pool.clone()).await });
    let dir = tempfile::tempdir().unwrap();

    let mut alice = GuiClient::start("alice", &server.base_url, &dir.path().join("alice"));
    let mut bob = GuiClient::start("bob", &server.base_url, &dir.path().join("bob"));
    alice.register_with_the_keyboard("alice", "Alice Nguyen");
    bob.register_with_the_keyboard("bob", "Bob Okafor");
    for text in [
        "the deploy to staging failed again",
        "lunch is at noon",
        "who owns the deploy runbook",
    ] {
        alice.type_text(text);
        alice.key("enter");
    }
    bob.wait_until("the three messages", |core| {
        core.selected_conversation()
            .is_some_and(|c| core.timeline(c).len() == 3)
    });

    // Ctrl+F from the composer, a word, Enter to search now.
    bob.key("ctrl-f");
    bob.type_text("deploy");
    bob.key("enter");
    bob.wait_until("the results", |core| {
        core.search().is_some_and(|v| {
            matches!(v.status, lazy_client_core::SearchStatus::Ready) && v.hits.len() == 2
        })
    });
    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        bob.screenshot_as(&dir, "search");
    }

    // Down to the first result, Enter opens it: the conversation is shown with the jump target.
    bob.keys("down enter");
    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        bob.screenshot_as(&dir, "search-after-enter");
    }
    // The view scrolls to the message, highlights it, and closes the search; the jump target is
    // consumed as soon as it has been used, so what is observable is that search is closed.
    bob.wait_until("the search to close on the opened message", |core| {
        core.search().is_none()
    });
    assert!(bob.read(|core| core.selected_conversation().is_some()));
    if let Some(dir) = std::env::var_os("LAZY_SCREENSHOT_DIR").map(PathBuf::from) {
        bob.screenshot_as(&dir, "search-opened");
    }

    alice.stop();
    bob.stop();
    rt.block_on(async {
        server.shutdown().await.unwrap();
        database.drop().await;
    });
}
