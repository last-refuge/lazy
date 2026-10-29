//! Suggestions while typing: emoji shortcodes (`:tad` offers 🎉 `:tada:`, and the workspace's
//! custom emoji first: `:shi` offers its `:shipit:` picture) and mentions (`@al` offers Alice).
//!
//! Which text asks for suggestions is a pure function ([`trigger`]) so every rule is a unit test:
//! a colon and two or more shortcode characters, or an `@` and any handle characters, at the start
//! of a word, up to the caret, not inside code. [`Suggestions`] holds what is showing and which row
//! is highlighted, and works out from the live input whether to show anything, so a caret that
//! moves away closes the list without the host noticing. It never edits the input;
//! [`Suggestions::accept`] says what to replace and with what.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui::{AnyElement, App, ElementId, FontWeight, RenderImage, Window, div, prelude::*, px};
use lazy_domain::EmojiId;
use lazy_domain::emoji::{self, ShortcodeMatch};

use crate::emoji_font;
use crate::text_input::{SuggestionKey, TextInput};
use crate::theme::{Theme, avatar_color};

/// Most suggestions shown.
pub const MAX_SUGGESTIONS: usize = 8;
/// Fewest shortcode characters after the colon before anything is offered.
pub const MIN_PREFIX: usize = 2;

/// What is being completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// `:tad`, an emoji shortcode.
    Emoji,
    /// `@al`, a mention.
    Mention,
}

/// The shortcode or mention being typed at the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// Byte offset of the colon or `@`.
    pub start: usize,
    /// The characters after it, up to the caret.
    pub prefix: String,
}

fn is_shortcode_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// Whether `before` (the text up to a point) ends inside code: an open inline code span on the
/// last line, or an open fenced block.
pub fn ends_in_code(before: &str) -> bool {
    let mut fenced = false;
    let mut lines = before.split('\n').peekable();
    while let Some(line) = lines.next() {
        let is_last = lines.peek().is_none();
        let ticks = line.trim_start().chars().take_while(|c| *c == '`').count();
        if ticks >= 3 {
            if is_last {
                // The fence line itself is code text; so is everything inside a fence.
                return true;
            }
            fenced = !fenced;
            continue;
        }
        if is_last {
            return fenced || inline_code_is_open(line);
        }
    }
    false
}

/// Whether an inline code span is still open at the end of `line`: a run of backticks opens one,
/// and the next run of the same length closes it. A backslash escapes the character after it.
fn inline_code_is_open(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if open.is_none() => i += 2,
            '`' => {
                let run = chars[i..].iter().take_while(|c| **c == '`').count();
                match open {
                    None => open = Some(run),
                    Some(len) if len == run => open = None,
                    Some(_) => {}
                }
                i += run;
            }
            _ => i += 1,
        }
    }
    open.is_some()
}

/// The shortcode being typed just before `caret` (a byte offset in `text`), if any.
///
/// A colon opens one only at the start of a word: not directly after a letter or digit (so
/// `10:30` and `https://` offer nothing), not after a backslash or another colon, and not inside
/// code. It needs at least [`MIN_PREFIX`] shortcode characters after it.
pub fn emoji_trigger(text: &str, caret: usize) -> Option<Trigger> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let before = &text[..caret];
    let prefix_len = before
        .chars()
        .rev()
        .take_while(|c| is_shortcode_char(*c))
        .map(char::len_utf8)
        .sum::<usize>();
    let colon = caret - prefix_len;
    if prefix_len < MIN_PREFIX || !before[..colon].ends_with(':') {
        return None;
    }
    let start = colon - 1;
    let before_colon = before[..start].chars().next_back();
    if before_colon.is_some_and(|c| c.is_alphanumeric() || matches!(c, '\\' | ':')) {
        return None;
    }
    if ends_in_code(&before[..start]) {
        return None;
    }
    Some(Trigger {
        kind: TriggerKind::Emoji,
        start,
        prefix: before[colon..].to_owned(),
    })
}

