//! Durability across a real server-process crash: runs the actual `lazy-server` binary, kills it
//! with SIGKILL right after it acknowledges a message, restarts it, and checks the message is
//! still there and its operation is still known.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lazy_domain::{DisplayName, MessageDocument, OperationId, Username};
use lazy_protocol::api::paths;
use lazy_protocol::{
    BootstrapResponse, HistoryPage, OperationStatus, Password, RegisterRequest, SendMessageRequest,
    SendMessageResponse, SessionResponse,
};
use sqlx::{Connection, PgConnection};

const DEADLINE: Duration = Duration::from_secs(30);

struct ServerProcess {
    child: Child,
    base: String,
}

impl ServerProcess {
    async fn start(database_url: &str, port: u16) -> ServerProcess {
        let child = Command::new(env!("CARGO_BIN_EXE_lazy-server"))
            .args([
                "serve",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--database-url",
                database_url,
            ])
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start lazy-server");
        let base = format!("http://127.0.0.1:{port}");
        let mut server = ServerProcess { child, base };
        server.wait_healthy().await;
        server
    }

    async fn wait_healthy(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        let client = reqwest::Client::new();
        loop {
            if let Ok(response) = client
                .get(format!("{}{}", self.base, paths::HEALTH))
                .send()
                .await
                && response.status().is_success()
            {
                return;
            }
            assert!(
                self.child.try_wait().expect("poll child").is_none(),
                "the server exited early"
            );
            assert!(Instant::now() < deadline, "the server never became healthy");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// SIGKILL: no shutdown hooks, no flushing, as in a crash or power loss of the process.
    fn crash(mut self) {
        self.child.kill().expect("kill the server");
        self.child.wait().expect("reap the server");
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn an_acknowledged_message_survives_a_server_process_crash() {
    // The test Postgres (`LAZY_PG_INSTANCE=test scripts/dev-db.sh url`) must be running; never the dev one.
    let admin_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://lazy@127.0.0.1:54330/lazy".to_owned());
    let database = format!("lazy_crash_{}", OperationId::generate().as_uuid().simple());
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect to Postgres");
    // The name is generated from a UUID's hex digits, so it cannot carry SQL.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .execute(&mut admin)
        .await
        .expect("create database");
    let url = {
        let (base, _) = admin_url.rsplit_once('/').expect("a database URL");
        format!("{base}/{database}")
    };

    let port = free_port();
    let server = ServerProcess::start(&url, port).await;
    let http = reqwest::Client::new();

    let session: SessionResponse = http
        .post(format!("{}{}", server.base, paths::REGISTER))
        .json(&RegisterRequest {
            username: Username::parse("durable").unwrap(),
            display_name: DisplayName::parse("Durable").unwrap(),
            password: Password::new("correct horse battery"),
            invite_code: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = session.token.expose().to_owned();
    let bootstrap: BootstrapResponse = http
        .get(format!("{}{}", server.base, paths::BOOTSTRAP))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let general = bootstrap.workspaces[0].conversations[0].id;

    let operation = OperationId::generate();
    let acknowledged: SendMessageResponse = http
        .post(format!("{}{}", server.base, paths::messages(general)))
        .bearer_auth(&token)
        .json(&SendMessageRequest {
            operation_id: operation,
            document: MessageDocument::from_plain_text("this must survive").unwrap(),
            thread_root: None,
            also_in_channel: false,
            attachment_ids: Vec::new(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .expect("an acknowledgment");

    // The acknowledgment is in hand. Crash the process immediately.
    server.crash();

    let server = ServerProcess::start(&url, port).await;
    let history: HistoryPage = http
        .get(format!("{}{}", server.base, paths::messages(general)))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history.messages.len(), 1);
    assert_eq!(history.messages[0].id, acknowledged.message.id);
    assert_eq!(history.messages[0].seq, acknowledged.message.seq);

    // The session, and the record of the operation, survived too.
    let status: OperationStatus = http
        .get(format!("{}{}", server.base, paths::operation(operation)))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status,
        OperationStatus::Committed {
            message: acknowledged.message.clone()
        }
    );

    drop(server);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {database} WITH (FORCE)"
    )))
    .execute(&mut admin)
    .await
    .expect("drop database");
}
