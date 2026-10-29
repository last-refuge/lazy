//! The pinned Unicode emoji dataset: lookup, search, skin tones, shortcodes, and the value a
//! reaction carries.
//!
//! The data comes from the `emojis` crate (Unicode CLDR names and GitHub shortcodes), pinned in
//! the workspace manifest, so every client and the server agree on which emoji exist. Search
//! keywords are the emoji's name and its shortcodes; the dataset has no further synonyms.

use std::borrow::Cow;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub use emojis::{Emoji, Group, SkinTone};

/// Most distinct emoji one message can carry as reactions.
pub const MAX_REACTION_KINDS: usize = 50;

/// Finds an emoji by its glyph. A glyph typed or pasted without its emoji variation selector
/// (`❤` for `❤️`), or with a stray one, still finds its emoji.
pub fn lookup(text: &str) -> Option<&'static Emoji> {
    emojis::get(text)
        .or_else(|| emojis::get(text.trim_end_matches('\u{fe0f}')))
        .or_else(|| emojis::get(&format!("{text}\u{fe0f}")))
}

/// Finds an emoji by a shortcode, with or without its colons: `tada`, `:tada:`.
pub fn by_shortcode(shortcode: &str) -> Option<&'static Emoji> {
    emojis::get_by_shortcode(shortcode.trim_matches(':'))
}

/// The emoji groups in the order a picker shows them.
pub fn groups() -> impl Iterator<Item = Group> {
    Group::iter()
}

/// The emoji of one group, in picker order, with each skin-toned family shown once (as its
/// default). Use [`tones`] to expand one.
pub fn in_group(group: Group) -> impl Iterator<Item = &'static Emoji> {
    group.emojis().filter(|emoji| {
        emoji
            .skin_tone()
            .is_none_or(|tone| tone == SkinTone::Default)
    })
}

/// Every variant of `emoji` across skin tones, default first; just the emoji itself when it has
/// none.
pub fn tones(emoji: &'static Emoji) -> Vec<&'static Emoji> {
    match emoji.skin_tones() {
        Some(tones) => tones.collect(),
        None => vec![emoji],
    }
}

/// `emoji` in `tone`, or `emoji` itself when it has no skin tones.
pub fn with_tone(emoji: &'static Emoji, tone: SkinTone) -> &'static Emoji {
    emoji.with_skin_tone(tone).unwrap_or(emoji)
}

/// A skin tone as a stable number for saving a preference: 0 is the default (yellow), 1 to 5 run
/// from light to dark. A preference is one of these six; two-person emoji with two different
/// tones exist in the dataset ([`tones`] lists them) but are not a setting.
pub fn skin_tone_index(tone: SkinTone) -> u8 {
    match tone {
        SkinTone::Default => 0,
        SkinTone::Light => 1,
        SkinTone::MediumLight => 2,
        SkinTone::Medium => 3,
        SkinTone::MediumDark => 4,
        SkinTone::Dark => 5,
        // The mixed tones of two-person emoji are not a preference.
        _ => 0,
    }
}

/// The inverse of [`skin_tone_index`]; anything out of range is the default tone.
pub fn skin_tone_from_index(index: u8) -> SkinTone {
    match index {
        1 => SkinTone::Light,
        2 => SkinTone::MediumLight,
        3 => SkinTone::Medium,
        4 => SkinTone::MediumDark,
        5 => SkinTone::Dark,
        _ => SkinTone::Default,
    }
}

struct Entry {
    emoji: &'static Emoji,
    name: String,
    /// Words of the name and of every shortcode, lowercase.
    words: Vec<String>,
}

fn entries() -> &'static [Entry] {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        Group::iter()
            .flat_map(in_group)
            .map(|emoji| {
                let name = emoji.name().to_lowercase();
                let mut words: Vec<String> = name
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .map(str::to_owned)
                    .collect();
                for shortcode in emoji.shortcodes() {
                    words.extend(
                        shortcode
                            .to_lowercase()
                            .split(|c: char| !c.is_alphanumeric())
                            .filter(|w| !w.is_empty())
                            .map(str::to_owned),
                    );
                }
                Entry { emoji, name, words }
            })
            .collect()
    })
}

