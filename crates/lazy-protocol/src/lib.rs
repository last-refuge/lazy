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

//! # Editing and deleting
//!
//! `PATCH /v1/messages/{id}` replaces a message's content ([`api::EditMessageRequest`]). It carries
//! the revision the editor started from; a newer revision makes the server answer
//! `stale_revision`, and the client fetches the message instead of overwriting. Like a send, an
//! edit has an operation id, is journaled before the request, and an unknown outcome is
//! reconciled through `GET /v1/operations/{id}` (`edit_committed`). `DELETE /v1/messages/{id}`
//! turns a message into a tombstone and is idempotent, so a lost response is simply retried.
//! Both produce events ([`realtime::Event::MessageEdited`], [`realtime::Event::MessageDeleted`])
//! at the message's conversation members. A message keeps the position it was created at, so
//! editing never reorders a conversation.
//!
//! # Reactions
//!
//! `PUT /v1/messages/{id}/reactions` sets whether the caller reacted with one emoji
//! ([`api::SetReactionRequest`]). It sets a state, so it is idempotent and carries no operation
//! id. Every [`api::Message`] carries its `reactions` and `reactions_as_of`; a change is announced
//! as [`realtime::Event::ReactionsChanged`] with the complete new list. Because reactions change
//! independently of content, a client keeps for each message whichever reactions have the highest
//! `reactions_as_of` (an event's position is its envelope `seq`), which makes a history page that
//! was in flight while a reaction changed harmless.
//!
//! # Notifications and attention
//!
//! What notifies a person is decided by one pure function, [`lazy_domain::notification::decide`],
//! from their settings ([`lazy_domain::notification::NotificationSettings`], per workspace, with
//! per-conversation overrides and mutes) and what they follow. The server stores and syncs the
//! preferences (bootstrap, `PUT` requests, owner-only events) and a person's status
//! ([`lazy_domain::notification::UserStatus`], on [`api::User`]); clients evaluate the rule for the
//! messages they receive live to show desktop notifications, and a push worker will evaluate the
//! same function later. No notification is stored per recipient. The thread inbox
//! (`GET /v1/workspaces/{id}/inbox`) lists the threads a person follows, most recently active
//! first.
//!
//! # Search
//!
//! `GET /v1/workspaces/{id}/search` ([`api::SearchParams`]) returns a page of [`api::SearchHit`]s:
//! the message, its conversation, what matched (the text, or the name of a file), and a snippet
//! with the matched words marked. The query language is [`lazy_domain::search::SearchQuery`]. A
//! person can find anything in a conversation they are in or a public channel, never anything
//! else, and never a deleted message; opening a hit is opening a permalink.
//!
//! # Custom emoji
//!
//! An admin adds a custom emoji by uploading an image (`PUT /v1/workspaces/{id}/emoji/{emoji_id}`
//! with the name in `x-emoji-name`; the id is the client's, so a retry is the same upload). The
//! server stores a small PNG of it. Emoji arrive in the bootstrap ([`api::CustomEmoji`], deleted
//! ones included) and are kept current by [`realtime::Event::EmojiChanged`]. A custom emoji is
//! used in a message as an inline item ([`lazy_domain::Inline::Emoji`], typed `:name:`) and as a
//! reaction ([`lazy_domain::ReactionEmoji::Custom`], `custom:{id}` on the wire); both name it by
//! identity, so a rename changes how it reads without disturbing anything.
//!
//! # Attachments
//!
//! A client uploads each file with `PUT /v1/conversations/{id}/attachments/{attachment_id}` (the
//! id is its own, so a retry is the same upload and an unknown outcome is simply repeated). The
//! upload is *pending* until a message is sent naming its id in
//! [`api::SendMessageRequest::attachment_ids`]; the message then carries the [`api::Attachment`]s.
//! Files are downloaded with `GET /v1/attachments/{id}` (and `/thumbnail` for images) by anyone
//! who can read the conversation. The server decides what each file is from its bytes, so the
//! media type on an attachment can differ from what the uploader said, and only raster images are
//! ever shown inline.
//!
//! # Incoming webhooks
//!
//! An admin creates an incoming webhook for a conversation ([`api::CreateWebhookRequest`]) and is
//! shown its address once ([`api::CreatedWebhook`]). Anything that can send an HTTP `POST` to
//! `/hooks/{id}/{secret}` with [`api::IncomingWebhookRequest`] then posts into the conversation
//! as the webhook's bot ([`api::User::is_bot`]), through the same send path as a person's
//! message. The bot is a member of the conversation (announced like any member) and is removed
//! when the webhook is deleted; its past messages stay.
//!
//! # Threads
//!
//! A reply is sent with [`api::SendMessageRequest::thread_root`] (optionally also to the channel)
//! and carries `thread_root` on the [`api::Message`]; its root carries a
//! [`api::ThreadSummary`] with its own version, `as_of`, which clients keep the highest of (as for
//! reactions). A conversation's history and bootstrap list top-level messages and replies sent
//! also to the channel; `GET /v1/messages/{root}/replies` pages a thread. A reply is announced by
//! [`realtime::Event::MessageCreated`] followed by [`realtime::Event::ThreadChanged`]. Following,
//! read positions, and unread counts per thread are [`api::ThreadUserState`], listed in the
//! bootstrap and kept current for the person by [`realtime::Event::ThreadStateChanged`]. Replies
//! inherit the conversation's access, and a deleted root that still has replies stays as a
//! tombstone.
//!
//! # Mentions and user groups
//!
//! A message document can contain mentions ([`lazy_domain::Inline::Mention`]): a person, a user
//! group, or `@channel`. The server works out who that notifies and returns it as
//! [`api::Mentions`] on the [`api::Message`]; clients count a conversation's `mention_count` from
//! it. An edit event also carries the mentions the message had before, and a deletion event the
//! mentions it had, so counts stay right without holding the earlier version. User groups
//! ([`api::UserGroup`]) are managed by admins, travel in the bootstrap, and are kept current by
//! [`realtime::Event::GroupChanged`] and [`realtime::Event::GroupDeleted`]. `GET
//! /v1/conversations/{id}/members` lists who is in a conversation, so autocomplete offers only
//! people who can see it.
//!
//! A permalink to a message is [`lazy_domain::Permalink`], which a client resolves with
//! `GET /v1/messages/{id}` to learn the message's conversation and position.

pub mod api;
pub mod error;
pub mod realtime;
pub mod secret;

pub use api::*;
pub use error::{ApiError, ErrorCode};
pub use realtime::{ClientFrame, Event, EventEnvelope, ServerFrame, SnapshotReason};
pub use secret::{InviteCode, Password, SessionToken};

/// Version of this protocol. Bumped for incompatible changes; clients compare it with the value
/// returned by `GET /v1/bootstrap` and `GET /healthz`.
pub const PROTOCOL_VERSION: u32 = 14;
