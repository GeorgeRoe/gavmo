//! GAVMO daemon entry point (ssr-only). Serves the Leptos control panel and
//! its `#[server]` functions with the shared `AppState` as context, plus the
//! MCP tool gateway on a separate port.

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::Request;
    use axum::routing::post;
    use axum::Router;
    use gavmo::app::*;
    use gavmo::auth;
    use gavmo::registry::AppState;
    use leptos::logging::log;
    use leptos::prelude::*;
    use leptos_axum::{generate_route_list, handle_server_fns_with_context, LeptosRoutes};

    // --- Configuration (CLI arg / env var) ---
    let base_image = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("GAVMO_BASE_IMAGE").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!(
                "error: no base image. Pass it as the first argument or set \
                 GAVMO_BASE_IMAGE=/path/to/nixos.qcow2"
            );
            std::process::exit(1);
        });

    let gcloud_dir = std::env::var("GAVMO_GCLOUD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::config_dir()
                .map(|c| c.join("gcloud"))
                .unwrap_or_else(|| PathBuf::from(".config/gcloud"))
        });

    let app_state = match AppState::new(base_image, gcloud_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to initialize GAVMO state: {e}");
            std::process::exit(1);
        }
    };

    warn_if_missing_from_path(&["qemu-img", "qemu-system-x86_64"]);

    // --- Control-panel session auth ---
    // Guests reach host loopback via 10.0.2.2, so 127.0.0.1 is no boundary;
    // the persisted token keeps guests out of `#[server]` functions.
    let ui_token = match auth::load_or_create_token() {
        Ok(t) => Arc::new(t),
        Err(e) => {
            eprintln!("error: failed to load/create the control-panel session token: {e}");
            std::process::exit(1);
        }
    };

    // --- Leptos / axum wiring ---
    let conf = get_configuration(None).unwrap();
    let addr = conf.leptos_options.site_addr;
    let leptos_options = conf.leptos_options;
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

    let app = Router::new()
        .route(
            "/api/{*fn_name}",
            post(server_fn_route.clone()).get(server_fn_route),
        )
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
        .merge(auth::login_router(ui_token.clone()))
        .layer(axum::middleware::from_fn_with_state(
            ui_token.clone(),
            auth::require_session,
        ));

    // --- MCP gateway ---
    // Separate port from the UI: guests must reach the bearer-authenticated
    // gateway (at 10.0.2.2) while the UI port stays session-cookie-gated.
    let mcp_router = Router::new().nest_service("/mcp", mcp::service(app_state.clone()));
    let mcp_addr = (std::net::Ipv4Addr::LOCALHOST, mcp::GAVMO_MCP_PORT);
    let mcp_listener = tokio::net::TcpListener::bind(mcp_addr).await.unwrap();
    log!(
        "GAVMO MCP gateway listening on http://127.0.0.1:{}/mcp",
        mcp::GAVMO_MCP_PORT
    );
    tokio::spawn(async move {
        axum::serve(mcp_listener, mcp_router.into_make_service())
            .await
            .unwrap();
    });

    // --- Login URL ---
    // `localhost`, not `127.0.0.1`: WSL2 localhost-forwarding resolves it from
    // the Windows browser. Printing it is the reliable path; opening is a bonus.
    let login_url = format!("http://localhost:{}/login?token={}", addr.port(), ui_token);
    log!(
        "\n\
         ==================================================================\n\
         GAVMO control panel — open this URL in your browser:\n\
         {login_url}\n\
         =================================================================="
    );
    try_open_browser(&login_url);

    log!("GAVMO daemon listening on http://{}", &addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

/// Warns (non-fatally) if any of `binaries` is missing from `$PATH`, so a
/// missing qemu shows up in the startup log rather than on the first Open.
#[cfg(feature = "ssr")]
fn warn_if_missing_from_path(binaries: &[&str]) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<_> = std::env::split_paths(&path).collect();

    let missing: Vec<&str> = binaries
        .iter()
        .filter(|bin| !dirs.iter().any(|dir| dir.join(bin).is_file()))
        .copied()
        .collect();

    if !missing.is_empty() {
        eprintln!(
            "warning: {} not found on $PATH — booting a workspace will fail \
             until qemu is available in this shell (e.g. `nix shell nixpkgs#qemu`)",
            missing.join(", ")
        );
    }
}

/// Best-effort browser launch: `wslview` (opens the Windows browser from
/// WSL), then `xdg-open`. Failures are silent; the printed URL suffices.
#[cfg(feature = "ssr")]
fn try_open_browser(url: &str) {
    for opener in ["wslview", "xdg-open"] {
        if std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // Client build has no server main; hydration happens via `lib::hydrate`.
}
