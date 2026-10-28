//! The realtime WebSocket: per-connection subscriptions with replay, then live delivery.
//!
//! # Subscription algorithm
//!
//! For each `Subscribe { workspace_id, after }` a task:
//!
//! 1. checks the caller is a workspace member;
//! 2. subscribes to the workspace's broadcast **first**;
//! 3. rejects a cursor beyond the log head with `snapshot_required` (`cursor_ahead`);
//! 4. replays committed events after the cursor from the database, in pages, answering
//!    `snapshot_required` (`cursor_expired`) if retention has pruned events after the cursor,
//!    including between two pages of the same replay;
//! 5. sends `caught_up`;
//! 6. drains the broadcast, skipping anything at or before its cursor.
//!
//! Everything the dispatcher published before step 2 was committed before step 4's reads, so the
//! replay sees it; everything published after arrives on the broadcast. If the broadcast reports
//! a lag, or a batch does not connect to the cursor, the task replays from the database again.
//!
//! # Back-pressure
//!
//! Each connection has a bounded outbound queue. A full queue suspends the subscription task, which
//! stops draining its broadcast receiver; once it falls [`crate::dispatcher`]'s capacity behind it
//! receives a lag notice and recovers by replaying from the database. A socket write that does not
//! complete within [`WRITE_TIMEOUT`] closes the connection.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use lazy_domain::{EventSeq, UserId, WorkspaceId};
use lazy_protocol::{ClientFrame, ErrorCode, ServerFrame, SnapshotReason};
use sqlx::PgPool;
use tokio::sync::{broadcast, mpsc};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::accounts::{self, Principal};
use crate::dispatcher::{BATCH_SIZE, Dispatcher};
use crate::error::AppError;
use crate::events::{self, StoredEvent, Visibility};

/// Frames queued for the socket before senders wait.
const OUTBOUND_QUEUE: usize = 256;
/// Longest a single socket write may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the server pings, and rechecks that the session is still valid.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A connection that sends nothing (not even pongs) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// WebSocket close code 1001 "going away".
const CLOSE_GOING_AWAY: u16 = 1001;
/// WebSocket close code 1008 "policy violation".
const CLOSE_POLICY: u16 = 1008;

/// Why a subscription task stopped.
enum Exit {
    /// The connection's output is gone.
    OutputClosed,
    /// The task finished its work (for example after reporting an error frame).
    Done,
    Failed(AppError),
}

impl From<AppError> for Exit {
    fn from(error: AppError) -> Self {
        Exit::Failed(error)
    }
}

/// Runs one WebSocket connection until it closes or the server shuts down.
pub async fn serve_connection(
    pool: PgPool,
    dispatcher: Dispatcher,
    principal: Principal,
    socket: WebSocket,
    shutdown: CancellationToken,
) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut queue) = mpsc::channel::<Message>(OUTBOUND_QUEUE);

    let writer = tokio::spawn(async move {
        while let Some(message) = queue.recv().await {
            let closing = matches!(message, Message::Close(_));
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(message)).await {
                Ok(Ok(())) if closing => break,
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::warn!("closing connection: client too slow to read");
                    break;
                }
            }
        }
        // Dropping the sink ends the connection if no close frame was sent.
    });

    let mut subscriptions: JoinSet<()> = JoinSet::new();
    let mut handles: HashMap<WorkspaceId, AbortHandle> = HashMap::new();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick completes immediately
    let mut last_inbound = Instant::now();
    let mut close: Option<(u16, &'static str)> = None;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                close = Some((CLOSE_GOING_AWAY, "server shutting down"));
                break;
            }
            () = out.closed() => break,
            _ = ping.tick() => {
                if last_inbound.elapsed() > IDLE_TIMEOUT {
                    close = Some((CLOSE_POLICY, "idle timeout"));
                    break;
                }
                match accounts::session_is_valid(&pool, principal.session_id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = send_frame(&out, &ServerFrame::Error {
                            code: ErrorCode::Unauthenticated,
                            message: "session ended".into(),
                        }).await;
                        close = Some((CLOSE_POLICY, "session ended"));
                        break;
                    }
                    Err(error) => tracing::warn!(%error, "cannot recheck session"),
                }
                if out.try_send(Message::Ping(Vec::new().into())).is_err() {
                    // Queue full or closed: the client is not keeping up.
                    break;
                }
            }
            Some(joined) = subscriptions.join_next(), if !subscriptions.is_empty() => {
                if let Err(error) = joined
                    && error.is_panic()
                {
                    tracing::error!(%error, "subscription task panicked");
                }
            }
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                last_inbound = Instant::now();
                match message {
                    Message::Text(text) => {
                        handle_client_text(
                            text.as_str(), &pool, &dispatcher, principal.user_id, &out,
                            &mut subscriptions, &mut handles,
                        ).await;
                    }
                    Message::Binary(_) => {
                        let _ = send_frame(&out, &ServerFrame::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "frames must be JSON text".into(),
                        }).await;
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
        }
    }

    // Stop subscription tasks, then say goodbye and let the writer drain.
    subscriptions.shutdown().await;
    if let Some((code, reason)) = close {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        let _ = out.send(Message::Close(Some(frame))).await;
    }
    drop(out);
    if let Err(error) = writer.await {
        tracing::warn!(%error, "websocket writer task failed");
    }
}