/// Emoji matching `query`, best first, at most `limit`.
///
/// Every word of the query must start a word of the emoji's name or shortcodes (`thumb up`
/// finds 👍). Ranking: an exact shortcode, then a shortcode that starts with the query, then a
/// name that starts with it, then the rest, shorter names before longer, ties in picker order.
/// A leading `:` is ignored. Skin-tone variants are not listed separately; see [`tones`].
pub fn search(query: &str, limit: usize) -> Vec<&'static Emoji> {
    let query = query.trim().trim_start_matches(':').to_lowercase();
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, usize, &'static Emoji)> = Vec::new();
    for (position, entry) in entries().iter().enumerate() {
        let matches_every_token = tokens
            .iter()
            .all(|token| entry.words.iter().any(|word| word.starts_with(token)));
        if !matches_every_token {
            continue;
        }
        let whole = tokens.join("_");
        let rank = if entry.emoji.shortcodes().any(|s| s == whole) {
            0
        } else if entry.emoji.shortcodes().any(|s| s.starts_with(&whole)) {
            1
        } else if entry.name.starts_with(&query) {
            2
        } else {
            3
        };
        hits.push((rank, entry.name.len(), position, entry.emoji));
    }
    hits.sort_by_key(|(rank, len, position, _)| (*rank, *len, *position));
    hits.into_iter()
        .take(limit)
        .map(|(_, _, _, emoji)| emoji)
        .collect()
}

/// One shortcode that completes a prefix, with its emoji.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcodeMatch {
    pub shortcode: &'static str,
    pub emoji: &'static Emoji,
}

/// Shortcodes starting with `prefix` (no colon), for `:tad` completion: the exact shortcode
/// first, then shorter ones before longer, at most `limit`, one entry per emoji.
pub fn complete_shortcode(prefix: &str, limit: usize) -> Vec<ShortcodeMatch> {
    let prefix = prefix.trim_start_matches(':').to_lowercase();
    if prefix.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut found: Vec<(usize, usize, ShortcodeMatch)> = Vec::new();
    for (position, entry) in entries().iter().enumerate() {
        let best = entry
            .emoji
            .shortcodes()
            .filter(|shortcode| shortcode.starts_with(&prefix))
            .min_by_key(|shortcode| shortcode.len());
        if let Some(shortcode) = best {
            found.push((
                shortcode.len(),
                position,
                ShortcodeMatch {
                    shortcode,
                    emoji: entry.emoji,
                },
            ));
        }
    }
    found.sort_by_key(|(len, position, _)| (*len, *position));
    found
        .into_iter()
        .take(limit)
        .map(|(_, _, item)| item)
        .collect()
}

fn is_shortcode_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '+' | '-')
}

