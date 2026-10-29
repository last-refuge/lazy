//! The emoji picker's layout and keyboard movement, as data.
//!
//! A [`Grid`] lays the emoji out in rows of a fixed number of cells: a header row for each section
//! (recently used, then the Unicode groups) and the cells under it. It never touches a window, so
//! how rows are cut, where a group starts, and where an arrow key lands are all unit tested, and
//! the picker can draw only the rows that are on screen (about 215 rows hold all ~1,900 emoji).

use lazy_domain::EmojiId;
use lazy_domain::emoji::{self, Emoji, Group, SkinTone};

/// Cells in one row of the picker.
pub const COLUMNS: usize = 9;

/// One cell of the picker: a standard emoji or one of the workspace's custom emoji.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Standard(&'static Emoji),
    Custom(EmojiId),
}

impl Pick {
    /// The standard emoji, or `None` for a custom one.
    pub fn emoji(self) -> Option<&'static Emoji> {
        match self {
            Self::Standard(emoji) => Some(emoji),
            Self::Custom(_) => None,
        }
    }
}

/// What a section holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// The emoji the person used lately, most recent first.
    Recent,
    /// The workspace's custom emoji, by name.
    Custom,
    /// One of the Unicode groups, in picker order.
    Group(Group),
    /// What a search found, best first.
    Results,
}

/// The name shown for a group.
pub fn group_title(group: Group) -> &'static str {
    match group {
        Group::SmileysAndEmotion => "Smileys & emotion",
        Group::PeopleAndBody => "People & body",
        Group::AnimalsAndNature => "Animals & nature",
        Group::FoodAndDrink => "Food & drink",
        Group::TravelAndPlaces => "Travel & places",
        Group::Activities => "Activities",
        Group::Objects => "Objects",
        Group::Symbols => "Symbols",
        Group::Flags => "Flags",
    }
}

/// The emoji that stands for a group on its tab.
pub fn group_icon(group: Group) -> &'static str {
    match group {
        Group::SmileysAndEmotion => "😀",
        Group::PeopleAndBody => "👋",
        Group::AnimalsAndNature => "🐻",
        Group::FoodAndDrink => "🍔",
        Group::TravelAndPlaces => "✈️",
        Group::Activities => "⚽",
        Group::Objects => "💡",
        Group::Symbols => "🔣",
        Group::Flags => "🏳️",
    }
}

/// A run of emoji under one heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    pub title: &'static str,
    /// The glyph on this section's tab.
    pub icon: &'static str,
    /// Row index of the heading, if the section has one.
    pub header_row: Option<usize>,
    /// Row index of the first row of cells.
    pub first_row: usize,
    /// Index into [`Grid::items`] of the section's first emoji.
    pub first_item: usize,
    pub len: usize,
}

/// One row of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// The heading of section `0`.
    Header(usize),
    /// `len` emoji starting at item `first_item`.
    Cells { first_item: usize, len: usize },
    /// A line of words under the heading of section `0`, which has no emoji: what the empty
    /// Custom section says.
    Note(usize),
}

/// A way to move the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
    /// The first emoji.
    Home,
    /// The last emoji.
    End,
    /// The start of the section the selection is in, or of the one before if it is already there.
    PageUp,
    /// The start of the next section.
    PageDown,
}

/// Emoji laid out in sections and rows.
#[derive(Debug, Clone)]
pub struct Grid {
    items: Vec<Pick>,
    sections: Vec<Section>,
    rows: Vec<Row>,
    columns: usize,
}

impl Grid {
    fn build(
        sections: Vec<(SectionKind, &'static str, &'static str, bool, Vec<Pick>)>,
        columns: usize,
    ) -> Self {
        let columns = columns.max(1);
        let mut items = Vec::new();
        let mut out_sections = Vec::new();
        let mut rows = Vec::new();
        for (kind, title, icon, header, found) in sections {
            // The Custom section stays when it is empty: it says so, and offers to add one.
            if found.is_empty() && kind != SectionKind::Custom {
                continue;
            }
            let header_row = header.then(|| {
                rows.push(Row::Header(out_sections.len()));
                rows.len() - 1
            });
            let first_row = rows.len();
            let first_item = items.len();
            if found.is_empty() {
                rows.push(Row::Note(out_sections.len()));
            }
            for chunk in found.chunks(columns) {
                rows.push(Row::Cells {
                    first_item: items.len(),
                    len: chunk.len(),
                });
                items.extend_from_slice(chunk);
            }
            out_sections.push(Section {
                kind,
                title,
                icon,
                header_row,
                first_row,
                first_item,
                len: items.len() - first_item,
            });
        }
        Self {
            items,
            sections: out_sections,
            rows,
            columns,
        }
    }

