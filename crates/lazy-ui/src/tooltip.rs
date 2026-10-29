//! A small text tooltip for elements that explain themselves on hover, such as reaction chips.

use gpui::{
    AnyView, App, AppContext as _, Context, IntoElement, Render, SharedString, Window, div,
    prelude::*, px,
};

use crate::emoji_font;
use crate::theme::Theme;

/// A bordered label that floats near the pointer. Emoji in the text are drawn in color.
pub struct TextTooltip {
    text: SharedString,
}

impl TextTooltip {
    /// The view to return from an element's `tooltip` callback.
    pub fn view(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
        let text = text.into();
        cx.new(|_| Self { text }).into()
    }
}

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        div()
            .max_w(px(320.))
            .px_2()
            .py_1()
            .rounded(theme.radii.small)
            .bg(colors.surface_raised)
            .border_1()
            .border_color(colors.border_strong)
            .shadow_md()
            .text_size(theme.type_scale.small)
            .child(emoji_font::label(
                self.text.clone(),
                colors.text,
                gpui::FontWeight::NORMAL,
            ))
    }
}
