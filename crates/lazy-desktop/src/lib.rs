//! Wiring between the client runtime and the UI, shared by the desktop binary and the
//! end-to-end tests that drive the real views.
//!
//! The runtime performs side effects on its own threads; the UI model lives on GPUI's foreground
//! executor. [`start_model`] connects them: the model's effects go to the runtime, and everything
//! the runtime reports is delivered to the model on the foreground executor.

use std::rc::Rc;

use gpui::{App, AppContext as _, Entity};
use lazy_client_core::{Effect, Input};
use lazy_client_runtime::Runtime;
use lazy_client_runtime::async_channel::Receiver;
use lazy_ui::app_model::{AppModel, EffectSink};

/// Hands the core's effects to the runtime.
pub struct RuntimeSink(Rc<Runtime>);

impl RuntimeSink {
    pub fn new(runtime: Rc<Runtime>) -> Self {
        Self(runtime)
    }
}

impl EffectSink for RuntimeSink {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.execute(effects);
    }
}

/// Creates the app model around `runtime` and starts feeding it the runtime's reports.
///
/// The pump holds the model weakly, so it ends when the model is released or the runtime's
/// channel closes; it is detached because nothing needs to await it.
pub fn start_model(
    runtime: Rc<Runtime>,
    inputs: Receiver<Input>,
    cx: &mut App,
) -> Entity<AppModel> {
    let model = cx.new(|_| AppModel::new(Rc::new(RuntimeSink::new(runtime))));
    let pump = model.downgrade();
    cx.spawn(async move |cx| {
        while let Ok(input) = inputs.recv().await {
            if pump
                .update(cx, |model, cx| model.dispatch(input, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    model
}
