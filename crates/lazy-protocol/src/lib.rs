//! Versioned wire types shared by the Lazy server and its clients.
//!
//! The API is JSON over HTTPS for commands and queries, plus one WebSocket per client for
//! realtime events. Everything that crosses the network is defined here, so the two sides cannot
//! drift apart. Types that carry invariants (names, message documents, sequence numbers) validate
//! while deserializing.
//!
//! # Synchronizing a client
//!
//! 1. `GET /v1/bootstrap` returns, per workspace, a consistent snapshot of conversations and
//!    members together with `head`, the event-log position that snapshot corresponds to.
//! 2. `GET /v1/conversations/{id}/messages` pages history. Pages may include messages newer
//!    than `head`; clients deduplicate by message id.
//! 3. Connect the WebSocket and send [`realtime::ClientFrame::Subscribe`] with `after` set to
//!    `head` (first run) or to the last event position the client applied (reconnect). The server
//!    replays every committed event after that position the caller may see, then sends
//!    [`realtime::ServerFrame::CaughtUp`], then streams live events. If the position cannot be
//!    replayed the server sends [`realtime::ServerFrame::SnapshotRequired`] and the client starts
//!    again from step 1.
//!
//! # Sending
//!
//! A client generates an [`lazy_domain::OperationId`] for each message it sends and records it
//! durably before the request. Retrying the same message always reuses that id. If a response is
//! lost, the client asks `GET /v1/operations/{id}`: `committed` means the message exists;
//! `not_found` means the server has no record, and the only safe next step is to resend the same
//! request with the same id. A different payload under a used id is rejected with `conflict`.

pub mod api;
pub mod error;
pub mod realtime;
pub mod secret;

pub use api::*;
pub use error::{ApiError, ErrorCode};
pub use realtime::{ClientFrame, Event, EventEnvelope, ServerFrame, SnapshotReason};
pub use secret::{Password, SessionToken};

/// Version of this protocol. Bumped for incompatible changes; clients compare it with the value
/// returned by `GET /v1/bootstrap` and `GET /healthz`.
pub const PROTOCOL_VERSION: u32 = 1;
