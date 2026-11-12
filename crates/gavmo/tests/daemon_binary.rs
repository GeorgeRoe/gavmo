//! The `gavmo` binary itself, run as a process: startup configuration
//! errors, port clashes, and a full start/login/SIGTERM cycle. Everything it
//! writes goes to temp dirs (`HOME` and the XDG dirs point there), `PATH`
//! holds only a stand-in browser opener, and no VM is ever booted.
//!
//! Each environment gets its own free control-panel and MCP gateway ports
//! (`--ui-port`, `--mcp-port`), so the tests run in parallel and alongside a
//! real GAVMO daemon.
#![cfg(feature = "ssr")]

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// A daemon environment under one temp dir.
    struct Env {
        dir: tempfile::TempDir,
        ui_port: u16,
        mcp_port: u16,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            // Records the URL instead of opening a browser.
            let opener = bin.join("xdg-open");
            std::fs::write(
                &opener,
                format!(
                    "#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n",
                    dir.path().join("opened").display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
            for sub in ["home", "data", "config", "run", "gcloud", "site"] {
                std::fs::create_dir(dir.path().join(sub)).unwrap();
            }
            std::fs::set_permissions(
                dir.path().join("run"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            Self {
                dir,
                ui_port: free_port(),
                mcp_port: free_port(),
            }
        }

        fn path(&self, sub: &str) -> PathBuf {
            self.dir.path().join(sub)
        }

        /// The daemon command, configured for this environment (ports through
        /// the environment, so tests can still override them with flags).
        /// The coverage variables (`LLVM_PROFILE_FILE`) are inherited on
        /// purpose; inherited GAVMO port settings are not.
        fn command(&self) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_gavmo"));
            command
                .env_remove("GAVMO_BIFROST_PORT")
                .env("GAVMO_UI_PORT", self.ui_port.to_string())
                .env("GAVMO_MCP_PORT", self.mcp_port.to_string())
                .env("PATH", self.path("bin"))
                .env("HOME", self.path("home"))
                .env("XDG_DATA_HOME", self.path("data"))
                .env("XDG_CONFIG_HOME", self.path("config"))
                .env("XDG_RUNTIME_DIR", self.path("run"))
                .env("GAVMO_BASE_IMAGE", self.path("base.qcow2"))
                .env("GAVMO_GCLOUD_DIR", self.path("gcloud"))
                .env("LEPTOS_OUTPUT_NAME", "gavmo")
                .env("LEPTOS_SITE_ROOT", self.path("site"))
                .env("RUST_LOG", "debug")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command
        }
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Runs a daemon that is expected to exit by itself; returns its status and
    /// stderr.
    fn run_to_exit(mut command: Command) -> (ExitStatus, String) {
        let output = command.output().unwrap();
        (
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn assert_fails_with(command: Command, message: &str) {
        let (status, stderr) = run_to_exit(command);
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains(message),
            "expected {message:?} in:\n{stderr}"
        );
    }

    /// `--version` / `-V` print the version and exit, needing no
    /// configuration at all (no base image here).
    #[test]
    fn version_flag_prints_the_version_and_exits() {
        let env = Env::new();
        for flag in ["--version", "-V"] {
            let mut command = env.command();
            command.env_remove("GAVMO_BASE_IMAGE").arg(flag);
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("gavmo {}\n", env!("CARGO_PKG_VERSION"))
            );
        }
    }

    /// `--help` documents the port flags and their environment variables.
    #[test]
    fn help_lists_the_port_options() {
        let output = Env::new().command().arg("--help").output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        for expected in [
            "--ui-port <PORT>",
            "GAVMO_UI_PORT",
            "--mcp-port <PORT>",
            "GAVMO_MCP_PORT",
            "--bifrost-port <PORT>",
            "GAVMO_BIFROST_PORT",
            "[BASE_IMAGE]",
        ] {
            assert!(
                help.contains(expected),
                "{expected:?} missing from:\n{help}"
            );
        }
    }

    /// Bad port values are usage errors (status 2) naming the option, whether
    /// from a flag or the environment.
    #[test]
    fn invalid_ports_are_refused() {
        let env = Env::new();
        for (flag, value) in [
            ("--ui-port", "0"),
            ("--mcp-port", "65536"),
            ("--bifrost-port", "http"),
        ] {
            let mut command = env.command();
            command.arg(format!("{flag}={value}"));
            let (status, stderr) = run_to_exit(command);
            assert_eq!(status.code(), Some(2), "{stderr}");
            assert!(stderr.contains(flag), "{flag} {value}: {stderr}");
        }
        let mut command = env.command();
        command.env("GAVMO_MCP_PORT", "not-a-port");
        let (status, stderr) = run_to_exit(command);
        assert_eq!(status.code(), Some(2), "{stderr}");
        assert!(stderr.contains("--mcp-port"), "{stderr}");
    }

    #[test]
    fn startup_refuses_the_same_port_for_ui_and_mcp() {
        let env = Env::new();
        let mut command = env.command();
        command.arg(format!("--mcp-port={}", env.ui_port));
        assert_fails_with(command, "need different ports");
    }

    #[test]
    fn startup_refuses_a_missing_base_image() {
        let env = Env::new();
        let mut command = env.command();
        command.env_remove("GAVMO_BASE_IMAGE");
        assert_fails_with(command, "no base image");
    }

    #[test]
    fn the_base_image_can_also_be_the_first_argument() {
        let env = Env::new();
        let mut command = env.command();
        command
            .env_remove("GAVMO_BASE_IMAGE")
            .arg(env.path("base.qcow2"))
            // Fails later, on purpose: the argument was accepted.
            .env("LEPTOS_SITE_ADDR", "not an address");
        assert_fails_with(command, "failed to load Leptos configuration");
    }

    #[test]
    fn startup_refuses_a_relative_gcloud_dir() {
        let env = Env::new();
        let mut command = env.command();
        command.env("GAVMO_GCLOUD_DIR", "gcloud");
        assert_fails_with(command, "GAVMO_GCLOUD_DIR must be an absolute path");
    }

    /// Without `GAVMO_GCLOUD_DIR`, the default under the config dir is used.
    #[test]
    fn the_gcloud_dir_defaults_to_the_config_dir() {
        let env = Env::new();
        let mut command = env.command();
        command
            .env_remove("GAVMO_GCLOUD_DIR")
            .env("LEPTOS_SITE_ADDR", "not an address");
        assert_fails_with(command, "failed to load Leptos configuration");
    }

    #[test]
    fn startup_fails_if_the_data_dir_is_unusable() {
        let env = Env::new();
        std::fs::write(env.path("not-a-dir"), b"").unwrap();
        let mut command = env.command();
        command.env("XDG_DATA_HOME", env.path("not-a-dir"));
        assert_fails_with(command, "failed to initialize GAVMO state");
    }

    /// Reads the daemon's stdout on a thread, forwarding lines.
    fn stdout_lines(child: &mut Child) -> mpsc::Receiver<String> {
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        rx
    }

    /// Waits for the login URL the daemon prints once both servers are bound.
    fn login_url(lines: &mpsc::Receiver<String>) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let line = lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if line.starts_with("http://localhost:") {
                return line;
            }
        }
    }

    /// One HTTP/1.1 request; returns the status code and the raw response.
    fn http(port: u16, request: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, response)
    }

    fn wait_with_timeout(child: &mut Child, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the daemon did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn signal(child: &Child, name: &str) {
        let status = Command::new("kill")
            .args([format!("-{name}"), child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Port clashes on either server fail startup by name; then a daemon that
    /// can bind both serves the control panel and the gateway, prints and opens
    /// the login URL, and exits cleanly on SIGTERM and on SIGINT.
    #[test]
    fn the_daemon_binds_both_servers_serves_and_shuts_down_on_signals() {
        let env = Env::new();

        let mcp_holder = TcpListener::bind(("127.0.0.1", env.mcp_port)).unwrap();
        assert_fails_with(env.command(), "failed to bind the MCP gateway");
        drop(mcp_holder);

        let ui_holder = TcpListener::bind(("127.0.0.1", env.ui_port)).unwrap();
        assert_fails_with(env.command(), "failed to bind the control panel");
        drop(ui_holder);

        // The opener: `xdg-open` (after `wslview` fails to start), then
        // `wslview` itself (WSL), then none at all, which is not an error.
        let (xdg_open, wslview) = (env.path("bin/xdg-open"), env.path("bin/wslview"));
        for (signal_name, opener) in [
            ("TERM", Some(&xdg_open)),
            ("INT", Some(&wslview)),
            ("TERM", None),
        ] {
            if env.path("opened").exists() {
                std::fs::remove_file(env.path("opened")).unwrap();
            }
            if opener == Some(&wslview) {
                std::fs::rename(&xdg_open, &wslview).unwrap();
            } else if opener.is_none() {
                std::fs::remove_file(&wslview).unwrap();
            }
            let mut child = env.command().spawn().unwrap();
            let lines = stdout_lines(&mut child);
            let stderr = drain_stderr(&mut child);
            let url = login_url(&lines);
            let token = url.split("token=").nth(1).unwrap();
            assert_eq!(
                url,
                format!("http://localhost:{}/login?token={token}", env.ui_port)
            );

            let (status, response) = http(
                env.ui_port,
                &format!(
                    "GET /login?token={token} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                ),
            );
            assert!((300..400).contains(&status), "{response}");
            assert!(
                response.to_ascii_lowercase().contains("set-cookie:"),
                "{response}"
            );

            let (status, response) = http(env.mcp_port, UNAUTHENTICATED_MCP_REQUEST);
            assert_eq!(status, 401, "{response}");

            signal(&child, signal_name);
            let exit = wait_with_timeout(&mut child, Duration::from_secs(30));
            let stderr = stderr.join().unwrap();
            assert!(exit.success(), "{stderr}");
            assert!(
                stderr.contains(&format!("received SIG{signal_name}")),
                "{stderr}"
            );
            // `qemu` isn't on this PATH: warned about at startup, not fatal.
            assert!(stderr.contains("not found on $PATH"), "{stderr}");
            // The stand-in opener got the URL the daemon printed.
            if opener.is_some() {
                wait_for(&env.path("opened"));
            }
            let opened = std::fs::read_to_string(env.path("opened")).ok();
            assert_eq!(opened.as_deref(), opener.map(|_| url.as_str()));
        }
    }

    /// `--ui-port` / `--mcp-port` beat `GAVMO_UI_PORT` / `GAVMO_MCP_PORT`
    /// (which [`Env::command`] sets).
    #[test]
    fn port_flags_beat_the_environment() {
        let env = Env::new();
        let (ui_port, mcp_port) = (free_port(), free_port());
        let mut command = env.command();
        command.args([
            "--ui-port",
            &ui_port.to_string(),
            "--mcp-port",
            &mcp_port.to_string(),
        ]);
        let mut child = command.spawn().unwrap();
        let lines = stdout_lines(&mut child);
        let stderr = drain_stderr(&mut child);

        let url = login_url(&lines);
        assert!(
            url.starts_with(&format!("http://localhost:{ui_port}/login?token=")),
            "{url}"
        );
        let (status, response) = http(mcp_port, UNAUTHENTICATED_MCP_REQUEST);
        assert_eq!(status, 401, "{response}");
        for unused in [env.ui_port, env.mcp_port] {
            assert!(TcpStream::connect(("127.0.0.1", unused)).is_err());
        }

        signal(&child, "TERM");
        let exit = wait_with_timeout(&mut child, Duration::from_secs(30));
        let stderr = stderr.join().unwrap();
        assert!(exit.success(), "{stderr}");
        assert!(
            stderr.contains(&format!("listening on http://127.0.0.1:{mcp_port}/mcp")),
            "{stderr}"
        );
    }

    const UNAUTHENTICATED_MCP_REQUEST: &str =
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    /// Collects the daemon's stderr on a thread. Drained concurrently: debug
    /// logging would otherwise fill the pipe.
    fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<String> {
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut text = String::new();
            BufReader::new(stderr).read_to_string(&mut text).unwrap();
            text
        })
    }

    /// Waits for `path` to be written (the opener runs in the background).
    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::metadata(path).map_or(true, |m| m.len() == 0) {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
