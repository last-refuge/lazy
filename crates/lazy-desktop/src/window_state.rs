//! Where the window was: its size, whether it was maximized or full screen, and where it sat, kept
//! in `window-state.json` in the profile folder and restored at the next start (D24).
//!
//! The rules are pure functions so they can be tested without a display: [`parse`] reads a file
//! (a damaged, unknown, or older-version file is no file), [`restore`] turns what was saved and
//! the displays now connected into a [`Placement`], and [`WindowState::capture`] makes the next
//! file. Saved bounds that fall outside every display, or are smaller than the window's minimum,
//! are discarded for a centred default; a window larger than any display is brought down to the
//! largest one. On Wayland an application cannot place its own window, so only the size and
//! whether it was maximized are restored.
//!
//! [`WindowStateWatcher`] watches the real window and writes the file, debounced.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    AppContext as _, Bounds, Context, Entity, Global, Pixels, Subscription, Task, Window,
    WindowBounds, point, px, size,
};
use serde::{Deserialize, Serialize};

/// The file's version; a file with another one is ignored (and replaced at the next save).
pub const VERSION: u32 = 1;
/// The file in the profile folder.
pub const FILE_NAME: &str = "window-state.json";
/// The smallest window the application allows (also set as the window's minimum size).
pub const MIN_SIZE: (f32, f32) = (640.0, 480.0);
/// The size of a first window.
pub const DEFAULT_SIZE: (f32, f32) = (1100.0, 720.0);
/// How much of the window's top edge must be on a display for its position to be kept: enough to
/// find the title bar and drag the window back.
const MIN_VISIBLE: (f32, f32) = (120.0, 40.0);
/// How long a change waits for the next one before it is written.
const DEBOUNCE: Duration = Duration::from_millis(600);

/// What is saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub version: u32,
    /// The top-left corner, where the platform lets the application place its window.
    pub x: Option<f32>,
    pub y: Option<f32>,
    /// The size to restore (for a maximized window, the size it has when it is not).
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub fullscreen: bool,
}

/// A display's area, or a window's, in the same logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The width and height of the part this has in common with `other`.
    fn overlap(&self, other: &Rect) -> (f32, f32) {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        (width.max(0.0), height.max(0.0))
    }
}

/// Where and how large to open the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// `None`: let the platform centre it.
    pub origin: Option<(f32, f32)>,
    pub size: (f32, f32),
    pub maximized: bool,
    pub fullscreen: bool,
}

impl Placement {
    /// A first window: the default size, centred.
    pub fn default_for(displays: &[Rect]) -> Self {
        Self {
            origin: None,
            size: fit(DEFAULT_SIZE, displays),
            maximized: false,
            fullscreen: false,
        }
    }
}

/// Reads a saved file. Anything that is not a current-version file with sane numbers is `None`.
pub fn parse(text: &str) -> Option<WindowState> {
    let state: WindowState = serde_json::from_str(text).ok()?;
    let finite = |value: Option<f32>| value.is_none_or(f32::is_finite);
    (state.version == VERSION
        && state.width.is_finite()
        && state.height.is_finite()
        && finite(state.x)
        && finite(state.y))
    .then_some(state)
}

/// The text of the file.
pub fn serialize(state: &WindowState) -> String {
    serde_json::to_string_pretty(state).unwrap_or_default()
}

/// The largest display's size brought down to `wanted` if it is bigger (a huge saved window, or
/// one from a bigger screen).
fn fit(wanted: (f32, f32), displays: &[Rect]) -> (f32, f32) {
    let largest = displays
        .iter()
        .filter(|display| display.width.is_finite() && display.height.is_finite())
        .max_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)));
    match largest {
        Some(display) => (
            wanted
                .0
                .min(display.width)
                .max(MIN_SIZE.0.min(display.width)),
            wanted
                .1
                .min(display.height)
                .max(MIN_SIZE.1.min(display.height)),
        ),
        None => wanted,
    }
}

