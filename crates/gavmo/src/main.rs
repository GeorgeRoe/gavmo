//! GAVMO daemon entry point (ssr-only). Serves the Leptos control panel and
//! its `#[server]` functions with the shared `AppState` as context, plus the
//! MCP tool gateway on a separate port.

#[cfg(feature = "ssr")]
mod daemon {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::process::ExitCode;

    use anyhow::{Context as _, Result, anyhow, bail};
    use clap::Parser as _;
    use gavmo::auth;
    use gavmo::bifrost::{GatewayOptions, ModelGateway};
    use gavmo::registry::{AppState, ModelOptions};
    use leptos::prelude::get_configuration;

    /// Default control-panel port.
    const DEFAULT_UI_PORT: u16 = 3000;

    /// The GAVMO daemon: serves the control panel, the guest-facing MCP and
    /// model gateway, and supervises the model gateway's Bifrost process.
    ///
    /// Every option can also be set through the environment variable shown;
    /// a flag beats its variable. Further settings (`GAVMO_GCLOUD_DIR`,
    /// `GAVMO_BIFROST`, ...) are environment-only. Model providers are set up
    /// in the control panel (Settings); `GAVMO_GCP_PROJECT` and
    /// `GAVMO_GCP_REGION` override what is saved there.
    #[derive(clap::Parser, Debug)]
    #[command(name = "gavmo", version)]
    struct Cli {
        /// The NixOS base image (qcow2) that workspace VMs boot from.
        #[arg(env = "GAVMO_BASE_IMAGE", value_name = "BASE_IMAGE")]
        base_image: Option<PathBuf>,

        /// Port for the control panel (the web UI), on 127.0.0.1.
        #[arg(long, env = "GAVMO_UI_PORT", value_name = "PORT",
              default_value_t = DEFAULT_UI_PORT, value_parser = port_parser())]
        ui_port: u16,

        /// Port for the gateway guests reach for tools (/mcp) and models
        /// (/llm), on 127.0.0.1. Guests are told it at boot.
        #[arg(long, env = "GAVMO_MCP_PORT", value_name = "PORT",
              default_value_t = mcp::DEFAULT_MCP_PORT, value_parser = port_parser())]
        mcp_port: u16,

        /// Port for Bifrost, the model gateway's backend, on 127.0.0.2 (out of
        /// guests' reach) [default: any free port]
        #[arg(long, env = "GAVMO_BIFROST_PORT", value_name = "PORT", value_parser = port_parser())]
        bifrost_port: Option<u16>,
    }

    /// Ports 1-65535: port 0 would mean "any", which no URL can name.
    fn port_parser() -> clap::builder::RangedI64ValueParser<u16> {
        clap::value_parser!(u16).range(1..)
    }

    /// Startup configuration from the CLI and environment.
    #[derive(Debug)]
    struct Config {
        base_image: PathBuf,
        gcloud_dir: PathBuf,
        ui_port: u16,
        mcp_port: u16,
        bifrost_port: Option<u16>,
    }

    impl Config {
        fn new(cli: Cli) -> Result<Self> {
            let base_image = cli.base_image.context(
                "no base image. Pass it as the first argument or set \
                 GAVMO_BASE_IMAGE=/path/to/nixos.qcow2",
            )?;

            // Both on 127.0.0.1; Bifrost is on 127.0.0.2, so any port is fine
            // for it.
            if cli.ui_port == cli.mcp_port {
                bail!(
                    "the control panel and the MCP gateway need different ports, \
                     both are {} (see --ui-port and --mcp-port)",
                    cli.ui_port
                );
            }

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
                ui_port: cli.ui_port,
                mcp_port: cli.mcp_port,
                bifrost_port: cli.bifrost_port,
            })
        }
    }

    pub(crate) async fn main() -> ExitCode {
        // Exits on `--help`, `--version` and usage errors (status 2).
        let cli = Cli::parse();
        init_tracing();
        match run(cli).await {
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

    async fn run(cli: Cli) -> Result<()> {
        let config = Config::new(cli)?;
        let gateway = ModelGateway::new(
            gateway_options(config.bifrost_port),
            dirs::data_dir()
                .context("cannot locate the user data dir")?
                .join("gavmo")
                .join("bifrost"),
        );
        let app_state = AppState::new(
            config.base_image,
            &config.gcloud_dir,
            config.mcp_port,
            ModelOptions::from_env(gateway),
        )
        .context("failed to initialize GAVMO state")?;

        warn_if_missing_from_path(&["qemu-img", "qemu-system-x86_64"]);

        // Guests reach host loopback via 10.0.2.2, so 127.0.0.1 is no
        // boundary; sessions (from the host-only token) keep guests out of
        // `#[server]` fns.
        let auth = auth::Auth::load_default()
            .await
            .context("failed to load the control-panel token or sessions")?;

        let mut conf = get_configuration(None).context("failed to load Leptos configuration")?;
        // `--ui-port` decides the address, not Leptos's `site-addr`
        // (`LEPTOS_SITE_ADDR`), which is only a build-time setting here.
        let ui_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.ui_port));
        conf.leptos_options.site_addr = ui_addr;
        let ui_router = gavmo::daemon::ui_router(conf.leptos_options, &app_state, &auth);

        // Starts Bifrost if a provider is configured; otherwise guests' model
        // requests fail with the reason until one is added in the UI.
        app_state.apply_model_settings().await;

        // Separate port from the UI: guests must reach the bearer-authenticated
        // gateway (at 10.0.2.2) while the UI port stays session-cookie-gated.
        let mcp_router = gavmo::daemon::mcp_router(&app_state);
        let mcp_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.mcp_port));

        // Bind both before announcing anything, so a port clash fails fast.
        let mcp_listener = bind(mcp_addr, "MCP gateway").await?;
        let ui_listener = bind(ui_addr, "control panel").await?;
        tracing::info!("GAVMO MCP gateway listening on http://{mcp_addr}/mcp");
        tracing::info!("GAVMO daemon listening on http://{ui_addr}");
        announce_login_url(ui_addr.port(), auth.token());

        // The models guests are offered, from models.dev: fetched now if the
        // cached copy is stale, then daily (the task lives as long as the
        // daemon).
        let _catalog_refresh = app_state.spawn_model_catalog_refresh();

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
        outcome
    }

    /// The model gateway's settings from the environment: `GAVMO_BIFROST`
    /// (the `bifrost-http` binary, required) and `GAVMO_BIFROST_DATA_DIR`
    /// (optional, its datasheets), both absolute. Without them the gateway
    /// never runs and guests' model requests fail with the reason.
    fn gateway_options(port: Option<u16>) -> GatewayOptions {
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
        let paths = absolute("GAVMO_BIFROST").and_then(|binary| {
            let binary =
                binary.ok_or("GAVMO_BIFROST is not set (path to the bifrost-http binary)")?;
            Ok((binary, absolute("GAVMO_BIFROST_DATA_DIR")?))
        });
        match paths {
            Ok((binary, data_dir)) => GatewayOptions {
                binary: Ok(binary),
                data_dir,
                port,
            },
            Err(why) => GatewayOptions {
                port,
                ..GatewayOptions::disabled(&why)
            },
        }
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
