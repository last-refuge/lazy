//! The Lazy server: authentication, authorization, durable messaging, and realtime delivery.
//!
//! See `llm-docs/SERVER.md` for the design and `llm-docs/DECISIONS.md` (D2 to D4) for the
//! contracts it implements.
//!
//! The binary is a thin wrapper around [`run`]. The service modules ([`accounts`], [`messages`],
//! [`bootstrap`], [`events`]) take a `PgPool` and contain the rules; [`http`] and [`realtime`]
//! adapt them to the network.

pub mod access;
pub mod accounts;
pub mod admin;
pub mod app;
pub mod attachments;
pub mod bootstrap;
pub mod config;
pub mod conversations;
pub mod crypto;
pub mod db;
pub mod discovery;
pub mod dispatcher;
pub mod emoji;
pub mod error;
pub mod events;
pub mod groups;
pub mod health;
pub mod http;
pub mod logging;
pub mod messages;
pub mod notifications;
pub mod onboarding;
pub mod profile;
pub mod pruner;
pub mod realtime;
pub mod receipts;
pub mod search;
pub mod threads;
pub mod webhooks;
pub mod workspaces;

#[cfg(feature = "testing")]
pub mod testing;

pub use app::{AppState, run, serve, serve_all, serve_full, serve_with};
pub use config::{
    AttachmentBackend, AttachmentOptions, Config, IntegrationOptions, PasswordCost,
    RetentionPolicy, SearchOptions, ServerOptions, SetupCode, WebhookRate,
};
pub use error::AppError;
