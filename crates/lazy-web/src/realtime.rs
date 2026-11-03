//! One workspace's realtime subscription in the browser: connect the WebSocket, subscribe after the
//! cursor, hand frames to the core, and reconnect with bounded backoff. The same contract as the
//! desktop runtime's (`lazy-client-runtime/src/realtime.rs`).
//!
//! The browser authenticates the socket with the session cookie and does not show a refused
//! handshake's status, so when a connection closes without ever opening, a cheap authenticated
//! request tells an expired session (the server says `unauthenticated`) from a network problem.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use futures_channel::mpsc;
use futures_util::StreamExt;
use futures_util::future::{Either, select};
use lazy_client_core::{Input, SocketEvent};
use lazy_domain::{EventSeq, OperationId, WorkspaceId};
use lazy_protocol::api::paths;
use lazy_protocol::{ClientFrame, ServerFrame};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

/// Longest wait between reconnection attempts.
const BACKOFF_CAP: Duration = Duration::from_secs(15);
/// First wait.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// How long the time told to the core before a frame serves the frames after it.
const CLOCK_EVERY_MS: f64 = 1_000.0;

/// The wait before attempt `attempt` (1-based): doubling from [`BACKOFF_BASE`] to
/// [`BACKOFF_CAP`], then a random half to all of it, so many clients do not return together.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(20);
    let nominal = BACKOFF_BASE
        .saturating_mul(1u32 << exponent)
        .min(BACKOFF_CAP);
    let half = nominal / 2;
    half + half.mul_f64(js_sys::Math::random())
}

enum Event {
    Open,
    Text(String),
    Closed,
    /// The core asked to resume after this position.
    Resubscribe(EventSeq),
    Stop,
}

/// A running subscription, as the runtime holds it.
pub(crate) struct Subscription {
    events: mpsc::UnboundedSender<Event>,
    stopped: Rc<Cell<bool>>,
}

impl Subscription {
    pub(crate) fn resubscribe(&self, after: EventSeq) -> bool {
        self.events
            .unbounded_send(Event::Resubscribe(after))
            .is_ok()
    }

    pub(crate) fn stop(&self) {
        self.stopped.set(true);
        let _ = self.events.unbounded_send(Event::Stop);
    }
}

/// Starts subscribing to `workspace` after `after`. Inputs go to `emit`.
pub(crate) fn start(
    workspace: WorkspaceId,
    after: EventSeq,
    emit: Rc<dyn Fn(Input)>,
) -> Subscription {
    let (events, receiver) = mpsc::unbounded();
    let stopped = Rc::new(Cell::new(false));
    let subscription = Subscription {
        events: events.clone(),
        stopped: stopped.clone(),
    };
    wasm_bindgen_futures::spawn_local(run(workspace, after, emit, events, receiver, stopped));
    subscription
}

fn socket_url() -> Option<String> {
    let location = web_sys::window()?.location();
    let scheme = match location.protocol().ok()?.as_str() {
        "https:" => "wss",
        _ => "ws",
    };
    Some(format!(
        "{scheme}://{}{}",
        location.host().ok()?,
        paths::REALTIME
    ))
}

/// What the task keeps across connections.
struct Progress {
    cursor: EventSeq,
    /// The server refused the cursor; wait for the core to resubscribe from a fresh position.
    stale: bool,
    caught_up: bool,
}