/// What to open the window as, given what was saved and the displays connected now.
/// `can_place` is false where the application cannot choose its window's position (Wayland).
pub fn restore(saved: Option<&WindowState>, displays: &[Rect], can_place: bool) -> Placement {
    let Some(saved) = saved else {
        return Placement::default_for(displays);
    };
    if saved.version != VERSION
        || !saved.width.is_finite()
        || !saved.height.is_finite()
        || saved.width < MIN_SIZE.0
        || saved.height < MIN_SIZE.1
    {
        return Placement::default_for(displays);
    }
    let size = fit((saved.width, saved.height), displays);
    let origin = match (can_place, saved.x, saved.y) {
        (true, Some(x), Some(y)) if x.is_finite() && y.is_finite() => {
            let top_strip = Rect::new(x, y, size.0, size.1.min(MIN_VISIBLE.1));
            let visible = displays.iter().any(|display| {
                let (width, height) = top_strip.overlap(display);
                width >= MIN_VISIBLE.0.min(size.0) && height >= MIN_VISIBLE.1.min(size.1)
            });
            if !visible && !displays.is_empty() {
                // Off every screen: start again from the default, centred.
                return Placement::default_for(displays);
            }
            Some((x, y))
        }
        _ => None,
    };
    Placement {
        origin,
        size,
        maximized: saved.maximized,
        fullscreen: saved.fullscreen,
    }
}

impl WindowState {
    /// What to save for a window now: `bounds` are the restore bounds for a maximized or full
    /// screen window. The position is left out where it cannot be restored.
    pub fn capture(
        bounds: (f32, f32, f32, f32),
        maximized: bool,
        fullscreen: bool,
        can_place: bool,
    ) -> Self {
        let (x, y, width, height) = bounds;
        Self {
            version: VERSION,
            x: can_place.then_some(x),
            y: can_place.then_some(y),
            width,
            height,
            maximized,
            fullscreen,
        }
    }
}

/// Whether the application can place its own window. Wayland compositors decide where windows go.
pub fn can_place_window() -> bool {
    !(cfg!(target_os = "linux")
        && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()))
}

/// Reads the saved state from the profile folder.
pub fn load(profile_dir: &Path) -> Option<WindowState> {
    parse(&std::fs::read_to_string(profile_dir.join(FILE_NAME)).ok()?)
}

/// Writes it, through a temporary file so a crash never leaves half a file.
pub fn save(profile_dir: &Path, state: &WindowState) -> std::io::Result<()> {
    let target = profile_dir.join(FILE_NAME);
    let temporary = profile_dir.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&temporary, serialize(state))?;
    std::fs::rename(&temporary, &target)
}

fn pixels(value: Pixels) -> f32 {
    f32::from(value)
}

/// The displays as plain rectangles.
pub fn display_rects(cx: &gpui::App) -> Vec<Rect> {
    cx.displays()
        .iter()
        .map(|display| {
            let bounds = display.bounds();
            Rect::new(
                pixels(bounds.origin.x),
                pixels(bounds.origin.y),
                pixels(bounds.size.width),
                pixels(bounds.size.height),
            )
        })
        .collect()
}

/// The bounds and mode to give the window options for `placement`.
pub fn window_bounds(placement: &Placement, cx: &gpui::App) -> WindowBounds {
    let window_size = size(px(placement.size.0), px(placement.size.1));
    let bounds = match placement.origin {
        Some((x, y)) => Bounds::new(point(px(x), px(y)), window_size),
        None => Bounds::centered(None, window_size, cx),
    };
    if placement.fullscreen {
        WindowBounds::Fullscreen(bounds)
    } else if placement.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    }
}

/// Watches a window and keeps its file current: after the last of a run of changes (debounced)
/// and when the window closes.
pub struct WindowStateWatcher {
    profile_dir: PathBuf,
    latest: Option<WindowState>,
    pending: Option<Task<()>>,
    _bounds: Subscription,
}