/// The mention being typed just before `caret`, if any.
///
/// An `@` opens one only at the start of a word: not directly after a letter or digit (so
/// `bob@example.com` offers nothing), not after a backslash or another `@`, not inside code, and
/// not in a web address (`https://medium.com/@someone`). Any number of handle characters may
/// follow, none at all included: a bare `@` lists the first few people.
pub fn mention_trigger(text: &str, caret: usize) -> Option<Trigger> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let before = &text[..caret];
    let prefix_len = before
        .chars()
        .rev()
        .take_while(|c| lazy_domain::mention::is_handle_char(*c))
        .map(char::len_utf8)
        .sum::<usize>();
    let at = caret - prefix_len;
    if !before[..at].ends_with('@') {
        return None;
    }
    let start = at - 1;
    let before_at = &before[..start];
    if before_at
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '\\' | '@'))
    {
        return None;
    }
    // The word the `@` sits in: a web address is not a place to mention anyone.
    let word_start = before_at.rfind(char::is_whitespace).map_or(0, |i| {
        i + before_at[i..].chars().next().map_or(1, char::len_utf8)
    });
    if before_at[word_start..].contains("://") || ends_in_code(before_at) {
        return None;
    }
    Some(Trigger {
        kind: TriggerKind::Mention,
        start,
        prefix: before[at..].to_owned(),
    })
}

/// Whichever shortcode or mention is being typed at the caret.
pub fn trigger(text: &str, caret: usize) -> Option<Trigger> {
    let emoji = emoji_trigger(text, caret);
    let mention = mention_trigger(text, caret);
    match (emoji, mention) {
        (Some(e), Some(m)) => Some(if e.start > m.start { e } else { m }),
        (e, m) => e.or(m),
    }
}

/// What a candidate for a mention is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    User,
    Group,
    /// `@channel`.
    Channel,
}

/// One thing the person could mention, as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionChoice {
    /// What is inserted after the `@`.
    pub handle: String,
    /// The person's display name, or the group's name.
    pub name: String,
    pub kind: MentionKind,
    /// A person who is not in this conversation: naming them notifies no one.
    pub outside: bool,
    /// How many people are in a group.
    pub members: Option<usize>,
    /// A person's status, shown beside their name.
    pub status: Option<crate::status_mark::StatusMark>,
}

/// A custom emoji offered for what has been typed after a colon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomChoice {
    pub id: EmojiId,
    /// Its name, without colons.
    pub name: String,
}

/// Where the candidates for a suggestion list come from (the host asks the client core).
pub struct Sources<'a> {
    /// The people and groups for what has been typed after an `@`.
    pub mentions: &'a dyn Fn(&str) -> Vec<MentionChoice>,
    /// The workspace's custom emoji for what has been typed after a colon.
    pub custom_emoji: &'a dyn Fn(&str) -> Vec<CustomChoice>,
}

/// The decoded pictures of custom emoji, for the rows that show them.
pub type Pictures = HashMap<EmojiId, Arc<RenderImage>>;

/// One row of a suggestion list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Emoji(ShortcodeMatch),
    /// A custom emoji, offered before the standard ones.
    Custom(CustomChoice),
    Mention(MentionChoice),
}

impl Item {
    /// Identifies the row, to tell whether the list changed under the highlight.
    fn key(&self) -> String {
        match self {
            Item::Emoji(m) => format!(":{}", m.shortcode),
            Item::Custom(c) => format!(":{}", c.name),
            Item::Mention(m) => format!("@{}", m.handle),
        }
    }
}

/// What accepting a suggestion does to the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accept {
    /// The typed `:tad` or `@al`, to replace.
    pub replace: Range<usize>,
    /// What goes there: the emoji, or `@handle ` with a space after it.
    pub insert: String,
    /// The emoji chosen, so the host can remember it as recently used.
    pub emoji: Option<&'static str>,
    /// The custom emoji chosen, likewise.
    pub custom: Option<EmojiId>,
}

/// What the suggestion list shows right now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    start: usize,
    caret: usize,
    items: Vec<Item>,
}

