//! The Lazy desktop application.
//!
//! This crate only wires things together: it starts the runtime (HTTP, realtime, local storage),
//! creates the UI's [`AppModel`] around it, pumps the runtime's inputs into the model on the
//! foreground executor, and opens the window where it was last. Everything else lives in
//! `lazy-ui`, `lazy-client-core`, and `lazy-client-runtime`.

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

fn main() -> ExitCode {
    // The terminal now; the file in the profile folder as soon as the runtime says where that is.
    let log = logging::init();

    let config = RuntimeConfig::from_env();
    tracing::info!(server = %config.server_url, "starting Lazy");
    let server_url = config.server_url.clone();
    let (runtime, inputs) = match Runtime::start(config) {
        Ok(started) => started,
        Err(error) => {
            eprintln!("Lazy could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = Rc::new(runtime);
    let profile_dir = runtime.profile_dir().to_path_buf();
    let log_dir = logging::log_dir(&profile_dir);
    if let Err(error) = log.attach(&log_dir) {
        eprintln!(
            "Lazy: could not open its log file in {}: {error}",
            log_dir.display()
        );
    }
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
    application().run(move |cx: &mut App| {
        let desktop = lazy_desktop::start_desktop(app_runtime, inputs, cx);
        let model = desktop.model.clone();
        set_desktop_info(cx, info);
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
