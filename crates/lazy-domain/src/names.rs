//! Validated names. Parsing is the only way to construct these types, so a value in hand is
//! known to satisfy the rules, whether it came from a request body or from storage.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("{what} must be at least {min} characters")]
    TooShort { what: &'static str, min: usize },
    #[error("{what} must be at most {max} characters")]
    TooLong { what: &'static str, max: usize },
    #[error("{what} contains a character that is not allowed: {found:?}")]
    InvalidCharacter { what: &'static str, found: char },
    #[error("{what} must start with a letter or digit")]
    InvalidStart { what: &'static str },
}

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = NameError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_newtype!(
    /// A sign-in name: 3 to 32 ASCII letters, digits, `.`, `_`, or `-`, starting with a letter
    /// or digit. Compared case-insensitively by storing the lowercase form.
    Username
);

impl Username {
    pub const MIN_LEN: usize = 3;
    pub const MAX_LEN: usize = 32;

    pub fn parse(input: &str) -> Result<Self, NameError> {
        let lowered = input.to_ascii_lowercase();
        validate_slug(
            "username",
            &lowered,
            Self::MIN_LEN,
            Self::MAX_LEN,
            &['.', '_', '-'],
        )?;
        Ok(Self(lowered))
    }
}

string_newtype!(
    /// The name a user group is mentioned by: a mention handle (2 to 32 lowercase ASCII letters,
    /// digits, `.`, `_`, or `-`, starting and ending with a letter or digit) that is not one of
    /// the reserved handles (`channel`, `here`, `everyone`).
    GroupHandle
);

impl GroupHandle {
    pub fn parse(input: &str) -> Result<Self, NameError> {
        let lowered = input.to_ascii_lowercase();
        let count = lowered.chars().count();
        if count < crate::mention::MIN_HANDLE_LEN {
            return Err(NameError::TooShort {
                what: "group handle",
                min: crate::mention::MIN_HANDLE_LEN,
            });
        }
        if count > crate::mention::MAX_HANDLE_LEN {
            return Err(NameError::TooLong {
                what: "group handle",
                max: crate::mention::MAX_HANDLE_LEN,
            });
        }
        if let Some(found) = lowered.chars().find(|c| {
            !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        }) {
            return Err(NameError::InvalidCharacter {
                what: "group handle",
                found,
            });
        }
        if !crate::mention::handle_is_valid(&lowered) {
            return Err(NameError::InvalidStart {
                what: "group handle",
            });
        }
        if crate::mention::handle_is_reserved(&lowered) {
            return Err(NameError::InvalidCharacter {
                what: "group handle (reserved)",
                found: '@',
            });
        }
        Ok(Self(lowered))
    }
}

string_newtype!(
    /// The name of a custom emoji, as written between colons: 2 to 32 lowercase ASCII letters,
    /// digits, and underscores, starting and ending with a letter or digit, and not a standard
    /// emoji shortcode.
    EmojiName
);

impl EmojiName {
    pub fn parse(input: &str) -> Result<Self, NameError> {
        let lowered = input.to_ascii_lowercase();
        let count = lowered.chars().count();
        if count < 2 {
            return Err(NameError::TooShort {
                what: "emoji name",
                min: 2,
            });
        }
        if count > 32 {
            return Err(NameError::TooLong {
                what: "emoji name",
                max: 32,
            });
        }
        if let Some(found) = lowered
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_'))
        {
            return Err(NameError::InvalidCharacter {
                what: "emoji name",
                found,
            });
        }
        if !crate::emoji::custom_name_is_valid(&lowered) {
            // Starts or ends with an underscore, or is already a standard shortcode.
            return Err(NameError::InvalidStart { what: "emoji name" });
        }
        Ok(Self(lowered))
    }
}

string_newtype!(
    /// How a person appears in conversations: 1 to 64 characters after trimming, with no
    /// control characters.
    DisplayName
);

impl DisplayName {
    pub const MAX_LEN: usize = 64;

    pub fn parse(input: &str) -> Result<Self, NameError> {
        let trimmed = input.trim();
        let count = trimmed.chars().count();
        if count == 0 {
            return Err(NameError::TooShort {
                what: "display name",
                min: 1,
            });
        }
        if count > Self::MAX_LEN {
            return Err(NameError::TooLong {
                what: "display name",
                max: Self::MAX_LEN,
            });
        }
        if let Some(found) = trimmed.chars().find(|c| c.is_control()) {
            return Err(NameError::InvalidCharacter {
                what: "display name",
                found,
            });
        }
        Ok(Self(trimmed.to_owned()))
    }
}

string_newtype!(
    /// The name of a workspace: 1 to 64 characters after trimming, with no control characters.
    WorkspaceName
);

impl WorkspaceName {
    pub const MAX_LEN: usize = 64;

    pub fn parse(input: &str) -> Result<Self, NameError> {
        let trimmed = input.trim();
        let count = trimmed.chars().count();
        if count == 0 {
            return Err(NameError::TooShort {
                what: "workspace name",
                min: 1,
            });
        }
        if count > Self::MAX_LEN {
            return Err(NameError::TooLong {
                what: "workspace name",
                max: Self::MAX_LEN,
            });
        }
        if let Some(found) = trimmed.chars().find(|c| c.is_control()) {
            return Err(NameError::InvalidCharacter {
                what: "workspace name",
                found,
            });
        }
        Ok(Self(trimmed.to_owned()))
    }
}

string_newtype!(
    /// A channel name: 1 to 80 lowercase ASCII letters, digits, `_`, or `-`, starting with a
    /// letter or digit.
    ChannelName
);

impl ChannelName {
    pub const MAX_LEN: usize = 80;

    pub fn parse(input: &str) -> Result<Self, NameError> {
        validate_slug("channel name", input, 1, Self::MAX_LEN, &['_', '-'])?;
        if let Some(found) = input.chars().find(|c| c.is_ascii_uppercase()) {
            return Err(NameError::InvalidCharacter {
                what: "channel name",
                found,
            });
        }
        Ok(Self(input.to_owned()))
    }
}

fn validate_slug(
    what: &'static str,
    value: &str,
    min: usize,
    max: usize,
    extra: &[char],
) -> Result<(), NameError> {
    let count = value.chars().count();
    if count < min {
        return Err(NameError::TooShort { what, min });
    }
    if count > max {
        return Err(NameError::TooLong { what, max });
    }
    if let Some(found) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || extra.contains(c)))
    {
        return Err(NameError::InvalidCharacter { what, found });
    }
    if !value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err(NameError::InvalidStart { what });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn group_handles_follow_the_mention_rules() {
        use super::GroupHandle;
        assert_eq!(
            GroupHandle::parse("Design-Team").unwrap().as_str(),
            "design-team"
        );
        for bad in [
            "", "a", "-a", "a-", "a b", "é", "channel", "HERE", "everyone",
        ] {
            assert!(GroupHandle::parse(bad).is_err(), "{bad:?}");
        }
        assert!(GroupHandle::parse(&"a".repeat(33)).is_err());
        assert!(serde_json::from_str::<GroupHandle>("\"channel\"").is_err());
    }

    use super::*;

    #[test]
    fn usernames_are_lowercased() {
        assert_eq!(Username::parse("Jason.K").unwrap().as_str(), "jason.k");
    }

    #[test]
    fn usernames_reject_bad_input() {
        assert!(matches!(
            Username::parse("ab"),
            Err(NameError::TooShort { .. })
        ));
        assert!(matches!(
            Username::parse("has space"),
            Err(NameError::InvalidCharacter { .. })
        ));
        assert!(matches!(
            Username::parse("-leading"),
            Err(NameError::InvalidStart { .. })
        ));
        assert!(matches!(
            Username::parse(&"a".repeat(33)),
            Err(NameError::TooLong { .. })
        ));
    }

    #[test]
    fn display_names_are_trimmed_and_unicode_aware() {
        assert_eq!(DisplayName::parse("  Zoë 🎉 ").unwrap().as_str(), "Zoë 🎉");
        assert!(DisplayName::parse("   ").is_err());
        assert!(DisplayName::parse("bad\u{7}name").is_err());
        assert!(DisplayName::parse(&"é".repeat(65)).is_err());
        assert!(DisplayName::parse(&"é".repeat(64)).is_ok());
    }

    #[test]
    fn channel_names_must_be_lowercase_slugs() {
        assert!(ChannelName::parse("general").is_ok());
        assert!(ChannelName::parse("dev-ops_2").is_ok());
        assert!(ChannelName::parse("General").is_err());
        assert!(ChannelName::parse("has space").is_err());
        assert!(ChannelName::parse("").is_err());
    }

    #[test]
    fn deserialization_validates() {
        assert!(serde_json::from_str::<Username>("\"x\"").is_err());
        assert!(serde_json::from_str::<Username>("\"valid_name\"").is_ok());
    }
}
