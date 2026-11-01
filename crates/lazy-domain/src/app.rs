//! Apps with bot tokens (D35), shared by the server and clients: what a token may do.
//!
//! An app is a bot a workspace admin creates in Integrations. Its token lets a program act as the
//! bot through Lazy's bot API, within the app's scopes.

use serde::{Deserialize, Serialize};

/// What an app's token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AppScope {
    /// Post messages (and thread replies) in the channels the bot is in.
    #[serde(rename = "chat:write")]
    ChatWrite,
    /// List the channels the bot is in.
    #[serde(rename = "channels:read")]
    ChannelsRead,
    /// Join public channels by itself.
    #[serde(rename = "channels:join")]
    ChannelsJoin,
    /// A scope this version does not know (sent by a newer server).
    #[serde(other)]
    Unknown,
}

impl AppScope {
    /// Every scope an app can be given, in the order they are shown.
    pub const ALL: [AppScope; 3] = [
        AppScope::ChatWrite,
        AppScope::ChannelsRead,
        AppScope::ChannelsJoin,
    ];

    /// What a new app gets unless the admin chooses.
    pub const DEFAULT: [AppScope; 2] = [AppScope::ChatWrite, AppScope::ChannelsRead];

    /// The stored name, as in Slack (`chat:write`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatWrite => "chat:write",
            Self::ChannelsRead => "channels:read",
            Self::ChannelsJoin => "channels:join",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == name)
    }

    /// What it allows, for the app's screen.
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatWrite => "Post messages in its channels",
            Self::ChannelsRead => "See which channels it is in",
            Self::ChannelsJoin => "Join public channels by itself",
            Self::Unknown => "Something this version does not know",
        }
    }
}

/// The prefix of every bot token, so one is recognisable (in a leak scanner, say).
pub const BOT_TOKEN_PREFIX: &str = "lzb_";

/// Whether `name` can be a slash command's name (without its `/`): 1 to 32 lower-case letters,
/// digits, `-` and `_`, starting with a letter or digit.
pub fn is_command_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// A composer text read as a slash command: `/name` and the rest (trimmed), or `None` if it is
/// not shaped like one. Whether the command exists is for the caller to know.
pub fn parse_command(text: &str) -> Option<(String, String)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = rest[..end].to_ascii_lowercase();
    is_command_name(&name).then(|| (name, rest[end..].trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_parsed_from_the_composer() {
        assert_eq!(
            parse_command("/deploy  web prod\n"),
            Some(("deploy".into(), "web prod".into()))
        );
        assert_eq!(
            parse_command(" /Standup"),
            Some(("standup".into(), String::new()))
        );
        assert_eq!(parse_command("/"), None);
        assert_eq!(parse_command("// comment"), None);
        assert_eq!(parse_command("not /a command"), None);
        assert_eq!(parse_command("/usr/bin/env"), None);
        assert!(is_command_name("a1_b-c"));
        assert!(!is_command_name("-x") && !is_command_name(&"x".repeat(33)));
    }

    #[test]
    fn scopes_use_slack_names_and_tolerate_new_ones() {
        assert_eq!(
            serde_json::to_string(&AppScope::ChatWrite).unwrap(),
            "\"chat:write\""
        );
        for scope in AppScope::ALL {
            assert_eq!(AppScope::from_name(scope.as_str()), Some(scope));
        }
        let parsed: AppScope = serde_json::from_str("\"files:write\"").unwrap();
        assert_eq!(parsed, AppScope::Unknown);
    }
}
