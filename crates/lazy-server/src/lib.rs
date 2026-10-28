//! The Lazy server: authentication, authorization, durable messaging, and realtime delivery.
//!
//! See `llm-docs/SERVER.md` for the design and `llm-docs/DECISIONS.md` (D2 to D4) for the
//! contracts it implements.
//!
//! The binary is a thin wrapper around [`run`]. The service modules ([`accounts`], [`messages`],
//! [`bootstrap`], [`events`]) take a `PgPool` and contain the rules; [`http`] and [`realtime`]
//! adapt them to the network.

pub mod accounts;
pub mod app;
pub mod bootstrap;
pub mod config;
pub mod crypto;
pub mod db;
pub mod dispatcher;
pub mod error;
pub mod events;
pub mod http;
pub mod messages;
pub mod pruner;
pub mod realtime;

#[cfg(feature = "testing")]
pub mod testing;

pub use app::{AppState, run, serve};
pub use config::{Config, PasswordCost, RetentionPolicy, ServerOptions};
pub use error::AppError;
