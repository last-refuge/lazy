//! Notices: messages the server posts about what happened in a conversation (D44).
//!
//! Someone joined, left, was added or removed; a channel was renamed, given a topic or
//! description, archived; a drop-in started or ended. A notice is an ordinary message whose author
//! is the person who did it, carrying a [`Notice`] beside its document. Only a drop-in's notice
//! counts as unread or notifies ([`Notice::counts_as_unread`]); the others are quiet. The document is the
//! notice in words ([`Notice::describe`], with the names as they were), for clients that do not
//! know notices, for search, and for desktop notifications; clients that do draw the notice from
//! the [`Notice`] with the names as they are now.
//!
//! Only the server posts notices. Reading is lenient: a kind this build does not know becomes
//! [`Notice::Unsupported`], drawn from the document.

use serde::{Deserialize, Serialize};

use crate::{Timestamp, UserId};

/// What happened. The message's author is who did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    /// The author joined the conversation.
    Joined,
    /// The author joined the workspace (posted in its default channel, which they joined with it).
    JoinedWorkspace,
    /// The author left the conversation.
    Left,
    /// The author added these people, in the order they were added.
    Added { users: Vec<UserId> },
    /// The author removed this person.
    Removed { user: UserId },
    /// The author renamed the channel (names without the `#`).
    Renamed { from: String, to: String },
    /// The author set the topic (`None`: cleared it).
    TopicChanged { topic: Option<String> },
    /// The author set the description (`None`: cleared it).
    DescriptionChanged { description: Option<String> },
    /// The author archived the channel.
    Archived,
    /// The author reopened the archived channel.
    Unarchived,
    /// The author started a drop-in. The server updates the notice as people come and go, and
    /// when it ends.
    DropIn(DropInNotice),
    /// A kind from a newer server: draw the document.
    #[serde(other)]
    Unsupported,
}

/// A drop-in, as its notice tells it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropInNotice {
    pub started_at: Timestamp,
    /// When the last person left; `None` while it goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    /// Who is in it now, in the order they joined; empty once it ended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<UserId>,
    /// Everyone who was in it at some point, in the order they first joined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joined: Vec<UserId>,
}

impl DropInNotice {
    pub fn is_live(&self) -> bool {
        self.ended_at.is_none()
    }

    /// Whole minutes from the start to the end (or to `now`, while it goes on), at least 1.
    pub fn minutes(&self, now: Timestamp) -> i64 {
        let end = self.ended_at.unwrap_or(now).as_offset_date_time();
        let seconds = (end - self.started_at.as_offset_date_time()).whole_seconds();
        (seconds / 60).max(1)
    }
}

/// How long a duration reads: "1 min", "45 min", "1 h", "2 h 5 min".
pub fn duration_words(minutes: i64) -> String {
    let minutes = minutes.max(1);
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// "Ada", "Ada and Bo", "Ada, Bo and Cy", "Ada, Bo and 3 others".
pub fn list_words(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second} and {third}"),
        [first, second, rest @ ..] => format!("{first}, {second} and {} others", rest.len()),
    }
}

impl Notice {
    /// The notice in words, without its author (who is shown before it): "joined #design", "added
    /// Ada and Bo", "started a drop-in". `place` is the conversation as it reads in a sentence
    /// ("#design", or "the conversation"); `name` names a person.
    pub fn describe(&self, place: &str, name: &dyn Fn(UserId) -> String) -> String {
        match self {
            Notice::Joined => format!("joined {place}"),
            Notice::JoinedWorkspace => "joined the workspace".into(),
            Notice::Left => format!("left {place}"),
            Notice::Added { users } => {
                let names: Vec<String> = users.iter().map(|user| name(*user)).collect();
                format!("added {} to {place}", list_words(&names))
            }
            Notice::Removed { user } => format!("removed {} from {place}", name(*user)),
            Notice::Renamed { from, to } => format!("renamed the channel from #{from} to #{to}"),
            Notice::TopicChanged { topic: Some(topic) } => format!("set the topic: {topic}"),
            Notice::TopicChanged { topic: None } => "cleared the topic".into(),
            Notice::DescriptionChanged {
                description: Some(description),
            } => format!("set the description: {description}"),
            Notice::DescriptionChanged { description: None } => "cleared the description".into(),
            Notice::Archived => format!("archived {place}"),
            Notice::Unarchived => format!("reopened {place}"),
            Notice::DropIn(drop_in) => match drop_in.ended_at {
                None => "started a drop-in".into(),
                Some(_) => format!(
                    "started a drop-in, which ended after {}",
                    duration_words(drop_in.minutes(Timestamp::now()))
                ),
            },
            Notice::Unsupported => String::new(),
        }
    }

