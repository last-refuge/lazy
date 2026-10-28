//! Shared helpers for the end-to-end tests: a real server on a loopback port, a plain HTTP
//! client, and a WebSocket client with bounded waits.

#![allow(dead_code)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use lazy_domain::{ConversationId, EventSeq, MessageDocument, OperationId, Username, WorkspaceId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    ApiError, BootstrapResponse, ClientFrame, ErrorCode, HistoryPage, LoginRequest, Message,
    OperationStatus, Password, RegisterRequest, SendMessageRequest, SendMessageResponse,
    ServerFrame, SessionResponse, User,
};
use lazy_server::db::{self, SeededWorkspace};
use lazy_server::testing::{self, TestServer};
use lazy_server::{PasswordCost, RetentionPolicy, ServerOptions};
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// How long any single wait in a test may take before the test fails.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// A retention policy that ages events out after 30 days and keeps the newest `keep_newest`.
pub fn retention(keep_newest: u32) -> RetentionPolicy {
    RetentionPolicy {
        days: 30,
        interval: Duration::from_millis(10),
        keep_newest,
    }
}

pub const PASSWORD: &str = "correct horse battery";

pub struct App {
    pub server: TestServer,
    pub pool: PgPool,
    pub http: reqwest::Client,
    pub workspace_id: WorkspaceId,
    pub general: ConversationId,
}

#[derive(Clone)]
pub struct Account {
    pub token: String,
    pub user: User,
}

impl App {
    /// Seeds the default workspace and starts a server with open registration.
    pub async fn start(pool: PgPool) -> App {
        App::start_with(
            pool,
            ServerOptions {
                open_registration: true,
                password_cost: PasswordCost::FAST_FOR_TESTS,
                retention: RetentionPolicy::disabled(),
            },
        )
        .await
    }

    pub async fn start_with(pool: PgPool, options: ServerOptions) -> App {
        let SeededWorkspace {
            workspace_id,
            general_id,
        } = db::seed_default_workspace(&pool)
            .await
            .expect("seed")
            .expect("fresh database has no workspace");
        let server = testing::spawn_test_server_with(pool.clone(), options).await;
        App {
            server,
            pool,
            http: reqwest::Client::new(),
            workspace_id,
            general: general_id,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.base_url)
    }

    pub async fn register(&self, username: &str) -> Account {
        let response = self.try_register(username, PASSWORD).await;
        assert!(
            response.status().is_success(),
            "register {username}: {}",
            response.status()
        );
        let session: SessionResponse = response.json().await.expect("session json");
        Account {
            token: session.token.expose().to_owned(),
            user: session.user,
        }
    }

