//! The signaling WebSocket: a client's first message is [`ClientMessage::Join`] with a token; after
//! that, messages pass between the socket and the media thread until either side ends.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use rtc_proto::{ClientMessage, ErrorCode, ParticipantId, SIGNAL_VERSION, ServerMessage, token};
use tokio::sync::mpsc;

use crate::Sfu;
use crate::room::Command;

/// How long a new socket has to send its join.
const JOIN_WITHIN: Duration = Duration::from_secs(10);

pub(crate) async fn upgrade(State(sfu): State<Sfu>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| serve(sfu, socket))
}

async fn refuse(socket: &mut WebSocket, code: ErrorCode, message: &str) {
    let text = ServerMessage::Error {
        code,
        message: message.into(),
    }
    .to_json();
    socket.send(Message::Text(text.into())).await.ok();
    socket.send(Message::Close(None)).await.ok();
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn serve(sfu: Sfu, mut socket: WebSocket) {
    let first = match tokio::time::timeout(JOIN_WITHIN, socket.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<ClientMessage>(&text).ok(),
        _ => None,
    };
    let Some(ClientMessage::Join {
        version,
        token,
        offer,
        audio_muted,
        video_on,
    }) = first
    else {
        refuse(
            &mut socket,
            ErrorCode::BadRequest,
            "the first message must be a join",
        )
        .await;
        return;
    };
    if version != SIGNAL_VERSION {
        refuse(
            &mut socket,
            ErrorCode::Version,
            "this SFU speaks another version",
        )
        .await;
        return;
    }
    let claims = match token::verify(&token, &sfu.inner.secret, now()) {
        Ok(claims) => claims,
        Err(e) => {
            tracing::info!("join refused: {e}");
            refuse(&mut socket, ErrorCode::Unauthorized, &e.to_string()).await;
            return;
        }
    };
    let id = ParticipantId(sfu.inner.next_participant.fetch_add(1, Ordering::Relaxed));
    let (out, mut outgoing) = mpsc::unbounded_channel();
    let commands = sfu.inner.commands.clone();
    if commands
        .send(Command::Join {
            id,
            claims,
            offer,
            audio_muted,
            video_on,
            out,
        })
        .is_err()
    {
        refuse(&mut socket, ErrorCode::Internal, "the SFU is stopping").await;
        return;
    }

    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            message = outgoing.recv() => {
                let Some(message) = message else { break };
                let closing = matches!(message, ServerMessage::Error { .. });
                if sink.send(Message::Text(message.to_json().into())).await.is_err() {
                    break;
                }
                if closing {
                    sink.send(Message::Close(None)).await.ok();
                    break;
                }
            }
            incoming = stream.next() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => continue,
                    _ => break,
                };
                let command = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Answer { sdp }) => Command::Answer { id, sdp },
                    Ok(ClientMessage::State { audio_muted, video_on }) => Command::State {
                        id,
                        audio_muted,
                        video_on,
                    },
                    Ok(ClientMessage::Leave) => break,
                    Ok(ClientMessage::Join { .. }) | Err(_) => {
                        tracing::debug!(%id, "ignored a message: {text}");
                        continue;
                    }
                };
                if commands.send(command).is_err() {
                    break;
                }
            }
        }
    }
    commands.send(Command::Leave { id }).ok();
}
