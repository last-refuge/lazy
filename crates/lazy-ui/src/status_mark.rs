//! A person's status, drawn small beside their name: the emoji, with the words in a tooltip and
//! for screen readers. A custom emoji has no glyph to draw here, so a dot stands in.

use gpui::{
    Div, ElementId, InteractiveElement, IntoElement, ParentElement, Pixels, Role, Stateful,
    StatefulInteractiveElement, Styled, div,
};
use lazy_client_core::ClientCore;
use lazy_domain::UserId;
use lazy_domain::notification::UserStatus;

use crate::emoji_font;
use crate::tooltip::TextTooltip;

/// What is shown of a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMark {
    /// The emoji's text, when it is a standard one.
    pub glyph: Option<String>,
    pub text: String,
}

impl StatusMark {
    pub fn of(status: &UserStatus) -> Self {
        Self {
            glyph: status
                .emoji
                .and_then(|emoji| emoji.glyph())
                .map(str::to_owned),
            text: status.text().to_owned(),
        }
    }

    /// Someone's status now, or none when they have none or it has ended.
    pub fn lookup(core: &ClientCore, user: UserId) -> Option<Self> {
        core.status_of(user).map(Self::of)
    }

    /// What a screen reader says.
    pub fn label(&self) -> String {
        format!("Status: {}", self.text)
    }
}

/// The small mark: `selector` names it for tests, `id` for the tooltip. `size` is the text size
/// of the line it sits in.
pub fn view(
    id: impl Into<ElementId>,
    selector: String,
    mark: &StatusMark,
    size: Pixels,
) -> Stateful<Div> {
    let tip = mark.text.clone();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .flex_none()
        .role(Role::Image)
        .aria_label(mark.label())
        .tooltip(move |_, cx| TextTooltip::view(tip.clone(), cx))
        .text_size(size)
        .child(match &mark.glyph {
            Some(glyph) => emoji_font::glyphs(glyph.clone()).into_any_element(),
            None => div().child("•").into_any_element(),
        })
}
