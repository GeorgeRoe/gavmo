//! The host-side model gateway: a supervised [Bifrost](https://github.com/maximhq/bifrost)
//! process that holds the user's provider credentials and translates
//! requests for them. Guests never reach it directly - only through the
//! token-checked `/llm` route on the gateway port (see `daemon::llm`).
//!
//! Bifrost runs with a generated, file-only `config.json` (no config store),
//! in which mode it has no inbound authentication at all and its admin API
//! is readable. It is therefore bound to `127.0.0.2`: guests reach the
//! host's loopback only as `10.0.2.2` -> `127.0.0.1`, so another loopback
//! address is out of their reach. Host processes can reach it, but they can
//! read the user's credentials file anyway.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

/// Loopback address Bifrost binds to; deliberately not `127.0.0.1`.
pub const BIND_ADDR: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

/// How long Bifrost may take to answer `/health` after starting. Without
/// local datasheets it downloads ~27 MB of pricing data first.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Restart backoff after Bifrost exits: doubles from the first to the last.
const RESTART_BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// A run that lasted this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Google Cloud Vertex AI settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpSettings {
    pub project: String,
    pub region: String,
}

impl GcpSettings {
    /// Validated settings. Project ids are 6-30 characters of lowercase
    /// letters, digits and hyphens, starting with a letter and not ending in
    /// a hyphen; regions are lowercase letters, digits and hyphens.
    pub fn new(project: &str, region: &str) -> Result<Self, String> {
        let project = project.trim();
        let region = region.trim();
        let valid_project = (6..=30).contains(&project.len())
            && project.starts_with(|c: char| c.is_ascii_lowercase())
            && !project.ends_with('-')
            && project
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid_project {
            return Err(format!(
                "{project:?} is not a valid Google Cloud project id"
            ));
        }
        let valid_region = !region.is_empty()
            && region.len() <= 40
            && region
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid_region {
            return Err(format!("{region:?} is not a valid Vertex AI region"));
        }
        Ok(Self {
            project: project.to_owned(),
            region: region.to_owned(),
        })
    }
}

/// Everything needed to run Bifrost.
#[derive(Debug, Clone)]
pub struct BifrostConfig {
    /// The `bifrost-http` executable.
    pub binary: PathBuf,
    /// Bifrost's app dir: holds the generated `config.json` and its log.
    pub app_dir: PathBuf,
    pub gcp: GcpSettings,
    /// The Application Default Credentials file Bifrost authenticates with
    /// (written by `gcloud auth application-default login`).
    pub credentials_file: PathBuf,
    /// Optional dir holding Bifrost's datasheets (`datasheet.json`,
    /// `model-parameters.json`), so it starts without downloading them.
    pub data_dir: Option<PathBuf>,
}

