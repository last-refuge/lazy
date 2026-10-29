//! The brand's pictures: the horizontal logo, the symbol, Moss, and the app icon (Soft Signal
//! 1.0.0; the files are in `assets/brand`).
//!
//! The pictures are embedded in the program, decoded once when the application starts
//! ([`install`], from [`crate::init`]), scaled there to twice the size they are drawn at (the
//! renderer does not filter a large reduction well, and a picture drawn at a sixteenth of its
//! size looks dithered), and kept as ready-to-draw images: drawing one is a lookup, never a
//! decode, and a picture has a fixed size, so nothing moves when a screen appears. The forest
//! variants are for light surfaces and the paper variants for dark ones; the active theme picks.
//!
//! # Rules the brand sets
//!
//! * The logo and the symbol are drawn as they are: not recolored, stretched, outlined, or
//!   shadowed, and nothing goes inside them. The logo is never drawn narrower than 120 px.
//! * Moss is a companion to a clear message and a useful action: compact empty states and the
//!   welcome, at 96 to 160 px (the symbol below 80 px). Never in an error, a destructive
//!   confirmation, or a security or outage state, never animated, never in the message list.
//!   Moss is decorative: it has no accessible label, and the copy beside it says everything.
//!   Moss does not stand in for the app mark.

use std::sync::Arc;

use gpui::{
    App, Div, Global, ObjectFit, RenderImage, Role, Stateful, StyledImage as _, div, img, px,
};
use gpui::{
    InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _,
};
use image::Frame;

use crate::theme::{Theme, ThemeMode};

const LOGO_FOREST: &[u8] = include_bytes!("../assets/brand/lazy-logo-forest-600.png");
const LOGO_PAPER: &[u8] = include_bytes!("../assets/brand/lazy-logo-paper-600.png");
const SYMBOL_FOREST: &[u8] = include_bytes!("../assets/brand/lazy-symbol-forest-512.png");
const SYMBOL_PAPER: &[u8] = include_bytes!("../assets/brand/lazy-symbol-paper-512.png");
const MOSS_PRIMARY: &[u8] = include_bytes!("../assets/brand/moss-primary-512.png");
const MOSS_WELCOME: &[u8] = include_bytes!("../assets/brand/moss-welcome-512.png");
const MOSS_CAUGHT_UP: &[u8] = include_bytes!("../assets/brand/moss-caught-up-512.png");
const MOSS_CELEBRATE: &[u8] = include_bytes!("../assets/brand/moss-celebrate-512.png");
const APP_ICON: &[u8] = include_bytes!("../assets/brand/lazy-app-256.png");

/// The logo's own proportions (600 by 224 pixels).
const LOGO_WIDTH: f32 = 600.0;
const LOGO_HEIGHT: f32 = 224.0;
/// The pixels each picture is prepared at: twice the size it is drawn at (the logo at 160, the
/// symbol at 32, Moss at 120 to 128). The renderer does not filter a large reduction well (a
/// 512 px picture drawn at 32 px looks dithered), so the reduction is done here, once, with a
/// proper filter; what is left for the renderer is 2 to 1 on a standard display and none on a
/// 2x one.
const LOGO_PREPARED: (u32, u32) = (320, 119);
const SYMBOL_PREPARED: (u32, u32) = (64, 64);
const MOSS_PREPARED: (u32, u32) = (256, 256);
/// Below this size Moss gives way to the symbol: Moss is not legible smaller.
pub const SYMBOL_BELOW: f32 = 80.0;
/// The narrowest the logo is drawn.
pub const MIN_LOGO_WIDTH: f32 = 120.0;

/// Which picture of Moss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moss {
    /// The everyday companion: the add-a-workspace dialog.
    Primary,
    /// The first run, and "pick a conversation".
    Welcome,
    /// "Nothing left": the empty thread inbox, the empty saved list.
    CaughtUp,
    /// A small success the screen already shows, such as a workspace that was just created.
    Celebrate,
}

/// The decoded pictures, kept as a global.
struct Pictures {
    logo_forest: Option<Arc<RenderImage>>,
    logo_paper: Option<Arc<RenderImage>>,
    symbol_forest: Option<Arc<RenderImage>>,
    symbol_paper: Option<Arc<RenderImage>>,
    moss_primary: Option<Arc<RenderImage>>,
    moss_welcome: Option<Arc<RenderImage>>,
    moss_caught_up: Option<Arc<RenderImage>>,
    moss_celebrate: Option<Arc<RenderImage>>,
}

impl Global for Pictures {}

