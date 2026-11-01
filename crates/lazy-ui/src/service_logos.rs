//! The logos of the services the Integrations dialog connects to (`assets/services`, from Simple
//! Icons). Each is an SVG drawn once, the first time a logo is asked for, in white at twice the
//! size it is shown, and kept ready to draw on its service's color. A logo that cannot be drawn is
//! logged and left out: its tile shows the service's initial instead.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use gpui::RenderImage;
use image::Frame;

/// The pixels each logo is drawn at: twice the largest size it is shown (about 24 px).
const PREPARED: u32 = 56;

const SOURCES: [(&str, &str); 5] = [
    ("github", include_str!("../assets/services/github.svg")),
    ("linear", include_str!("../assets/services/linear.svg")),
    ("jira", include_str!("../assets/services/jira.svg")),
    ("sentry", include_str!("../assets/services/sentry.svg")),
    (
        "pagerduty",
        include_str!("../assets/services/pagerduty.svg"),
    ),
];

/// Every logo, drawn the first time one is asked for.
fn logos() -> &'static HashMap<&'static str, Arc<RenderImage>> {
    static LOGOS: OnceLock<HashMap<&'static str, Arc<RenderImage>>> = OnceLock::new();
    LOGOS.get_or_init(|| {
        let mut logos = HashMap::new();
        for (name, source) in SOURCES {
            match draw(source, PREPARED) {
                Some(image) => {
                    logos.insert(name, Arc::new(RenderImage::new(vec![Frame::new(image)])));
                }
                None => tracing::warn!(logo = name, "could not draw a service logo"),
            }
        }
        logos
    })
}

/// The logo of the service named `name` (`github`, `linear`, ...), if there is one.
pub fn logo(name: &str) -> Option<Arc<RenderImage>> {
    logos().get(name).cloned()
}

/// An SVG drawn in white, `size` pixels square, in the renderer's order (BGRA, premultiplied
/// colors made straight).
fn draw(source: &str, size: u32) -> Option<image::RgbaImage> {
    // Simple Icons' shapes have no fill of their own (black by default): white on the color.
    let white = source.replacen("<path ", "<path fill=\"#ffffff\" ", 1);
    let tree = resvg::usvg::Tree::from_str(&white, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let view = tree.size();
    let scale = size as f32 / view.width().max(view.height());
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut picture = image::RgbaImage::new(size, size);
    for (pixel, source) in picture.pixels_mut().zip(pixmap.pixels()) {
        let color = source.demultiply();
        *pixel = image::Rgba([color.blue(), color.green(), color.red(), color.alpha()]);
    }
    Some(picture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logo_draws_white_on_clear() {
        for (name, source) in SOURCES {
            let picture = draw(source, PREPARED).unwrap_or_else(|| panic!("{name} did not draw"));
            let painted: Vec<_> = picture.pixels().filter(|p| p.0[3] > 200).collect();
            assert!(
                painted.len() > (PREPARED * PREPARED / 10) as usize,
                "{name} drew almost nothing"
            );
            assert!(
                painted
                    .iter()
                    .all(|p| p.0[0] > 240 && p.0[1] > 240 && p.0[2] > 240),
                "{name} is not white"
            );
            assert!(
                picture.get_pixel(0, 0).0[3] < 255 || name == "github",
                "{name} has no clear edge"
            );
        }
    }
}
