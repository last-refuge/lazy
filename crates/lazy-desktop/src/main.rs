//! The Lazy desktop application.
//!
//! This crate only wires things together: it starts the runtime (HTTP, realtime, local storage),
//! creates the UI's [`AppModel`] around it, pumps the runtime's inputs into the model on the
//! foreground executor, and opens the window where it was last. Everything else lives in
//! `lazy-ui`, `lazy-client-core`, and `lazy-client-runtime`.
//!
//! A Windows release has no console window, so what would go to the terminal also goes to the log
//! file: a start that fails leaves its reason in `logs/lazy.log`.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use gpui::{App, AppContext as _, TitlebarOptions, WindowOptions, px, size};
use gpui_platform::application;
use lazy_client_runtime::{Runtime, RuntimeConfig};
use lazy_desktop::logging;
use lazy_desktop::window_state::{self, WindowStateWatcher};
use lazy_ui::dialogs::{DesktopInfo, set_desktop_info};
use lazy_ui::root_view::RootView;
use lazy_ui::theme;

/// Where the profile will be, as the runtime will choose it: what `LAZY_PROFILE_DIR` or the
/// configuration says, else the platform's per-user data folder. Known before the runtime starts so
/// that a start that fails can still be logged.
fn expected_profile_dir(config: &RuntimeConfig) -> Option<PathBuf> {
    config
        .profile_dir
        .clone()
        .or_else(|| std::env::var_os("LAZY_PROFILE_DIR").map(PathBuf::from))
        .or_else(|| {
            directories::ProjectDirs::from("app", "lazy", "Lazy")
                .map(|dirs| dirs.data_local_dir().to_owned())
        })
}

fn main() -> ExitCode {
    // The terminal now; the file in the profile folder as soon as it is known.
    let log = logging::init();

    let config = RuntimeConfig::from_env();
    let mut log_dir = expected_profile_dir(&config).map(|dir| logging::log_dir(&dir));
    if let Some(dir) = &log_dir
        && let Err(error) = log.attach(dir)
    {
        eprintln!(
            "Lazy: could not open its log file in {}: {error}",
            dir.display()
        );
        log_dir = None;
    }
    tracing::info!(server = %config.server_url, "starting Lazy");
    let server_url = config.server_url.clone();
    let (runtime, inputs) = match Runtime::start(config) {
        Ok(started) => started,
        Err(error) => {
            tracing::error!(%error, "Lazy could not start");
            eprintln!("Lazy could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = Rc::new(runtime);
    let profile_dir = runtime.profile_dir().to_path_buf();
    let log_dir = match log_dir {
        Some(dir) => dir,
        None => {
            // The profile was not where it was expected to be: the log goes where it really is.
            let dir = logging::log_dir(&profile_dir);
            if let Err(error) = log.attach(&dir) {
                eprintln!(
                    "Lazy: could not open its log file in {}: {error}",
                    dir.display()
                );
            }
            dir
        }
    };
    tracing::info!(profile = %profile_dir.display(), logs = %log_dir.display(), "using profile");
    let saved_window = window_state::load(&profile_dir);
    let info = DesktopInfo::new(
        env!("CARGO_PKG_VERSION"),
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        &server_url,
        profile_dir.display().to_string(),
        log_dir.display().to_string(),
    );

    let app_runtime = runtime.clone();
    let update_runtime = runtime.clone();
    let update_profile = profile_dir.clone();
    let update_logs = log_dir.clone();
    application().run(move |cx: &mut App| {
        let desktop = lazy_desktop::start_desktop(app_runtime, inputs, cx);
        let model = desktop.model.clone();
        set_desktop_info(cx, info);
        // A new install opens on the first-run screens (D29); someone who chose a server before,
        // or whose environment fixed one, opens on the sign-in card.
        lazy_ui::onboarding::enable(cx);
        // Looking for a newer release every half hour, and installing one when the person agrees
        // (D28). Before the window opens, so that the window's root finds the updater.
        lazy_desktop::updater::start_updates(&update_runtime, &update_profile, &update_logs, cx);
        // A Mac has a native menu bar; everywhere else the same menus are the sidebar's "Menu".
        if cfg!(target_os = "macos") {
            cx.set_menus(lazy_ui::commands::native_menus());
        }

        let placement = window_state::restore(
            saved_window.as_ref(),
            &window_state::display_rects(cx),
            window_state::can_place_window(),
        );
        let window_bounds = window_state::window_bounds(&placement, cx);
        let watched_profile = profile_dir.clone();
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(window_bounds),
                titlebar: Some(TitlebarOptions {
                    title: Some("Lazy".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(
                    px(window_state::MIN_SIZE.0),
                    px(window_state::MIN_SIZE.1),
                )),
                app_id: Some("lazy".into()),
                // GPUI takes a window icon on X11 only; the other platforms use the desktop
                // entry, the executable's resources, or the bundle.
                icon: lazy_ui::brand::app_icon(),
                ..Default::default()
            },
            move |window, cx| {
                lazy_ui::init(cx, window.appearance());
                window
                    .observe_window_appearance(|window, cx| {
                        theme::install(cx, window.appearance());
                        window.refresh();
                    })
                    .detach();
                WindowStateWatcher::watch(watched_profile, window, cx);
                cx.new(|cx| RootView::new(model, window, cx))
            },
        );
        match opened {
            // A notification's click raises this window; the count becomes its title.
            Ok(window) => desktop.attach(window.into(), cx),
            Err(error) => {
                tracing::error!(%error, "Lazy could not open its window");
                eprintln!("Lazy could not open its window: {error}");
                cx.quit();
                return;
            }
        }

        cx.on_window_closed(|cx, _window| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });

    // Dropping the last reference flushes pending saves and stops the runtime's tasks.
    drop(runtime);
    ExitCode::SUCCESS
}