async fn run(
    workspace: WorkspaceId,
    after: EventSeq,
    emit: Rc<dyn Fn(Input)>,
    events: mpsc::UnboundedSender<Event>,
    mut receiver: mpsc::UnboundedReceiver<Event>,
    stopped: Rc<Cell<bool>>,
) {
    let mut attempt: u32 = 0;
    let mut progress = Progress {
        cursor: after,
        stale: false,
        caught_up: false,
    };
    let last_clock = Cell::new(f64::NEG_INFINITY);
    while !stopped.get() {
        emit(Input::Socket(SocketEvent::Connecting {
            workspace,
            attempt,
        }));
        let Some(url) = socket_url() else { return };
        let Ok(socket) = WebSocket::new(&url) else {
            return;
        };
        let handlers = Handlers::attach(&socket, &events);
        progress.caught_up = false;
        let mut opened = false;
        let ended = loop {
            let Some(event) = receiver.next().await else {
                break Ended::Stop;
            };
            match event {
                Event::Stop => break Ended::Stop,
                Event::Open => {
                    opened = true;
                    emit(Input::Socket(SocketEvent::Opened { workspace }));
                    if !send_subscribe(&socket, workspace, progress.cursor) {
                        break Ended::Closed;
                    }
                }
                Event::Resubscribe(after) => {
                    progress.cursor = after;
                    progress.stale = false;
                    if opened && !send_subscribe(&socket, workspace, after) {
                        break Ended::Closed;
                    }
                }
                Event::Text(text) => {
                    let Ok(frame) = serde_json::from_str::<ServerFrame>(&text) else {
                        // Not quoted: it can be a message.
                        web_sys::console::warn_1(&JsValue::from_str(
                            "Lazy ignored a realtime frame it could not read",
                        ));
                        continue;
                    };
                    match &frame {
                        ServerFrame::Event(envelope) if envelope.workspace_id == workspace => {
                            progress.cursor = progress.cursor.max(envelope.seq);
                        }
                        ServerFrame::CaughtUp { workspace_id, seq }
                            if *workspace_id == workspace =>
                        {
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
                    let now = js_sys::Date::now();
                    if now - last_clock.get() >= CLOCK_EVERY_MS {
                        last_clock.set(now);
                        emit(Input::Clock(lazy_domain::Timestamp::now()));
                    }
                    emit(Input::Frame(frame));
                }
                Event::Closed => break Ended::Closed,
            }
        };
        handlers.detach(&socket);
        let _ = socket.close();
        if matches!(ended, Ended::Stop) || stopped.get() {
            return;
        }
        if progress.caught_up {
            attempt = 0;
        }
        if !opened && session_expired().await {
            emit(Input::SessionExpired);
            return;
        }
        attempt = attempt.saturating_add(1);
        emit(Input::Socket(SocketEvent::Closed { workspace, attempt }));
        if progress.stale {
            // Reconnecting with a refused cursor would be refused again.
            while progress.stale {
                match receiver.next().await {
                    Some(Event::Resubscribe(after)) => {
                        progress.cursor = after;
                        progress.stale = false;
                    }
                    Some(Event::Stop) | None => return,
                    Some(_) => {}
                }
            }
            continue;
        }
        // Wait out the backoff, or less if the core resubscribes meanwhile.
        let wait = Box::pin(crate::api::sleep(backoff(attempt)));
        let mut next = receiver.next();
        match select(wait, &mut next).await {
            Either::Left(((), _)) => {}
            Either::Right((event, _)) => match event {
                Some(Event::Resubscribe(after)) => progress.cursor = after,
                Some(Event::Stop) | None => return,
                Some(_) => {}
            },
        }
    }
}

enum Ended {
    Closed,
    Stop,
}

fn send_subscribe(socket: &WebSocket, workspace: WorkspaceId, after: EventSeq) -> bool {
    let frame = ClientFrame::Subscribe {
        workspace_id: workspace,
        after,
    };
    serde_json::to_string(&frame)
        .ok()
        .is_some_and(|text| socket.send_with_str(&text).is_ok())
}

/// Whether the server no longer accepts the session: asked with a request that changes nothing
/// and needs one (an operation nobody made answers `not_found` to a signed-in caller).
async fn session_expired() -> bool {
    let path = paths::operation(OperationId::generate());
    let answer = crate::api::send("GET", &path, crate::api::Body::None, crate::api::TIMEOUT).await;
    matches!(answer, Ok(answer) if answer.status == 401)
}

/// The socket's callbacks, kept alive while it is open.
struct Handlers {
    _open: Closure<dyn FnMut()>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
    _error: Closure<dyn FnMut()>,
    closed: Rc<RefCell<bool>>,
}

impl Handlers {
    fn attach(socket: &WebSocket, events: &mpsc::UnboundedSender<Event>) -> Handlers {
        let closed = Rc::new(RefCell::new(false));
        let tx = events.clone();
        let open = Closure::<dyn FnMut()>::new(move || {
            let _ = tx.unbounded_send(Event::Open);
        });
        let tx = events.clone();
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if let Some(text) = event.data().as_string() {
                let _ = tx.unbounded_send(Event::Text(text));
            }
        });
        let tx = events.clone();
        let once = closed.clone();
        let close = Closure::<dyn FnMut(CloseEvent)>::new(move |_: CloseEvent| {
            if !once.replace(true) {
                let _ = tx.unbounded_send(Event::Closed);
            }
        });
        let tx = events.clone();
        let once = closed.clone();
        let error = Closure::<dyn FnMut()>::new(move || {
            if !once.replace(true) {
                let _ = tx.unbounded_send(Event::Closed);
            }
        });
        socket.set_onopen(Some(open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(close.as_ref().unchecked_ref()));
        socket.set_onerror(Some(error.as_ref().unchecked_ref()));
        Handlers {
            _open: open,
            _message: message,
            _close: close,
            _error: error,
            closed,
        }
    }

    fn detach(self, socket: &WebSocket) {
        *self.closed.borrow_mut() = true;
        socket.set_onopen(None);
        socket.set_onmessage(None);
        socket.set_onclose(None);
        socket.set_onerror(None);
    }
}