/// Bifrost's `config.json` for `cfg`: file-only (no config store, so no
/// database and no writable admin API), no request logging, Vertex with
/// ADC. Pure, for testing.
pub fn render_config(cfg: &BifrostConfig) -> serde_json::Value {
    let mut framework = serde_json::Map::new();
    framework.insert("mcp_library_sync_interval".to_owned(), json!(0));
    if let Some(dir) = &cfg.data_dir {
        framework.insert(
            "pricing".to_owned(),
            json!({
            "pricing_url": file_url(&dir.join("datasheet.json")),
            "model_parameters_url": file_url(&dir.join("model-parameters.json")),
            }),
        );
    }
    json!({
        "config_store": { "enabled": false },
        "logs_store": { "enabled": false },
        "disable_onboarding": true,
        "client": {
            "allow_direct_keys": false,
            "enable_logging": false,
            "disable_content_logging": true,
            "drop_excess_requests": false,
            "enforce_auth_on_inference": false,
        },
        "framework": framework,
        "providers": {
            "vertex": {
                "keys": [{
                    "name": "vertex-adc",
                    "value": "",
                    "models": ["*"],
                    "weight": 1.0,
                    "vertex_key_config": {
                        "project_id": cfg.gcp.project,
                        "region": cfg.gcp.region,
                        // Empty: Application Default Credentials, i.e. the
                        // file named by GOOGLE_APPLICATION_CREDENTIALS.
                        "auth_credentials": "",
                    },
                }],
            },
        },
    })
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Where model requests go, as far as the `/llm` route is concerned.
#[derive(Clone)]
pub enum Upstream {
    /// Bifrost isn't configured; requests fail with this explanation.
    NotConfigured(String),
    Bifrost(Arc<Supervisor>),
    /// A fixed URL, for tests.
    #[cfg(test)]
    Fixed(String),
}

impl Upstream {
    /// Base URL to forward to, or why there is none.
    pub fn base_url(&self) -> Result<String, String> {
        match self {
            Self::NotConfigured(why) => Err(why.clone()),
            Self::Bifrost(supervisor) => Ok(supervisor.base_url()),
            #[cfg(test)]
            Self::Fixed(url) => Ok(url.clone()),
        }
    }

    /// Stops Bifrost, if running.
    pub async fn shutdown(&self) {
        if let Self::Bifrost(supervisor) = self {
            supervisor.shutdown().await;
        }
    }
}

/// A running Bifrost and the task that restarts it if it dies.
pub struct Supervisor {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Supervisor {
    /// Writes Bifrost's config and starts it in the background on a free
    /// port on [`BIND_ADDR`]. Returns once it is spawned, not once it is
    /// ready; requests before then fail as "starting".
    pub fn start(cfg: BifrostConfig) -> io::Result<Arc<Self>> {
        check_executable(&cfg.binary)?;
        vm::create_private_dir_all(&cfg.app_dir).map_err(io::Error::other)?;
        let config = serde_json::to_vec_pretty(&render_config(&cfg)).map_err(io::Error::other)?;
        crate::atomic_file::write_atomically(&cfg.app_dir.join("config.json"), &config)?;
        let addr = free_port()?;
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(supervise(cfg, addr, stopped));
        Ok(Arc::new(Self { addr, task, stop }))
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Kills Bifrost and stops restarting it.
    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        // The task kills the child when it sees the stop; give it a moment.
        let finished = tokio::time::timeout(Duration::from_secs(5), async {
            while !self.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if finished.is_err() {
            tracing::warn!("Bifrost did not stop in time; aborting its supervisor");
        }
        self.task.abort();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Refuses anything that isn't an executable regular file, with a clear
/// message (instead of a spawn error in a background task).
fn check_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an executable file", path.display()),
        ));
    }
    Ok(())
}

/// A currently free port on [`BIND_ADDR`]. (Racy, but only another local
/// process grabbing it in between can break it; the restart loop retries.)
fn free_port() -> io::Result<SocketAddr> {
    std::net::TcpListener::bind((BIND_ADDR, 0))?.local_addr()
}

/// Runs Bifrost until told to stop, restarting it with backoff whenever it
/// exits.
async fn supervise(
    cfg: BifrostConfig,
    addr: SocketAddr,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = RESTART_BACKOFF.0;
    loop {
        let started = std::time::Instant::now();
        match spawn(&cfg, addr) {
            Ok(mut child) => {
                tokio::spawn(log_when_ready(addr));
                tokio::select! {
                    status = child.wait() => match status {
                        Ok(status) => tracing::warn!(%status, log = %log_path(&cfg).display(), "Bifrost exited"),
                        Err(e) => tracing::warn!(error = %e, "failed to wait for Bifrost"),
                    },
                    () = stop_requested(&mut stopped) => {
                        if let Err(e) = child.kill().await {
                            tracing::warn!(error = %e, "failed to stop Bifrost");
                        }
                        return;
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to start Bifrost"),
        }
        if started.elapsed() >= HEALTHY_RUN {
            backoff = RESTART_BACKOFF.0;
        }
        tracing::info!(in_secs = backoff.as_secs(), "restarting Bifrost");
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = stop_requested(&mut stopped) => return,
        }
        backoff = (backoff * 2).min(RESTART_BACKOFF.1);
    }
}

/// Resolves once a stop is requested (or the supervisor is gone).
async fn stop_requested(stopped: &mut tokio::sync::watch::Receiver<bool>) {
    // An error means the sender was dropped: stop as well.
    if stopped.wait_for(|stop| *stop).await.is_err() {
        tracing::debug!("Bifrost supervisor handle dropped");
    }
}

fn log_path(cfg: &BifrostConfig) -> PathBuf {
    cfg.app_dir.join("bifrost.log")
}

/// Starts one Bifrost process, its output appended to `bifrost.log`.
fn spawn(cfg: &BifrostConfig, addr: SocketAddr) -> io::Result<tokio::process::Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(cfg))?;
    let mut command = tokio::process::Command::new(&cfg.binary);
    command
        .arg("-app-dir")
        .arg(&cfg.app_dir)
        .arg("-host")
        .arg(addr.ip().to_string())
        .arg("-port")
        .arg(addr.port().to_string())
        .args(["-log-style", "json"])
        .env("GOOGLE_APPLICATION_CREDENTIALS", &cfg.credentials_file)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    vm::die_with_parent(&mut command);
    tracing::info!(binary = %cfg.binary.display(), %addr, "starting Bifrost");
    command.spawn()
}

/// Logs once Bifrost answers `/health` (or that it never did).
async fn log_when_ready(addr: SocketAddr) {
    let url = format!("http://{addr}/health");
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return;
    };
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if client
            .get(&url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            tracing::info!(%addr, "Bifrost is ready");
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tracing::warn!(%addr, "Bifrost did not become ready within {READY_TIMEOUT:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, binary: PathBuf) -> BifrostConfig {
        BifrostConfig {
            binary,
            app_dir: dir.join("bifrost"),
            gcp: GcpSettings::new("my-project-1", "global").unwrap(),
            credentials_file: dir.join("adc.json"),
            data_dir: None,
        }
    }

    #[test]
    fn gcp_settings_are_validated() {
        assert!(GcpSettings::new("my-project-1", "global").is_ok());
        assert_eq!(
            GcpSettings::new(" my-project-1 ", " us-east5 ").unwrap(),
            GcpSettings {
                project: "my-project-1".to_owned(),
                region: "us-east5".to_owned()
            }
        );
        for bad in [
            "short",
            "1starts-with-digit",
            "ends-with-",
            "Upper-case-1",
            "has_underscore",
            "a\"quote\"x",
            &"x".repeat(31),
        ] {
            assert!(GcpSettings::new(bad, "global").is_err(), "{bad}");
        }
        for bad in ["", "us east", "us\neast5", &"r".repeat(41)] {
            assert!(GcpSettings::new("my-project-1", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_config_is_file_only_without_logging_and_uses_adc() {
        let mut c = cfg(Path::new("/d"), PathBuf::from("/bin/bifrost"));
        let v = render_config(&c);
        assert_eq!(v["config_store"]["enabled"], false);
        assert_eq!(v["logs_store"]["enabled"], false);
        assert_eq!(v["client"]["allow_direct_keys"], false);
        assert_eq!(v["client"]["enable_logging"], false);
        let key = &v["providers"]["vertex"]["keys"][0]["vertex_key_config"];
        assert_eq!(key["project_id"], "my-project-1");
        assert_eq!(key["region"], "global");
        assert_eq!(key["auth_credentials"], "");
        assert!(v["framework"].get("pricing").is_none());

        c.data_dir = Some(PathBuf::from("/data"));
        let v = render_config(&c);
        assert_eq!(
            v["framework"]["pricing"]["pricing_url"],
            "file:///data/datasheet.json"
        );
        assert_eq!(
            v["framework"]["pricing"]["model_parameters_url"],
            "file:///data/model-parameters.json"
        );
    }

    #[test]
    fn upstream_reports_why_it_is_not_configured() {
        let upstream = Upstream::NotConfigured("set GAVMO_BIFROST".to_owned());
        assert_eq!(upstream.base_url(), Err("set GAVMO_BIFROST".to_owned()));
    }

    #[tokio::test]
    async fn a_missing_or_non_executable_binary_is_refused_up_front() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Supervisor::start(cfg(tmp.path(), tmp.path().join("nope")))
            .err()
            .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        let err = Supervisor::start(cfg(tmp.path(), plain)).err().unwrap();
        assert!(err.to_string().contains("not an executable"), "{err}");
    }

    /// Writes an executable shell script standing in for `bifrost-http`.
    fn fake_binary(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("fake-bifrost");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Bifrost is started with its config, bind address and credentials,
    /// restarted when it exits, and killed on shutdown.
    #[tokio::test]
    async fn bifrost_is_configured_restarted_and_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let pids = tmp.path().join("pids");
        // Records its args and env, then exits on the first run (to be
        // restarted) and stays up on the second.
        let script = format!(
            "echo \"$* GAC=$GOOGLE_APPLICATION_CREDENTIALS\" >> '{runs}'\n\
             echo $$ >> '{pids}'\n\
             [ \"$(wc -l < '{runs}')\" -ge 2 ] && exec sleep 600\n\
             exit 3",
            runs = runs.display(),
            pids = pids.display()
        );
        let c = cfg(tmp.path(), fake_binary(tmp.path(), &script));
        let supervisor = Supervisor::start(c.clone()).unwrap();
        assert!(supervisor.base_url().starts_with("http://127.0.0.2:"));

        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(c.app_dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(config, render_config(&c));

        wait_for("a restart", || {
            std::fs::read_to_string(&runs).is_ok_and(|r| r.lines().count() >= 2)
        })
        .await;
        let line = std::fs::read_to_string(&runs).unwrap();
        let first = line.lines().next().unwrap();
        let port = supervisor.base_url().rsplit(':').next().unwrap().to_owned();
        assert!(first.contains("-host 127.0.0.2"), "{first}");
        assert!(first.contains(&format!("-port {port}")), "{first}");
        assert!(
            first.contains(&format!("-app-dir {}", c.app_dir.display())),
            "{first}"
        );
        assert!(
            first.contains(&format!("GAC={}", c.credentials_file.display())),
            "{first}"
        );

        let pid = std::fs::read_to_string(&pids)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_owned();
        let alive = || Path::new(&format!("/proc/{pid}")).exists();
        assert!(alive());
        supervisor.shutdown().await;
        wait_for("the child to be killed", || {
            !alive()
                || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|s| s.contains(") Z "))
        })
        .await;
    }
}
