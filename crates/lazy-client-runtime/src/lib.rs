//! Executes [`lazy_client_core::Effect`]s: HTTP, realtime connections, encrypted local state, and
//! secure storage.
//!
//! The runtime owns every side effect so the core stays pure. A host feeds the core's effects to
//! [`Runtime::execute`] and feeds the [`lazy_client_core::Input`]s from the receiver back into the
//! core. The runtime never touches UI code.

mod active_workspace;
mod api;
mod appearance;
mod blob;
mod config;
mod files;
mod images;
mod outbox;
mod prefs;
mod realtime;
mod recent_conversations;
mod runtime;
mod saved_server;
mod searches;
mod secure;
mod store;

pub use async_channel;
pub use config::{
    BackoffPolicy, DEFAULT_SERVER_URL, RuntimeConfig, SecureStorageChoice, default_server_url,
};
#[doc(hidden)]
pub use runtime::inspect_state;
pub use runtime::{Runtime, RuntimeError};
pub use secure::{
    InsecureFileStorage, KeyringStorage, MemoryStorage, SecretVault, SecureStorage, StorageError,
    secret_namespace,
};
