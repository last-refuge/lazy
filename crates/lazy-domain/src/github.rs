//! Rules for the GitHub connector (D32) that the server and clients share: what a repository and
//! an organization name look like, and the kinds of activity a channel can follow.

use serde::{Deserialize, Serialize};

/// A kind of repository activity a channel can follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubEvent {
    /// Pull requests opened, ready for review, reopened, merged, or closed.
    PullRequest,
    /// Reviews submitted: approved, changes requested, or commented.
    Review,
    /// Issues opened, closed, or reopened.
    Issues,
    /// New comments on issues and pull requests.
    Comments,
    /// Commits pushed to the default branch.
    Push,
    /// Releases published.
    Releases,
    /// Workflow runs that failed.
    Ci,
    /// A kind this version does not know (sent by a newer server).
    #[serde(other)]
    Unknown,
}

impl GithubEvent {
    /// Every kind a subscription can choose, in the order they are shown.
    pub const ALL: [GithubEvent; 7] = [
        GithubEvent::PullRequest,
        GithubEvent::Review,
        GithubEvent::Issues,
        GithubEvent::Comments,
        GithubEvent::Push,
        GithubEvent::Releases,
        GithubEvent::Ci,
    ];

    /// What a new subscription follows unless the admin chooses: the activity a team wants to
    /// see without being flooded (no comments or reviews).
    pub const DEFAULT: [GithubEvent; 5] = [
        GithubEvent::PullRequest,
        GithubEvent::Issues,
        GithubEvent::Push,
        GithubEvent::Releases,
        GithubEvent::Ci,
    ];

    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PullRequest => "pull_request",
            Self::Review => "review",
            Self::Issues => "issues",
            Self::Comments => "comments",
            Self::Push => "push",
            Self::Releases => "releases",
            Self::Ci => "ci",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == name)
    }

    /// What it is called in the app.
    pub fn label(self) -> &'static str {
        match self {
            Self::PullRequest => "Pull requests",
            Self::Review => "Reviews",
            Self::Issues => "Issues",
            Self::Comments => "Comments",
            Self::Push => "Commits to the default branch",
            Self::Releases => "Releases",
            Self::Ci => "Failed workflow runs",
            Self::Unknown => "Other",
        }
    }
}

/// A repository as `owner/name`, lower case, or `None` if it is not one. Accepts a full
/// `https://github.com/owner/name` address and a trailing `.git` or `/`.
pub fn normalize_repository(text: &str) -> Option<String> {
    let mut text = text.trim();
    for prefix in ["https://github.com/", "http://github.com/", "github.com/"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
        }
    }
    let text = text.trim_end_matches('/');
    let text = text.strip_suffix(".git").unwrap_or(text);
    let (owner, name) = text.split_once('/')?;
    let owner_ok = is_login(owner);
    let name_ok = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    (owner_ok && name_ok).then(|| format!("{owner}/{name}").to_ascii_lowercase())
}

/// Whether `text` is a GitHub account or organization name: 1 to 39 letters, digits and single
/// hyphens, not starting or ending with one.
pub fn is_login(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 39
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !text.starts_with('-')
        && !text.ends_with('-')
        && !text.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_are_normalized_or_refused() {
        for (input, want) in [
            ("last-refuge/lazy", Some("last-refuge/lazy")),
            ("  Last-Refuge/Lazy ", Some("last-refuge/lazy")),
            (
                "https://github.com/last-refuge/lazy",
                Some("last-refuge/lazy"),
            ),
            (
                "https://github.com/last-refuge/lazy.git",
                Some("last-refuge/lazy"),
            ),
            ("github.com/last-refuge/lazy/", Some("last-refuge/lazy")),
            ("last-refuge/my_repo.js", Some("last-refuge/my_repo.js")),
            ("lazy", None),
            ("last-refuge/", None),
            ("/lazy", None),
            ("last-refuge/lazy/issues", None),
            ("-bad/lazy", None),
            ("last-refuge/..", None),
            ("last refuge/lazy", None),
        ] {
            assert_eq!(normalize_repository(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn logins_follow_githubs_rules() {
        assert!(is_login("last-refuge") && is_login("a") && is_login(&"x".repeat(39)));
        for bad in ["", "-a", "a-", "a--b", "a_b", &"x".repeat(40), "a b"] {
            assert!(!is_login(bad), "{bad}");
        }
    }

    #[test]
    fn events_round_trip_and_unknown_ones_are_tolerated() {
        for event in GithubEvent::ALL {
            assert_eq!(GithubEvent::from_name(event.as_str()), Some(event));
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(json, format!("\"{}\"", event.as_str()));
        }
        assert_eq!(
            serde_json::from_str::<GithubEvent>("\"deployments\"").unwrap(),
            GithubEvent::Unknown
        );
        assert!(
            GithubEvent::DEFAULT
                .iter()
                .all(|e| GithubEvent::ALL.contains(e))
        );
    }
}
