//! Lazy's web client (D47): the shared client core ([`lazy_client_core`]) running in the browser,
//! served by the Lazy server under `/web/`.
//!
//! * [`runtime`] carries out the core's effects with `fetch`, a WebSocket per workspace
//!   ([`realtime`]), and `localStorage` ([`store`]).
//! * [`view`] draws the screens as HTML from the core's state (pure functions, tested without a
//!   browser).
//! * [`app`] holds the core, feeds it inputs, puts the views on the page, and turns what the person
//!   does into inputs.

mod api;
mod app;
mod realtime;
mod runtime;
mod store;
mod view;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    app::start();
}