    /// Everything, for browsing: recently used first when there are any, then each group. Emoji
    /// that have skin tones are shown in `tone`.
    pub fn browse(recent: &[Pick], custom: &[EmojiId], tone: SkinTone, columns: usize) -> Self {
        let mut sections = vec![
            (
                SectionKind::Recent,
                "Recently used",
                "🕘",
                true,
                recent.to_vec(),
            ),
            (
                SectionKind::Custom,
                "Custom",
                "✨",
                true,
                custom.iter().copied().map(Pick::Custom).collect(),
            ),
        ];
        for group in emoji::groups() {
            let found = emoji::in_group(group)
                .map(|e| Pick::Standard(crate::emoji_font::drawable_tone(e, tone)))
                .collect();
            sections.push((
                SectionKind::Group(group),
                group_title(group),
                group_icon(group),
                true,
                found,
            ));
        }
        Self::build(sections, columns)
    }

    /// A search's results as one section with no heading.
    pub fn results(found: Vec<Pick>, columns: usize) -> Self {
        Self::build(
            vec![(SectionKind::Results, "Results", "🔍", false, found)],
            columns,
        )
    }

    pub fn items(&self) -> &[Pick] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Index of the section holding `item`.
    pub fn section_of(&self, item: usize) -> usize {
        self.sections
            .partition_point(|section| section.first_item <= item)
            .saturating_sub(1)
    }

    /// The row an emoji is in.
    pub fn row_of(&self, item: usize) -> usize {
        let section = &self.sections[self.section_of(item)];
        section.first_row + (item - section.first_item) / self.columns
    }

    /// The row to scroll to to show a section from its heading.
    pub fn scroll_row(&self, section: usize) -> usize {
        let section = &self.sections[section];
        section.header_row.unwrap_or(section.first_row)
    }

    /// Where `nav` takes the selection from `from`. Always a valid item index when the grid is not
    /// empty; the selection never leaves the grid or lands on a heading.
    pub fn step(&self, from: usize, nav: Nav) -> usize {
        let count = self.items.len();
        if count == 0 {
            return 0;
        }
        let from = from.min(count - 1);
        let columns = self.columns;
        let at = self.section_of(from);
        let section = &self.sections[at];
        let local = from - section.first_item;
        let column = local % columns;
        match nav {
            Nav::Left => from.saturating_sub(1),
            Nav::Right => (from + 1).min(count - 1),
            Nav::Home => 0,
            Nav::End => count - 1,
            Nav::Up => {
                if local >= columns {
                    from - columns
                } else if let Some(previous) = self.sections[..at].iter().rev().find(|s| s.len > 0)
                {
                    let last_row_start = (previous.len - 1) / columns * columns;
                    previous.first_item + (last_row_start + column).min(previous.len - 1)
                } else {
                    from
                }
            }
            Nav::Down => {
                if local + columns < section.len {
                    from + columns
                } else if local / columns < (section.len - 1) / columns {
                    // The row below is shorter than this column: land on its last emoji.
                    section.first_item + section.len - 1
                } else if let Some(next) = self.sections[at + 1..].iter().find(|s| s.len > 0) {
                    next.first_item + column.min(next.len - 1)
                } else {
                    from
                }
            }
            Nav::PageUp => {
                if local > 0 {
                    section.first_item
                } else {
                    self.sections[..at]
                        .iter()
                        .rev()
                        .find(|s| s.len > 0)
                        .map_or(0, |previous| previous.first_item)
                }
            }
            Nav::PageDown => self.sections[at + 1..]
                .iter()
                .find(|s| s.len > 0)
                .map_or(count - 1, |next| next.first_item),
        }
    }
}

#[cfg(test)]
mod tests {
    use lazy_domain::emoji::lookup;

    use super::*;

    fn e(glyph: &str) -> Pick {
        Pick::Standard(lookup(glyph).unwrap())
    }

    fn small() -> Grid {
        // Section A: 7 emoji in rows of 3; section B: 2; section C: 3.
        let many: Vec<Pick> = emoji::in_group(Group::Flags)
            .take(12)
            .map(Pick::Standard)
            .collect();
        Grid::build(
            vec![
                (SectionKind::Recent, "A", "a", true, many[0..7].to_vec()),
                (
                    SectionKind::Group(Group::Flags),
                    "B",
                    "b",
                    true,
                    many[7..9].to_vec(),
                ),
                (
                    SectionKind::Group(Group::Symbols),
                    "C",
                    "c",
                    true,
                    many[9..12].to_vec(),
                ),
            ],
            3,
        )
    }

