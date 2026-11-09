//! GAVMO daemon entry point (ssr-only). Serves the Leptos control panel and
//! its `#[server]` functions with the shared `AppState` as context, plus the
//! MCP tool gateway on a separate port.

#[cfg(feature = "ssr")]
mod daemon {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::process::ExitCode;

    use anyhow::{Context as _, Result, anyhow, bail};
    use gavmo::auth;
    use gavmo::bifrost::{BifrostConfig, GcpSettings, Supervisor, Upstream};
    use gavmo::registry::AppState;
    use leptos::prelude::get_configuration;

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
            // working directory and hand whatever lives there to the model gateway.
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
        let app_state = AppState::new(config.base_image, &config.gcloud_dir)
            .context("failed to initialize GAVMO state")?;

        warn_if_missing_from_path(&["qemu-img", "qemu-system-x86_64"]);

        // Guests reach host loopback via 10.0.2.2, so 127.0.0.1 is no
        // boundary; sessions (from the host-only token) keep guests out of
        // `#[server]` fns.
        let auth = auth::Auth::load_default()
            .await
            .context("failed to load the control-panel token or sessions")?;

        let conf = get_configuration(None).context("failed to load Leptos configuration")?;
        let ui_addr = conf.leptos_options.site_addr;
        let ui_router = gavmo::daemon::ui_router(conf.leptos_options, &app_state, &auth);

        let llm = start_model_gateway(&config.gcloud_dir);

        // Separate port from the UI: guests must reach the bearer-authenticated
        // gateway (at 10.0.2.2) while the UI port stays session-cookie-gated.
        let mcp_router = gavmo::daemon::mcp_router(&app_state, llm.clone());
        let mcp_addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, mcp::GAVMO_MCP_PORT));

        // Bind both before announcing anything, so a port clash fails fast.
        let mcp_listener = bind(mcp_addr, "MCP gateway").await?;
        let ui_listener = bind(ui_addr, "control panel").await?;
        tracing::info!("GAVMO MCP gateway listening on http://{mcp_addr}/mcp");
        tracing::info!("GAVMO daemon listening on http://{ui_addr}");
        announce_login_url(ui_addr.port(), auth.token());

        // Either server stopping is fatal: a UI without its gateway (or vice
        // versa) would look healthy while silently failing. Every exit path
        // stops the VMs first, so none outlives the daemon.
        let outcome = tokio::select! {
            res = axum::serve(mcp_listener, mcp_router.into_make_service()) => {
                res.context("MCP gateway server failed")
                    .and_then(|()| Err(anyhow!("MCP gateway server exited unexpectedly")))
            }
            res = axum::serve(ui_listener, ui_router.into_make_service()) => {
                res.context("control panel server failed")
                    .and_then(|()| Err(anyhow!("control panel server exited unexpectedly")))
            }
            signal = shutdown_signal() => {
                tracing::info!("received {signal}, shutting down");
                Ok(())
            }
        };
        app_state.shutdown(VM_SHUTDOWN_LIMIT).await;
        llm.shutdown().await;
        outcome
    }

    /// Starts Bifrost, the host-side model gateway, if configured:
    /// `GAVMO_BIFROST` (the `bifrost-http` binary) and `GAVMO_GCP_PROJECT`
    /// are required, `GAVMO_GCP_REGION` defaults to `global`, and
    /// `GAVMO_BIFROST_DATA_DIR` optionally holds its datasheets. Otherwise
    /// the daemon runs without one and guests' model requests fail with the
    /// reason (logged here too).
    fn start_model_gateway(gcloud_dir: &std::path::Path) -> Upstream {
        match model_gateway_config(gcloud_dir).and_then(|cfg| {
            Supervisor::start(cfg).map_err(|e| format!("cannot start Bifrost: {e}"))
        }) {
            Ok(supervisor) => Upstream::Bifrost(supervisor),
            Err(why) => {
                tracing::warn!("model gateway disabled: {why}");
                Upstream::NotConfigured(why)
            }
        }
    }

    fn model_gateway_config(
        gcloud_dir: &std::path::Path,
    ) -> std::result::Result<BifrostConfig, String> {
        let absolute = |var: &str| -> std::result::Result<Option<PathBuf>, String> {
            match std::env::var_os(var).map(PathBuf::from) {
                Some(p) if p.is_absolute() => Ok(Some(p)),
                Some(p) => Err(format!(
                    "{var} must be an absolute path, got {}",
                    p.display()
                )),
                None => Ok(None),
            }
        };
        let binary = absolute("GAVMO_BIFROST")?
            .ok_or("GAVMO_BIFROST is not set (path to the bifrost-http binary)")?;
        let project = std::env::var("GAVMO_GCP_PROJECT").map_err(|e| {
            format!("GAVMO_GCP_PROJECT (the Google Cloud project for Vertex AI): {e}")
        })?;
        let region = std::env::var("GAVMO_GCP_REGION").unwrap_or_else(|_| "global".to_owned());
        let gcp = GcpSettings::new(&project, &region)?;
        let credentials_file = gcloud_dir.join("application_default_credentials.json");
        if !credentials_file.is_file() {
            return Err(format!(
                "{} not found; run `gcloud auth application-default login`",
                credentials_file.display()
            ));
        }
        let app_dir = dirs::data_dir()
            .ok_or("no data dir")?
            .join("gavmo")
            .join("bifrost");
        Ok(BifrostConfig {
            binary,
            app_dir,
            gcp,
            credentials_file,
            data_dir: absolute("GAVMO_BIFROST_DATA_DIR")?,
        })
    }

    /// Upper bound on stopping all VMs at exit: one VM's graceful (30s) plus
    /// forced (5s) shutdown, with margin. VMs stop concurrently.
    const VM_SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(45);

    /// Resolves on SIGTERM or SIGINT (Ctrl-C), naming which. If the handlers
    /// can't be installed, never resolves (logged): the daemon then still
    /// relies on the kernel signalling VMs when it dies.
    async fn shutdown_signal() -> &'static str {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            },
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!(error = %e, "cannot install signal handlers");
                std::future::pending().await
            }
        }
    }

    async fn bind(addr: SocketAddr, what: &str) -> Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind the {what} to {addr}"))
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
