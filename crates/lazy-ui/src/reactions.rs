//! Reaction chips: what each shows, who reacted, and how it reads aloud.
//!
//! The data side lives here so the wording is unit tested; drawing is in `message_row`.

use gpui::SharedString;
use lazy_client_core::{ClientCore, TimelineItem};
use lazy_domain::ReactionEmoji;

use crate::emoji_images;
use crate::image_store::ImageStore;
use crate::rich_text::EmojiPicture;

/// One reaction as a chip shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub emoji: ReactionEmoji,
    pub count: usize,
    /// The person reacted with this emoji (as shown, including a change still being saved).
    pub mine: bool,
    /// Who reacted and with what, in words: the tooltip and the accessible name.
    pub people: String,
    /// For a custom emoji: how it reads (`:name:`) and, once decoded, its picture.
    pub custom: Option<CustomChip>,
}

/// What a chip of a custom emoji shows instead of a glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomChip {
    /// `:name:`, drawn muted when there is no picture.
    pub label: String,
    pub picture: Option<EmojiPicture>,
    /// The emoji was deleted: it can be withdrawn but not added again.
    pub deleted: bool,
}

impl Chip {
    /// The sentence a screen reader reads and the tooltip shows: "Alice, Bob and 2 others reacted
    /// with party popper".
    pub fn label(&self) -> SharedString {
        match &self.custom {
            Some(custom) => format!("{} reacted with {}", self.people, custom.label).into(),
            None => format!("{} reacted with {}", self.people, emoji_name(self.emoji)).into(),
        }
    }

    /// What the chip's glyph is, for a standard emoji; `None` for a custom one.
    pub fn glyph(&self) -> Option<&'static str> {
        self.emoji.glyph()
    }

    /// How the chip's emoji reads in a line of text: the glyph, or `:name:`.
    pub fn text(&self) -> String {
        match (&self.custom, self.emoji.glyph()) {
            (Some(custom), _) => custom.label.clone(),
            (None, Some(glyph)) => glyph.to_owned(),
            (None, None) => String::new(),
        }
    }

    /// The tooltip: the same, with the shortcode a person could type.
    pub fn tooltip(&self) -> SharedString {
        match self.emoji.unicode().and_then(|emoji| emoji.shortcode()) {
            Some(shortcode) => format!("{} reacted with :{shortcode}:", self.people).into(),
            None => self.label(),
        }
    }
}

fn emoji_name(emoji: ReactionEmoji) -> String {
    emoji
        .unicode()
        .map(|emoji| emoji.name().to_owned())
        .unwrap_or_default()
}

/// "You, Bob and 2 others": the person (first, as "You") and the others who reacted, by name.
///
/// One or two or three people are named in full; with four or more the first two are named and the
/// rest counted.
pub fn who_reacted(me: bool, others: &[String]) -> String {
    let mut names: Vec<&str> = Vec::with_capacity(others.len() + 1);
    if me {
        names.push("You");
    }
    names.extend(others.iter().map(String::as_str));
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} others", rest.len()),
    }
}

/// The chips for a message, in the order each emoji was first used.
pub fn chips_for(core: &ClientCore, store: &ImageStore, item: &TimelineItem) -> Vec<Chip> {
    let me = core.me().map(|user| user.id);
    item.reactions
        .iter()
        .map(|reaction| {
            let mine = me.is_some_and(|me| reaction.users.contains(&me));
            let others: Vec<String> = reaction
                .users
                .iter()
                .filter(|user| Some(**user) != me)
                .map(|user| core.display_name(*user).to_owned())
                .collect();
            Chip {
                emoji: reaction.emoji,
                count: reaction.users.len(),
                mine,
                people: who_reacted(mine, &others),
                custom: reaction.emoji.custom_id().map(|id| {
                    let look = emoji_images::look(core, store, id, None);
                    CustomChip {
                        label: look.label,
                        picture: look.picture,
                        deleted: core
                            .custom_emoji_by_id(id)
                            .is_none_or(|emoji| emoji.deleted),
                    }
                }),
            }
        })
        .collect()
}

/// The menu line for a reaction on a message: "👍 3 – you reacted" or "👍 3".
pub fn menu_label(chip: &Chip) -> String {
    let text = chip.text();
    if chip.mine {
        format!("{text} {} – you reacted", chip.count)
    } else {
        format!("{text} {}", chip.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn people_are_named_in_full_up_to_three() {
        assert_eq!(who_reacted(false, &names(&["Alice"])), "Alice");
        assert_eq!(
            who_reacted(false, &names(&["Alice", "Bob"])),
            "Alice and Bob"
        );
        assert_eq!(
            who_reacted(false, &names(&["Alice", "Bob", "Carol"])),
            "Alice, Bob and Carol"
        );
    }

    #[test]
    fn four_or_more_name_two_and_count_the_rest() {
        assert_eq!(
            who_reacted(false, &names(&["Alice", "Bob", "Carol", "Dan"])),
            "Alice, Bob and 2 others"
        );
        assert_eq!(
            who_reacted(false, &names(&["Alice", "Bob", "Carol", "Dan", "Eve"])),
            "Alice, Bob and 3 others"
        );
    }

    #[test]
    fn the_person_comes_first_as_you() {
        assert_eq!(who_reacted(true, &[]), "You");
        assert_eq!(who_reacted(true, &names(&["Bob"])), "You and Bob");
        assert_eq!(
            who_reacted(true, &names(&["Bob", "Carol", "Dan"])),
            "You, Bob and 2 others"
        );
        assert_eq!(who_reacted(false, &[]), "");
    }

    #[test]
    fn a_chip_reads_as_a_sentence_and_a_tooltip() {
        let chip = Chip {
            emoji: ReactionEmoji::parse("🎉").unwrap(),
            count: 4,
            mine: false,
            people: "Alice, Bob and 2 others".into(),
            custom: None,
        };
        assert_eq!(
            chip.label().as_ref(),
            "Alice, Bob and 2 others reacted with party popper"
        );
        assert_eq!(
            chip.tooltip().as_ref(),
            "Alice, Bob and 2 others reacted with :tada:"
        );
    }

    #[test]
    fn the_menu_line_says_whether_the_person_reacted() {
        let mut chip = Chip {
            emoji: ReactionEmoji::parse("👍").unwrap(),
            count: 3,
            mine: true,
            custom: None,
            people: String::new(),
        };
        assert_eq!(menu_label(&chip), "👍 3 – you reacted");
        chip.mine = false;
        assert_eq!(menu_label(&chip), "👍 3");
    }
}
