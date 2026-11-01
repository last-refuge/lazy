//! Rules for webhook connectors (D34) that the server and clients share: which providers there
//! are, the kinds of activity a channel can follow from each, how a team or project key is
//! written, and which side makes the signing secret.
//!
//! A webhook connector is a provider that sends Lazy signed deliveries from a webhook an admin
//! creates in the provider's own settings (Linear, Jira). Lazy shows the address to paste there;
//! the provider's signing secret is pasted into Lazy, or Lazy makes one to paste there.

use serde::{Deserialize, Serialize};

/// A provider with a webhook connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorProvider {
    Linear,
    Jira,
    Sentry,
    PagerDuty,
    /// A provider this version does not know (sent by a newer server).
    #[serde(other)]
    Unknown,
}

/// Which side makes a connector's signing secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    /// The provider shows one after the webhook is made; the admin pastes it into Lazy.
    Provider,
    /// Lazy makes one; the admin pastes it into the provider's webhook form.
    Lazy,
}

/// A kind of activity a channel can follow from a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorKind {
    /// The stored name.
    pub key: &'static str,
    /// What it is called in the app.
    pub label: &'static str,
    /// Followed by a new subscription unless the admin chooses otherwise.
    pub default: bool,
}

const fn kind(key: &'static str, label: &'static str, default: bool) -> ConnectorKind {
    ConnectorKind {
        key,
        label,
        default,
    }
}

const LINEAR_KINDS: [ConnectorKind; 5] = [
    kind("issue_created", "New issues", true),
    kind("issue_status", "Status changes", true),
    kind("issue_assigned", "Assignments", false),
    kind("comment", "Comments", false),
    kind("project", "Projects and project updates", true),
];

const SENTRY_KINDS: [ConnectorKind; 5] = [
    kind("issue_created", "New issues", true),
    kind("issue_unresolved", "Issues back again", true),
    kind("issue_resolved", "Resolved issues", false),
    kind("issue_assigned", "Assignments", false),
    kind("alert", "Alerts", true),
];

const PAGERDUTY_KINDS: [ConnectorKind; 5] = [
    kind("triggered", "Triggered incidents", true),
    kind("acknowledged", "Acknowledged", true),
    kind("resolved", "Resolved", true),
    kind("escalated", "Escalations and reassignments", false),
    kind("note", "Notes", false),
];

const JIRA_KINDS: [ConnectorKind; 4] = [
    kind("issue_created", "New issues", true),
    kind("issue_status", "Status changes", true),
    kind("issue_assigned", "Assignments", false),
    kind("comment", "Comments", false),
];

impl ConnectorProvider {
    /// Every provider, in the order they are shown.
    pub const ALL: [ConnectorProvider; 4] = [
        ConnectorProvider::Linear,
        ConnectorProvider::Jira,
        ConnectorProvider::Sentry,
        ConnectorProvider::PagerDuty,
    ];

