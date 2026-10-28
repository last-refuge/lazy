//! One workspace's realtime subscription: connect, subscribe after the cursor, forward frames to
//! the core, and reconnect with bounded backoff.
//!
//! Ownership: [`crate::Runtime`] owns the task's `JoinSet` slot, its cancellation token, and the
//! command sender. The task owns the socket and the cursor. When the task ends, by cancellation,
//! an expired session, or the receiver of inputs going away, nothing else needs to clean up.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use lazy_client_core::{Input, SocketEvent};
use lazy_domain::{EventSeq, WorkspaceId};
use lazy_protocol::{ClientFrame, ServerFrame};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;

use crate::api::Api;
use crate::config::BackoffPolicy;

/// How long connecting may take before it counts as a failed attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct Context {
    pub(crate) api: Arc<Api>,
    pub(crate) inputs: async_channel::Sender<Input>,
    pub(crate) backoff: BackoffPolicy,
    pub(crate) idle_timeout: Duration,
}

pub(crate) enum Command {
    /// Resume after this position, on the current connection if there is one.
    Resubscribe(EventSeq),
}

enum Ended {
    /// The connection closed or failed; reconnect.
    Closed(String),
    /// The server rejected the session.
    Unauthorized,
    /// Shutdown, or nobody is listening any more.
    Stop,
}

/// What the task knows about its subscription, kept across reconnects.
struct Progress {
    /// The last position forwarded to the core; the next connection resumes after it.
    cursor: EventSeq,
    /// The server said the cursor can no longer be resumed; wait for a fresh position.
    stale: bool,
    /// The current connection finished replaying (reset for each connection).
    caught_up: bool,
}

pub(crate) async fn run(
    ctx: Context,
    workspace: WorkspaceId,
    cursor: EventSeq,
    mut commands: mpsc::UnboundedReceiver<Command>,
    cancel: CancellationToken,
) {
    let mut attempt: u32 = 0;
    let mut progress = Progress {
        cursor,
        stale: false,
        caught_up: false,
    };
    loop {
        if !emit(
            &ctx,
            Input::Socket(SocketEvent::Connecting { workspace, attempt }),
        )
        .await
        {
            return;
        }
        let ended = match connect(&ctx.api).await {
            Ok(socket) => {
                if !emit(&ctx, Input::Socket(SocketEvent::Opened { workspace })).await {
                    return;
                }
                progress.caught_up = false;
                let ended = serve(
                    &ctx,
                    workspace,
                    socket,
                    &mut progress,
                    &mut commands,
                    &cancel,
                )
                .await;
                if progress.caught_up {
                    attempt = 0;
                }
                ended
            }
            Err(Ended::Closed(reason)) => Ended::Closed(reason),
            Err(other) => other,
        };
        match ended {
            Ended::Stop => return,
            Ended::Unauthorized => {
                emit(&ctx, Input::SessionExpired).await;
                return;
            }
            Ended::Closed(reason) => {
                tracing::debug!(%workspace, attempt, reason, "realtime connection ended");
                attempt = attempt.saturating_add(1);
                if !emit(
                    &ctx,
                    Input::Socket(SocketEvent::Closed { workspace, attempt }),
                )
                .await
                {
                    return;
                }
                // The server refused the cursor. Reconnecting with it would only be refused
                // again, so wait for the core to subscribe from a fresh position.
                if progress.stale {
                    while progress.stale {
                        tokio::select! {
                            () = cancel.cancelled() => return,
                            command = commands.recv() => match command {
                                Some(Command::Resubscribe(after)) => {
                                    progress.cursor = after;
                                    progress.stale = false;
                                }
                                None => return,
                            },
                        }
                    }
                    continue;
                }
                let delay = ctx.backoff.delay(attempt, entropy());
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                    command = commands.recv() => match command {
                        Some(Command::Resubscribe(after)) => progress.cursor = after,
                        None => return,
                    },
                }
            }
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(api: &Api) -> Result<Socket, Ended> {
    let Some(token) = api.token() else {
        return Err(Ended::Unauthorized);
    };
    let mut request = api
        .realtime_url()
        .into_client_request()
        .map_err(|e| Ended::Closed(format!("bad realtime URL: {e}")))?;
    let header = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| Ended::Closed("session token is not a valid header".into()))?;
    request.headers_mut().insert("authorization", header);
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Err(_) => Err(Ended::Closed("connect timed out".into())),
        Ok(Ok((socket, _response))) => Ok(socket),
        Ok(Err(WsError::Http(response))) if response.status().as_u16() == 401 => {
            Err(Ended::Unauthorized)
        }
        Ok(Err(error)) => Err(Ended::Closed(error.to_string())),
    }
}

async fn serve(
    ctx: &Context,
    workspace: WorkspaceId,
    socket: Socket,
    progress: &mut Progress,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    cancel: &CancellationToken,
) -> Ended {
    let (mut sink, mut stream) = socket.split();
    if let Err(error) = send_subscribe(&mut sink, workspace, progress.cursor).await {
        return Ended::Closed(error);
    }
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                return Ended::Stop;
            }
            command = commands.recv() => match command {
                Some(Command::Resubscribe(after)) => {
                    progress.cursor = after;
                    progress.stale = false;
                    if let Err(error) = send_subscribe(&mut sink, workspace, after).await {
                        return Ended::Closed(error);
                    }
                }
                None => return Ended::Stop,
            },
            next = tokio::time::timeout(ctx.idle_timeout, stream.next()) => match next {
                Err(_) => return Ended::Closed("no traffic from the server".into()),
                Ok(None) => return Ended::Closed("connection closed".into()),
                Ok(Some(Err(error))) => return Ended::Closed(error.to_string()),
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame = match serde_json::from_str::<ServerFrame>(text.as_str()) {
                        Ok(frame) => frame,
                        Err(error) => {
                            tracing::warn!(%error, "ignoring a realtime frame that could not be parsed");
                            continue;
                        }
                    };
                    match &frame {
                        ServerFrame::Event(envelope) if envelope.workspace_id == workspace => {
                            progress.cursor = progress.cursor.max(envelope.seq);
                        }
                        ServerFrame::CaughtUp { workspace_id, seq } if *workspace_id == workspace => {
                            progress.cursor = progress.cursor.max(*seq);
                            progress.caught_up = true;
                        }
                        ServerFrame::SnapshotRequired { workspace_id, .. }
                            if *workspace_id == workspace =>
                        {
                            progress.stale = true;
                        }
                        _ => {}
                    }
                    if !emit(ctx, Input::Frame(frame)).await {
                        return Ended::Stop;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) => return Ended::Closed("closed by the server".into()),
                Ok(Some(Ok(_))) => {}
            },
        }
    }
}

async fn send_subscribe<S>(
    sink: &mut S,
    workspace: WorkspaceId,
    after: EventSeq,
) -> Result<(), String>
where
    S: SinkExt<Message, Error = WsError> + Unpin,
{
    let frame = ClientFrame::Subscribe {
        workspace_id: workspace,
        after,
    };
    let text = serde_json::to_string(&frame).map_err(|e| e.to_string())?;
    sink.send(Message::text(text))
        .await
        .map_err(|e| e.to_string())
}

/// Delivers an input to the host. `false` means nobody is listening and the task should end.
async fn emit(ctx: &Context, input: Input) -> bool {
    ctx.inputs.send(input).await.is_ok()
}

fn entropy() -> u32 {
    let mut bytes = [0u8; 4];
    match getrandom::fill(&mut bytes) {
        Ok(()) => u32::from_le_bytes(bytes),
        Err(_) => u32::MAX / 2,
    }
}
