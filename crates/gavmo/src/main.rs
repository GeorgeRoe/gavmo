//! GAVMO daemon entry point (ssr-only). Boots an axum server that serves the
//! hydrated Leptos control panel and its `#[server]` functions, injecting the
//! shared [`AppState`] into every server-function call. See
//! `crates/gavmo/plan.md` §§6–7.

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use std::path::PathBuf;

    use axum::body::Body;
    use axum::extract::Request;
    use axum::routing::post;
    use axum::Router;
    use gavmo::app::*;
    use gavmo::registry::AppState;
    use leptos::logging::log;
    use leptos::prelude::*;
    use leptos_axum::{generate_route_list, handle_server_fns_with_context, LeptosRoutes};

    // --- configuration (CLI arg / env var; see plan §2) ---------------------
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

    // --- leptos / axum wiring ----------------------------------------------
    let conf = get_configuration(None).unwrap();
    let addr = conf.leptos_options.site_addr;
    let leptos_options = conf.leptos_options;
    let routes = generate_route_list(App);

    // Server functions get the AppState injected as Leptos context.
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
        .with_state(leptos_options);

    // --- MCP gateway (brief Phase 4 — see `crates/mcp/plan.md`) -------------
    // A dedicated loopback listener, deliberately separate from the UI's
    // `addr` above: keeps the (currently unauthenticated) control-panel API
    // and the (always bearer-token-authenticated) tool gateway on different
    // ports, so the UI port can later be restricted from guest access
    // without touching MCP reachability. The guest reaches this at
    // `http://10.0.2.2:<mcp::GAVMO_MCP_PORT>/mcp` (QEMU user-mode
    // networking's host address) via the managed `mcp.gavmo-tools` entry in
    // `nix/base/configuration.nix`'s `opencode.json`.
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

    log!("GAVMO daemon listening on http://{}", &addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

/// Warns (non-fatally) if any of `binaries` can't be found on `$PATH`. The
/// `vm` crate spawns `qemu-img`/`qemu-system-x86_64` by bare name; if
/// they're missing, `open_workspace` fails per-workspace with a "failed to
/// spawn" error surfaced in the UI — this check just makes the likely cause
/// obvious in the daemon's own startup log instead of only showing up on
/// the first Open click.
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

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // Client build has no server main; hydration happens via `lib::hydrate`.
}
