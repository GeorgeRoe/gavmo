//! GAVMO daemon + Leptos control panel (M3).
//!
//! A single crate compiled twice by cargo-leptos: natively (the `ssr`
//! feature) as the axum server binary, and for `wasm32` (the `hydrate`
//! feature) as the browser bundle. The VM-management `vm` crate is an
//! `ssr`-only optional dependency and must never reach the wasm build —
//! see `crates/gavmo/plan.md` §5.

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
    use crate::app::*;
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
