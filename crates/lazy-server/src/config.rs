//! Server configuration.

use std::net::SocketAddr;
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
#[derive(Debug, Clone)]
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
    pub password_cost: PasswordCost,
    /// Days of event history to keep; `0` keeps everything. See [`RetentionPolicy`].
    pub event_retention_days: u32,
    pub max_db_connections: u32,
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
            password_cost: PasswordCost::DEFAULT,
            event_retention_days: RetentionPolicy::DEFAULT_DAYS,
            max_db_connections: 20,
        }
    }
}

/// The subset of configuration the running server keeps in its shared state.
#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    pub open_registration: bool,
    pub password_cost: PasswordCost,
    pub retention: RetentionPolicy,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            open_registration: true,
            password_cost: PasswordCost::DEFAULT,
            retention: RetentionPolicy::default(),
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
