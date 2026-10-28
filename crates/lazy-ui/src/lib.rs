//! GPUI views, themes, and reusable components for Lazy clients.

pub mod app_model;
pub mod composer;
pub mod fonts;
pub mod keyboard;
pub mod message_row;
pub mod root_view;
pub mod shell;
pub mod sign_in;
pub mod text_input;
pub mod theme;
pub mod time_fmt;
pub mod timeline;

/// One-time UI setup: installs the theme for the system appearance and registers key bindings.
/// Call before opening the first window.
pub fn init(cx: &mut gpui::App, appearance: gpui::WindowAppearance) {
    fonts::register(cx);
    theme::install(cx, appearance);
    text_input::bind_keys(cx);
    keyboard::bind_keys(cx);
}