    pub async fn try_register(&self, username: &str, password: &str) -> reqwest::Response {
        let body = RegisterRequest {
            username: Username::parse(username).expect("valid username"),
            display_name: lazy_domain::DisplayName::parse(&format!("User {username}")).unwrap(),
            password: Password::new(password),
        };
        self.http
            .post(self.url(paths::REGISTER))
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    pub async fn try_login(&self, username: &str, password: &str) -> reqwest::Response {
        let body = LoginRequest {
            username: Username::parse(username).expect("valid username"),
            password: Password::new(password),
        };
        self.http
            .post(self.url(paths::LOGIN))
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    pub async fn bootstrap(&self, token: &str) -> reqwest::Response {
        self.http
            .get(self.url(paths::BOOTSTRAP))
            .bearer_auth(token)
            .send()
            .await
            .expect("request")
    }

    pub async fn bootstrap_ok(&self, account: &Account) -> BootstrapResponse {
        let response = self.bootstrap(&account.token).await;
        assert!(
            response.status().is_success(),
            "bootstrap: {}",
            response.status()
        );
        response.json().await.expect("bootstrap json")
    }

    pub async fn send_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        operation: OperationId,
        text: &str,
    ) -> reqwest::Response {
        let body = SendMessageRequest {
            operation_id: operation,
            document: MessageDocument::from_plain_text(text).expect("document"),
        };
        self.http
            .post(self.url(&paths::messages(conversation)))
            .bearer_auth(&account.token)
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    /// Sends with a fresh operation id and expects success.
    pub async fn send(
        &self,
        account: &Account,
        conversation: ConversationId,
        text: &str,
    ) -> Message {
        let response = self
            .send_raw(account, conversation, OperationId::generate(), text)
            .await;
        assert!(
            response.status().is_success(),
            "send: {}",
            response.status()
        );
        response
            .json::<SendMessageResponse>()
            .await
            .expect("send json")
            .message
    }

    pub async fn history(
        &self,
        account: &Account,
        conversation: ConversationId,
        query: &[(&str, String)],
    ) -> reqwest::Response {
        let mut url = self.url(&paths::messages(conversation));
        for (index, (key, value)) in query.iter().enumerate() {
            url.push(if index == 0 { '?' } else { '&' });
            url.push_str(&format!("{key}={value}"));
        }
        self.http
            .get(url)
            .bearer_auth(&account.token)
            .send()
            .await
            .expect("request")
    }

    pub async fn history_ok(
        &self,
        account: &Account,
        conversation: ConversationId,
        query: &[(&str, String)],
    ) -> HistoryPage {
        let response = self.history(account, conversation, query).await;
        assert!(
            response.status().is_success(),
            "history: {}",
            response.status()
        );
        response.json().await.expect("history json")
    }

    pub async fn operation(&self, account: &Account, operation: OperationId) -> OperationStatus {
        let response = self
            .http
            .get(self.url(&paths::operation(operation)))
            .bearer_auth(&account.token)
            .send()
            .await
            .expect("request");
        assert!(
            response.status().is_success(),
            "operation: {}",
            response.status()
        );
        response.json().await.expect("operation json")
    }

    /// The committed head of the workspace's log.
    pub async fn head(&self) -> EventSeq {
        let head: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces WHERE id = $1")
            .bind(self.workspace_id.as_uuid())
            .fetch_one(&self.pool)
            .await
            .expect("head");
        EventSeq::new(head).unwrap()
    }

    /// Every position in the workspace's log after `after`, in order.
    pub async fn log_positions_after(&self, after: EventSeq) -> Vec<i64> {
        sqlx::query_scalar(
            "SELECT seq FROM events WHERE workspace_id = $1 AND seq > $2 ORDER BY seq",
        )
        .bind(self.workspace_id.as_uuid())
        .bind(after.get())
        .fetch_all(&self.pool)
        .await
        .expect("log positions")
    }

    /// Makes events up to and including `upto` look `days` older than they are, so retention
    /// treats them as aged out without the test waiting for time to pass.
    pub async fn backdate_events(&self, upto: EventSeq, days: i32) {
        sqlx::query(
            "UPDATE events SET occurred_at = occurred_at - make_interval(days => $3) \
             WHERE workspace_id = $1 AND seq <= $2",
        )
        .bind(self.workspace_id.as_uuid())
        .bind(upto.get())
        .bind(days)
        .execute(&self.pool)
        .await
        .expect("backdate events");
    }

    /// The workspace's retention floor.
    pub async fn event_floor(&self) -> EventSeq {
        let floor: i64 = sqlx::query_scalar("SELECT event_floor FROM workspaces WHERE id = $1")
            .bind(self.workspace_id.as_uuid())
            .fetch_one(&self.pool)
            .await
            .expect("floor");
        EventSeq::new(floor).unwrap()
    }

    /// Creates a conversation directly in the database.
    pub async fn create_conversation(
        &self,
        name: &str,
        visibility: &str,
        archived: bool,
    ) -> ConversationId {
        let id = ConversationId::generate();
        sqlx::query(
            "INSERT INTO conversations (id, workspace_id, kind, name, visibility, archived) \
             VALUES ($1, $2, 'channel', $3, $4, $5)",
        )
        .bind(id.as_uuid())
        .bind(self.workspace_id.as_uuid())
        .bind(name)
        .bind(visibility)
        .bind(archived)
        .execute(&self.pool)
        .await
        .expect("create conversation");
        id
    }

    pub async fn add_to_conversation(&self, conversation: ConversationId, account: &Account) {
        sqlx::query("INSERT INTO conversation_members (conversation_id, user_id) VALUES ($1, $2)")
            .bind(conversation.as_uuid())
            .bind(account.user.id.as_uuid())
            .execute(&self.pool)
            .await
            .expect("add member");
    }

    pub async fn remove_from_conversation(&self, conversation: ConversationId, account: &Account) {
        sqlx::query("DELETE FROM conversation_members WHERE conversation_id = $1 AND user_id = $2")
            .bind(conversation.as_uuid())
            .bind(account.user.id.as_uuid())
            .execute(&self.pool)
            .await
            .expect("remove member");
    }

    pub async fn connect_ws(&self, account: &Account) -> Ws {
        Ws::connect(&self.server, &account.token).await
    }
}

pub async fn api_error(response: reqwest::Response) -> (u16, ApiError) {
    let status = response.status().as_u16();
    let error: ApiError = response.json().await.expect("error body is an ApiError");
    (status, error)
}

pub fn assert_error(status: u16, error: &ApiError, code: ErrorCode) {
    assert_eq!(error.code, code, "{error}");
    assert_eq!(status, code.http_status(), "{error}");
}

/// What the server did to a socket.
#[derive(Debug)]
pub enum Closed {
    Frame(Option<CloseCode>),
    Dropped,
}

pub struct Ws {
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Ws {
    pub async fn try_connect(
        server: &TestServer,
        token: &str,
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://{}{}", server.addr, paths::REALTIME)
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (inner, _) = connect_async(request).await?;
        Ok(Ws { inner })
    }

    pub async fn connect(server: &TestServer, token: &str) -> Ws {
        Ws::try_connect(server, token)
            .await
            .expect("websocket handshake")
    }

    pub async fn send(&mut self, frame: &ClientFrame) {
        let json = serde_json::to_string(frame).unwrap();
        self.send_text(&json).await;
    }

    pub async fn send_text(&mut self, text: &str) {
        self.inner
            .send(tokio_tungstenite::tungstenite::Message::text(
                text.to_owned(),
            ))
            .await
            .expect("send websocket frame");
    }

    pub async fn subscribe(&mut self, workspace_id: WorkspaceId, after: EventSeq) {
        self.send(&ClientFrame::Subscribe {
            workspace_id,
            after,
        })
        .await;
    }

    /// The next server frame, or the way the socket ended. Pings and pongs are skipped.
    pub async fn next_or_closed(&mut self) -> Result<ServerFrame, Closed> {
        use tokio_tungstenite::tungstenite::Message as Wire;
        loop {
            let next = tokio::time::timeout(DEADLINE, self.inner.next())
                .await
                .expect("timed out waiting for a websocket frame");
            match next {
                None | Some(Err(_)) => return Err(Closed::Dropped),
                Some(Ok(Wire::Text(text))) => {
                    return Ok(serde_json::from_str(text.as_str()).expect("server frame json"));
                }
                Some(Ok(Wire::Close(frame))) => return Err(Closed::Frame(frame.map(|f| f.code))),
                Some(Ok(_)) => {}
            }
        }
    }

    pub async fn next(&mut self) -> ServerFrame {
        match self.next_or_closed().await {
            Ok(frame) => frame,
            Err(closed) => panic!("socket closed while waiting for a frame: {closed:?}"),
        }
    }

    /// Reads frames until `caught_up`, returning the events seen before it.
    pub async fn replay(&mut self) -> (Vec<lazy_protocol::EventEnvelope>, EventSeq) {
        let mut events = Vec::new();
        loop {
            match self.next().await {
                ServerFrame::Event(envelope) => events.push(envelope),
                ServerFrame::CaughtUp { seq, .. } => return (events, seq),
                other => panic!("unexpected frame during replay: {other:?}"),
            }
        }
    }

    /// Reads events until one with position `seq` arrives, returning every event position seen.
    pub async fn events_through(&mut self, seq: EventSeq) -> Vec<lazy_protocol::EventEnvelope> {
        let mut events = Vec::new();
        loop {
            match self.next().await {
                ServerFrame::Event(envelope) => {
                    let done = envelope.seq >= seq;
                    events.push(envelope);
                    if done {
                        return events;
                    }
                }
                ServerFrame::CaughtUp { .. } => {}
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    /// Whether the socket yields nothing within `wait`. Only for asserting absence after a
    /// barrier has already proven that earlier events would have arrived.
    pub async fn is_quiet_for(&mut self, wait: Duration) -> bool {
        tokio::time::timeout(wait, self.next_or_closed())
            .await
            .is_err()
    }
}
