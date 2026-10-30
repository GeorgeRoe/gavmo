//! GAVMO daemon entry point (ssr-only). Serves the Leptos control panel and
//! its `#[server]` functions with the shared `AppState` as context, plus the
//! MCP tool gateway on a separate port.

#[cfg(feature = "ssr")]
mod daemon {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;

    use anyhow::{Context as _, Result, anyhow, bail};
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::routing::post;
    use gavmo::app::{App, shell};
    use gavmo::auth;
    use gavmo::registry::AppState;
    use leptos::prelude::*;
    use leptos_axum::{LeptosRoutes, generate_route_list, handle_server_fns_with_context};

    /// Startup configuration from the CLI and environment.
    struct Config {
        base_image: PathBuf,
        gcloud_dir: PathBuf,
    }

    impl Config {
        fn from_env() -> Result<Self> {
            let base_image = std::env::args()
                .nth(1)
                .or_else(|| std::env::var("GAVMO_BASE_IMAGE").ok())
                .map(PathBuf::from)
                .context(
                    "no base image. Pass it as the first argument or set \
                     GAVMO_BASE_IMAGE=/path/to/nixos.qcow2",
                )?;

            // No relative fallback: it would resolve against the daemon's
            // working directory and mount whatever lives there into every VM.
            let gcloud_dir = match std::env::var_os("GAVMO_GCLOUD_DIR") {
                Some(dir) => PathBuf::from(dir),
                None => dirs::config_dir()
                    .map(|c| c.join("gcloud"))
                    .context("cannot locate the user config dir; set GAVMO_GCLOUD_DIR")?,
            };
            if !gcloud_dir.is_absolute() {
                bail!(
                    "GAVMO_GCLOUD_DIR must be an absolute path, got {}",
                    gcloud_dir.display()
                );
            }

            Ok(Self {
                base_image,
                gcloud_dir,
            })
        }
    }

    pub(crate) async fn main() -> ExitCode {
        init_tracing();
        match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("{e:#}");
                ExitCode::FAILURE
            }
        }
    }

    /// Logs to stderr at `info` by default; override with `RUST_LOG`.
    fn init_tracing() {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }

    async fn run() -> Result<()> {
        let config = Config::from_env()?;
        let app_state = AppState::new(config.base_image, config.gcloud_dir)
            .context("failed to initialize GAVMO state")?;

        warn_if_missing_from_path(&["qemu-img", "qemu-system-x86_64"]);

        // Guests reach host loopback via 10.0.2.2, so 127.0.0.1 is no
        // boundary; the persisted token keeps guests out of `#[server]` fns.
        let ui_token = Arc::new(
            auth::load_or_create_token()
                .context("failed to load/create the control-panel session token")?,
        );

        let conf = get_configuration(None).context("failed to load Leptos configuration")?;
        let ui_addr = conf.leptos_options.site_addr;
        let ui_router = build_ui_router(conf.leptos_options, &app_state, &ui_token);

        // Separate port from the UI: guests must reach the bearer-authenticated
        // gateway (at 10.0.2.2) while the UI port stays session-cookie-gated.
        let mcp_router = Router::new().nest_service("/mcp", mcp::service(app_state));
        let mcp_addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, mcp::GAVMO_MCP_PORT));

        // Bind both before announcing anything, so a port clash fails fast.
        let mcp_listener = bind(mcp_addr, "MCP gateway").await?;
        let ui_listener = bind(ui_addr, "control panel").await?;
        tracing::info!("GAVMO MCP gateway listening on http://{mcp_addr}/mcp");
        tracing::info!("GAVMO daemon listening on http://{ui_addr}");
        announce_login_url(ui_addr.port(), &ui_token);

        // Either server stopping is fatal: a UI without its gateway (or vice
        // versa) would look healthy while silently failing.
        tokio::select! {
            res = axum::serve(mcp_listener, mcp_router.into_make_service()) => {
                res.context("MCP gateway server failed")?;
                Err(anyhow!("MCP gateway server exited unexpectedly"))
            }
            res = axum::serve(ui_listener, ui_router.into_make_service()) => {
                res.context("control panel server failed")?;
                Err(anyhow!("control panel server exited unexpectedly"))
            }
        }
    }

    async fn bind(addr: SocketAddr, what: &str) -> Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind the {what} to {addr}"))
    }

    fn build_ui_router(
        leptos_options: LeptosOptions,
        app_state: &AppState,
        ui_token: &Arc<String>,
    ) -> Router {
        let routes = generate_route_list(App);

        let server_fn_route = {
            let state = app_state.clone();
            move |req: Request<Body>| {
                let state = state.clone();
                async move {
                    handle_server_fns_with_context(move || provide_context(state.clone()), req)
                        .await
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
            .merge(auth::login_router(Arc::clone(ui_token)))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(ui_token),
                auth::require_session,
            ))
    }

    /// Prints the login URL to stdout (deliberately not via `tracing`, so the
    /// token doesn't end up in log collectors) and tries to open it.
    ///
    /// `localhost`, not `127.0.0.1`: WSL2 localhost-forwarding resolves it from
    /// the Windows browser. Printing is the reliable path; opening is a bonus.
    #[expect(clippy::print_stdout, reason = "the URL is for the user's eyes")]
    fn announce_login_url(port: u16, ui_token: &str) {
        let login_url = format!("http://localhost:{port}/login?token={ui_token}");
        println!(
            "\n\
             ==================================================================\n\
             GAVMO control panel — open this URL in your browser:\n\
             {login_url}\n\
             =================================================================="
        );
        try_open_browser(&login_url);
    }

    /// Warns (non-fatally) if any of `binaries` is missing from `$PATH`, so a
    /// missing qemu shows up in the startup log rather than on the first Open.
    fn warn_if_missing_from_path(binaries: &[&str]) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<_> = std::env::split_paths(&path).collect();

        let missing: Vec<&str> = binaries
            .iter()
            .filter(|bin| !dirs.iter().any(|dir| dir.join(bin).is_file()))
            .copied()
            .collect();

        if !missing.is_empty() {
            tracing::warn!(
                "{} not found on $PATH — booting a workspace will fail until qemu is \
                 available in this shell (e.g. `nix shell nixpkgs#qemu`)",
                missing.join(", ")
            );
        }
    }

    /// Best-effort browser launch: `wslview` (opens the Windows browser from
    /// WSL), then `xdg-open`. Failures are only logged at debug; the printed
    /// URL suffices.
    fn try_open_browser(url: &str) {
        for opener in ["wslview", "xdg-open"] {
            match std::process::Command::new(opener)
                .arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    // Reap it so it doesn't linger as a zombie until exit.
                    std::thread::spawn(move || {
                        if let Err(e) = child.wait() {
                            tracing::debug!(opener, error = %e, "failed to reap browser opener");
                        }
                    });
                    return;
                }
                Err(e) => tracing::debug!(opener, error = %e, "could not launch browser opener"),
            }
        }
    }
}

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    daemon::main().await
}

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // Client build has no server main; hydration happens via `lib::hydrate`.
}
