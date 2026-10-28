//! The Lazy desktop application.
//!
//! This crate only wires things together: it starts the runtime (HTTP, realtime, local storage),
//! creates the UI's [`AppModel`] around it, pumps the runtime's inputs into the model on the
//! foreground executor, and opens the window. Everything else lives in `lazy-ui`,
//! `lazy-client-core`, and `lazy-client-runtime`.

use std::process::ExitCode;
use std::rc::Rc;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;
use lazy_client_runtime::{Runtime, RuntimeConfig};
use lazy_ui::root_view::RootView;
use lazy_ui::theme;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("lazy=info")),
        )
        .init();

    let config = RuntimeConfig::from_env();
    tracing::info!(server = %config.server_url, "starting Lazy");
    let (runtime, inputs) = match Runtime::start(config) {
        Ok(started) => started,
        Err(error) => {
            eprintln!("Lazy could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = Rc::new(runtime);
    tracing::info!(profile = %runtime.profile_dir().display(), "using profile");

    let app_runtime = runtime.clone();
    application().run(move |cx: &mut App| {
        let model = lazy_desktop::start_model(app_runtime, inputs, cx);

        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Lazy".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(640.), px(480.))),
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
                cx.new(|cx| RootView::new(model, window, cx))
            },
        );
        if let Err(error) = opened {
            eprintln!("Lazy could not open its window: {error}");
            cx.quit();
            return;
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
