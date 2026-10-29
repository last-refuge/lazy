//! Server configuration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Argon2id cost parameters for password hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordCost {
    /// Memory in KiB.
    pub memory_kib: u32,
    /// Number of passes.
    pub iterations: u32,
    pub parallelism: u32,
}

impl PasswordCost {
    /// OWASP-recommended minimum for Argon2id (19 MiB, 2 passes, 1 lane).
    pub const DEFAULT: PasswordCost = PasswordCost {
        memory_kib: 19 * 1024,
        iterations: 2,
        parallelism: 1,
    };

    /// Deliberately cheap parameters so tests that register many users stay fast. Never use these
    /// in a deployment.
    pub const FAST_FOR_TESTS: PasswordCost = PasswordCost {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };
}

impl Default for PasswordCost {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How long the event log is kept.
///
/// Events older than `days` are pruned, except that the newest `keep_newest` events of every
/// workspace are always kept. A client that has been away for longer than the log reaches back
/// simply bootstraps again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Age in days after which events may be pruned. `0` disables pruning.
    pub days: u32,
    /// How often the pruner runs after its pass at startup.
    pub interval: Duration,
    /// Newest events per workspace that are never pruned.
    pub keep_newest: u32,
}

impl RetentionPolicy {
    pub const DEFAULT_DAYS: u32 = 30;
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);
    pub const DEFAULT_KEEP_NEWEST: u32 = 1000;

    /// Retains events for `days` with the default interval and floor of kept events.
    pub const fn from_days(days: u32) -> Self {
        Self {
            days,
            interval: Self::DEFAULT_INTERVAL,
            keep_newest: Self::DEFAULT_KEEP_NEWEST,
        }
    }

    /// A policy that never prunes.
    pub const fn disabled() -> Self {
        Self::from_days(0)
    }

    pub const fn enabled(&self) -> bool {
        self.days > 0
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::from_days(Self::DEFAULT_DAYS)
    }
}

/// Everything [`crate::run`] needs.
#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// Postgres connection URL. Contains credentials; never log it.
    pub database_url: String,
    /// Apply embedded migrations at startup.
    pub migrate: bool,
    /// Create the default workspace and `#general` when no workspace exists.
    pub seed: bool,
    /// Allow anyone who can reach the server to register.
    pub open_registration: bool,
    /// Allow any signed-in person to create workspaces of their own.
    pub open_workspace_creation: bool,
    pub password_cost: PasswordCost,
    /// Days of event history to keep; `0` keeps everything. See [`RetentionPolicy`].
    pub event_retention_days: u32,
    pub max_db_connections: u32,
    /// The address webhooks are shown under (`{public_url}/hooks/{id}/{secret}`). Defaults to
    /// `http://` and the address the server listens on; set it when clients reach the server by
    /// another name.
    pub public_url: Option<String>,
    /// Where attachment files are kept (see [`AttachmentOptions`]).
    pub attachments_dir: PathBuf,
    /// The largest attachment, in bytes.
    pub max_attachment_bytes: u64,
}

impl std::fmt::Debug for Config {
    /// Everything but the database URL, which holds credentials: it is shown as its host, port and
    /// database name only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field(
                "database",
                &crate::logging::describe_database_url(&self.database_url),
            )
            .field("migrate", &self.migrate)
            .field("seed", &self.seed)
            .field("open_registration", &self.open_registration)
            .field("open_workspace_creation", &self.open_workspace_creation)
            .field("password_cost", &self.password_cost)
            .field("event_retention_days", &self.event_retention_days)
            .field("max_db_connections", &self.max_db_connections)
            .field(
                "public_url",
                &self.public_url.as_deref().map(crate::logging::describe_url),
            )
            .field("attachments_dir", &self.attachments_dir)
            .field("max_attachment_bytes", &self.max_attachment_bytes)
            .finish()
    }
}

impl Config {
    /// A configuration with development defaults for the given address and database.
    pub fn new(listen: SocketAddr, database_url: impl Into<String>) -> Self {
        Self {
            listen,
            database_url: database_url.into(),
            migrate: true,
            seed: true,
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::DEFAULT,
            event_retention_days: RetentionPolicy::DEFAULT_DAYS,
            max_db_connections: 20,
            public_url: None,
            attachments_dir: PathBuf::from("attachments"),
            max_attachment_bytes: lazy_domain::attachment::MAX_ATTACHMENT_BYTES,
        }
    }
}

/// The subset of configuration the running server keeps in its shared state.
#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    pub open_registration: bool,
    /// Whether a signed-in person may create a workspace (`PUT /v1/workspaces/{id}`).
    pub open_workspace_creation: bool,
    pub password_cost: PasswordCost,
    pub retention: RetentionPolicy,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::DEFAULT,
            retention: RetentionPolicy::default(),
        }
    }
}