    /// The stored name, also used in addresses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Jira => "jira",
            Self::Sentry => "sentry",
            Self::PagerDuty => "pagerduty",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }

    /// Its name in the app (and its bot's display name).
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Jira => "Jira",
            Self::Sentry => "Sentry",
            Self::PagerDuty => "PagerDuty",
            Self::Unknown => "Unknown",
        }
    }

    /// The kinds of activity a channel can follow, in the order they are shown.
    pub fn kinds(self) -> &'static [ConnectorKind] {
        match self {
            Self::Linear => &LINEAR_KINDS,
            Self::Jira => &JIRA_KINDS,
            Self::Sentry => &SENTRY_KINDS,
            Self::PagerDuty => &PAGERDUTY_KINDS,
            Self::Unknown => &[],
        }
    }

    pub fn kind(self, key: &str) -> Option<ConnectorKind> {
        self.kinds().iter().copied().find(|k| k.key == key)
    }

    /// The keys a new subscription follows unless the admin chooses.
    pub fn default_kinds(self) -> Vec<&'static str> {
        self.kinds()
            .iter()
            .filter(|k| k.default)
            .map(|k| k.key)
            .collect()
    }

    pub fn secret_source(self) -> SecretSource {
        match self {
            Self::Linear | Self::Sentry | Self::PagerDuty | Self::Unknown => SecretSource::Provider,
            Self::Jira => SecretSource::Lazy,
        }
    }

    /// What a subscription's scope is called: the provider's unit of work.
    pub fn scope_label(self) -> &'static str {
        match self {
            Self::Linear => "Team",
            Self::Jira | Self::Sentry => "Project",
            Self::PagerDuty => "Service",
            Self::Unknown => "Scope",
        }
    }

    /// An example key, for a text field's placeholder.
    pub fn scope_example(self) -> &'static str {
        match self {
            Self::Linear => "ENG",
            Self::Jira => "PROJ",
            Self::Sentry => "web-app",
            Self::PagerDuty => "PXYZ123",
            Self::Unknown => "",
        }
    }

    /// A team or project key as stored (upper case), from a key or an address that holds one, or
    /// `None` if it is not one. An empty key means everything and is `Some("")`.
    pub fn normalize_scope(self, text: &str) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return Some(String::new());
        }
        let key = match self {
            // linear.app/<org>/team/ENG/..., or a key, or an issue identifier (ENG-12).
            Self::Linear => path_after(text, "/team/").unwrap_or(text),
            // .../browse/PROJ-1, .../projects/PROJ/..., or a key.
            Self::Jira => path_after(text, "/projects/")
                .or_else(|| path_after(text, "/browse/"))
                .unwrap_or(text),
            // .../projects/<slug>/... (settings or the project page), or a slug.
            Self::Sentry => {
                let slug = path_after(text, "/projects/").unwrap_or(text);
                let ok = (1..=50).contains(&slug.len())
                    && slug.as_bytes()[0].is_ascii_alphanumeric()
                    && slug
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                return ok.then(|| slug.to_ascii_uppercase());
            }
            // .../service-directory/PXYZ123 or .../services/PXYZ123, or an id.
            Self::PagerDuty => {
                let id = path_after(text, "/service-directory/")
                    .or_else(|| path_after(text, "/services/"))
                    .unwrap_or(text);
                let ok =
                    (2..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric());
                return ok.then(|| id.to_ascii_uppercase());
            }
            Self::Unknown => return None,
        };
        let key = match key.split_once('-') {
            Some((key, number)) if number.bytes().all(|b| b.is_ascii_digit()) => key,
            _ => key,
        };
        let valid = match self {
            Self::Linear => {
                (1..=16).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric())
            }
            Self::Jira => {
                (1..=32).contains(&key.len())
                    && key.as_bytes()[0].is_ascii_alphabetic()
                    && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }
            Self::Sentry | Self::PagerDuty | Self::Unknown => false,
        };
        valid.then(|| key.to_ascii_uppercase())
    }
}

/// The path segment after `marker` in an address.
fn path_after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let segment = rest.split(['/', '?', '#']).next()?;
    (!segment.is_empty()).then_some(segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_unknown_is_tolerated() {
        for provider in ConnectorProvider::ALL {
            assert_eq!(
                ConnectorProvider::from_name(provider.as_str()),
                Some(provider)
            );
            assert!(!provider.default_kinds().is_empty());
        }
        let parsed: ConnectorProvider = serde_json::from_str("\"trello\"").unwrap();
        assert_eq!(parsed, ConnectorProvider::Unknown);
    }

    #[test]
    fn scopes_from_keys_and_addresses() {
        let linear = ConnectorProvider::Linear;
        assert_eq!(linear.normalize_scope(" eng "), Some("ENG".into()));
        assert_eq!(linear.normalize_scope(""), Some(String::new()));
        assert_eq!(linear.normalize_scope("ENG-123"), Some("ENG".into()));
        assert_eq!(
            linear.normalize_scope("https://linear.app/acme/team/OPS/active"),
            Some("OPS".into())
        );
        assert_eq!(linear.normalize_scope("not a key"), None);
        let jira = ConnectorProvider::Jira;
        assert_eq!(jira.normalize_scope("proj_2"), Some("PROJ_2".into()));
        assert_eq!(
            jira.normalize_scope("https://acme.atlassian.net/browse/WEB-42"),
            Some("WEB".into())
        );
        assert_eq!(
            jira.normalize_scope("https://acme.atlassian.net/jira/software/projects/APP/boards/1"),
            Some("APP".into())
        );
        assert_eq!(jira.normalize_scope("2BAD"), None);
        let sentry = ConnectorProvider::Sentry;
        assert_eq!(sentry.normalize_scope("web-app"), Some("WEB-APP".into()));
        assert_eq!(
            sentry.normalize_scope("https://acme.sentry.io/settings/projects/api-server/"),
            Some("API-SERVER".into())
        );
        assert_eq!(
            sentry.normalize_scope("https://sentry.io/organizations/acme/projects/web/?project=1"),
            Some("WEB".into())
        );
        assert_eq!(sentry.normalize_scope("bad slug"), None);
        let pd = ConnectorProvider::PagerDuty;
        assert_eq!(
            pd.normalize_scope("https://acme.pagerduty.com/service-directory/PXYZ123"),
            Some("PXYZ123".into())
        );
        assert_eq!(pd.normalize_scope("p1abc"), Some("P1ABC".into()));
        assert_eq!(pd.normalize_scope("not an id"), None);
    }
}
