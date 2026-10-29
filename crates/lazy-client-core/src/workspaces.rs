//! Several workspaces, the person's profile and avatar, and the appearance of this device (slice
//! 3G, D23): the types the interface reads.

use lazy_domain::{WorkspaceId, WorkspaceRole};
use serde::{Deserialize, Serialize};

use crate::model::ConnectionState;

/// One square of the rail, a slim column at the window's left edge with one entry per workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailItem {
    pub id: WorkspaceId,
    /// The workspace's name, for the tooltip.
    pub name: String,
    /// Up to two letters standing for the name.
    pub initials: String,
    /// The person's role there.
    pub role: WorkspaceRole,
    /// The workspace the screen shows.
    pub active: bool,
    /// What needs the person's attention there: the number `attention_count()` would give if the
    /// workspace were the active one.
    pub attention: u32,
    /// There are unread messages that do not need attention (show a dot, not a number).
    pub unread: bool,
    /// How this workspace's realtime connection is doing.
    pub connection: ConnectionState,
}

/// The color scheme of this device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    /// Follow the operating system's appearance.
    #[default]
    System,
    Light,
    Dark,
}

/// Whether the interface animates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionChoice {
    /// Follow the operating system's preference.
    #[default]
    System,
    /// No animation; scrolling to a message jumps.
    Reduce,
    Full,
}

/// How this device looks and moves. Local to the device (not synced), kept in the profile
/// directory with the other small preferences, and applied at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appearance {
    #[serde(default)]
    pub theme: ThemeChoice,
    #[serde(default)]
    pub motion: MotionChoice,
}

/// The most avatars the core holds decoded-ready bytes for at once, and the most bytes of them.
pub const MAX_HELD_AVATARS: usize = 200;
pub const MAX_HELD_AVATAR_BYTES: usize = 24 * 1024 * 1024;
/// The most avatar fetches asked of the runtime at once, and queued behind them.
pub const MAX_QUEUED_AVATAR_FETCHES: usize = 64;
