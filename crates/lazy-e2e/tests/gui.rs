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
            .unwrap_or_else(|_| "postgres://lazy@127.0.0.1:54329/lazy".to_owned());
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
        // Create-account order is: username, display name, password, button, link. Back to the top.
        self.keys("shift-tab shift-tab shift-tab shift-tab");
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
        std::fs::create_dir_all(dir).expect("create screenshot directory");
        self.settle();
        match self.cx.capture_screenshot(self.window.into()) {
            Ok(image) => image
                .save(dir.join(format!("gui-{}.png", self.name)))
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