    /// Whether the notice counts as unread (and may notify): only a drop-in starting, which
    /// someone may be waiting in. Joins, leaves, and changes to a channel are quiet.
    pub fn counts_as_unread(&self) -> bool {
        matches!(self, Notice::DropIn(_))
    }

    pub fn as_drop_in(&self) -> Option<&DropInNotice> {
        match self {
            Notice::DropIn(drop_in) => Some(drop_in),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_offset_date_time(
            time::OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).unwrap(),
        )
    }

    #[test]
    fn notices_round_trip_and_unknown_kinds_are_unsupported() {
        let ada = UserId::generate();
        let notices = [
            Notice::Joined,
            Notice::Added { users: vec![ada] },
            Notice::TopicChanged { topic: None },
            Notice::DropIn(DropInNotice {
                started_at: at(0),
                ended_at: None,
                participants: vec![ada],
                joined: vec![ada],
            }),
        ];
        for notice in notices {
            let json = serde_json::to_string(&notice).unwrap();
            assert_eq!(serde_json::from_str::<Notice>(&json).unwrap(), notice);
        }
        assert_eq!(
            serde_json::to_value(Notice::Joined).unwrap(),
            serde_json::json!({"kind": "joined"})
        );
        let newer: Notice = serde_json::from_str(r#"{"kind": "pinned", "message": "x"}"#).unwrap();
        assert_eq!(newer, Notice::Unsupported);
    }

    #[test]
    fn notices_read_as_sentences() {
        let names = |user: UserId| format!("person {}", &user.to_string()[..4]);
        let ada = UserId::generate();
        let bo = UserId::generate();
        let name = |user: UserId| {
            if user == ada {
                "Ada".to_owned()
            } else if user == bo {
                "Bo".to_owned()
            } else {
                names(user)
            }
        };
        assert_eq!(Notice::Joined.describe("#design", &name), "joined #design");
        assert_eq!(
            Notice::Added {
                users: vec![ada, bo]
            }
            .describe("#design", &name),
            "added Ada and Bo to #design"
        );
        assert_eq!(
            Notice::Removed { user: bo }.describe("#design", &name),
            "removed Bo from #design"
        );
        assert_eq!(
            Notice::Renamed {
                from: "old".into(),
                to: "new".into()
            }
            .describe("#new", &name),
            "renamed the channel from #old to #new"
        );
        let ended = Notice::DropIn(DropInNotice {
            started_at: at(0),
            ended_at: Some(at(23 * 60 + 10)),
            participants: vec![],
            joined: vec![ada, bo],
        });
        assert_eq!(
            ended.describe("the conversation", &name),
            "started a drop-in, which ended after 23 min"
        );
    }

    #[test]
    fn durations_and_lists_read_naturally() {
        assert_eq!(duration_words(0), "1 min");
        assert_eq!(duration_words(45), "45 min");
        assert_eq!(duration_words(60), "1 h");
        assert_eq!(duration_words(125), "2 h 5 min");
        let names: Vec<String> = ["A", "B", "C", "D", "E"].map(String::from).to_vec();
        assert_eq!(list_words(&names[..1]), "A");
        assert_eq!(list_words(&names[..2]), "A and B");
        assert_eq!(list_words(&names[..3]), "A, B and C");
        assert_eq!(list_words(&names), "A, B and 3 others");
    }
}
