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
}

impl EventEmitter<AppEvent> for AppModel {}

impl AppModel {
    pub fn new(sink: Rc<dyn EffectSink>) -> Self {
        Self {
            core: ClientCore::new(),
            sink,
        }
    }

    /// The current read model.
    pub fn core(&self) -> &ClientCore {
        &self.core
    }

    /// Applies an input from a person or the runtime.
    pub fn dispatch(&mut self, input: Input, cx: &mut Context<Self>) {
        let output = self.core.handle(input);
        if !output.effects.is_empty() {
            self.sink.execute(output.effects);
        }
        if !output.changes.is_empty() {
            cx.emit(AppEvent::Changed(output.changes));
            cx.notify();
        }
    }
}