/// The suggestion list's state: what is offered, which row is highlighted, and whether the person
/// dismissed it.
#[derive(Debug, Default, Clone)]
pub struct Suggestions {
    active: Option<Active>,
    selected: usize,
    /// The trigger the person dismissed the list for. It stays closed until a different one.
    dismissed: Option<usize>,
    /// What `active` was computed from, to skip recomputing for an unchanged input.
    seen: Option<(u64, usize, bool)>,
}

impl Suggestions {
    /// Brings the list up to date with the input. `sources` lists the candidates for what has been
    /// typed after a colon or an `@`. Returns whether it is showing.
    pub fn refresh(&mut self, input: &TextInput, sources: &Sources<'_>) -> bool {
        let key = (input.revision(), input.cursor(), input.is_composing());
        if self.seen != Some(key) {
            self.seen = Some(key);
            let previous = self
                .active
                .as_ref()
                .and_then(|a| a.items.first().map(Item::key));
            let found = if key.2 {
                // An input method is composing: the text is not final, so offer nothing.
                None
            } else {
                trigger(input.text(), key.1)
            };
            self.active = found
                .as_ref()
                .filter(|found| self.dismissed != Some(found.start))
                .map(|found| Active {
                    start: found.start,
                    caret: key.1,
                    items: match found.kind {
                        TriggerKind::Emoji => emoji_items(&found.prefix, sources),
                        TriggerKind::Mention => (sources.mentions)(&found.prefix)
                            .into_iter()
                            .take(MAX_SUGGESTIONS)
                            .map(Item::Mention)
                            .collect(),
                    },
                })
                .filter(|active| !active.items.is_empty());
            if found.is_none() {
                self.dismissed = None;
            }
            let now = self
                .active
                .as_ref()
                .and_then(|a| a.items.first().map(Item::key));
            if previous != now {
                self.selected = 0;
            }
        }
        self.active.is_some()
    }

    pub fn visible(&self) -> bool {
        self.active.is_some()
    }

    /// Forgets what the list was computed from, so the next refresh asks again. For when the
    /// people and groups behind a mention list changed while the text did not.
    pub fn invalidate(&mut self) {
        self.seen = None;
    }

    pub fn items(&self) -> &[Item] {
        self.active.as_ref().map_or(&[], |a| &a.items)
    }