/// Decodes the brand's pictures, scales each to the size it is drawn at, and keeps them. A picture
/// that cannot be decoded is logged and left out: the screens that would show it simply do
/// without.
pub fn install(cx: &mut App) {
    let one = |name: &'static str, bytes: &[u8], (width, height): (u32, u32)| match scaled(
        bytes, width, height,
    ) {
        Ok(picture) => Some(Arc::new(RenderImage::new(vec![Frame::new(bgra(picture))]))),
        Err(error) => {
            tracing::warn!(?error, picture = name, "could not prepare a brand picture");
            None
        }
    };
    cx.set_global(Pictures {
        logo_forest: one("logo (forest)", LOGO_FOREST, LOGO_PREPARED),
        logo_paper: one("logo (paper)", LOGO_PAPER, LOGO_PREPARED),
        symbol_forest: one("symbol (forest)", SYMBOL_FOREST, SYMBOL_PREPARED),
        symbol_paper: one("symbol (paper)", SYMBOL_PAPER, SYMBOL_PREPARED),
        moss_primary: one("Moss (primary)", MOSS_PRIMARY, MOSS_PREPARED),
        moss_welcome: one("Moss (welcome)", MOSS_WELCOME, MOSS_PREPARED),
        moss_caught_up: one("Moss (caught up)", MOSS_CAUGHT_UP, MOSS_PREPARED),
        moss_celebrate: one("Moss (celebrate)", MOSS_CELEBRATE, MOSS_PREPARED),
    });
}

/// Decodes `bytes` and scales the picture to `width` by `height` pixels (straight RGBA).
///
/// The colors are weighted by their opacity while scaling, so the clear pixels around a shape do
/// not tint its edge, and put back afterwards.
fn scaled(bytes: &[u8], width: u32, height: u32) -> Result<image::RgbaImage, image::ImageError> {
    let mut picture = image::load_from_memory(bytes)?.to_rgba8();
    for pixel in picture.pixels_mut() {
        let alpha = u32::from(pixel.0[3]);
        for channel in &mut pixel.0[..3] {
            let weighted = (u32::from(*channel) * alpha + 127) / 255;
            *channel = u8::try_from(weighted).unwrap_or(u8::MAX);
        }
    }
    let mut picture = image::imageops::resize(
        &picture,
        width,
        height,
        image::imageops::FilterType::Lanczos3,
    );
    for pixel in picture.pixels_mut() {
        let alpha = u32::from(pixel.0[3]);
        if alpha == 0 {
            continue;
        }
        for channel in &mut pixel.0[..3] {
            let restored = (u32::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = u8::try_from(restored).unwrap_or(u8::MAX);
        }
    }
    Ok(picture)
}

/// `picture` in the order the renderer wants (BGRA).
fn bgra(mut picture: image::RgbaImage) -> image::RgbaImage {
    for pixel in picture.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    picture
}

/// `image` filling a box of `width` by `height`, kept in proportion.
fn picture(image: Arc<RenderImage>, width: f32, height: f32) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .child(img(image).size_full().object_fit(ObjectFit::Contain))
}

/// The horizontal logo, `width` pixels wide (at least [`MIN_LOGO_WIDTH`]), in the variant that
/// suits the active theme. Its accessible name is the product's: "Lazy".
pub fn logo(cx: &App, width: f32) -> Option<Stateful<Div>> {
    let width = width.max(MIN_LOGO_WIDTH);
    let pictures = cx.try_global::<Pictures>()?;
    let image = match Theme::get(cx).mode {
        ThemeMode::Light => pictures.logo_forest.clone(),
        ThemeMode::Dark => pictures.logo_paper.clone(),
    }?;
    Some(
        picture(image, width, width * LOGO_HEIGHT / LOGO_WIDTH)
            .id("brand-logo")
            .role(Role::Image)
            .aria_label("Lazy"),
    )
}

/// The symbol, `size` pixels square, for the app itself where a generic glyph would stand.
/// Decorative: the words beside it name the app.
pub fn symbol(cx: &App, size: f32) -> Option<Div> {
    let pictures = cx.try_global::<Pictures>()?;
    let image = match Theme::get(cx).mode {
        ThemeMode::Light => pictures.symbol_forest.clone(),
        ThemeMode::Dark => pictures.symbol_paper.clone(),
    }?;
    Some(picture(image, size, size))
}

/// Moss, `size` pixels square (96 to 160 is the range the brand sets; below
/// [`SYMBOL_BELOW`] the symbol is drawn instead). Decorative.
pub fn moss(cx: &App, which: Moss, size: f32) -> Option<Div> {
    if size < SYMBOL_BELOW {
        return symbol(cx, size);
    }
    let pictures = cx.try_global::<Pictures>()?;
    let image = match which {
        Moss::Primary => pictures.moss_primary.clone(),
        Moss::Welcome => pictures.moss_welcome.clone(),
        Moss::CaughtUp => pictures.moss_caught_up.clone(),
        Moss::Celebrate => pictures.moss_celebrate.clone(),
    }?;
    Some(picture(image, size, size))
}

/// The headline of the screens a person sees before they are in: the brand's.
pub const HEADLINE: &str = "Good work. Easy conversation.";