    #[test]
    fn sections_rows_and_headings_are_laid_out_in_order() {
        let grid = small();
        assert_eq!(grid.len(), 12);
        assert_eq!(grid.sections().len(), 3);
        // Header, 3 rows (3+3+1), header, 1 row, header, 1 row.
        let shape: Vec<String> = grid
            .rows()
            .iter()
            .map(|row| match row {
                Row::Header(s) => format!("H{s}"),
                Row::Cells { len, .. } => format!("{len}"),
                Row::Note(s) => format!("N{s}"),
            })
            .collect();
        assert_eq!(shape, ["H0", "3", "3", "1", "H1", "2", "H2", "3"]);
        assert_eq!(grid.scroll_row(1), 4);
        assert_eq!(grid.row_of(0), 1);
        assert_eq!(grid.row_of(6), 3);
        assert_eq!(grid.row_of(7), 5);
        assert_eq!(grid.section_of(6), 0);
        assert_eq!(grid.section_of(7), 1);
        assert_eq!(grid.section_of(11), 2);
    }

    #[test]
    fn browsing_lists_every_emoji_once_with_recent_first_only_when_there_is_one() {
        let without = Grid::browse(&[], &[], SkinTone::Default, COLUMNS);
        // The groups, and the Custom section, which is there (empty) to say so.
        assert_eq!(without.sections().len(), emoji::groups().count() + 1);
        assert!(
            without
                .sections()
                .iter()
                .all(|s| s.kind != SectionKind::Recent)
        );
        assert_eq!(without.sections()[0].kind, SectionKind::Custom);
        assert_eq!(without.sections()[0].len, 0);
        assert!(
            without.rows().iter().any(|row| matches!(row, Row::Note(0))),
            "an empty Custom section has a line of its own"
        );
        let expected: usize = emoji::groups().map(|g| emoji::in_group(g).count()).sum();
        assert_eq!(without.len(), expected);
        assert!(
            without.len() > 1_500,
            "the whole dataset is here: {}",
            without.len()
        );

        let with = Grid::browse(&[e("🎉"), e("👍")], &[], SkinTone::Default, COLUMNS);
        assert_eq!(with.sections()[0].kind, SectionKind::Recent);
        assert_eq!(with.sections()[0].len, 2);
        assert_eq!(with.sections()[1].kind, SectionKind::Custom);
        assert_eq!(with.len(), expected + 2);
        assert!(with.rows().iter().all(|row| match row {
            Row::Cells { len, .. } => (1..=COLUMNS).contains(len),
            Row::Header(_) | Row::Note(_) => true,
        }));
    }

