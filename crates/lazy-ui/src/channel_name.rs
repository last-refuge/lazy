//! Helping people type channel names: normalize free text into a valid slug and explain what
//! changed. The server and the client core still validate; this only guides.

use lazy_domain::ChannelName;

/// What the Create channel dialog shows under the name box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameGuidance {
    /// Nothing typed yet.
    Empty,
    /// The text is already a valid name.
    Valid,
    /// The text is not valid as typed, but this slug would be; the dialog shows it as a preview
    /// and uses it when the person submits.
    WillBecome(String),
    /// Nothing usable remains after cleaning, so the person must type something else.
    Unusable,
}

/// Lowercases, turns runs of spaces and punctuation into single `-`, drops other characters, and
/// trims separators from the ends. May return an empty string.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut pending_separator = false;
    for ch in input.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            if pending_separator && !out.is_empty() {
                out.push('-');
            }
            pending_separator = false;
            out.push(lower);
        } else if lower == '-' || lower == '_' {
            // Keep an explicit separator as typed (but never leading, and not doubled).
            if !out.is_empty() && !out.ends_with(['-', '_']) {
                out.push(lower);
            }
            pending_separator = false;
        } else if ch.is_whitespace() || ch.is_ascii_punctuation() {
            pending_separator = true;
        }
        // Anything else (emoji, accented letters) is dropped.
    }
    while out.ends_with(['-', '_']) {
        out.pop();
    }
    out.chars()
        .take(ChannelName::MAX_LEN)
        .collect::<String>()
        .trim_end_matches(['-', '_'])
        .to_owned()
}

/// Describes `input` for the dialog.
pub fn guidance(input: &str) -> NameGuidance {
    if input.trim().is_empty() {
        return NameGuidance::Empty;
    }
    if ChannelName::parse(input).is_ok() {
        return NameGuidance::Valid;
    }
    let slug = slugify(input);
    if ChannelName::parse(&slug).is_ok() {
        NameGuidance::WillBecome(slug)
    } else {
        NameGuidance::Unusable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_and_punctuation_become_single_dashes() {
        assert_eq!(slugify("Design Team!"), "design-team");
        assert_eq!(slugify("  Q4   planning / 2026 "), "q4-planning-2026");
        assert_eq!(slugify("a--b"), "a-b");
        assert_eq!(slugify("snake_case Name"), "snake_case-name");
    }

    #[test]
    fn unusable_characters_are_dropped_and_ends_are_trimmed() {
        assert_eq!(slugify("🎉 party 🎉"), "party");
        assert_eq!(slugify("café"), "caf");
        assert_eq!(slugify("---x---"), "x");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn long_names_are_cut_to_the_limit_without_a_trailing_separator() {
        let long = format!("{}-{}", "a".repeat(79), "b".repeat(10));
        let slug = slugify(&long);
        assert!(slug.chars().count() <= ChannelName::MAX_LEN);
        assert!(!slug.ends_with('-'));
        assert!(ChannelName::parse(&slug).is_ok());
    }

    #[test]
    fn guidance_distinguishes_the_four_cases() {
        assert_eq!(guidance(""), NameGuidance::Empty);
        assert_eq!(guidance("   "), NameGuidance::Empty);
        assert_eq!(guidance("design"), NameGuidance::Valid);
        assert_eq!(
            guidance("Design Team"),
            NameGuidance::WillBecome("design-team".into())
        );
        assert_eq!(guidance("🎉"), NameGuidance::Unusable);
    }

    #[test]
    fn every_slug_is_a_valid_channel_name() {
        for input in ["Hello World", "x", "9 lives", "a_b", "Mixed-CASE_name 2"] {
            let slug = slugify(input);
            assert!(ChannelName::parse(&slug).is_ok(), "{input:?} -> {slug:?}");
        }
    }
}
