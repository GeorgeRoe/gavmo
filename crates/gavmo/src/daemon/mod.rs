//! Assembly of the daemon's two HTTP services, shared by `main` and the
//! end-to-end tests so both exercise exactly the same routing and layers.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::routing::post;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes, generate_route_list, handle_server_fns_with_context};

use crate::app::{App, shell};
use crate::auth;
use crate::registry::AppState;

mod body_limit;
mod llm;

#[cfg(test)]
mod boot_tests;
#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
mod tests;

/// The control panel: Leptos pages, `/api/*` server functions, `/login` and
/// `/logout`, all behind the session layer.
pub fn ui_router(
    leptos_options: LeptosOptions,
    app_state: &AppState,
    auth: &Arc<auth::Auth>,
) -> Router {
    let routes = generate_route_list(App);

    let server_fn_route = {
        let state = app_state.clone();
        move |req: Request<Body>| {
            let state = state.clone();
            async move {
                handle_server_fns_with_context(move || provide_context(state.clone()), req).await
            }
        }
    };

    Router::new()
        // POST only; `auth::require_session` also refuses other methods.
        .route("/api/{*fn_name}", post(server_fn_route))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                let state = app_state.clone();
                move || provide_context(state.clone())
            },
            {
                let leptos_options = leptos_options.clone();
                move || shell(leptos_options.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_options)
        // `/login` sits behind the auth layer too; it stays reachable only
        // because `auth::is_public_path` allowlists it, not via layer order.
        .merge(auth::login_router(Arc::clone(auth)))
        // Inside the auth layer: a request without a session is refused before
        // its body is read at all.
        .layer(axum::middleware::from_fn_with_state(
            body_limit::BodyLimits::UI,
            body_limit::buffer_body,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(auth),
            auth::require_session,
        ))
}

/// The guest-facing gateway, on its own port: guests reach it (at 10.0.2.2)
/// with their workspace token, while the UI port stays session-gated.
/// `/mcp` is the tool gateway; `/llm` forwards model requests to `llm`.
pub fn mcp_router(app_state: &AppState, llm: crate::bifrost::Upstream) -> Router {
    Router::new()
        .nest_service("/mcp", mcp::service(app_state.clone()))
        .nest("/llm", llm::router(Arc::new(app_state.clone()), llm))
}
