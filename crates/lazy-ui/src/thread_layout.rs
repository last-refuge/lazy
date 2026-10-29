//! Where the thread pane goes and how wide it is.
//!
//! The pane opens to the right of the conversation. The width a person drags it to is remembered
//! for the session (in the app model) as a wish; this module turns the wish into what fits.
//!
//! # Narrow windows
//!
//! The conversation keeps at least [`MIN_CONVERSATION_WIDTH`] and the pane at least
//! [`MIN_PANE_WIDTH`]. When the area right of the sidebar is narrower than the two together
//! (680 px), both cannot be shown side by side, so the pane **replaces** the conversation and its
//! header carries a Back button. The conversation's scroll position is kept for the way back.

/// The pane's width until the person drags it.
pub const DEFAULT_PANE_WIDTH: f32 = 400.0;
/// The narrowest the pane is drawn beside a conversation.
pub const MIN_PANE_WIDTH: f32 = 320.0;
/// The widest the pane can be dragged to.
pub const MAX_PANE_WIDTH: f32 = 720.0;
/// What the conversation keeps beside an open pane.
pub const MIN_CONVERSATION_WIDTH: f32 = 360.0;
/// Width of the grab strip between the conversation and the pane.
pub const HANDLE_WIDTH: f32 = 6.0;
/// Left or Right on the focused handle moves it by this much; with Shift, by the large step.
pub const KEY_STEP: f32 = 16.0;
pub const KEY_STEP_LARGE: f32 = 64.0;

/// Whether `available` pixels (the area right of the sidebar) can hold the conversation and the
/// pane together. If not, the pane replaces the conversation.
pub fn fits_side_by_side(available: f32) -> bool {
    available >= MIN_CONVERSATION_WIDTH + MIN_PANE_WIDTH
}

/// The widest the pane may be in `available` pixels, keeping the conversation its minimum.
pub fn max_width_in(available: f32) -> f32 {
    MAX_PANE_WIDTH
        .min(available - MIN_CONVERSATION_WIDTH)
        .max(MIN_PANE_WIDTH)
}

/// The width to draw the pane at: the wish, kept between the minimum and what fits.
pub fn pane_width(wanted: f32, available: f32) -> f32 {
    wanted.clamp(MIN_PANE_WIDTH, max_width_in(available))
}

/// The wish for a drag that has the pointer at `pointer_x`, with the pane's right edge at
/// `right_edge`. Kept within the absolute limits only; what fits is decided at layout time.
pub fn wish_from_drag(pointer_x: f32, right_edge: f32) -> f32 {
    (right_edge - pointer_x).clamp(MIN_PANE_WIDTH, MAX_PANE_WIDTH)
}

/// A key on the focused handle: Left widens the pane (the handle moves left), Right narrows it.
/// Home and End jump to the limits. `None` for any other key.
pub fn wish_from_key(current: f32, key: &str, shift: bool) -> Option<f32> {
    let step = if shift { KEY_STEP_LARGE } else { KEY_STEP };
    let wish = match key {
        "left" => current + step,
        "right" => current - step,
        "home" => MAX_PANE_WIDTH,
        "end" => MIN_PANE_WIDTH,
        _ => return None,
    };
    Some(wish.clamp(MIN_PANE_WIDTH, MAX_PANE_WIDTH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pane_goes_beside_the_conversation_only_when_both_fit() {
        assert!(fits_side_by_side(680.0));
        assert!(fits_side_by_side(1200.0));
        assert!(!fits_side_by_side(679.0));
        assert!(!fits_side_by_side(400.0));
    }

    #[test]
    fn the_wish_is_kept_between_the_minimum_and_what_fits() {
        // Plenty of room: the wish stands, up to the absolute maximum.
        assert_eq!(pane_width(400.0, 1400.0), 400.0);
        assert_eq!(pane_width(900.0, 1400.0), MAX_PANE_WIDTH);
        assert_eq!(pane_width(100.0, 1400.0), MIN_PANE_WIDTH);
        // Tight: the conversation keeps its share.
        assert_eq!(pane_width(500.0, 800.0), 800.0 - MIN_CONVERSATION_WIDTH);
        // Barely side by side: the pane is at its minimum whatever is wished.
        assert_eq!(pane_width(700.0, 680.0), MIN_PANE_WIDTH);
    }

    #[test]
    fn narrowing_the_window_does_not_forget_a_wide_wish() {
        let wish = 600.0;
        assert_eq!(pane_width(wish, 1400.0), 600.0);
        assert_eq!(pane_width(wish, 800.0), 440.0);
        assert_eq!(
            pane_width(wish, 1400.0),
            600.0,
            "back to the wish when it grows"
        );
    }

    #[test]
    fn dragging_sets_the_width_from_the_pointer() {
        assert_eq!(wish_from_drag(900.0, 1300.0), 400.0);
        assert_eq!(wish_from_drag(1290.0, 1300.0), MIN_PANE_WIDTH);
        assert_eq!(wish_from_drag(100.0, 1300.0), MAX_PANE_WIDTH);
    }

    #[test]
    fn the_keyboard_moves_the_handle_in_steps_and_to_the_limits() {
        assert_eq!(wish_from_key(400.0, "left", false), Some(400.0 + KEY_STEP));
        assert_eq!(wish_from_key(400.0, "right", false), Some(400.0 - KEY_STEP));
        assert_eq!(
            wish_from_key(400.0, "left", true),
            Some(400.0 + KEY_STEP_LARGE)
        );
        assert_eq!(
            wish_from_key(MIN_PANE_WIDTH, "right", false),
            Some(MIN_PANE_WIDTH)
        );
        assert_eq!(
            wish_from_key(MAX_PANE_WIDTH, "left", true),
            Some(MAX_PANE_WIDTH)
        );
        assert_eq!(wish_from_key(400.0, "home", false), Some(MAX_PANE_WIDTH));
        assert_eq!(wish_from_key(400.0, "end", false), Some(MIN_PANE_WIDTH));
        assert_eq!(wish_from_key(400.0, "up", false), None);
    }
}