    #[test]
    fn a_populated_custom_section_follows_recently_used_and_precedes_the_groups() {
        let (a, b, c) = (
            EmojiId::generate(),
            EmojiId::generate(),
            EmojiId::generate(),
        );
        let grid = Grid::browse(&[e("🎉")], &[a, b, c], SkinTone::Default, COLUMNS);
        let kinds: Vec<SectionKind> = grid.sections().iter().take(3).map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SectionKind::Recent,
                SectionKind::Custom,
                SectionKind::Group(emoji::groups().next().unwrap())
            ]
        );
        let custom = &grid.sections()[1];
        assert_eq!(custom.len, 3);
        assert_eq!(custom.first_item, 1, "right after the one recent emoji");
        assert_eq!(
            &grid.items()[1..4],
            &[Pick::Custom(a), Pick::Custom(b), Pick::Custom(c)],
            "in the order given"
        );
        assert!(
            !grid.rows().iter().any(|row| matches!(row, Row::Note(_))),
            "a populated section needs no note"
        );
        assert_eq!(grid.section_of(2), 1);
        assert_eq!(grid.section_of(4), 2);
    }

    #[test]
    fn moving_over_an_empty_custom_section_lands_on_emoji_not_on_the_note() {
        let grid = Grid::browse(&[], &[], SkinTone::Default, COLUMNS);
        let first_group = &grid.sections()[1];
        assert_eq!(grid.sections()[0].len, 0);
        assert_eq!(first_group.first_item, 0, "the empty section holds no item");
        // Page Up from the first emoji has nowhere better to go; Page Down goes to the next group.
        assert_eq!(grid.step(0, Nav::PageUp), 0);
        assert_eq!(grid.step(0, Nav::Up), 0);
        let second = &grid.sections()[2];
        assert_eq!(grid.step(0, Nav::PageDown), second.first_item);
        assert_eq!(
            grid.step(second.first_item, Nav::PageUp),
            first_group.first_item
        );
        assert_eq!(grid.items()[0].emoji().map(|e| e.as_str()), Some("😀"));
    }

    #[test]
    fn a_custom_emoji_is_not_an_emoji_with_a_glyph_or_a_skin_tone() {
        let id = EmojiId::generate();
        let pick = Pick::Custom(id);
        assert!(pick.emoji().is_none());
        let grid = Grid::results(vec![pick, e("🚢")], COLUMNS);
        assert_eq!(
            grid.items()[0],
            Pick::Custom(id),
            "custom results come first"
        );
        assert_eq!(grid.sections().len(), 1);
    }

    #[test]
    fn the_skin_tone_applies_to_emoji_that_have_one() {
        let dark = Grid::browse(&[], &[], SkinTone::Dark, COLUMNS);
        let wave = dark
            .items()
            .iter()
            .filter_map(|item| item.emoji())
            .find(|item| item.name().starts_with("waving hand"))
            .unwrap();
        assert_eq!(wave.skin_tone(), Some(SkinTone::Dark));
        let default = Grid::browse(&[], &[], SkinTone::Default, COLUMNS);
        assert!(
            default
                .items()
                .iter()
                .any(|item| item.emoji().is_some_and(|e| e.as_str() == "👋"))
        );
    }

    #[test]
    fn searching_makes_one_section_with_no_heading() {
        let grid = Grid::results(
            emoji::search("heart", 40)
                .into_iter()
                .map(Pick::Standard)
                .collect(),
            COLUMNS,
        );
        assert_eq!(grid.sections().len(), 1);
        assert!(
            grid.rows()
                .iter()
                .all(|row| matches!(row, Row::Cells { .. }))
        );
        assert!(Grid::results(vec![], COLUMNS).is_empty());
    }

    #[test]
    fn left_and_right_walk_every_emoji_in_order_across_rows_and_sections() {
        let grid = small();
        let mut at = 0;
        let mut seen = vec![at];
        while grid.step(at, Nav::Right) != at {
            at = grid.step(at, Nav::Right);
            seen.push(at);
        }
        assert_eq!(seen, (0..12).collect::<Vec<_>>());
        assert_eq!(grid.step(0, Nav::Left), 0);
    }

    #[test]
    fn up_and_down_keep_the_column_and_clamp_into_shorter_rows() {
        let grid = small();
        // Section A cells: row0 = 0,1,2; row1 = 3,4,5; row2 = 6.
        assert_eq!(grid.step(1, Nav::Down), 4);
        assert_eq!(grid.step(4, Nav::Up), 1);
        assert_eq!(grid.step(5, Nav::Down), 6, "the last row has one emoji");
        assert_eq!(
            grid.step(6, Nav::Down),
            7,
            "then into the next section's first row"
        );
        // Section B has 2 emoji: column 2 clamps to its last.
        assert_eq!(grid.step(2, Nav::Down), 5);
        assert_eq!(
            grid.step(8, Nav::Up),
            6,
            "back up to the last row of A, clamped to its length"
        );
        assert_eq!(grid.step(1, Nav::Up), 1, "nothing above the first row");
        assert_eq!(grid.step(11, Nav::Down), 11, "nothing below the last row");
    }

    #[test]
    fn home_end_and_page_keys_jump_by_section() {
        let grid = small();
        assert_eq!(grid.step(8, Nav::Home), 0);
        assert_eq!(grid.step(3, Nav::End), 11);
        assert_eq!(grid.step(4, Nav::PageUp), 0, "to the start of this section");
        assert_eq!(
            grid.step(7, Nav::PageUp),
            0,
            "already at the start: the section before"
        );
        assert_eq!(grid.step(0, Nav::PageUp), 0);
        assert_eq!(grid.step(4, Nav::PageDown), 7);
        assert_eq!(grid.step(7, Nav::PageDown), 9);
        assert_eq!(
            grid.step(10, Nav::PageDown),
            11,
            "past the last section: the end"
        );
    }

    #[test]
    fn no_movement_leaves_the_grid_or_lands_nowhere() {
        let grid = Grid::browse(&[e("🎉")], &[], SkinTone::Default, COLUMNS);
        let last = grid.len() - 1;
        for from in (0..grid.len()).step_by(7).chain([last]) {
            for nav in [
                Nav::Left,
                Nav::Right,
                Nav::Up,
                Nav::Down,
                Nav::Home,
                Nav::End,
                Nav::PageUp,
                Nav::PageDown,
            ] {
                let to = grid.step(from, nav);
                assert!(to <= last, "{nav:?} from {from} went to {to}");
            }
        }
        assert_eq!(Grid::results(vec![], COLUMNS).step(0, Nav::Down), 0);
    }

    #[test]
    fn moving_down_then_up_returns_in_the_middle_of_a_section() {
        let grid = Grid::browse(&[], &[], SkinTone::Default, COLUMNS);
        let section = grid
            .sections()
            .iter()
            .find(|section| matches!(section.kind, SectionKind::Group(_)))
            .unwrap();
        let from = section.first_item + COLUMNS + 3;
        assert_eq!(grid.step(grid.step(from, Nav::Down), Nav::Up), from);
    }
}