    /// The custom emoji on show, whose pictures the host should have ready.
    pub fn custom_ids(&self) -> Vec<EmojiId> {
        self.items()
            .iter()
            .filter_map(|item| match item {
                Item::Custom(choice) => Some(choice.id),
                _ => None,
            })
            .collect()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Hides the list until the person starts another shortcode or mention.
    pub fn dismiss(&mut self) {
        if let Some(active) = self.active.take() {
            self.dismissed = Some(active.start);
        }
    }

    /// Highlights another row, wrapping at the ends.
    pub fn highlight(&mut self, key: SuggestionKey) {
        let count = self.items().len();
        if count == 0 {
            return;
        }
        match key {
            SuggestionKey::Up => self.selected = (self.selected + count - 1) % count,
            SuggestionKey::Down => self.selected = (self.selected + 1) % count,
            SuggestionKey::Accept => {}
        }
    }

    /// What taking row `index` does to the text.
    pub fn accept(&self, index: usize) -> Option<Accept> {
        let active = self.active.as_ref()?;
        let item = active.items.get(index)?;
        let replace = active.start..active.caret;
        Some(match item {
            Item::Emoji(m) => Accept {
                replace,
                insert: m.emoji.as_str().to_owned(),
                emoji: Some(m.emoji.as_str()),
                custom: None,
            },
            Item::Custom(c) => Accept {
                replace,
                insert: format!(":{}:", c.name),
                emoji: None,
                custom: Some(c.id),
            },
            Item::Mention(m) => Accept {
                replace,
                insert: format!("@{} ", m.handle),
                emoji: None,
                custom: None,
            },
        })
    }

    /// What taking the highlighted row does to the text.
    pub fn accept_selected(&self) -> Option<Accept> {
        self.accept(self.selected)
    }
}

/// The rows for `:prefix`: the workspace's custom emoji first, then the standard ones, together
/// at most [`MAX_SUGGESTIONS`].
fn emoji_items(prefix: &str, sources: &Sources<'_>) -> Vec<Item> {
    let mut items: Vec<Item> = (sources.custom_emoji)(prefix)
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(Item::Custom)
        .collect();
    let room = MAX_SUGGESTIONS - items.len();
    if room > 0 {
        items.extend(
            emoji::complete_shortcode(prefix, room)
                .into_iter()
                .map(Item::Emoji),
        );
    }
    items
}

/// Called with the index of a suggestion the pointer picked.
pub type PickSuggestion = std::rc::Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// A number to pick a stable avatar color for a handle.
fn color_seed(handle: &str) -> u128 {
    handle.bytes().fold(0xcbf29ce484222325u128, |hash, byte| {
        (hash ^ u128::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn emoji_row(item: &ShortcodeMatch, theme: &Theme) -> [AnyElement; 2] {
    let colors = theme.colors;
    [
        div()
            .w(px(24.))
            .text_size(px(18.))
            .child(emoji_font::glyphs(item.emoji.as_str().to_owned()))
            .into_any_element(),
        div()
            .text_size(theme.type_scale.body)
            .text_color(colors.text)
            .child(emoji_font::label(
                format!(":{}:", item.shortcode),
                colors.text,
                FontWeight::NORMAL,
            ))
            .into_any_element(),
    ]
}

fn custom_row(
    item: &CustomChoice,
    picture: Option<&Arc<RenderImage>>,
    theme: &Theme,
) -> [AnyElement; 2] {
    let colors = theme.colors;
    let leading = match picture {
        Some(image) => crate::emoji_images::square_picture(image, 24.0)
            .debug_selector(|| "suggestion-emoji-picture".into()),
        // Until the picture is ready (or when it cannot be fetched) a quiet square holds the place.
        None => div()
            .debug_selector(|| "suggestion-emoji-placeholder".into())
            .size(px(24.))
            .flex_none()
            .rounded(theme.radii.small)
            .bg(colors.surface_hover),
    };
    [
        leading.into_any_element(),
        div()
            .truncate()
            .text_size(theme.type_scale.body)
            .text_color(colors.text)
            .child(format!(":{}:", item.name))
            .into_any_element(),
    ]
}

fn mention_row(item: &MentionChoice, theme: &Theme) -> [AnyElement; 2] {
    let colors = theme.colors;
    let leading = match item.kind {
        MentionKind::User => div()
            .size(px(24.))
            .flex_none()
            .rounded(px(6.))
            .bg(avatar_color(color_seed(&item.handle)))
            .flex()
            .items_center()
            .justify_center()
            .text_size(theme.type_scale.small)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(gpui::white())
            .child(
                item.name
                    .chars()
                    .next()
                    .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().to_string()),
            ),
        MentionKind::Group => div()
            .size(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(16.))
            .child(emoji_font::glyphs("👥")),
        MentionKind::Channel => div()
            .size(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(16.))
            .child(emoji_font::glyphs("📢")),
    };
    let note: Option<String> = match (item.kind, item.outside, item.members) {
        (MentionKind::User, true, _) => Some("not in this channel".to_owned()),
        (MentionKind::Group, _, Some(1)) => Some("1 member".to_owned()),
        (MentionKind::Group, _, Some(n)) => Some(format!("{n} members")),
        _ => None,
    };
    let text = div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_baseline()
        .gap_2()
        .child(
            div()
                .truncate()
                .text_size(theme.type_scale.body)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if item.outside {
                    colors.text_muted
                } else {
                    colors.text
                })
                .child(item.name.clone()),
        )
        .children(item.status.as_ref().map(|mark| {
            crate::status_mark::view(
                ElementId::Name(format!("mention-status-{}", item.handle).into()),
                format!("mention-status-{}", item.handle),
                mark,
                theme.type_scale.small,
            )
        }))
        .child(
            div()
                .flex_none()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(format!("@{}", item.handle)),
        )
        .children(note.map(|note| {
            div()
                .flex_none()
                .ml_auto()
                .text_size(theme.type_scale.small)
                // Muted, not faint: this sits on the highlighted row too.
                .text_color(colors.text_muted)
                .child(note)
        }));
    [leading.into_any_element(), text.into_any_element()]
}

/// The suggestion list: one row per item, the highlighted one filled.
pub fn render_list(
    suggestions: &Suggestions,
    pictures: &Pictures,
    theme: &Theme,
    id: &'static str,
    on_pick: PickSuggestion,
) -> AnyElement {
    let colors = theme.colors;
    let mut list = div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .occlude()
        .flex()
        .flex_col()
        .p_1()
        .min_w(px(220.))
        .max_w(px(420.))
        .rounded(theme.radii.medium)
        .bg(colors.surface_raised)
        .border_1()
        .border_color(colors.border_strong)
        .shadow_lg();
    for (index, item) in suggestions.items().iter().enumerate() {
        let selected = index == suggestions.selected();
        let on_pick = on_pick.clone();
        let [leading, text] = match item {
            Item::Emoji(m) => emoji_row(m, theme),
            Item::Custom(c) => custom_row(c, pictures.get(&c.id), theme),
            Item::Mention(m) => mention_row(m, theme),
        };
        list = list.child(
            div()
                .id(ElementId::NamedInteger(id.into(), index as u64))
                .debug_selector(move || format!("{id}-{index}"))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(theme.radii.small)
                .cursor_pointer()
                .when(selected, |row| row.bg(colors.selection))
                .hover(|style| style.bg(colors.surface_hover))
                .on_click(move |_, window, cx| on_pick(index, window, cx))
                .child(leading)
                .child(text),
        );
    }
    list.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> Option<(usize, String)> {
        emoji_trigger(text, text.len()).map(|t| (t.start, t.prefix))
    }

    #[test]
    fn a_colon_and_two_characters_at_a_word_start_trigger() {
        assert_eq!(t(":ta"), Some((0, "ta".into())));
        assert_eq!(t("nice :tad"), Some((5, "tad".into())));
        assert_eq!(t("(:smi"), Some((1, "smi".into())), "after punctuation");
        assert_eq!(
            t("line one\n:thu"),
            Some((9, "thu".into())),
            "at the start of a line"
        );
        assert_eq!(t(":+1"), Some((0, "+1".into())));
        assert_eq!(t(":thumbs_up"), Some((0, "thumbs_up".into())));
    }

    #[test]
    fn too_few_characters_or_no_colon_does_not() {
        assert_eq!(t(":"), None);
        assert_eq!(t(":t"), None);
        assert_eq!(t("ta"), None);
        assert_eq!(t("hello"), None);
        assert_eq!(t(""), None);
    }

    #[test]
    fn a_colon_after_a_letter_or_digit_is_not_a_shortcode() {
        assert_eq!(t("10:30"), None);
        assert_eq!(t("https://example"), None);
        assert_eq!(t("a:tada"), None);
        assert_eq!(t("note:tada"), None);
        assert_eq!(t("::tada"), None, "a second colon");
        assert_eq!(t("\\:tada"), None, "escaped");
    }

    #[test]
    fn a_finished_shortcode_or_a_space_closes_it() {
        assert_eq!(t(":tada:"), None);
        assert_eq!(t(":tada "), None);
        assert_eq!(t(":tada x"), None);
    }

    #[test]
    fn code_never_triggers() {
        assert_eq!(t("`:tad"), None, "inside inline code");
        assert_eq!(t("``a`b :tad"), None, "a longer run stays open");
        assert_eq!(
            t("`done` :tad"),
            Some((7, "tad".into())),
            "after the code closed"
        );
        assert_eq!(t("```\n:tad"), None, "inside a fence");
        assert_eq!(
            t("```\ncode\n```\n:tad"),
            Some((13, "tad".into())),
            "after the fence closed"
        );
        assert_eq!(t("```rust :tad"), None, "on the fence line");
    }

    #[test]
    fn it_works_at_the_caret_not_the_end() {
        let text = ":tada and more";
        assert_eq!(
            emoji_trigger(text, 4).map(|t| (t.start, t.prefix)),
            Some((0, "tad".into()))
        );
        assert_eq!(emoji_trigger(text, 100), None, "a caret past the end");
        let accents = "é :tad";
        assert_eq!(
            emoji_trigger(accents, accents.len()).map(|t| t.start),
            Some("é ".len())
        );
        assert_eq!(emoji_trigger(accents, 1), None, "inside a character");
    }

    #[test]
    fn matches_come_from_the_shortcode_dataset() {
        let found = emoji::complete_shortcode("tad", MAX_SUGGESTIONS);
        assert_eq!(found.first().map(|m| m.shortcode), Some("tada"));
        assert!(found.len() <= MAX_SUGGESTIONS);
    }

    #[test]
    fn highlight_wraps_and_accept_replaces_the_typed_part() {
        let mut suggestions = Suggestions {
            active: Some(Active {
                start: 5,
                caret: 9,
                items: emoji::complete_shortcode("tad", 3)
                    .into_iter()
                    .map(Item::Emoji)
                    .collect(),
            }),
            ..Suggestions::default()
        };
        let count = suggestions.items().len();
        assert!(count >= 1);
        suggestions.highlight(SuggestionKey::Up);
        assert_eq!(
            suggestions.selected(),
            count - 1,
            "up from the first wraps to the last"
        );
        suggestions.highlight(SuggestionKey::Down);
        assert_eq!(suggestions.selected(), 0);
        let accept = suggestions.accept_selected().unwrap();
        assert_eq!(accept.replace, 5..9);
        assert_eq!(accept.insert, "🎉");
        assert_eq!(accept.emoji, Some("🎉"));
        assert_eq!(accept.custom, None);
        suggestions.dismiss();
        assert!(!suggestions.visible());
        assert_eq!(suggestions.accept_selected(), None);
    }

    // ---- Custom emoji ----

    fn custom_choice(name: &str) -> CustomChoice {
        CustomChoice {
            id: EmojiId::generate(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn custom_emoji_are_offered_before_the_standard_ones() {
        let shipit = custom_choice("shipit");
        let all = [shipit.clone()];
        let sources = Sources {
            mentions: &|_| Vec::new(),
            custom_emoji: &|prefix| {
                all.iter()
                    .filter(|c| c.name.starts_with(prefix))
                    .cloned()
                    .collect()
            },
        };
        let items = emoji_items("shi", &sources);
        assert_eq!(items.first(), Some(&Item::Custom(shipit)));
        assert!(
            items[1..].iter().all(|item| matches!(item, Item::Emoji(_))),
            "then the standard ones: {items:?}"
        );
        assert!(items.len() > 1 && items.len() <= MAX_SUGGESTIONS);
        assert_eq!(emoji_items("qxvq", &sources), vec![], "nothing matches");
    }

    #[test]
    fn many_custom_matches_fill_the_list_and_never_overflow_it() {
        let many: Vec<CustomChoice> = (0..20)
            .map(|n| custom_choice(&format!("blob_{n}")))
            .collect();
        let sources = Sources {
            mentions: &|_| Vec::new(),
            custom_emoji: &|_| many.clone(),
        };
        let items = emoji_items("blo", &sources);
        assert_eq!(items.len(), MAX_SUGGESTIONS);
        assert!(items.iter().all(|item| matches!(item, Item::Custom(_))));
    }

    #[test]
    fn accepting_a_custom_emoji_writes_its_name_between_colons_and_names_it_for_recents() {
        let shipit = custom_choice("shipit");
        let suggestions = Suggestions {
            active: Some(Active {
                start: 5,
                caret: 9,
                items: vec![Item::Custom(shipit.clone())],
            }),
            ..Suggestions::default()
        };
        let accept = suggestions.accept_selected().unwrap();
        assert_eq!(accept.replace, 5..9);
        assert_eq!(accept.insert, ":shipit:");
        assert_eq!(accept.custom, Some(shipit.id));
        assert_eq!(accept.emoji, None, "not a standard emoji");
        assert_eq!(suggestions.custom_ids(), vec![shipit.id]);
    }

    // ---- Mentions ----

    fn m(text: &str) -> Option<(usize, String)> {
        mention_trigger(text, text.len()).map(|t| (t.start, t.prefix))
    }

    #[test]
    fn an_at_sign_at_a_word_start_triggers_with_any_number_of_characters() {
        assert_eq!(m("@"), Some((0, String::new())));
        assert_eq!(m("@a"), Some((0, "a".into())));
        assert_eq!(m("hi @ali"), Some((3, "ali".into())));
        assert_eq!(m("(@bob"), Some((1, "bob".into())), "after punctuation");
        assert_eq!(
            m("one\n@zo"),
            Some((4, "zo".into())),
            "at the start of a line"
        );
        assert_eq!(m("@design-team"), Some((0, "design-team".into())));
        assert_eq!(m("@a.b_c-d"), Some((0, "a.b_c-d".into())));
        assert_eq!(
            m("@Ali"),
            Some((0, "Ali".into())),
            "case is the directory's business"
        );
    }

    #[test]
    fn an_at_sign_inside_a_word_or_an_address_does_not() {
        assert_eq!(m("bob@example"), None, "an email address");
        assert_eq!(m("a1@b"), None);
        assert_eq!(m("@@al"), None, "a second at sign");
        assert_eq!(m("\\@al"), None, "escaped");
        assert_eq!(
            m("see https://medium.com/@someone"),
            None,
            "in a web address"
        );
        assert_eq!(m("hello"), None);
        assert_eq!(m(""), None);
    }

    #[test]
    fn a_space_or_punctuation_after_the_name_ends_it() {
        assert_eq!(m("@alice "), None);
        assert_eq!(m("@alice,"), None);
        assert_eq!(m("@alice x"), None);
    }

    #[test]
    fn mentions_are_not_offered_in_code() {
        assert_eq!(m("`@al"), None, "inside inline code");
        assert_eq!(
            m("`done` @al"),
            Some((7, "al".into())),
            "after the code closed"
        );
        assert_eq!(m("```\n@al"), None, "inside a fence");
        assert_eq!(m("```\ncode\n```\n@al"), Some((13, "al".into())));
    }

    #[test]
    fn the_nearer_of_a_shortcode_and_a_mention_wins() {
        assert_eq!(
            trigger("@al", 3).map(|t| (t.kind, t.start)),
            Some((TriggerKind::Mention, 0))
        );
        assert_eq!(
            trigger(":tad", 4).map(|t| (t.kind, t.start)),
            Some((TriggerKind::Emoji, 0))
        );
        assert_eq!(
            trigger("a:b @", 5).map(|t| t.kind),
            Some(TriggerKind::Mention)
        );
    }

    fn choice(handle: &str, kind: MentionKind) -> MentionChoice {
        MentionChoice {
            handle: handle.into(),
            name: handle.to_uppercase(),
            kind,
            outside: false,
            members: None,
            status: None,
        }
    }

    #[test]
    fn accepting_a_mention_writes_the_handle_and_a_space() {
        let suggestions = Suggestions {
            active: Some(Active {
                start: 3,
                caret: 6,
                items: vec![
                    Item::Mention(choice("alice", MentionKind::User)),
                    Item::Mention(choice("channel", MentionKind::Channel)),
                ],
            }),
            ..Suggestions::default()
        };
        let accept = suggestions.accept(1).unwrap();
        assert_eq!(accept.replace, 3..6);
        assert_eq!(accept.insert, "@channel ");
        assert_eq!(accept.emoji, None, "a mention is not an emoji to remember");
    }
}
