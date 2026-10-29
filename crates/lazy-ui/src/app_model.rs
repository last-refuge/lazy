//! The single owner of client state inside the UI process.
//!
//! [`AppModel`] holds the [`ClientCore`] and forwards its effects to an [`EffectSink`] (the
//! runtime, in the real app). Views never mutate the core directly: they call
//! [`AppModel::dispatch`], then react to the [`AppEvent`] it emits. Keeping one owner means every
//! person action and every runtime report passes through the same state machine.

use std::rc::Rc;

use gpui::{Context, EventEmitter};
use lazy_client_core::{Change, ClientCore, Effect, Input};

/// Executes the work the core asks for. Implemented by the runtime in the application and by a
/// recorder in tests. Must not block.
pub trait EffectSink {
    fn execute(&self, effects: Vec<Effect>);
}

/// Emitted after the core handled an input.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Read-model changes, in order. Views apply them and then read the core.
    Changed(Vec<Change>),
}

pub struct AppModel {
    core: ClientCore,
    sink: Rc<dyn EffectSink>,
    /// The width the person last dragged the thread pane to, kept for the session. A wish, not
    /// the drawn width: see [`crate::thread_layout::pane_width`].
    thread_pane_width: f32,
}

impl EventEmitter<AppEvent> for AppModel {}

impl AppModel {
    pub fn new(sink: Rc<dyn EffectSink>) -> Self {
        Self {
            core: ClientCore::new(),
            sink,
            thread_pane_width: crate::thread_layout::DEFAULT_PANE_WIDTH,
        }
    }

    /// The thread pane width the person wants, before it is fitted to the window.
    pub fn thread_pane_width(&self) -> f32 {
        self.thread_pane_width
    }

    /// Remembers the thread pane width for the rest of the session.
    pub fn set_thread_pane_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(
            crate::thread_layout::MIN_PANE_WIDTH,
            crate::thread_layout::MAX_PANE_WIDTH,
        );
        if (width - self.thread_pane_width).abs() > f32::EPSILON {
            self.thread_pane_width = width;
            cx.notify();
        }
    }

    /// The current read model.
    pub fn core(&self) -> &ClientCore {
        &self.core
    }

    /// Applies an input from a person or the runtime.
    pub fn dispatch(&mut self, input: Input, cx: &mut Context<Self>) {
        // Starting restores the saved choices without announcing them as a change.
        let started = matches!(input, Input::Started(_));
        let output = self.core.handle(input);
        if !output.effects.is_empty() {
            self.sink.execute(output.effects);
        }
        // How this device looks and moves is the core's to keep and the screen's to show.
        if started
            || output
                .changes
                .iter()
                .any(|change| matches!(change, Change::AppearanceChanged))
        {
            crate::theme::apply_appearance(cx, self.core.appearance());
        }
        if !output.changes.is_empty() {
            cx.emit(AppEvent::Changed(output.changes));
            cx.notify();
        }
    }
}
