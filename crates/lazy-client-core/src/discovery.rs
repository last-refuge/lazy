//! Discovery (slice 3E, D21): saved messages, the people directory and profiles, the channel
//! directory, and the quick switcher. The types the screen reads.

use lazy_domain::{ConversationId, UserId};
use lazy_protocol::{BrowsedChannel, ChannelSort, Person, SavedItem};

pub use crate::notify::LoadStatus;

/// How many people or channels a directory request asks for.
pub const DIRECTORY_PAGE: u32 = 50;
/// How many saved messages a request asks for.
pub const SAVED_PAGE: u32 = 20;
/// The most results the quick switcher gives.
pub const QUICK_SWITCHER_LIMIT: usize = 12;
/// How many recently visited conversations the core remembers.
pub const MAX_RECENT_CONVERSATIONS: usize = 20;

/// The person's saved messages, most recently saved first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedView {
    /// `Input::OpenSaved` has been sent and `Input::CloseSaved` has not.
    pub open: bool,
    pub items: Vec<SavedItem>,
    pub has_more: bool,
    pub status: LoadStatus,
}

impl Default for SavedView {
    fn default() -> Self {
        Self {
            open: false,
            items: Vec::new(),
            has_more: false,
            status: LoadStatus::Idle,
        }
    }
}

/// The workspace's people, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeopleView {
    pub open: bool,
    /// The text in the search box.
    pub query: String,
    /// Deactivated people are listed too (admins and owners only).
    pub include_deactivated: bool,
    pub people: Vec<Person>,
    pub has_more: bool,
    pub status: LoadStatus,
}

impl Default for PeopleView {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            include_deactivated: false,
            people: Vec::new(),
            has_more: false,
            status: LoadStatus::Idle,
        }
    }
}

/// One person's profile. While `person` is `None` the screen shows what the core already knows
/// (`ClientCore::member`) and a loading mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileView {
    pub user_id: UserId,
    pub person: Option<Person>,
    pub status: LoadStatus,
}

/// The channel directory, as asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseView {
    /// The text in the search box.
    pub query: String,
    /// `Some(true)` only channels the person has joined, `Some(false)` only those they have not.
    pub joined: Option<bool>,
    pub sort: ChannelSort,
    pub channels: Vec<BrowsedChannel>,
    pub has_more: bool,
    pub status: LoadStatus,
}

impl Default for BrowseView {
    fn default() -> Self {
        Self {
            query: String::new(),
            joined: None,
            sort: ChannelSort::Name,
            channels: Vec::new(),
            has_more: false,
            status: LoadStatus::Idle,
        }
    }
}

/// Where choosing a quick switcher result goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickSwitcherTarget {
    Conversation(ConversationId),
    /// A person with no direct conversation yet: choosing opens (or creates) it.
    Person(UserId),
    Threads,
    Saved,
    /// Opens the search view, empty.
    Search,
    /// Opens the channel directory with this text.
    BrowseChannels {
        query: String,
    },
}

/// One row of the quick switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickSwitcherItem {
    pub target: QuickSwitcherTarget,
    /// `#design`, the people of a conversation, a person's name, or the destination's name.
    pub title: String,
    /// `@username` for a person, or when the text matched a username instead of the title.
    pub subtitle: Option<String>,
    /// The characters of `title` the text matched, as positions in its characters (strictly
    /// increasing; empty when the title was not what matched).
    pub matched: Vec<usize>,
    pub unread: u32,
    pub mentions: u32,
    pub muted: bool,
    pub archived: bool,
}
