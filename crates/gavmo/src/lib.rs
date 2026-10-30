//! GAVMO daemon + Leptos control panel.
//!
//! Compiled twice by cargo-leptos: natively (`ssr`) as the axum server and
//! for `wasm32` (`hydrate`) as the browser bundle. Server-only modules and
//! dependencies (e.g. `vm`) must never reach the wasm build.

pub mod app;

#[cfg(feature = "ssr")]
pub mod auth;

#[cfg(feature = "ssr")]
pub mod integrations;

#[cfg(feature = "ssr")]
pub mod registry;

#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    use crate::app::App;
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