/// Replaces `:shortcode:` with its emoji in composer text.
///
/// A shortcode is replaced only where it stands alone: not directly after a letter or digit
/// (so `10:30:45` is untouched), not after a backslash (`\:tada:` stays literal), and not inside
/// a code span or code block. Unknown shortcodes are left as typed.
pub fn replace_shortcodes(text: &str) -> Cow<'_, str> {
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut in_fence = false;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start_matches(' ');
        let fence_ticks = trimmed.chars().take_while(|c| *c == '`').count();
        if fence_ticks >= 3 {
            in_fence = !in_fence;
            out.push_str(line);
        } else if in_fence {
            out.push_str(line);
        } else {
            changed |= replace_in_line(line, &mut out);
        }
        if lines.peek().is_some() {
            out.push('\n');
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

fn replace_in_line(line: &str, out: &mut String) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut changed = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {
                // A code span runs to a closing run of the same length on this line.
                let run = chars[i..].iter().take_while(|x| **x == '`').count();
                let mut j = i + run;
                let mut end = None;
                while j < chars.len() {
                    if chars[j] == '`' {
                        let closing = chars[j..].iter().take_while(|x| **x == '`').count();
                        if closing == run {
                            end = Some(j + closing);
                            break;
                        }
                        j += closing;
                    } else {
                        j += 1;
                    }
                }
                let stop = end.unwrap_or(i + run);
                out.extend(&chars[i..stop]);
                i = stop;
            }
            '\\' if i + 1 < chars.len() => {
                out.push('\\');
                out.push(chars[i + 1]);
                i += 2;
            }
            ':' if i == 0 || !chars[i - 1].is_alphanumeric() => {
                let name_len = chars[i + 1..]
                    .iter()
                    .take_while(|x| is_shortcode_char(**x))
                    .count();
                let closes = chars.get(i + 1 + name_len) == Some(&':');
                let name: String = chars[i + 1..i + 1 + name_len].iter().collect();
                match (name_len > 0 && closes)
                    .then(|| by_shortcode(&name))
                    .flatten()
                {
                    Some(emoji) => {
                        out.push_str(emoji.as_str());
                        changed = true;
                        i += name_len + 2;
                    }
                    None => {
                        out.push(':');
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    changed
}

/// Why text is not a usable reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not an emoji")]
pub struct NotAnEmoji;

/// Most custom emoji a workspace can have (deleted ones keep their name and do not count).
pub const MAX_CUSTOM_EMOJI: usize = 500;
/// Largest custom emoji image that may be uploaded, in bytes.
pub const MAX_EMOJI_BYTES: u64 = 512 * 1024;
/// The long side of the image the server stores and serves for a custom emoji, in pixels.
pub const EMOJI_IMAGE_EDGE: u32 = 128;
/// Most pixels an uploaded emoji image may have before it is refused undecoded.
pub const MAX_EMOJI_SOURCE_PIXELS: u64 = 4 * 1024 * 1024;
/// Most custom emoji one message document may contain.
pub const MAX_EMOJI_PER_MESSAGE: usize = 100;
/// The text a custom emoji reaction is written as: `custom:` and the emoji's id.
const CUSTOM_PREFIX: &str = "custom:";

/// Whether `name` can be the name of a custom emoji, as written between colons: 2 to 32 lowercase
/// ASCII letters, digits, and underscores, starting and ending with a letter or digit.
///
/// A name that is already a standard emoji shortcode (`tada`) is refused too: it would be ambiguous
/// which one `:tada:` meant.
pub fn custom_name_is_valid(name: &str) -> bool {
    custom_name_is_well_formed(name) && by_shortcode(name).is_none()
}

/// The syntax of a custom emoji name alone, without the clash with standard shortcodes (which a
/// message document, written when the name was free, still has to be able to hold).
pub fn custom_name_is_well_formed(name: &str) -> bool {
    let count = name.chars().count();
    (2..=32).contains(&count)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.ends_with(|c: char| c.is_ascii_alphanumeric())
}

/// The emoji of a reaction: a standard emoji or a custom one of the workspace.
///
/// A standard emoji is one glyph from the dataset in its canonical spelling (with the variation
/// selector the dataset uses), so `❤` and `❤️` are the same reaction, and skin-toned variants are
/// distinct reactions. A custom emoji is named by its identity, never its name, so renaming it
/// does not disturb reactions. On the wire a standard one is its glyph and a custom one is
/// `custom:` and its id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ReactionEmoji {
    Unicode(&'static Emoji),
    Custom(crate::EmojiId),
}

impl ReactionEmoji {
    /// A standard glyph or `custom:` and an id. Anything else is not a reaction.
    pub fn parse(text: &str) -> Result<Self, NotAnEmoji> {
        if let Some(id) = text.strip_prefix(CUSTOM_PREFIX) {
            return id.parse().map(Self::Custom).map_err(|_| NotAnEmoji);
        }
        lookup(text).map(Self::Unicode).ok_or(NotAnEmoji)
    }

    pub fn custom(id: crate::EmojiId) -> Self {
        Self::Custom(id)
    }

    /// The standard emoji, or `None` for a custom one.
    pub fn unicode(&self) -> Option<&'static Emoji> {
        match self {
            Self::Unicode(emoji) => Some(emoji),
            Self::Custom(_) => None,
        }
    }

    /// The canonical glyph of a standard emoji, or `None` for a custom one.
    pub fn glyph(&self) -> Option<&'static str> {
        self.unicode().map(Emoji::as_str)
    }

    /// The identity of a custom emoji, or `None` for a standard one.
    pub fn custom_id(&self) -> Option<crate::EmojiId> {
        match self {
            Self::Custom(id) => Some(*id),
            Self::Unicode(_) => None,
        }
    }
}

impl std::fmt::Debug for ReactionEmoji {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReactionEmoji({self})")
    }
}

impl std::fmt::Display for ReactionEmoji {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unicode(emoji) => f.write_str(emoji.as_str()),
            Self::Custom(id) => write!(f, "{CUSTOM_PREFIX}{id}"),
        }
    }
}

impl TryFrom<String> for ReactionEmoji {
    type Error = NotAnEmoji;

    fn try_from(text: String) -> Result<Self, NotAnEmoji> {
        Self::parse(&text)
    }
}

