//! GPUI views, themes, and reusable components for Lazy clients.

pub mod app_model;
pub mod attachment_view;
pub mod attachments;
pub mod avatar;
pub mod avatar_images;
pub mod brand;
pub mod channel_name;
pub mod commands;
pub mod composer;
pub mod dialogs;
pub mod downloads;
pub mod emoji_font;
pub mod emoji_grid;
pub mod emoji_images;
pub mod emoji_picker;
pub mod emoji_spacing;
pub mod emphasis;
pub mod fonts;
pub mod form;
pub mod group_form;
pub mod image_store;
pub mod inbox_view;
pub mod keyboard;
pub mod mention_source;
pub mod menu;
pub mod message_row;
pub mod modal;
pub mod notification_form;
pub mod onboarding;
pub mod people_picker;
pub mod people_view;
pub mod rail;
pub mod reactions;
pub mod rich_text;
pub mod root_view;
pub mod saved_view;
pub mod search_text;
pub mod search_view;
pub mod selection;
pub mod server_row;
pub mod service_logos;
pub mod shell;
pub mod sidebar;
pub mod sign_in;
pub mod states;
pub mod status_mark;
pub mod suggest;
pub mod text_input;
pub mod theme;
pub mod thread_layout;
pub mod thread_pane;
pub mod time_fmt;
pub mod timeline;
pub mod tooltip;
pub mod updates;
pub mod viewer;
pub mod workspace_form;

/// One-time UI setup: installs the theme for the system appearance and registers key bindings.
/// Call before opening the first window.
pub fn init(cx: &mut gpui::App, appearance: gpui::WindowAppearance) {
    fonts::register(cx);
    emoji_spacing::measure(cx);
    theme::install(cx, appearance);
    image_store::ImageStore::install(cx);
    brand::install(cx);
    text_input::bind_keys(cx);
    keyboard::bind_keys(cx);
    emoji_picker::bind_keys(cx);
    modal::bind_keys(cx);
    dialogs::bind_quick_switcher_keys(cx);
}
