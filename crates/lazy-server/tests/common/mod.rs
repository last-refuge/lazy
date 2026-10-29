//! Shared helpers for the end-to-end tests: a real server on a loopback port, a plain HTTP
//! client, and a WebSocket client with bounded waits.

#![allow(dead_code)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use lazy_domain::mention::{MapDirectory, MentionTarget};
use lazy_domain::{
    ConversationId, EventSeq, MessageDocument, OperationId, UserId, Username, WorkspaceId,
};
use lazy_protocol::api::paths;
use lazy_protocol::{
    ApiError, BootstrapResponse, ClientFrame, ErrorCode, HistoryPage, LoginRequest, Message,
    OperationStatus, Password, RegisterRequest, SendMessageRequest, SendMessageResponse,
    ServerFrame, SessionResponse, User,
};
use lazy_server::db::{self, SeededWorkspace};
use lazy_server::testing::{self, TestServer};
use lazy_server::{
    AttachmentOptions, IntegrationOptions, PasswordCost, RetentionPolicy, SearchOptions,
    ServerOptions,
};
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
                open_workspace_creation: true,
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

    /// A server on a database that is already seeded and in use (after another server stopped),
    /// with the given options.
    pub async fn restart(
        pool: &PgPool,
        options: ServerOptions,
        workspace_id: WorkspaceId,
        general: ConversationId,
    ) -> App {
        let server = testing::spawn_test_server_with(pool.clone(), options).await;
        App {
            server,
            pool: pool.clone(),
            http: reqwest::Client::new(),
            workspace_id,
            general,
        }
    }

    /// Like [`App::start`], with settings for search (its time limit and how many matches it ranks).
    pub async fn start_with_search(pool: PgPool, search: SearchOptions) -> App {
        let SeededWorkspace {
            workspace_id,
            general_id,
        } = db::seed_default_workspace(&pool)
            .await
            .expect("seed")
            .expect("fresh database has no workspace");
        let server = testing::spawn_test_server_all(
            pool.clone(),
            ServerOptions {
                open_registration: true,
                open_workspace_creation: true,
                password_cost: PasswordCost::FAST_FOR_TESTS,
                retention: RetentionPolicy::disabled(),
            },
            IntegrationOptions::default(),
            AttachmentOptions::default(),
            search,
        )
        .await;
        App {
            server,
            pool,
            http: reqwest::Client::new(),
            workspace_id,
            general: general_id,
        }
    }

    /// Like [`App::start`], with settings for attachments (limits, timings, a hook for failures).
    pub async fn start_with_attachments(pool: PgPool, attachments: AttachmentOptions) -> App {
        let SeededWorkspace {
            workspace_id,
            general_id,
        } = db::seed_default_workspace(&pool)
            .await
            .expect("seed")
            .expect("fresh database has no workspace");
        let server = testing::spawn_test_server_full(
            pool.clone(),
            ServerOptions {
                open_registration: true,
                open_workspace_creation: true,
                password_cost: PasswordCost::FAST_FOR_TESTS,
                retention: RetentionPolicy::disabled(),
            },
            IntegrationOptions::default(),
            attachments,
        )
        .await;
        App {
            server,
            pool,
            http: reqwest::Client::new(),
            workspace_id,
            general: general_id,
        }
    }

    /// Like [`App::start`], with settings for integrations (webhook address and rate).
    pub async fn start_with_integrations(pool: PgPool, integrations: IntegrationOptions) -> App {
        let SeededWorkspace {
            workspace_id,
            general_id,
        } = db::seed_default_workspace(&pool)
            .await
            .expect("seed")
            .expect("fresh database has no workspace");
        let server = testing::spawn_test_server_configured(
            pool.clone(),
            ServerOptions {
                open_registration: true,
                open_workspace_creation: true,
                password_cost: PasswordCost::FAST_FOR_TESTS,
                retention: RetentionPolicy::disabled(),
            },
            integrations,
        )
        .await;
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
        self.try_register_with(username, password, None).await
    }

    /// Registers with an invitation code (or none).
    pub async fn try_register_with(
        &self,
        username: &str,
        password: &str,
        invite_code: Option<&str>,
    ) -> reqwest::Response {
        let body = RegisterRequest {
            username: Username::parse(username).expect("valid username"),
            display_name: lazy_domain::DisplayName::parse(&format!("User {username}")).unwrap(),
            password: Password::new(password),
            invite_code: invite_code.map(lazy_protocol::InviteCode::new),
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
            thread_root: None,
            also_in_channel: false,
            attachment_ids: Vec::new(),
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

    /// The positions in any workspace's log, in order.
    pub async fn log_positions_in(&self, workspace: WorkspaceId) -> Vec<i64> {
        sqlx::query_scalar("SELECT seq FROM events WHERE workspace_id = $1 ORDER BY seq")
            .bind(workspace.as_uuid())
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

    /// `PATCH /v1/messages/{id}` with a document written in the composer's markup.
    pub async fn edit_raw(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
        operation: OperationId,
        base_revision: u32,
        markdown: &str,
    ) -> reqwest::Response {
        let document = MessageDocument::from_markdown(markdown).expect("document");
        self.call(
            reqwest::Method::PATCH,
            &paths::message(message),
            account,
            Some(serde_json::json!({
                "operation_id": operation, "base_revision": base_revision, "document": document,
            })),
        )
        .await
    }

    /// Edits a message with a fresh operation id and expects success.
    pub async fn edit(&self, account: &Account, message: &Message, markdown: &str) -> Message {
        let response = self
            .edit_raw(
                account,
                message.id,
                OperationId::generate(),
                message.revision,
                markdown,
            )
            .await;
        assert!(
            response.status().is_success(),
            "edit: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::MessageResponse>()
            .await
            .expect("json")
            .message
    }

    pub async fn delete_raw(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::DELETE,
            &paths::message(message),
            account,
            None,
        )
        .await
    }

    /// Deletes a message and expects success.
    pub async fn delete(&self, account: &Account, message: lazy_domain::MessageId) -> Message {
        let response = self.delete_raw(account, message).await;
        assert!(
            response.status().is_success(),
            "delete: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::MessageResponse>()
            .await
            .expect("json")
            .message
    }

    pub async fn get_message_raw(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::GET,
            &paths::message(message),
            account,
            None,
        )
        .await
    }

    /// Fetches one message (a tombstone included) and expects success.
    pub async fn get_message(&self, account: &Account, message: lazy_domain::MessageId) -> Message {
        let response = self.get_message_raw(account, message).await;
        assert!(
            response.status().is_success(),
            "get message: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::MessageResponse>()
            .await
            .expect("json")
            .message
    }

    /// `PUT /v1/messages/{id}/reactions` with an arbitrary body, so tests can send bad ones.
    pub async fn react_body(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PUT,
            &paths::reactions(message),
            account,
            Some(body),
        )
        .await
    }

    /// Sets or clears one reaction; the response is left for the caller to inspect.
    pub async fn react_raw(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
        glyph: &str,
        present: bool,
    ) -> reqwest::Response {
        self.react_body(
            account,
            message,
            serde_json::json!({"emoji": glyph, "present": present}),
        )
        .await
    }

    /// Sets or clears one reaction and expects success.
    pub async fn react(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
        glyph: &str,
        present: bool,
    ) -> Message {
        let response = self.react_raw(account, message, glyph, present).await;
        assert!(
            response.status().is_success(),
            "react: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::MessageResponse>()
            .await
            .expect("json")
            .message
    }

    /// Positions of the `reactions_changed` events about a message, in order.
    pub async fn reaction_events(&self, message: lazy_domain::MessageId) -> Vec<i64> {
        self.message_events(message)
            .await
            .into_iter()
            .filter(|(_, kind)| kind == "reactions_changed")
            .map(|(seq, _)| seq)
            .collect()
    }

    /// Rows in `message_reactions` for a message.
    pub async fn reaction_rows(&self, message: lazy_domain::MessageId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM message_reactions WHERE message_id = $1")
            .bind(message.as_uuid())
            .fetch_one(&self.pool)
            .await
            .expect("reaction rows")
    }

    /// A document with mentions, written the way the composer writes it: `@name` becomes a mention
    /// wherever `entries` resolves it.
    pub fn mention_document(text: &str, entries: &[(&str, MentionTarget)]) -> MessageDocument {
        let mut directory = MapDirectory::new();
        for (handle, target) in entries {
            directory.insert(handle, *target);
        }
        MessageDocument::from_markdown_with(text, &directory).expect("document")
    }

    /// Sends a message that may contain mentions; the response is left for the caller.
    pub async fn send_doc_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        document: MessageDocument,
    ) -> reqwest::Response {
        let body = SendMessageRequest {
            operation_id: OperationId::generate(),
            document,
            thread_root: None,
            also_in_channel: false,
            attachment_ids: Vec::new(),
        };
        self.call(
            reqwest::Method::POST,
            &paths::messages(conversation),
            account,
            Some(serde_json::to_value(&body).expect("body")),
        )
        .await
    }

    /// Sends a message with mentions and expects success.
    pub async fn send_mentioning(
        &self,
        account: &Account,
        conversation: ConversationId,
        text: &str,
        entries: &[(&str, MentionTarget)],
    ) -> Message {
        let response = self
            .send_doc_raw(account, conversation, Self::mention_document(text, entries))
            .await;
        assert!(
            response.status().is_success(),
            "send: {}",
            response.status()
        );
        response
            .json::<SendMessageResponse>()
            .await
            .expect("json")
            .message
    }

    /// Edits a message to a document with mentions; the response is left for the caller.
    pub async fn edit_mentioning_raw(
        &self,
        account: &Account,
        message: &Message,
        operation: OperationId,
        text: &str,
        entries: &[(&str, MentionTarget)],
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PATCH,
            &paths::message(message.id),
            account,
            Some(serde_json::json!({
                "operation_id": operation,
                "base_revision": message.revision,
                "document": Self::mention_document(text, entries),
            })),
        )
        .await
    }

    /// Edits a message to a document with mentions and expects success.
    pub async fn edit_mentioning(
        &self,
        account: &Account,
        message: &Message,
        text: &str,
        entries: &[(&str, MentionTarget)],
    ) -> Message {
        let response = self
            .edit_mentioning_raw(account, message, OperationId::generate(), text, entries)
            .await;
        assert!(
            response.status().is_success(),
            "edit: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::MessageResponse>()
            .await
            .expect("json")
            .message
    }

    pub async fn create_group_raw(
        &self,
        account: &Account,
        handle: &str,
        name: &str,
        members: &[&Account],
        operation: OperationId,
    ) -> reqwest::Response {
        let members: Vec<_> = members.iter().map(|m| m.user.id).collect();
        self.call(
            reqwest::Method::POST,
            &paths::groups(self.workspace_id),
            account,
            Some(serde_json::json!({
                "operation_id": operation, "handle": handle, "name": name, "members": members,
            })),
        )
        .await
    }

    /// Creates a group and expects success.
    pub async fn create_group(
        &self,
        account: &Account,
        handle: &str,
        members: &[&Account],
    ) -> lazy_protocol::UserGroup {
        let response = self
            .create_group_raw(
                account,
                handle,
                &format!("Group {handle}"),
                members,
                OperationId::generate(),
            )
            .await;
        assert!(
            response.status().is_success(),
            "create group {handle}: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::GroupResponse>()
            .await
            .expect("json")
            .group
    }

    pub async fn update_group_raw(
        &self,
        account: &Account,
        group: lazy_domain::GroupId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PATCH,
            &paths::group(group),
            account,
            Some(body),
        )
        .await
    }

    pub async fn delete_group_raw(
        &self,
        account: &Account,
        group: lazy_domain::GroupId,
    ) -> reqwest::Response {
        self.call(reqwest::Method::DELETE, &paths::group(group), account, None)
            .await
    }

    pub async fn list_groups(&self, account: &Account) -> Vec<lazy_protocol::UserGroup> {
        let response = self
            .call(
                reqwest::Method::GET,
                &paths::groups(self.workspace_id),
                account,
                None,
            )
            .await;
        assert!(
            response.status().is_success(),
            "list groups: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::GroupsResponse>()
            .await
            .expect("json")
            .groups
    }

    /// A second workspace, created directly in the database, with the given people in it.
    pub async fn other_workspace(&self, members: &[(&Account, &str)]) -> WorkspaceId {
        let id = WorkspaceId::generate();
        sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'Elsewhere')")
            .bind(id.as_uuid())
            .execute(&self.pool)
            .await
            .expect("other workspace");
        for (account, role) in members {
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)",
            )
            .bind(id.as_uuid())
            .bind(account.user.id.as_uuid())
            .bind(*role)
            .execute(&self.pool)
            .await
            .expect("other workspace member");
        }
        id
    }

    /// The people `message_mentions` has for a message, in id order.
    pub async fn mention_rows(&self, message: lazy_domain::MessageId) -> Vec<UserId> {
        let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM message_mentions WHERE message_id = $1 ORDER BY user_id",
        )
        .bind(message.as_uuid())
        .fetch_all(&self.pool)
        .await
        .expect("mention rows");
        rows.into_iter().map(UserId::from_uuid).collect()
    }

    /// Prior revisions kept for a message.
    pub async fn kept_revisions(&self, message: lazy_domain::MessageId) -> Vec<i32> {
        sqlx::query_scalar(
            "SELECT revision FROM message_revisions WHERE message_id = $1 ORDER BY revision",
        )
        .bind(message.as_uuid())
        .fetch_all(&self.pool)
        .await
        .expect("revisions")
    }

    /// Positions and kinds of the events about one message, in order.
    pub async fn message_events(&self, message: lazy_domain::MessageId) -> Vec<(i64, String)> {
        sqlx::query_as("SELECT seq, kind FROM events WHERE message_id = $1 ORDER BY seq")
            .bind(message.as_uuid())
            .fetch_all(&self.pool)
            .await
            .expect("events")
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

    /// Adds a member directly in the database, without events, keeping the member count and the
    /// person's state row consistent with what the server does.
    pub async fn add_to_conversation(&self, conversation: ConversationId, account: &Account) {
        sqlx::query("INSERT INTO conversation_members (conversation_id, user_id) VALUES ($1, $2)")
            .bind(conversation.as_uuid())
            .bind(account.user.id.as_uuid())
            .execute(&self.pool)
            .await
            .expect("add member");
        sqlx::query(
            "INSERT INTO conversation_user_state (user_id, conversation_id, last_read) \
             VALUES ($1, $2, COALESCE((SELECT max(seq) FROM messages WHERE conversation_id = $2), 0))",
        )
        .bind(account.user.id.as_uuid())
        .bind(conversation.as_uuid())
        .execute(&self.pool)
        .await
        .expect("add state");
        sqlx::query("UPDATE conversations SET member_count = member_count + 1 WHERE id = $1")
            .bind(conversation.as_uuid())
            .execute(&self.pool)
            .await
            .expect("count member");
    }

    /// Removes a member directly in the database, without events.
    pub async fn remove_from_conversation(&self, conversation: ConversationId, account: &Account) {
        sqlx::query("DELETE FROM conversation_members WHERE conversation_id = $1 AND user_id = $2")
            .bind(conversation.as_uuid())
            .bind(account.user.id.as_uuid())
            .execute(&self.pool)
            .await
            .expect("remove member");
        sqlx::query(
            "DELETE FROM conversation_user_state WHERE conversation_id = $1 AND user_id = $2",
        )
        .bind(conversation.as_uuid())
        .bind(account.user.id.as_uuid())
        .execute(&self.pool)
        .await
        .expect("remove state");
        sqlx::query("UPDATE conversations SET member_count = member_count - 1 WHERE id = $1")
            .bind(conversation.as_uuid())
            .execute(&self.pool)
            .await
            .expect("count member");
    }

    /// Changes someone's workspace role directly in the database.
    pub async fn set_role(&self, account: &Account, role: &str) {
        sqlx::query(
            "UPDATE workspace_members SET role = $1 WHERE workspace_id = $2 AND user_id = $3",
        )
        .bind(role)
        .bind(self.workspace_id.as_uuid())
        .bind(account.user.id.as_uuid())
        .execute(&self.pool)
        .await
        .expect("set role");
    }

    /// Sends an authenticated request with an optional JSON body.
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        account: &Account,
        body: Option<serde_json::Value>,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .request(method, self.url(path))
            .bearer_auth(&account.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.expect("request")
    }

    /// Creates a channel and expects success.
    pub async fn create_channel(
        &self,
        account: &Account,
        name: &str,
        visibility: &str,
    ) -> lazy_protocol::Conversation {
        let response = self
            .create_channel_raw(account, name, visibility, OperationId::generate())
            .await;
        assert!(
            response.status().is_success(),
            "create {name}: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ConversationResponse>()
            .await
            .expect("json")
            .conversation
    }

    pub async fn create_channel_raw(
        &self,
        account: &Account,
        name: &str,
        visibility: &str,
        operation: OperationId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::channels(self.workspace_id),
            account,
            Some(serde_json::json!({
                "operation_id": operation, "name": name, "visibility": visibility,
                "topic": null, "description": null,
            })),
        )
        .await
    }

    pub async fn open_raw(
        &self,
        account: &Account,
        others: &[&Account],
        operation: OperationId,
    ) -> reqwest::Response {
        let participants: Vec<_> = others.iter().map(|other| other.user.id).collect();
        self.call(
            reqwest::Method::POST,
            &paths::open_conversation(self.workspace_id),
            account,
            Some(serde_json::json!({"operation_id": operation, "participants": participants})),
        )
        .await
    }

    /// Opens a direct or group conversation and expects success.
    pub async fn open(
        &self,
        account: &Account,
        others: &[&Account],
    ) -> lazy_protocol::Conversation {
        let response = self
            .open_raw(account, others, OperationId::generate())
            .await;
        assert!(
            response.status().is_success(),
            "open: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ConversationResponse>()
            .await
            .expect("json")
            .conversation
    }

    /// `POST /v1/conversations/{id}/{action}` with no body.
    pub async fn act(
        &self,
        account: &Account,
        conversation: ConversationId,
        action: &str,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::conversation_action(conversation, action),
            account,
            None,
        )
        .await
    }

    pub async fn add_members(
        &self,
        account: &Account,
        conversation: ConversationId,
        users: &[&Account],
    ) -> reqwest::Response {
        let ids: Vec<_> = users.iter().map(|user| user.user.id).collect();
        self.call(
            reqwest::Method::POST,
            &paths::conversation_action(conversation, "members"),
            account,
            Some(serde_json::json!({"user_ids": ids})),
        )
        .await
    }

    pub async fn remove_member(
        &self,
        account: &Account,
        conversation: ConversationId,
        user: &Account,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::DELETE,
            &paths::member(conversation, user.user.id),
            account,
            None,
        )
        .await
    }

    pub async fn update(
        &self,
        account: &Account,
        conversation: ConversationId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PATCH,
            &paths::conversation(conversation),
            account,
            Some(body),
        )
        .await
    }

    pub async fn mark_read(
        &self,
        account: &Account,
        conversation: ConversationId,
        up_to: i64,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::conversation_action(conversation, "read"),
            account,
            Some(serde_json::json!({"up_to": up_to})),
        )
        .await
    }

    pub async fn favorite(
        &self,
        account: &Account,
        conversation: ConversationId,
        favorite: bool,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PUT,
            &paths::conversation_action(conversation, "favorite"),
            account,
            Some(serde_json::json!({"favorite": favorite})),
        )
        .await
    }

    /// The caller's conversations as bootstrap reports them.
    pub async fn conversations_of(&self, account: &Account) -> Vec<lazy_protocol::Conversation> {
        self.bootstrap_ok(account).await.workspaces[0]
            .conversations
            .clone()
    }

    /// The caller's state for one conversation as bootstrap reports it.
    pub async fn state_of(
        &self,
        account: &Account,
        conversation: ConversationId,
    ) -> lazy_protocol::ConversationUserState {
        self.bootstrap_ok(account).await.workspaces[0]
            .user_state
            .iter()
            .find(|state| state.conversation_id == conversation)
            .cloned()
            .unwrap_or_else(|| panic!("no state for {conversation}"))
    }

    /// Number of members according to the table, for checking the maintained count.
    pub async fn counted_members(&self, conversation: ConversationId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM conversation_members WHERE conversation_id = $1")
            .bind(conversation.as_uuid())
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }

    // --- Custom emoji ----------------------------------------------------------------------

    /// `PUT`s an image as a new custom emoji, naming it in the header (or not, with `None`).
    pub async fn upload_emoji_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        id: lazy_domain::EmojiId,
        bytes: Vec<u8>,
        name: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .put(self.url(&paths::emoji_upload(workspace, id)))
            .bearer_auth(&account.token)
            .header("content-type", "application/octet-stream");
        if let Some(name) = name {
            request = request.header(paths::EMOJI_NAME_HEADER, name);
        }
        request.body(bytes).send().await.expect("request")
    }

    /// Adds a custom emoji under a fresh id and expects success.
    pub async fn upload_emoji(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        name: &str,
        bytes: Vec<u8>,
    ) -> lazy_protocol::CustomEmoji {
        let response = self
            .upload_emoji_raw(
                account,
                workspace,
                lazy_domain::EmojiId::generate(),
                bytes,
                Some(name),
            )
            .await;
        assert!(
            response.status().is_success(),
            "upload emoji: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::CustomEmojiResponse>()
            .await
            .expect("emoji json")
            .emoji
    }

    /// `GET`, `PATCH` (with a body) or `DELETE` on an emoji, with extra request headers.
    pub async fn emoji_raw(
        &self,
        account: &Account,
        id: lazy_domain::EmojiId,
        method: reqwest::Method,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut request = self
            .http
            .request(method, self.url(&paths::emoji(id)))
            .bearer_auth(&account.token);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.expect("request")
    }

    pub async fn rename_emoji_raw(
        &self,
        account: &Account,
        id: lazy_domain::EmojiId,
        name: &str,
    ) -> reqwest::Response {
        self.emoji_raw(
            account,
            id,
            reqwest::Method::PATCH,
            Some(serde_json::json!({"name": name})),
            &[],
        )
        .await
    }

    // --- Attachments -----------------------------------------------------------------------

    /// `PUT`s a file for a conversation, naming it `name` (percent-encoded here) and declaring
    /// `content_type`; the response is left for the caller.
    pub async fn upload_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        id: lazy_domain::AttachmentId,
        bytes: Vec<u8>,
        content_type: &str,
        name: &str,
    ) -> reqwest::Response {
        self.upload_with_name_header_raw(
            account,
            conversation,
            id,
            bytes,
            content_type,
            Some(&percent_encode(name)),
        )
        .await
    }

    /// Like [`App::upload_raw`] with the name header exactly as given (or absent).
    pub async fn upload_with_name_header_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        id: lazy_domain::AttachmentId,
        bytes: Vec<u8>,
        content_type: &str,
        name_header: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .put(self.url(&paths::attachment_upload(conversation, id)))
            .bearer_auth(&account.token)
            .header("content-type", content_type);
        if let Some(name) = name_header {
            request = request.header(paths::ATTACHMENT_NAME_HEADER, name);
        }
        request.body(bytes).send().await.expect("request")
    }

    /// Uploads a file under a fresh id and expects success.
    pub async fn upload(
        &self,
        account: &Account,
        conversation: ConversationId,
        bytes: Vec<u8>,
        content_type: &str,
        name: &str,
    ) -> lazy_protocol::Attachment {
        let response = self
            .upload_raw(
                account,
                conversation,
                lazy_domain::AttachmentId::generate(),
                bytes,
                content_type,
                name,
            )
            .await;
        assert!(
            response.status().is_success(),
            "upload: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::AttachmentResponse>()
            .await
            .expect("upload json")
            .attachment
    }

    /// `GET`s (or with `head`, `HEAD`s) a file or its thumbnail, with extra request headers.
    pub async fn download_raw(
        &self,
        account: &Account,
        id: lazy_domain::AttachmentId,
        thumbnail: bool,
        head: bool,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let path = if thumbnail {
            paths::attachment_thumbnail(id)
        } else {
            paths::attachment(id)
        };
        let method = if head {
            reqwest::Method::HEAD
        } else {
            reqwest::Method::GET
        };
        let mut request = self
            .http
            .request(method, self.url(&path))
            .bearer_auth(&account.token);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.expect("request")
    }

    /// Sends a message of `text` (empty for none) naming `files`; the response is left for the
    /// caller.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_files_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        operation: OperationId,
        text: &str,
        files: &[lazy_domain::AttachmentId],
        root: Option<&Message>,
        also_in_channel: bool,
    ) -> reqwest::Response {
        let document = if text.is_empty() {
            MessageDocument::empty()
        } else {
            MessageDocument::from_plain_text(text).expect("document")
        };
        let body = SendMessageRequest {
            operation_id: operation,
            document,
            thread_root: root.map(|root| root.id),
            also_in_channel,
            attachment_ids: files.to_vec(),
        };
        self.call(
            reqwest::Method::POST,
            &paths::messages(conversation),
            account,
            Some(serde_json::to_value(&body).expect("body")),
        )
        .await
    }

    /// Sends a message with files and expects success.
    pub async fn send_with_files(
        &self,
        account: &Account,
        conversation: ConversationId,
        text: &str,
        files: &[lazy_domain::AttachmentId],
    ) -> Message {
        let response = self
            .send_with_files_raw(
                account,
                conversation,
                OperationId::generate(),
                text,
                files,
                None,
                false,
            )
            .await;
        assert!(
            response.status().is_success(),
            "send: {}",
            response.status()
        );
        response
            .json::<SendMessageResponse>()
            .await
            .expect("json")
            .message
    }

    // --- Notifications, status, inbox ------------------------------------------------------

    /// `PUT` with a JSON body.
    pub async fn put_json(
        &self,
        account: &Account,
        path: &str,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(reqwest::Method::PUT, path, account, Some(body))
            .await
    }

    pub async fn set_settings_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.put_json(account, &paths::notification_settings(workspace), body)
            .await
    }

    /// Replaces the caller's settings in this workspace and expects success.
    pub async fn set_settings(
        &self,
        account: &Account,
        settings: &lazy_domain::notification::NotificationSettings,
    ) -> lazy_domain::notification::NotificationSettings {
        let response = self
            .set_settings_raw(
                account,
                self.workspace_id,
                serde_json::to_value(settings).expect("settings"),
            )
            .await;
        assert!(
            response.status().is_success(),
            "settings: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::NotificationSettingsResponse>()
            .await
            .expect("settings json")
            .settings
    }

    pub async fn set_prefs_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.put_json(
            account,
            &paths::conversation_notification_prefs(conversation),
            body,
        )
        .await
    }

    /// Sets the caller's preference for a conversation and expects success.
    pub async fn set_prefs(
        &self,
        account: &Account,
        conversation: ConversationId,
        level: Option<lazy_domain::notification::NotifyLevel>,
        muted: bool,
    ) -> lazy_domain::notification::ConversationNotificationPrefs {
        let response = self
            .set_prefs_raw(
                account,
                conversation,
                serde_json::json!({"level": level, "muted": muted}),
            )
            .await;
        assert!(
            response.status().is_success(),
            "prefs: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ConversationNotificationPrefsResponse>()
            .await
            .expect("prefs json")
            .prefs
    }

    pub async fn set_status_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.put_json(account, &paths::status(workspace), body)
            .await
    }

    /// Sets a status with just text (and optionally an emoji) and expects success.
    pub async fn set_status(&self, account: &Account, text: &str, emoji: Option<&str>) -> User {
        let response = self
            .set_status_raw(
                account,
                self.workspace_id,
                serde_json::json!({"status": {"text": text, "emoji": emoji}}),
            )
            .await;
        assert!(
            response.status().is_success(),
            "status: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::StatusResponse>()
            .await
            .expect("status json")
            .user
    }

    pub async fn clear_status(&self, account: &Account) -> User {
        let response = self
            .set_status_raw(
                account,
                self.workspace_id,
                serde_json::json!({"status": null}),
            )
            .await;
        assert!(response.status().is_success(), "{}", response.status());
        response
            .json::<lazy_protocol::StatusResponse>()
            .await
            .expect("status json")
            .user
    }

    pub async fn inbox_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        query: &[(&str, &str)],
    ) -> reqwest::Response {
        self.http
            .get(self.url(&paths::inbox(workspace)))
            .bearer_auth(&account.token)
            .query(query)
            .send()
            .await
            .expect("request")
    }

    /// The caller's inbox for this workspace and expects success.
    pub async fn inbox(
        &self,
        account: &Account,
        query: &[(&str, &str)],
    ) -> lazy_protocol::InboxPage {
        let response = self.inbox_raw(account, self.workspace_id, query).await;
        assert!(
            response.status().is_success(),
            "inbox: {}",
            response.status()
        );
        response.json().await.expect("inbox json")
    }

    pub async fn inbox_read_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::inbox_read(workspace),
            account,
            None,
        )
        .await
    }

    pub async fn inbox_read(&self, account: &Account) -> lazy_protocol::InboxReadResponse {
        let response = self.inbox_read_raw(account, self.workspace_id).await;
        assert!(
            response.status().is_success(),
            "inbox read: {}",
            response.status()
        );
        response.json().await.expect("inbox read json")
    }

    // --- Discovery -------------------------------------------------------------------------

    /// A `GET` of `path` with the given query-string pairs.
    pub async fn get_query(
        &self,
        account: &Account,
        path: &str,
        query: &[(&str, &str)],
    ) -> reqwest::Response {
        self.http
            .get(self.url(path))
            .bearer_auth(&account.token)
            .query(query)
            .send()
            .await
            .expect("request")
    }

    pub async fn browse_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        query: &[(&str, &str)],
    ) -> reqwest::Response {
        self.get_query(account, &paths::browse_channels(workspace), query)
            .await
    }

    /// The channel directory of this workspace and expects success.
    pub async fn browse(
        &self,
        account: &Account,
        query: &[(&str, &str)],
    ) -> lazy_protocol::BrowseChannelsResponse {
        let response = self.browse_raw(account, self.workspace_id, query).await;
        assert!(
            response.status().is_success(),
            "browse: {}",
            response.status()
        );
        response.json().await.expect("browse json")
    }

    pub async fn people_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        query: &[(&str, &str)],
    ) -> reqwest::Response {
        self.get_query(
            account,
            &format!("/v1/workspaces/{workspace}/people"),
            query,
        )
        .await
    }

    pub async fn people(
        &self,
        account: &Account,
        query: &[(&str, &str)],
    ) -> lazy_protocol::PeoplePage {
        let response = self.people_raw(account, self.workspace_id, query).await;
        assert!(
            response.status().is_success(),
            "people: {}",
            response.status()
        );
        response.json().await.expect("people json")
    }

    pub async fn person_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        user: UserId,
    ) -> reqwest::Response {
        self.get_query(
            account,
            &format!("/v1/workspaces/{workspace}/people/{user}"),
            &[],
        )
        .await
    }

    pub async fn set_saved_raw(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
        saved: bool,
    ) -> reqwest::Response {
        self.put_json(
            account,
            &paths::message_saved(message),
            serde_json::json!({"saved": saved}),
        )
        .await
    }

    /// Saves or unsaves a message and expects success.
    pub async fn set_saved(
        &self,
        account: &Account,
        message: lazy_domain::MessageId,
        saved: bool,
    ) -> lazy_protocol::SavedResponse {
        let response = self.set_saved_raw(account, message, saved).await;
        assert!(
            response.status().is_success(),
            "saved: {}",
            response.status()
        );
        response.json().await.expect("saved json")
    }

    pub async fn saved_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        query: &[(&str, &str)],
    ) -> reqwest::Response {
        self.get_query(account, &paths::saved(workspace), query)
            .await
    }

    /// The caller's saved list for this workspace and expects success.
    pub async fn saved_page(
        &self,
        account: &Account,
        query: &[(&str, &str)],
    ) -> lazy_protocol::SavedPage {
        let response = self.saved_raw(account, self.workspace_id, query).await;
        assert!(
            response.status().is_success(),
            "saved list: {}",
            response.status()
        );
        response.json().await.expect("saved json")
    }

    // --- Administration --------------------------------------------------------------------

    pub async fn patch_workspace_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PATCH,
            &paths::workspace(workspace),
            account,
            Some(body),
        )
        .await
    }

    pub async fn set_role_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        target: UserId,
        role: &str,
    ) -> reqwest::Response {
        self.put_json(
            account,
            &paths::person_role(workspace, target),
            serde_json::json!({"role": role}),
        )
        .await
    }

    pub async fn set_deactivated_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        target: UserId,
        deactivated: bool,
    ) -> reqwest::Response {
        self.put_json(
            account,
            &paths::person_deactivated(workspace, target),
            serde_json::json!({"deactivated": deactivated}),
        )
        .await
    }

    /// Deactivates (or reactivates) someone in this workspace and expects success.
    pub async fn deactivate(&self, by: &Account, who: &Account, deactivated: bool) {
        let response = self
            .set_deactivated_raw(by, self.workspace_id, who.user.id, deactivated)
            .await;
        assert!(
            response.status().is_success(),
            "deactivate: {}",
            response.status()
        );
    }

    pub async fn create_invitation_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::invitations(workspace),
            account,
            Some(body),
        )
        .await
    }

    /// Makes an invitation in this workspace and expects success.
    pub async fn create_invitation(
        &self,
        account: &Account,
        body: serde_json::Value,
    ) -> lazy_protocol::CreatedInvitation {
        let response = self
            .create_invitation_raw(account, self.workspace_id, body)
            .await;
        assert!(
            response.status().is_success(),
            "invitation: {}",
            response.status()
        );
        response.json().await.expect("invitation json")
    }

    pub async fn list_invitations_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::GET,
            &paths::invitations(workspace),
            account,
            None,
        )
        .await
    }

    pub async fn list_invitations(&self, account: &Account) -> Vec<lazy_protocol::Invitation> {
        let response = self.list_invitations_raw(account, self.workspace_id).await;
        assert!(
            response.status().is_success(),
            "invitations: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::InvitationList>()
            .await
            .expect("invitations json")
            .invitations
    }

    pub async fn revoke_invitation_raw(
        &self,
        account: &Account,
        invitation: lazy_domain::InvitationId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::DELETE,
            &paths::invitation(invitation),
            account,
            None,
        )
        .await
    }

    // --- Search ----------------------------------------------------------------------------

    /// `GET /v1/workspaces/{id}/search` with the given query-string pairs.
    pub async fn search_raw(
        &self,
        account: &Account,
        workspace: WorkspaceId,
        query: &[(&str, &str)],
    ) -> reqwest::Response {
        self.http
            .get(self.url(&paths::search(workspace)))
            .bearer_auth(&account.token)
            .query(query)
            .send()
            .await
            .expect("request")
    }

    /// Searches this workspace for `q` (default sort and paging) and expects success.
    pub async fn find(&self, account: &Account, q: &str) -> lazy_protocol::SearchResponse {
        self.find_with(account, q, &[]).await
    }

    /// Like [`App::find`] with more parameters (`limit`, `offset`, `sort`).
    pub async fn find_with(
        &self,
        account: &Account,
        q: &str,
        extra: &[(&str, &str)],
    ) -> lazy_protocol::SearchResponse {
        let mut query = vec![("q", q)];
        query.extend_from_slice(extra);
        let response = self.search_raw(account, self.workspace_id, &query).await;
        assert!(
            response.status().is_success(),
            "search {q:?}: {}",
            response.status()
        );
        response.json().await.expect("search json")
    }

    /// Inserts `count` messages straight into the database, in `conversation` by `author`, newest
    /// last, one per second ending now; `text(n)` is the text of the nth (from 0). Bumps the
    /// workspace's event head so later sends do not collide. Returns their ids, oldest first.
    pub async fn seed_messages(
        &self,
        conversation: ConversationId,
        author: &Account,
        count: usize,
        text: impl Fn(usize) -> String,
    ) -> Vec<lazy_domain::MessageId> {
        let mut tx = self.pool.begin().await.expect("tx");
        let head: i64 = sqlx::query_scalar(
            "UPDATE workspaces SET event_head = event_head + $2 WHERE id = $1 RETURNING event_head",
        )
        .bind(self.workspace_id.as_uuid())
        .bind(count as i64)
        .fetch_one(&mut *tx)
        .await
        .expect("head");
        let mut ids = Vec::new();
        for n in 0..count {
            let id = lazy_domain::MessageId::generate();
            let body = text(n);
            let document = serde_json::json!({
                "version": 2,
                "blocks": [{"type": "paragraph", "inlines": [{"type": "text", "text": body}]}],
            });
            sqlx::query(
                "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, now() - make_interval(secs => $7))",
            )
            .bind(id.as_uuid())
            .bind(conversation.as_uuid())
            .bind(author.user.id.as_uuid())
            .bind(sqlx::types::Json(&document))
            .bind(&body)
            .bind(head - count as i64 + 1 + n as i64)
            .bind((count - n) as f64)
            .execute(&mut *tx)
            .await
            .expect("seed message");
            ids.push(id);
        }
        tx.commit().await.expect("commit");
        ids
    }

    // --- Webhooks --------------------------------------------------------------------------

    pub async fn create_webhook_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        name: &str,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::webhooks(conversation),
            account,
            Some(serde_json::json!({ "name": name })),
        )
        .await
    }

    /// Creates a webhook and expects success.
    pub async fn create_webhook(
        &self,
        account: &Account,
        conversation: ConversationId,
        name: &str,
    ) -> lazy_protocol::CreatedWebhook {
        let response = self.create_webhook_raw(account, conversation, name).await;
        assert!(
            response.status().is_success(),
            "create webhook: {}",
            response.status()
        );
        response.json().await.expect("created webhook json")
    }

    pub async fn list_webhooks_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::GET,
            &paths::webhooks(conversation),
            account,
            None,
        )
        .await
    }

    pub async fn list_webhooks(
        &self,
        account: &Account,
        conversation: ConversationId,
    ) -> Vec<lazy_protocol::Webhook> {
        let response = self.list_webhooks_raw(account, conversation).await;
        assert!(
            response.status().is_success(),
            "list webhooks: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::WebhooksResponse>()
            .await
            .expect("webhooks json")
            .webhooks
    }

    pub async fn delete_webhook_raw(
        &self,
        account: &Account,
        webhook: lazy_domain::WebhookId,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::DELETE,
            &paths::webhook(webhook),
            account,
            None,
        )
        .await
    }

    /// `POST`s `{"text": text}` to a webhook address, with extra headers; the response is left for
    /// the caller.
    pub async fn deliver_raw(
        &self,
        url: &str,
        text: &str,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        self.deliver_body_raw(
            url,
            serde_json::json!({ "text": text }).to_string(),
            headers,
        )
        .await
    }

    pub async fn deliver_body_raw(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut request = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.expect("request")
    }

    /// Delivers `text` and expects success.
    pub async fn deliver(
        &self,
        hook: &lazy_protocol::CreatedWebhook,
        text: &str,
    ) -> lazy_domain::MessageId {
        let response = self.deliver_raw(&hook.url, text, &[]).await;
        assert!(
            response.status().is_success(),
            "deliver: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::IncomingWebhookResponse>()
            .await
            .expect("delivery json")
            .message_id
    }

    // --- Threads ---------------------------------------------------------------------------

    /// Sends a reply to `root`'s thread with the given operation id; the response is left for the
    /// caller.
    pub async fn reply_raw(
        &self,
        account: &Account,
        root: &Message,
        operation: OperationId,
        document: MessageDocument,
        also_in_channel: bool,
    ) -> reqwest::Response {
        self.reply_to_raw(
            account,
            root.conversation_id,
            root.id,
            operation,
            document,
            also_in_channel,
        )
        .await
    }

    /// Like [`App::reply_raw`] for a root named by conversation and id (which may be wrong).
    pub async fn reply_to_raw(
        &self,
        account: &Account,
        conversation: ConversationId,
        root: lazy_domain::MessageId,
        operation: OperationId,
        document: MessageDocument,
        also_in_channel: bool,
    ) -> reqwest::Response {
        let body = SendMessageRequest {
            operation_id: operation,
            document,
            thread_root: Some(root),
            also_in_channel,
            attachment_ids: Vec::new(),
        };
        self.call(
            reqwest::Method::POST,
            &paths::messages(conversation),
            account,
            Some(serde_json::to_value(&body).expect("body")),
        )
        .await
    }

    /// Replies in a thread with a fresh operation id and expects success.
    pub async fn reply(&self, account: &Account, root: &Message, text: &str) -> Message {
        self.reply_with(account, root, text, false).await
    }

    /// Replies in a thread and also sends the reply to the channel.
    pub async fn reply_in_channel(&self, account: &Account, root: &Message, text: &str) -> Message {
        self.reply_with(account, root, text, true).await
    }

    async fn reply_with(
        &self,
        account: &Account,
        root: &Message,
        text: &str,
        also_in_channel: bool,
    ) -> Message {
        let document = MessageDocument::from_plain_text(text).expect("document");
        let response = self
            .reply_raw(
                account,
                root,
                OperationId::generate(),
                document,
                also_in_channel,
            )
            .await;
        assert!(
            response.status().is_success(),
            "reply: {}",
            response.status()
        );
        response
            .json::<SendMessageResponse>()
            .await
            .expect("json")
            .message
    }

    /// Replies with mentions and expects success.
    pub async fn reply_mentioning(
        &self,
        account: &Account,
        root: &Message,
        text: &str,
        entries: &[(&str, MentionTarget)],
        also_in_channel: bool,
    ) -> Message {
        let response = self
            .reply_raw(
                account,
                root,
                OperationId::generate(),
                Self::mention_document(text, entries),
                also_in_channel,
            )
            .await;
        assert!(
            response.status().is_success(),
            "reply: {}",
            response.status()
        );
        response
            .json::<SendMessageResponse>()
            .await
            .expect("json")
            .message
    }

    /// `GET /v1/messages/{root}/replies`; the response is left for the caller.
    pub async fn replies_raw(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
        query: &[(&str, String)],
    ) -> reqwest::Response {
        let mut path = paths::replies(root);
        for (index, (key, value)) in query.iter().enumerate() {
            path.push(if index == 0 { '?' } else { '&' });
            path.push_str(&format!("{key}={value}"));
        }
        self.call(reqwest::Method::GET, &path, account, None).await
    }

    pub async fn replies_ok(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
        query: &[(&str, String)],
    ) -> HistoryPage {
        let response = self.replies_raw(account, root, query).await;
        assert!(
            response.status().is_success(),
            "replies: {}",
            response.status()
        );
        response.json().await.expect("page")
    }

    /// `PUT .../thread/follow`; the response is left for the caller.
    pub async fn follow_raw(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
        following: bool,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::PUT,
            &paths::thread_action(root, "follow"),
            account,
            Some(serde_json::json!({"following": following})),
        )
        .await
    }

    pub async fn follow(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
        following: bool,
    ) -> lazy_protocol::ThreadUserState {
        let response = self.follow_raw(account, root, following).await;
        assert!(
            response.status().is_success(),
            "follow: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ThreadStateResponse>()
            .await
            .expect("json")
            .state
    }

    /// `POST .../thread/read`; the response is left for the caller.
    pub async fn read_thread_raw(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
        up_to: EventSeq,
    ) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            &paths::thread_action(root, "read"),
            account,
            Some(serde_json::json!({"up_to": up_to})),
        )
        .await
    }

    pub async fn read_thread(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
        up_to: EventSeq,
    ) -> lazy_protocol::ThreadUserState {
        let response = self.read_thread_raw(account, root, up_to).await;
        assert!(
            response.status().is_success(),
            "read: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ThreadStateResponse>()
            .await
            .expect("json")
            .state
    }

    /// The thread states in the caller's bootstrap.
    pub async fn threads_of(&self, account: &Account) -> Vec<lazy_protocol::ThreadUserState> {
        self.bootstrap_ok(account).await.workspaces[0]
            .threads
            .clone()
    }

    /// The caller's bootstrap state for one thread, if it is listed.
    pub async fn thread_state(
        &self,
        account: &Account,
        root: lazy_domain::MessageId,
    ) -> Option<lazy_protocol::ThreadUserState> {
        self.threads_of(account)
            .await
            .into_iter()
            .find(|state| state.root_id == root)
    }

    // --- Several workspaces, profiles and avatars ---------------------------------------------

    /// `PUT /v1/workspaces/{id}` with a name (not validated by the helper).
    pub async fn create_workspace_raw(
        &self,
        account: &Account,
        id: WorkspaceId,
        name: &str,
    ) -> reqwest::Response {
        self.put_json(
            account,
            &paths::workspace(id),
            serde_json::json!({ "name": name }),
        )
        .await
    }

    /// Creates a workspace under a fresh id and expects success.
    pub async fn create_workspace(
        &self,
        account: &Account,
        name: &str,
    ) -> lazy_protocol::WorkspaceBootstrap {
        let id = WorkspaceId::generate();
        let response = self.create_workspace_raw(account, id, name).await;
        assert!(
            response.status().is_success(),
            "create workspace: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::JoinedWorkspace>()
            .await
            .expect("joined workspace json")
            .workspace
    }

    /// `POST /v1/invitations/accept` with any code text.
    pub async fn accept_raw(&self, account: &Account, code: &str) -> reqwest::Response {
        self.call(
            reqwest::Method::POST,
            paths::ACCEPT_INVITATION_ROUTE,
            account,
            Some(serde_json::json!({ "code": code })),
        )
        .await
    }

    /// An invitation to `workspace` (by an account that may make one) and expects success.
    pub async fn invitation_to(
        &self,
        by: &Account,
        workspace: WorkspaceId,
        body: serde_json::Value,
    ) -> lazy_protocol::CreatedInvitation {
        let response = self.create_invitation_raw(by, workspace, body).await;
        assert!(
            response.status().is_success(),
            "invitation: {}",
            response.status()
        );
        response.json().await.expect("invitation json")
    }

    /// Accepts an invitation and expects success.
    pub async fn accept(&self, account: &Account, code: &str) -> lazy_protocol::WorkspaceBootstrap {
        let response = self.accept_raw(account, code).await;
        assert!(
            response.status().is_success(),
            "accept: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::JoinedWorkspace>()
            .await
            .expect("joined workspace json")
            .workspace
    }

    /// `PATCH /v1/me`.
    pub async fn patch_me_raw(
        &self,
        account: &Account,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.call(reqwest::Method::PATCH, paths::ME_ROUTE, account, Some(body))
            .await
    }

    /// Renames the caller and expects success.
    pub async fn rename_me(&self, account: &Account, name: &str) -> User {
        let response = self
            .patch_me_raw(account, serde_json::json!({ "display_name": name }))
            .await;
        assert!(
            response.status().is_success(),
            "rename: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ProfileResponse>()
            .await
            .expect("profile json")
            .user
    }

    /// `PUT /v1/me/avatar` with a body and a `Content-Type` hint.
    pub async fn set_avatar_raw(
        &self,
        account: &Account,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> reqwest::Response {
        self.http
            .put(self.url(paths::ME_AVATAR_ROUTE))
            .bearer_auth(&account.token)
            .header("content-type", content_type)
            .body(bytes)
            .send()
            .await
            .expect("request")
    }

    /// Sets the caller's avatar from an image and expects success.
    pub async fn set_avatar(&self, account: &Account, bytes: Vec<u8>) -> User {
        let response = self
            .set_avatar_raw(account, bytes, "application/octet-stream")
            .await;
        assert!(
            response.status().is_success(),
            "set avatar: {}",
            response.status()
        );
        response
            .json::<lazy_protocol::ProfileResponse>()
            .await
            .expect("profile json")
            .user
    }

    /// `DELETE /v1/me/avatar`.
    pub async fn delete_avatar_raw(&self, account: &Account) -> reqwest::Response {
        self.call(
            reqwest::Method::DELETE,
            paths::ME_AVATAR_ROUTE,
            account,
            None,
        )
        .await
    }

    /// `GET /v1/users/{id}/avatar`, with `?v=` if a version is given.
    pub async fn avatar_raw(
        &self,
        viewer: &Account,
        person: UserId,
        version: Option<u32>,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let path = match version {
            Some(version) => paths::user_avatar(person, version),
            None => format!("/v1/users/{person}/avatar"),
        };
        let mut request = self.http.get(self.url(&path)).bearer_auth(&viewer.token);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.expect("request")
    }

    /// Makes an account a member of a workspace directly in the database.
    pub async fn put_in(&self, account: &Account, workspace: WorkspaceId, role: &str) {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)",
        )
        .bind(workspace.as_uuid())
        .bind(account.user.id.as_uuid())
        .bind(role)
        .execute(&self.pool)
        .await
        .expect("member row");
    }

    /// The `user_profile_changed` events in a workspace's log after `after`, in order, as
    /// (position, user, name, avatar version).
    pub async fn profile_events(
        &self,
        workspace: WorkspaceId,
        after: EventSeq,
    ) -> Vec<(i64, UserId, String, u32)> {
        let rows: Vec<(i64, serde_json::Value)> = sqlx::query_as(
            "SELECT seq, payload FROM events WHERE workspace_id = $1 AND seq > $2 \
               AND kind = 'user_profile_changed' ORDER BY seq",
        )
        .bind(workspace.as_uuid())
        .bind(after.get())
        .fetch_all(&self.pool)
        .await
        .expect("profile events");
        rows.into_iter()
            .map(|(seq, payload)| {
                let event: lazy_protocol::Event =
                    serde_json::from_value(payload).expect("event json");
                let lazy_protocol::Event::UserProfileChanged {
                    user_id,
                    display_name,
                    avatar_version,
                } = event
                else {
                    panic!("not a profile event");
                };
                (seq, user_id, display_name.to_string(), avatar_version)
            })
            .collect()
    }

    /// The newest position in any workspace's log.
    pub async fn head_of(&self, workspace: WorkspaceId) -> EventSeq {
        let head: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces WHERE id = $1")
            .bind(workspace.as_uuid())
            .fetch_one(&self.pool)
            .await
            .expect("head");
        EventSeq::new(head).expect("head is not negative")
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

    /// Reads events until the arrival of `marker` in the workspace (a person registered after the
    /// actions under test), returning everything that came before it. Because events are delivered
    /// in order, anything deliverable that was committed before the marker is in the result, so
    /// this proves absence without sleeping.
    pub async fn events_until_marker(
        &mut self,
        marker: UserId,
    ) -> Vec<lazy_protocol::EventEnvelope> {
        let mut events = Vec::new();
        loop {
            match self.next().await {
                ServerFrame::Event(envelope) => {
                    if matches!(&envelope.event, lazy_protocol::Event::MemberJoined { user } if user.id == marker)
                    {
                        return events;
                    }
                    events.push(envelope);
                }
                ServerFrame::CaughtUp { .. } => {}
                other => panic!("unexpected frame while waiting for the marker: {other:?}"),
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

/// A file name as it travels in the `x-attachment-name` header: percent-encoded UTF-8.
pub fn percent_encode(name: &str) -> String {
    name.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A PNG of one colour, `width` by `height` pixels, with the given alpha.
pub fn png_bytes(width: u32, height: u32, alpha: u8) -> Vec<u8> {
    use image::ImageEncoder;
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba([40, 120, 200, alpha]));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .expect("encode a png");
    out
}

/// A JPEG of `width` by `height` pixels whose left half is red and right half blue, claiming in its
/// EXIF block that it should be shown with `orientation` (1 to 8) applied, as a phone's photo does.
pub fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, _| {
        if x < width / 2 {
            image::Rgb([220, 20, 20])
        } else {
            image::Rgb([20, 20, 220])
        }
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
        .encode_image(&image)
        .expect("encode a jpeg");
    // APP1 with a big-endian TIFF block holding one entry: tag 0x0112 (orientation), a short.
    let mut tiff = vec![
        b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1,
    ];
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let mut segment = vec![0xFF, 0xE1];
    segment.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    segment.extend_from_slice(b"Exif\0\0");
    segment.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&segment);
    out.extend_from_slice(&jpeg[2..]);
    out
}
