//! A person's picture: a circle that shows their avatar when it has been fetched and their
//! initial on a colour of their own until then, or when they have none.
//!
//! The size is fixed by the caller and never depends on the picture, so a row does not move when
//! an image arrives. The initial is the same one the squares drew before pictures existed; the
//! circle is the new shape.

use std::sync::Arc;

use gpui::{
    Div, FontWeight, IntoElement, ObjectFit, ParentElement, Pixels, RenderImage, SharedString,
    Styled, StyledImage as _, div, img, px,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::avatar_color;

/// What to draw for one person.
pub struct Avatar {
    seed: u128,
    name: SharedString,
    size: Pixels,
    text_size: Option<Pixels>,
    image: Option<Arc<RenderImage>>,
}

impl Avatar {
    /// `seed` picks the colour (the person's id as a number), `name` gives the initial.
    pub fn new(seed: u128, name: impl Into<SharedString>, size: f32) -> Self {
        Self {
            seed,
            name: name.into(),
            size: px(size),
            text_size: None,
            image: None,
        }
    }

    /// The decoded picture, when the cache has it.
    pub fn image(mut self, image: Option<Arc<RenderImage>>) -> Self {
        self.image = image;
        self
    }

    /// The size of the initial; by default a bit under half the circle.
    pub fn text_size(mut self, size: Pixels) -> Self {
        self.text_size = Some(size);
        self
    }

    /// The circle, ready for an id, a selector, or a click handler.
    pub fn into_div(self) -> Div {
        let text_size = self.text_size.unwrap_or(self.size * 0.42);
        let circle = div()
            .size(self.size)
            .flex_none()
            .rounded_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        match self.image {
            // The picture rounds its own corners: a parent's `overflow_hidden` clips to a
            // rectangle, not to the circle.
            Some(image) => circle.child(
                img(image)
                    .size_full()
                    .rounded_full()
                    .object_fit(ObjectFit::Cover),
            ),
            None => circle
                .bg(avatar_color(self.seed))
                .text_color(gpui::white())
                .text_size(text_size)
                .font_weight(FontWeight::SEMIBOLD)
                .child(initial(&self.name)),
        }
    }
}

impl IntoElement for Avatar {
    type Element = <Div as IntoElement>::Element;

    fn into_element(self) -> Self::Element {
        self.into_div().into_element()
    }
}

/// The first letter of a name, upper case; `?` when there is none.
pub fn initial(name: &str) -> String {
    name.graphemes(true).next().unwrap_or("?").to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_is_the_first_grapheme_in_upper_case() {
        assert_eq!(initial("alice"), "A");
        assert_eq!(initial("Zoë"), "Z");
        assert_eq!(initial("ébène"), "É");
        assert_eq!(initial(""), "?");
        assert_eq!(initial("🎯 team"), "🎯");
    }
}