/// The brand's opening for the screens before a workspace: the logo (160 px wide, with clear space
/// around it), the headline in Nunito Bold, and, when given, a line of muted text under it.
pub fn masthead(cx: &App, supporting: Option<&'static str>) -> Div {
    let theme = *Theme::get(cx);
    let colors = theme.colors;
    div()
        .flex()
        .flex_col()
        .gap_2()
        .children(logo(cx, 160.).map(|logo| div().pb_2().child(logo)))
        .child(crate::fonts::heading(
            div()
                .text_size(theme.type_scale.heading)
                .text_color(colors.text)
                .child(HEADLINE),
        ))
        .children(supporting.map(|line| {
            div()
                .text_size(theme.type_scale.body)
                .text_color(colors.text_muted)
                .child(line)
        }))
}

/// The app tile as the window's icon, for the platforms where GPUI takes one (X11; Wayland, Windows
/// and macOS take theirs from the desktop entry, the executable's resources, or the bundle).
pub fn app_icon() -> Option<Arc<image::RgbaImage>> {
    image::load_from_memory(APP_ICON)
        .ok()
        .map(|icon| Arc::new(icon.to_rgba8()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_picture_decodes_at_its_size() {
        for (name, bytes, width, height) in [
            ("logo forest", LOGO_FOREST, 600, 224),
            ("logo paper", LOGO_PAPER, 600, 224),
            ("symbol forest", SYMBOL_FOREST, 512, 512),
            ("symbol paper", SYMBOL_PAPER, 512, 512),
            ("moss primary", MOSS_PRIMARY, 512, 512),
            ("moss welcome", MOSS_WELCOME, 512, 512),
            ("moss caught up", MOSS_CAUGHT_UP, 512, 512),
            ("moss celebrate", MOSS_CELEBRATE, 512, 512),
            ("app tile", APP_ICON, 256, 256),
        ] {
            let picture = image::load_from_memory(bytes)
                .unwrap_or_else(|error| panic!("{name} does not decode: {error}"));
            assert_eq!((picture.width(), picture.height()), (width, height), "{name}");
        }
    }

    #[test]
    fn pictures_are_prepared_at_twice_the_size_they_are_drawn_at_and_keep_their_proportions() {
        // Drawn at 160 (the logo), 32 (the symbol), and up to 128 (Moss).
        assert!(LOGO_PREPARED.0 >= 2 * 160);
        assert!(SYMBOL_PREPARED.0 >= 2 * 32);
        assert!(MOSS_PREPARED.0 >= 2 * 128);
        // The prepared logo is the original's shape to within a pixel; the others are square.
        let logo_ratio = LOGO_PREPARED.0 as f32 / LOGO_PREPARED.1 as f32;
        assert!((logo_ratio - LOGO_WIDTH / LOGO_HEIGHT).abs() < 0.03, "{logo_ratio}");
        assert_eq!(SYMBOL_PREPARED.0, SYMBOL_PREPARED.1);
        assert_eq!(MOSS_PREPARED.0, MOSS_PREPARED.1);
    }

    #[test]
    fn a_scaled_picture_has_the_asked_size_and_keeps_its_clear_margin_and_its_ink() {
        let (width, height) = SYMBOL_PREPARED;
        let picture = scaled(SYMBOL_FOREST, width, height).expect("the symbol scales");
        assert_eq!(picture.dimensions(), (width, height));
        assert_eq!(picture.get_pixel(0, 0).0[3], 0, "the corner stays clear");
        assert!(picture.pixels().any(|pixel| pixel.0[3] > 128), "the ink stays");
    }

    #[test]
    fn scaling_does_not_tint_the_edge_of_a_shape() {
        // Opaque red on the left, clear black on the right: averaging the colors as they are
        // would make the edge a dark red.
        let source = image::RgbaImage::from_fn(8, 8, |x, _| {
            if x < 4 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        let mut png = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("the test picture encodes");
        let picture = scaled(&png.into_inner(), 5, 5).expect("the test picture scales");
        assert!(
            picture
                .pixels()
                .any(|pixel| (8..248).contains(&pixel.0[3])),
            "some pixel straddles the edge"
        );
        for pixel in picture.pixels().filter(|pixel| pixel.0[3] >= 64) {
            let [red, green, blue, _] = pixel.0;
            assert!(red >= 240 && green <= 8 && blue <= 8, "{pixel:?} is tinted");
        }
    }

    #[test]
    fn the_logo_keeps_its_proportions_and_the_app_icon_is_a_square_picture() {
        let icon = app_icon().expect("the app icon decodes");
        assert_eq!((icon.width(), icon.height()), (256, 256));
        assert!((LOGO_WIDTH / LOGO_HEIGHT - 600.0 / 224.0).abs() < f32::EPSILON);
    }
}
