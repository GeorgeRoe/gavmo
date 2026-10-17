//! GAVMO daemon entry point (ssr-only). Boots an axum server that serves the
//! hydrated Leptos control panel and its `#[server]` functions, injecting the
//! shared [`AppState`] into every server-function call. See
//! `crates/gavmo/plan.md` §§6–7.

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

    // --- control-panel session auth ------------------------------------------
    // See `crates/gavmo/src/auth.rs`'s module doc for why this exists: QEMU's
    // user-mode networking means a workspace's guest VM can reach this
    // daemon's loopback listener via `10.0.2.2` exactly as easily as the
    // host's own browser can, so binding to `127.0.0.1` alone is not a
    // boundary against the guest — every `#[server]` function (host
    // filesystem browsing, workspace reset/remove, stored-credential
    // probing, etc.) was reachable from inside any workspace with no auth at
    // all. `ui_token` is this boundary's secret: host-only, persisted so a
    // browser's session cookie survives a daemon restart.
    let ui_token = match auth::load_or_create_token() {
        Ok(t) => Arc::new(t),
        Err(e) => {
            eprintln!("error: failed to load/create the control-panel session token: {e}");
            std::process::exit(1);
        }
    };

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
        .with_state(leptos_options)
        // `/login` merged in before the auth layer below, same as every
        // other route — it stays reachable with no cookie purely because
        // `auth::is_public_path` allowlists it by path, not because of
        // merge/layer ordering. One source of truth for "what's public"
        // rather than two (see `auth.rs`'s doc comment on `require_session`).
        .merge(auth::login_router(ui_token.clone()))
        .layer(axum::middleware::from_fn_with_state(
            ui_token.clone(),
            auth::require_session,
        ));

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

    // --- the login URL: the one thing a user actually needs ------------------
    // `localhost` (not `127.0.0.1`) deliberately: GAVMO's primary deployment
    // target is a WSL2 Ubuntu shell (see `crates/gavmo/plan.md`'s auth
    // section) — `localhost` is what Windows' WSL2 localhost-forwarding
    // resolves from the host browser's side, and it works identically for a
    // native Linux desktop too. This URL (not a bare "open your browser"
    // instruction) is the **primary, reliable** way a user authenticates —
    // `try_open_browser` below is only a convenience layered on top of it,
    // never a requirement.
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

/// Best-effort: launches the host's default browser at `url`. Tries
/// `wslview` first — the WSL-specific opener that hands off to the *Windows*
/// default browser, GAVMO's primary deployment target (see
/// `crates/gavmo/plan.md`'s auth section) — then falls back to `xdg-open`
/// for a native Linux desktop.
///
/// Failure here is never fatal and never even logged: the URL printed just
/// above this call is the **reliable, primary** way a user reaches the
/// control panel (same model as Jupyter's printed token URL) — this is
/// purely a convenience layered on top of it, so a headless host or a
/// missing opener binary silently just means the user copies the URL by
/// hand instead, exactly as if this function didn't exist.
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