struct Held(#[allow(dead_code)] Entity<WindowStateWatcher>);
impl Global for Held {}

impl WindowStateWatcher {
    /// Starts watching `window` (call inside the window's creation or an update of it) and
    /// keeps the watcher alive for as long as the application runs.
    pub fn watch(profile_dir: PathBuf, window: &mut Window, cx: &mut gpui::App) {
        let watcher = cx.new(|cx| {
            let bounds = cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
                this.changed(window, cx)
            });
            Self {
                profile_dir,
                latest: None,
                pending: None,
                _bounds: bounds,
            }
        });
        let closing = watcher.clone();
        window.on_window_should_close(cx, move |window, cx| {
            closing.update(cx, |watcher, _| {
                watcher.latest = Some(Self::snapshot(window));
                watcher.write();
            });
            true
        });
        cx.set_global(Held(watcher));
    }

    fn snapshot(window: &Window) -> WindowState {
        let (bounds, maximized, fullscreen) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) => (bounds, false, false),
            WindowBounds::Maximized(bounds) => (bounds, true, false),
            WindowBounds::Fullscreen(bounds) => (bounds, false, true),
        };
        WindowState::capture(
            (
                pixels(bounds.origin.x),
                pixels(bounds.origin.y),
                pixels(bounds.size.width),
                pixels(bounds.size.height),
            ),
            maximized,
            fullscreen,
            can_place_window(),
        )
    }

    fn changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.latest = Some(Self::snapshot(window));
        // Each change restarts the wait; the file is written once things have settled.
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, _| this.write()).ok();
        }));
    }

    fn write(&mut self) {
        let Some(state) = self.latest.take() else {
            return;
        };
        if let Err(error) = save(&self.profile_dir, &state) {
            tracing::warn!(%error, "could not save the window state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Vec<Rect> {
        vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)]
    }

    fn two_screens() -> Vec<Rect> {
        vec![
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            Rect::new(1920.0, 0.0, 1280.0, 1024.0),
        ]
    }

    fn state(x: f32, y: f32, width: f32, height: f32) -> WindowState {
        WindowState {
            version: VERSION,
            x: Some(x),
            y: Some(y),
            width,
            height,
            maximized: false,
            fullscreen: false,
        }
    }

    #[test]
    fn a_first_start_opens_the_default_size_centred() {
        let placement = restore(None, &screen(), true);
        assert_eq!(placement.origin, None);
        assert_eq!(placement.size, DEFAULT_SIZE);
        assert!(!placement.maximized && !placement.fullscreen);
    }

    #[test]
    fn a_window_on_a_display_comes_back_where_it_was() {
        let saved = state(300.0, 120.0, 1000.0, 700.0);
        let placement = restore(Some(&saved), &screen(), true);
        assert_eq!(placement.origin, Some((300.0, 120.0)));
        assert_eq!(placement.size, (1000.0, 700.0));
        // And on the second display of a pair.
        let second = state(2100.0, 50.0, 900.0, 600.0);
        assert_eq!(
            restore(Some(&second), &two_screens(), true).origin,
            Some((2100.0, 50.0))
        );
    }

    #[test]
    fn maximized_and_full_screen_are_remembered_with_the_size_to_restore_to() {
        let mut saved = state(100.0, 100.0, 900.0, 600.0);
        saved.maximized = true;
        let placement = restore(Some(&saved), &screen(), true);
        assert!(placement.maximized && !placement.fullscreen);
        assert_eq!(placement.size, (900.0, 600.0));
        saved.maximized = false;
        saved.fullscreen = true;
        assert!(restore(Some(&saved), &screen(), true).fullscreen);
    }

    #[test]
    fn a_window_off_every_display_is_discarded_for_the_centred_default() {
        // The second display was unplugged.
        let saved = state(2100.0, 50.0, 900.0, 600.0);
        let placement = restore(Some(&saved), &screen(), true);
        assert_eq!(placement, Placement::default_for(&screen()));
        // Far off in any direction, and with only the top strip's edge showing.
        for (x, y) in [
            (-5000.0, 0.0),
            (0.0, 9000.0),
            (-870.0, 100.0),
            (1900.0, 0.0),
        ] {
            let placement = restore(Some(&state(x, y, 900.0, 600.0)), &screen(), true);
            assert_eq!(placement.origin, None, "({x}, {y})");
        }
        // Mostly off, with enough of the title bar on screen to drag it back: kept.
        let kept = restore(Some(&state(-700.0, 100.0, 900.0, 600.0)), &screen(), true);
        assert_eq!(kept.origin, Some((-700.0, 100.0)));
    }

    #[test]
    fn a_window_below_the_minimum_size_is_discarded() {
        for (width, height) in [(300.0, 200.0), (639.0, 700.0), (900.0, 479.0), (0.0, 0.0)] {
            let placement = restore(Some(&state(10.0, 10.0, width, height)), &screen(), true);
            assert_eq!(
                placement,
                Placement::default_for(&screen()),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn a_huge_window_is_brought_down_to_the_largest_display() {
        let placement = restore(Some(&state(0.0, 0.0, 9000.0, 7000.0)), &screen(), true);
        assert_eq!(placement.size, (1920.0, 1080.0));
        // The default is brought down too, on a small display.
        let small = vec![Rect::new(0.0, 0.0, 800.0, 600.0)];
        assert_eq!(Placement::default_for(&small).size, (800.0, 600.0));
        // With the largest of several.
        let big = restore(Some(&state(0.0, 0.0, 9000.0, 9000.0)), &two_screens(), true);
        assert_eq!(big.size, (1920.0, 1080.0));
    }

    #[test]
    fn numbers_that_are_not_numbers_are_discarded() {
        let text = r#"{"version":1,"x":1.0,"y":1.0,"width":1e999,"height":700.0}"#;
        // JSON has no NaN or infinity: an overflowing number does not parse at all.
        assert_eq!(parse(text), None);
        let nan_like = WindowState {
            width: f32::NAN,
            ..state(0.0, 0.0, 900.0, 600.0)
        };
        assert_eq!(
            restore(Some(&nan_like), &screen(), true),
            Placement::default_for(&screen())
        );
        let infinite_position = WindowState {
            x: Some(f32::INFINITY),
            ..state(0.0, 0.0, 900.0, 600.0)
        };
        assert_eq!(
            restore(Some(&infinite_position), &screen(), true).origin,
            None
        );
        assert_eq!(parse(&serialize(&nan_like)), None);
    }

    #[test]
    fn damaged_unknown_and_older_files_mean_the_defaults() {
        for text in [
            "",
            "not json",
            "{}",
            r#"{"version":1}"#,
            r#"{"version":0,"width":900.0,"height":600.0}"#,
            r#"{"version":2,"width":900.0,"height":600.0}"#,
            r#"{"width":900.0,"height":600.0}"#,
            "[1,2,3]",
            "null",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
        // A current file with extra, unknown fields still reads (a newer program wrote more).
        let text = r#"{"version":1,"x":10.0,"y":20.0,"width":900.0,"height":600.0,"extra":true}"#;
        assert_eq!(parse(text), Some(state(10.0, 20.0, 900.0, 600.0)));
        // Without the optional flags.
        let minimal = r#"{"version":1,"x":null,"y":null,"width":900.0,"height":600.0}"#;
        assert!(parse(minimal).is_some_and(|s| !s.maximized && !s.fullscreen && s.x.is_none()));
    }

    #[test]
    fn where_the_application_cannot_place_its_window_only_size_and_maximized_return() {
        let mut saved = state(-5000.0, -5000.0, 1000.0, 700.0);
        saved.maximized = true;
        let placement = restore(Some(&saved), &screen(), false);
        assert_eq!(placement.origin, None, "the position is not restorable");
        assert_eq!(placement.size, (1000.0, 700.0));
        assert!(placement.maximized);
        // Nor is it written down.
        let captured = WindowState::capture((10.0, 20.0, 1000.0, 700.0), true, false, false);
        assert_eq!((captured.x, captured.y), (None, None));
        let placed = WindowState::capture((10.0, 20.0, 1000.0, 700.0), false, false, true);
        assert_eq!((placed.x, placed.y), (Some(10.0), Some(20.0)));
    }

    #[test]
    fn with_no_displays_known_the_saved_window_is_trusted() {
        let placement = restore(Some(&state(50.0, 60.0, 1000.0, 700.0)), &[], true);
        assert_eq!(placement.origin, Some((50.0, 60.0)));
        assert_eq!(placement.size, (1000.0, 700.0));
    }

    #[test]
    fn a_file_round_trips_and_is_written_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "lazy-window-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir), None, "no file yet");
        let saved = state(40.0, 50.0, 1280.0, 800.0);
        save(&dir, &saved).unwrap();
        assert_eq!(load(&dir), Some(saved.clone()));
        assert!(
            !dir.join(format!("{FILE_NAME}.tmp")).exists(),
            "no temporary file is left"
        );
        // A damaged file is no file.
        std::fs::write(dir.join(FILE_NAME), "{ half").unwrap();
        assert_eq!(load(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    struct Blank;

    impl gpui::Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    #[gpui::test]
    fn the_watcher_writes_the_file_once_a_run_of_resizes_has_settled(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "lazy-window-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (_view, cx) = cx.add_window_view(|_, _| Blank);
        let watched = dir.clone();
        cx.update(|window, cx| WindowStateWatcher::watch(watched, window, cx));
        assert_eq!(
            load(&dir),
            None,
            "nothing is written before anything changes"
        );
        // A drag of the window's corner: many changes, then it stops.
        for width in [900.0, 910.0, 920.0, 930.0] {
            cx.simulate_resize(size(px(width), px(700.0)));
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            assert_eq!(load(&dir), None, "still being resized: not written yet");
        }
        cx.executor()
            .advance_clock(DEBOUNCE + Duration::from_millis(50));
        cx.run_until_parked();
        let written = load(&dir).expect("the settled size was written");
        assert_eq!((written.width, written.height), (930.0, 700.0));
        assert!(!written.maximized && !written.fullscreen);
        std::fs::remove_dir_all(&dir).ok();
    }
}