/// How fast one incoming webhook may post: a burst of `burst` deliveries, then one more every
/// `refill_every`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebhookRate {
    pub burst: u32,
    pub refill_every: Duration,
}

impl WebhookRate {
    pub const DEFAULT: WebhookRate = WebhookRate {
        burst: 30,
        refill_every: Duration::from_secs(1),
    };
}

impl Default for WebhookRate {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Settings for integrations, kept apart from [`ServerOptions`] so that adding one does not change
/// the struct other crates construct.
#[derive(Debug, Clone, Default)]
pub struct IntegrationOptions {
    /// See [`Config::public_url`]; `None` means `http://` and the listening address.
    pub public_url: Option<String>,
    pub webhook_rate: WebhookRate,
}

/// Where the bytes of attachments live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentBackend {
    /// In memory, gone when the server stops. For tests that do not look at the disk.
    Memory,
    /// Files under this directory (`{dir}/{first two hex digits of the id}/{id}`, uploads staged in
    /// `{dir}/tmp`).
    Directory(PathBuf),
}

/// Settings for attachments, kept apart from [`ServerOptions`] so that adding one does not change
/// the struct other crates construct. The defaults are the ones the server runs with, except the
/// backend, which is [`AttachmentBackend::Memory`].
#[derive(Debug, Clone)]
pub struct AttachmentOptions {
    pub backend: AttachmentBackend,
    /// The largest file, in bytes.
    pub max_bytes: u64,
    /// How long an upload that no message names is kept.
    pub pending_ttl: Duration,
    /// How often the cleanup job runs (expired uploads, files of deleted messages).
    pub cleanup_every: Duration,
    /// How often the sweep for files that have no row runs.
    pub sweep_every: Duration,
    /// How old a file with no row must be before the sweep removes it: an upload in progress has
    /// a file and no row for a moment.
    pub sweep_grace: Duration,
    /// Thumbnails and image headers decoded at once.
    pub thumbnail_slots: usize,
    /// Test hook: an upload fails after its files are stored and before its row is written, as a
    /// crash there would. Never set by the server binary.
    pub fail_between_file_and_row: bool,
}

impl Default for AttachmentOptions {
    fn default() -> Self {
        Self {
            backend: AttachmentBackend::Memory,
            max_bytes: lazy_domain::attachment::MAX_ATTACHMENT_BYTES,
            pending_ttl: Duration::from_secs(
                lazy_domain::attachment::PENDING_ATTACHMENT_TTL_HOURS as u64 * 3600,
            ),
            cleanup_every: Duration::from_secs(3600),
            sweep_every: Duration::from_secs(24 * 3600),
            sweep_grace: Duration::from_secs(3600),
            thumbnail_slots: 4,
            fail_between_file_and_row: false,
        }
    }
}

/// How many of the newest matches a relevance search ranks (D19).
pub const RANK_CANDIDATES: usize = 2000;

/// Settings for search, kept apart from [`ServerOptions`] like [`AttachmentOptions`]. The defaults
/// are the ones the server runs with.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// How long one search may run before it is cancelled and answered `unavailable` (D19: 3 s).
    pub statement_timeout: Duration,
    /// How many of the newest matches a relevance search ranks (D19: 2,000); more matches than
    /// this and the response says `truncated`.
    pub rank_candidates: usize,
    /// Test hook: every search sleeps this long (in the database, under its statement timeout)
    /// before it runs, so a test can make one slow. Never set by the server binary.
    pub test_delay: Option<Duration>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            statement_timeout: Duration::from_secs(3),
            rank_candidates: RANK_CANDIDATES,
            test_delay: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_days_disables_pruning() {
        assert!(!RetentionPolicy::from_days(0).enabled());
        assert!(!RetentionPolicy::disabled().enabled());
        assert!(RetentionPolicy::from_days(1).enabled());
    }

    #[test]
    fn the_default_keeps_thirty_days_and_the_newest_thousand_events() {
        let policy = RetentionPolicy::default();
        assert_eq!(policy.days, 30);
        assert_eq!(policy.keep_newest, 1000);
        assert_eq!(policy.interval, Duration::from_secs(3600));
        assert!(policy.enabled());
    }

    #[test]
    fn a_new_config_keeps_events_for_the_default_period() {
        let config = Config::new("127.0.0.1:0".parse().unwrap(), "postgres://example");
        assert_eq!(config.event_retention_days, RetentionPolicy::DEFAULT_DAYS);
    }
}