async fn handle_client_text(
    text: &str,
    pool: &PgPool,
    dispatcher: &Dispatcher,
    user: UserId,
    out: &mpsc::Sender<Message>,
    subscriptions: &mut JoinSet<()>,
    handles: &mut HashMap<WorkspaceId, AbortHandle>,
) {
    match serde_json::from_str::<ClientFrame>(text) {
        Ok(ClientFrame::Subscribe {
            workspace_id,
            after,
        }) => {
            if let Some(previous) = handles.remove(&workspace_id) {
                previous.abort();
            }
            let task = Subscription {
                pool: pool.clone(),
                dispatcher: dispatcher.clone(),
                user,
                workspace_id,
                out: out.clone(),
                cursor: after,
            };
            handles.insert(workspace_id, subscriptions.spawn(task.run_to_end()));
        }
        Err(error) => {
            // Report what was wrong with the frame's shape, never its contents.
            tracing::debug!(%error, "malformed client frame");
            let _ = send_frame(
                out,
                &ServerFrame::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "could not understand that frame".into(),
                },
            )
            .await;
        }
    }
}

async fn send_frame(out: &mpsc::Sender<Message>, frame: &ServerFrame) -> Result<(), Exit> {
    let json = serde_json::to_string(frame)
        .map_err(|e| Exit::Failed(AppError::internal(format!("cannot serialize frame: {e}"))))?;
    out.send(Message::Text(json.into()))
        .await
        .map_err(|_| Exit::OutputClosed)
}

/// One workspace subscription on one connection.
struct Subscription {
    pool: PgPool,
    dispatcher: Dispatcher,
    user: UserId,
    workspace_id: WorkspaceId,
    out: mpsc::Sender<Message>,
    /// Every event at or before this position has been delivered or deliberately withheld.
    cursor: EventSeq,
}

impl Subscription {
    async fn run_to_end(mut self) {
        match self.run().await {
            Ok(()) | Err(Exit::Done | Exit::OutputClosed) => {}
            Err(Exit::Failed(error)) => {
                tracing::warn!(workspace_id = %self.workspace_id, %error, "subscription failed");
                let api = error.to_api_error();
                let _ = send_frame(
                    &self.out,
                    &ServerFrame::Error {
                        code: api.code,
                        message: api.message,
                    },
                )
                .await;
            }
        }
    }

    async fn run(&mut self) -> Result<(), Exit> {
        if accounts::workspace_role(&self.pool, self.workspace_id, self.user)
            .await?
            .is_none()
        {
            return self
                .reject(
                    ErrorCode::Forbidden,
                    "you are not a member of that workspace",
                )
                .await;
        }
        // Subscribe before reading anything from the database (see module docs).
        let mut live = self.dispatcher.subscribe(self.workspace_id).await?;

        let head = events::head(&self.pool, self.workspace_id)
            .await?
            .ok_or_else(|| AppError::not_found("workspace not found"))?;
        if self.cursor > head {
            send_frame(
                &self.out,
                &ServerFrame::SnapshotRequired {
                    workspace_id: self.workspace_id,
                    reason: SnapshotReason::CursorAhead,
                },
            )
            .await?;
            return Ok(());
        }

        self.replay().await?;
        send_frame(
            &self.out,
            &ServerFrame::CaughtUp {
                workspace_id: self.workspace_id,
                seq: self.cursor,
            },
        )
        .await?;

        loop {
            match live.recv().await {
                Ok(batch) => {
                    if batch.last_seq <= self.cursor {
                        continue;
                    }
                    if batch.after > self.cursor {
                        // Not contiguous with what we have delivered: recover from the log.
                        self.replay().await?;
                        if batch.last_seq <= self.cursor {
                            continue;
                        }
                    }
                    let fresh: Vec<&StoredEvent> = batch
                        .events
                        .iter()
                        .filter(|event| event.seq > self.cursor)
                        .collect();
                    self.deliver(&fresh).await?;
                    self.cursor = batch.last_seq;
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "subscriber lagged; replaying from the log");
                    self.replay().await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }

    /// Delivers every committed event after the cursor, reading the log in pages.
    async fn replay(&mut self) -> Result<(), Exit> {
        loop {
            let page =
                events::fetch_after(&self.pool, self.workspace_id, self.cursor, BATCH_SIZE).await?;
            if page.floor > self.cursor {
                // Events after the cursor have been pruned, possibly since the previous page of
                // this very replay. Delivering what remains would silently skip the rest.
                send_frame(
                    &self.out,
                    &ServerFrame::SnapshotRequired {
                        workspace_id: self.workspace_id,
                        reason: SnapshotReason::CursorExpired,
                    },
                )
                .await?;
                return Err(Exit::Done);
            }
            let refs: Vec<&StoredEvent> = page.events.iter().collect();
            self.deliver(&refs).await?;
            self.cursor = page.scanned_to;
            if page.scanned < BATCH_SIZE {
                return Ok(());
            }
        }
    }

    /// Sends the events the viewer may currently see, in order.
    async fn deliver(&self, events: &[&StoredEvent]) -> Result<(), Exit> {
        if events.is_empty() {
            return Ok(());
        }
        match events::visibility(&self.pool, self.user, self.workspace_id, events).await? {
            Visibility::NotAMember => {
                self.reject(
                    ErrorCode::Forbidden,
                    "you are no longer a member of that workspace",
                )
                .await
            }
            Visibility::Flags(flags) => {
                for (event, visible) in events.iter().zip(flags) {
                    if visible {
                        send_frame(&self.out, &ServerFrame::Event(event.envelope())).await?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Reports a refusal and ends the subscription.
    async fn reject(&self, code: ErrorCode, message: &str) -> Result<(), Exit> {
        send_frame(
            &self.out,
            &ServerFrame::Error {
                code,
                message: message.into(),
            },
        )
        .await?;
        Err(Exit::Done)
    }
}
