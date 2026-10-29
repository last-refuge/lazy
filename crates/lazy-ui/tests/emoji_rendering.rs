//! Proves, without a window or the machine's fonts, that the bundled emoji font does its job: each
//! emoji cluster shapes into one color glyph from the emoji font.
//!
//! This uses GPUI's real text system (the one the desktop app uses on Linux) with only Lazy's own
//! fonts loaded, so the result does not depend on which fonts are installed. It checks shaping, not
//! how the glyphs look; the offscreen renders (`cargo run -p lazy-ui --example screens`) show that.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use gpui::{TextRun, TextSystem, WindowTextSystem, font, px};
use lazy_ui::emoji_font;
use lazy_ui::fonts;

fn text_system() -> WindowTextSystem {
    let system = TextSystem::new(Arc::new(
        gpui_wgpu::CosmicTextSystem::new_without_system_fonts(fonts::FAMILY),
    ));
    let started = Instant::now();
    system
        .add_fonts(
            fonts::FILES
                .iter()
                .map(|bytes| Cow::Borrowed(*bytes))
                .collect(),
        )
        .expect("the bundled fonts load");
    eprintln!("registering the bundled fonts took {:?}", started.elapsed());
    WindowTextSystem::new(Arc::new(system))
}

fn glyph_runs(system: &WindowTextSystem, text: &str) -> Vec<(bool, usize)> {
    let base = TextRun {
        len: text.len(),
        font: font(fonts::FAMILY),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = emoji_font::with_emoji_font(text, vec![base]);
    let lines = system
        .shape_text(text.to_owned().into(), px(22.), &runs, None, None)
        .expect("the text shapes");
    lines[0]
        .unwrapped_layout
        .runs
        .iter()
        .map(|run| {
            (
                run.glyphs.iter().all(|glyph| glyph.is_emoji),
                run.glyphs.len(),
            )
        })
        .collect()
}

#[test]
fn every_kind_of_emoji_shapes_into_a_single_color_glyph() {
    let system = text_system();
    for emoji in [
        "🎉",
        "😁",
        "👍",
        "☕",
        "❤️",
        "✈️",
        "☀️",
        "👍🏽",
        "🇨🇦",
        "🏳️‍🌈",
        "👨‍👩‍👧‍👦",
        "👩🏽‍🚀",
        "1️⃣",
        "#️⃣",
    ] {
        assert_eq!(
            glyph_runs(&system, emoji),
            vec![(true, 1)],
            "{emoji} is one color glyph of the emoji font"
        );
    }
}

#[test]
fn the_text_around_emoji_keeps_the_text_font() {
    let system = text_system();
    let runs = glyph_runs(&system, "ship ❤️ it");
    assert_eq!(runs.len(), 3, "text, emoji, text: {runs:?}");
    assert!(!runs[0].0 && runs[1] == (true, 1) && !runs[2].0);
}