impl From<ReactionEmoji> for String {
    fn from(emoji: ReactionEmoji) -> String {
        emoji.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(found: &[&'static Emoji]) -> Vec<&'static str> {
        found.iter().map(|e| e.as_str()).collect()
    }

    #[test]
    fn the_dataset_has_what_a_picker_needs() {
        assert!(emojis::iter().count() > 1_800);
        assert_eq!(groups().count(), 9);
        for group in groups() {
            assert!(in_group(group).count() > 10, "{group:?}");
        }
        // Skin-toned variants are folded into their default.
        assert!(
            in_group(Group::PeopleAndBody)
                .all(|e| e.skin_tone().is_none_or(|t| t == SkinTone::Default))
        );
    }

    #[test]
    fn lookup_is_forgiving_about_the_variation_selector() {
        let heart = lookup("❤️").unwrap();
        assert_eq!(lookup("❤"), Some(heart));
        assert_eq!(lookup("❤️\u{fe0f}"), Some(heart));
        assert_eq!(lookup("not an emoji"), None);
        assert_eq!(lookup(""), None);
        assert_eq!(lookup("👍👍"), None, "one glyph, not a run of them");
        assert_eq!(lookup("a"), None);
    }

    #[test]
    fn shortcodes_resolve_with_or_without_colons() {
        assert_eq!(by_shortcode("tada").map(|e| e.as_str()), Some("🎉"));
        assert_eq!(by_shortcode(":tada:").map(|e| e.as_str()), Some("🎉"));
        assert_eq!(by_shortcode("+1").map(|e| e.as_str()), Some("👍"));
        assert_eq!(by_shortcode("no_such_thing"), None);
    }

    #[test]
    fn skin_tones_expand_and_apply() {
        let thumbs = lookup("👍").unwrap();
        let all = tones(thumbs);
        assert_eq!(glyphs(&all), ["👍", "👍🏻", "👍🏼", "👍🏽", "👍🏾", "👍🏿"]);
        assert_eq!(with_tone(thumbs, SkinTone::Dark).as_str(), "👍🏿");
        let tada = lookup("🎉").unwrap();
        assert_eq!(tones(tada), vec![tada]);
        assert_eq!(with_tone(tada, SkinTone::Dark), tada);
    }

    #[test]
    fn skin_tone_indexes_round_trip_and_clamp() {
        for index in 0..=5 {
            assert_eq!(skin_tone_index(skin_tone_from_index(index)), index);
        }
        assert_eq!(skin_tone_from_index(6), SkinTone::Default);
        assert_eq!(skin_tone_from_index(255), SkinTone::Default);
    }

    #[test]
    fn search_ranks_exact_shortcodes_first() {
        assert_eq!(search("tada", 5)[0].as_str(), "🎉");
        assert_eq!(search(":tada", 5)[0].as_str(), "🎉");
        assert_eq!(search("  TADA ", 5)[0].as_str(), "🎉");
        // A prefix of the name or shortcode finds it, and each word of the query must match.
        assert!(glyphs(&search("thumb", 10)).contains(&"👍"));
        assert!(glyphs(&search("thumbs up", 10)).contains(&"👍"));
        assert!(!glyphs(&search("thumbs up", 10)).contains(&"👎"));
        assert!(glyphs(&search("grin", 20)).contains(&"😀"));
    }

    #[test]
    fn search_is_bounded_and_ignores_empty_queries() {
        assert!(search("", 10).is_empty());
        assert!(search("   ", 10).is_empty());
        assert!(search(":", 10).is_empty());
        assert!(search("heart", 0).is_empty());
        assert_eq!(search("heart", 3).len(), 3);
        assert!(search("zzzzzzzz", 10).is_empty());
        // No result repeats.
        let found = glyphs(&search("a", 200));
        let mut unique = found.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(found.len(), unique.len());
    }

    #[test]
    fn shortcode_completion_prefers_the_shortest_match() {
        let found = complete_shortcode("tad", 5);
        assert_eq!(found[0].shortcode, "tada");
        assert_eq!(found[0].emoji.as_str(), "🎉");
        assert!(complete_shortcode("", 5).is_empty());
        assert!(complete_shortcode("smil", 50).len() > 3);
        assert!(complete_shortcode("smil", 2).len() == 2);
        // One entry per emoji even when several shortcodes match.
        let thumbs = complete_shortcode("thumbs", 50);
        let mut glyphs: Vec<_> = thumbs.iter().map(|m| m.emoji.as_str()).collect();
        let before = glyphs.len();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(before, glyphs.len());
    }

    #[test]
    fn shortcodes_in_composer_text_become_emoji() {
        assert_eq!(replace_shortcodes("ship it :tada:"), "ship it 🎉");
        assert_eq!(replace_shortcodes(":tada::+1:"), "🎉👍");
        assert_eq!(replace_shortcodes("a :tada: b :tada: c"), "a 🎉 b 🎉 c");
        assert_eq!(replace_shortcodes("(:tada:)"), "(🎉)");
    }

    #[test]
    fn shortcodes_are_left_alone_where_they_are_not_shortcodes() {
        for text in [
            "10:30:45",
            "meeting at 3:tada:",
            "word:tada:",
            ":no_such_thing:",
            "::",
            ":tada",
            "tada:",
            ": tada :",
            "\\:tada:",
            "`:tada:`",
            "see ``:tada:`` here",
            "```\n:tada:\n```",
            "```rust\nlet x = \":tada:\";\n```",
            "no colons at all",
            "",
        ] {
            assert_eq!(replace_shortcodes(text), text, "{text:?}");
        }
    }

    #[test]
    fn code_protection_ends_with_the_code() {
        assert_eq!(replace_shortcodes("`a` :tada: `b`"), "`a` 🎉 `b`");
        assert_eq!(
            replace_shortcodes("```\n:tada:\n```\n:tada:"),
            "```\n:tada:\n```\n🎉"
        );
        // An unclosed backtick is not code.
        assert_eq!(replace_shortcodes("`oops :tada:"), "`oops 🎉");
    }

    #[test]
    fn reactions_use_the_canonical_glyph() {
        let heart = ReactionEmoji::parse("❤").unwrap();
        assert_eq!(heart, ReactionEmoji::parse("❤️").unwrap());
        assert_eq!(heart.glyph(), Some("❤️"));
        assert_eq!(ReactionEmoji::parse("hello"), Err(NotAnEmoji));
        assert_eq!(ReactionEmoji::parse(""), Err(NotAnEmoji));
        // Skin tones are different reactions.
        assert_ne!(
            ReactionEmoji::parse("👍").unwrap(),
            ReactionEmoji::parse("👍🏽").unwrap()
        );
    }

    #[test]
    fn custom_emoji_names_are_short_slugs_that_do_not_shadow_standard_shortcodes() {
        for good in ["party_parrot", "qz", "q1", "team_2024", "x_y_z"] {
            assert!(custom_name_is_valid(good), "{good}");
        }
        for bad in [
            "",
            "a",
            "_a",
            "a_",
            "Party",
            "has space",
            "dash-ed",
            "é",
            "tada",
            "thumbsup",
            "+1",
            &"a".repeat(33),
        ] {
            assert!(!custom_name_is_valid(bad), "{bad:?}");
        }
        // The syntax alone accepts a name that has since become a standard shortcode.
        assert!(custom_name_is_well_formed("tada"));
        assert!(!custom_name_is_well_formed("Tada"));
    }

    #[test]
    fn custom_reactions_are_named_by_identity() {
        let id = crate::EmojiId::generate();
        let custom = ReactionEmoji::custom(id);
        assert_eq!(custom.custom_id(), Some(id));
        assert_eq!(custom.glyph(), None);
        assert_eq!(custom.to_string(), format!("custom:{id}"));
        assert_eq!(ReactionEmoji::parse(&custom.to_string()), Ok(custom));
        let json = serde_json::to_string(&custom).unwrap();
        assert_eq!(json, format!("\"custom:{id}\""));
        assert_eq!(
            serde_json::from_str::<ReactionEmoji>(&json).unwrap(),
            custom
        );
        // Distinct from every standard emoji, and from another custom one.
        assert_ne!(custom, ReactionEmoji::parse("🎉").unwrap());
        assert_ne!(custom, ReactionEmoji::custom(crate::EmojiId::generate()));
        for bad in [
            "custom:",
            "custom:nope",
            "custom:1234",
            "Custom:00000000-0000-0000-0000-000000000000",
        ] {
            assert_eq!(ReactionEmoji::parse(bad), Err(NotAnEmoji), "{bad}");
        }
    }

    #[test]
    fn reactions_are_plain_strings_on_the_wire() {
        let emoji = ReactionEmoji::parse("🎉").unwrap();
        let json = serde_json::to_string(&emoji).unwrap();
        assert_eq!(json, "\"🎉\"");
        assert_eq!(serde_json::from_str::<ReactionEmoji>(&json).unwrap(), emoji);
        // An unvalidated string is refused while parsing, so a request cannot carry one.
        assert!(serde_json::from_str::<ReactionEmoji>("\"nope\"").is_err());
        // The unqualified form is accepted and becomes the canonical one.
        let parsed: ReactionEmoji = serde_json::from_str("\"❤\"").unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), "\"❤️\"");
    }
}
