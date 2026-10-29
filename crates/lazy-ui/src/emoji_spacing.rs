//! Room for an inline custom emoji's picture in a line of text.
//!
//! GPUI lays text out as one shaped string, so a picture cannot be a part of the line. Instead the
//! line reserves the picture's width with characters that draw nothing and never let the line
//! break inside them (a figure space, a no-break space, a narrow no-break space), and the picture
//! is painted over that room afterwards, at the position the text layout reports for it.
//!
//! The widths of those characters belong to the font, so they are measured once, when the
//! interface starts, against the interface font. A request is rounded up to what the three
//! characters can make: the room is never smaller than the picture, and at most about a fifth of
//! an em larger.

use std::sync::OnceLock;

use gpui::{App, font, px};

/// A reserving character and how wide it is, in em.
#[derive(Debug, Clone, Copy)]
struct Space {
    ch: char,
    em: f32,
}

/// Widest first.
#[derive(Debug, Clone, Copy)]
struct Metrics([Space; 3]);

/// What the widths are when they could not be measured (and in tests that never start the
/// interface).
const FALLBACK: Metrics = Metrics([
    Space {
        ch: '\u{2007}',
        em: 0.6,
    },
    Space {
        ch: '\u{a0}',
        em: 0.26,
    },
    Space {
        ch: '\u{202f}',
        em: 0.2,
    },
]);

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Measures the reserving characters in the interface font. Called once by [`crate::init`], after
/// the fonts are registered; later calls change nothing.
pub fn measure(cx: &App) {
    METRICS.get_or_init(|| {
        let system = cx.text_system();
        let id = system.resolve_font(&font(crate::fonts::FAMILY));
        let width = |ch: char, fallback: f32| {
            system
                .advance(id, px(100.), ch)
                .map(|size| f32::from(size.width) / 100.0)
                .ok()
                .filter(|em| *em > 0.01 && *em < 2.0)
                .unwrap_or(fallback)
        };
        let mut spaces = [
            Space {
                ch: '\u{2007}',
                em: width('\u{2007}', 0.6),
            },
            Space {
                ch: '\u{a0}',
                em: width('\u{a0}', 0.26),
            },
            Space {
                ch: '\u{202f}',
                em: width('\u{202f}', 0.2),
            },
        ];
        spaces.sort_by(|a, b| b.em.total_cmp(&a.em));
        Metrics(spaces)
    });
}

fn metrics() -> Metrics {
    METRICS.get().copied().unwrap_or(FALLBACK)
}

/// Characters whose total width is at least `em` and less than `em` plus the narrowest of them:
/// the room a picture `em` wide needs. Never empty.
pub fn reserve(em: f32) -> (String, f32) {
    let spaces = metrics().0;
    let mut out = String::new();
    let mut total = 0.0;
    for space in spaces {
        while em - total >= space.em - 0.01 {
            out.push(space.ch);
            total += space.em;
        }
    }
    if total < em - 0.01 || out.is_empty() {
        let narrowest = spaces[2];
        out.push(narrowest.ch);
        total += narrowest.em;
    }
    (out, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_room_is_never_smaller_than_the_picture_and_not_much_larger() {
        let narrowest = metrics().0[2].em;
        for tenth in 5..=30 {
            let want = tenth as f32 / 10.0 * 1.0;
            let (text, got) = reserve(want);
            assert!(!text.is_empty());
            assert!(got >= want - 0.011, "{want}: {got}");
            assert!(got < want + narrowest + 0.011, "{want}: {got}");
        }
    }

    #[test]
    fn the_room_never_lets_a_line_break_inside_it() {
        let (text, _) = reserve(1.8);
        assert!(
            text.chars()
                .all(|c| matches!(c, '\u{2007}' | '\u{a0}' | '\u{202f}'))
        );
    }
}
