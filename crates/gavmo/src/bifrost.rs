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
//!
//! [`ModelGateway`] owns the running Bifrost and replaces it when the model
//! settings change (see [`crate::providers`] for what is configurable).

use std::ffi::OsString;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::json;

use crate::types::GatewayStatus;

/// Loopback address Bifrost binds to; deliberately not `127.0.0.1`.
pub const BIND_ADDR: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

/// How long Bifrost may take to answer `/health` after starting. Without
/// local datasheets it downloads ~27 MB of pricing data first.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Restart backoff after Bifrost exits: doubles from the first to the last.
const RESTART_BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// A run that lasted this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// One provider's part of Bifrost's configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBlock {
    /// Bifrost's name for the provider (`vertex`, `anthropic`, ...): the key
    /// under `providers`, and the prefix of the model names it routes.
    pub key: String,
    /// The `providers.<key>` entry.
    pub config: serde_json::Value,
    /// Environment variables Bifrost needs for it (e.g. a credentials file).
    pub env: Vec<(String, OsString)>,
}

/// Everything needed to run Bifrost.
#[derive(Debug, Clone)]
pub struct BifrostConfig {
    /// The `bifrost-http` executable.
    pub binary: PathBuf,
    /// Bifrost's app dir: holds the generated `config.json` and its log.
    pub app_dir: PathBuf,
    pub providers: Vec<ProviderBlock>,
    /// Optional dir holding Bifrost's datasheets (`datasheet.json`,
    /// `model-parameters.json`), so it starts without downloading them.
    pub data_dir: Option<PathBuf>,
    /// Port on [`BIND_ADDR`] to listen on (`--bifrost-port`); `None` picks a
    /// free one at startup.
    pub port: Option<u16>,
}

/// Bifrost's `config.json` for `cfg`: file-only (no config store, so no
/// database and no writable admin API), no request logging, and the
/// configured providers. Pure, for testing.
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
    let providers: serde_json::Map<String, serde_json::Value> = cfg
        .providers
        .iter()
        .map(|p| (p.key.clone(), p.config.clone()))
        .collect();
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
        "providers": providers,
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

    pub fn status(&self) -> GatewayStatus {
        match self {
            Self::NotConfigured(reason) => GatewayStatus::NotConfigured {
                reason: reason.clone(),
            },
            Self::Bifrost(supervisor) => supervisor.status(),
            #[cfg(test)]
            Self::Fixed(_) => GatewayStatus::Ready,
        }
    }

    /// Stops Bifrost, if running.
    pub async fn shutdown(&self) {
        if let Self::Bifrost(supervisor) = self {
            supervisor.shutdown().await;
        }
    }
}

/// Settings of the gateway that come from the daemon's command line and
/// environment rather than the control panel.
#[derive(Debug, Clone)]
pub struct GatewayOptions {
    /// The `bifrost-http` binary (`GAVMO_BIFROST`), or why there is none.
    pub binary: Result<PathBuf, String>,
    /// `GAVMO_BIFROST_DATA_DIR`, see [`BifrostConfig::data_dir`].
    pub data_dir: Option<PathBuf>,
    /// `--bifrost-port`, see [`BifrostConfig::port`].
    pub port: Option<u16>,
}

impl GatewayOptions {
    /// No Bifrost binary: the gateway never runs.
    pub fn disabled(reason: &str) -> Self {
        Self {
            binary: Err(reason.to_owned()),
            data_dir: None,
            port: None,
        }
    }
}

/// What the gateway should serve: the providers' blocks, or why it can't
/// serve anything.
pub type GatewayPlan = Result<Vec<ProviderBlock>, String>;

/// The model gateway as the rest of the daemon sees it: the current
/// [`Upstream`], replaced as a whole when the model settings change.
pub struct ModelGateway {
    options: GatewayOptions,
    app_dir: PathBuf,
    upstream: RwLock<Upstream>,
    /// What the current upstream was made from. Its lock serializes applies.
    applied: tokio::sync::Mutex<Applied>,
}

enum Applied {
    /// Before the first apply, or after a failed start (so it is retried).
    Nothing,
    Plan(GatewayPlan),
    /// Shut down, or fixed for tests: applies do nothing.
    Frozen,
}

impl ModelGateway {
    /// A gateway that serves nothing until the first [`Self::apply`].
    /// Bifrost's files go in `app_dir`.
    pub fn new(options: GatewayOptions, app_dir: PathBuf) -> Self {
        Self {
            options,
            app_dir,
            upstream: RwLock::new(Upstream::NotConfigured(
                "the model gateway hasn't started yet".to_owned(),
            )),
            applied: tokio::sync::Mutex::new(Applied::Nothing),
        }
    }

    /// A gateway fixed to `upstream`, for tests: applies leave it be.
    #[cfg(test)]
    pub fn fixed(upstream: Upstream) -> Self {
        Self {
            options: GatewayOptions::disabled("fixed"),
            app_dir: PathBuf::from("/nonexistent"),
            upstream: RwLock::new(upstream),
            applied: tokio::sync::Mutex::new(Applied::Frozen),
        }
    }

    /// Where requests go right now (a snapshot: a request in flight keeps
    /// using the upstream it started with).
    pub fn upstream(&self) -> Upstream {
        self.upstream
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn status(&self) -> GatewayStatus {
        self.upstream().status()
    }

    fn write_upstream(&self) -> std::sync::RwLockWriteGuard<'_, Upstream> {
        self.upstream
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Makes the gateway serve `plan`, restarting Bifrost if it differs from
    /// what is running (requests in flight on the old one are cut off). The
    /// old Bifrost is stopped before the new one starts, so a pinned
    /// `--bifrost-port` is free again. A missing binary or an empty plan
    /// leaves the gateway not configured, with the reason.
    pub async fn apply(&self, plan: GatewayPlan) {
        let mut applied = self.applied.lock().await;
        match &*applied {
            Applied::Frozen => return,
            Applied::Plan(current) if *current == plan => return,
            Applied::Plan(_) | Applied::Nothing => {}
        }
        let old = std::mem::replace(
            &mut *self.write_upstream(),
            Upstream::NotConfigured("the model gateway is restarting".to_owned()),
        );
        old.shutdown().await;
        // A failed start is retried on the next apply, even of the same plan.
        let (started, retry) = match (&self.options.binary, &plan) {
            (Err(why), _) | (Ok(_), Err(why)) => (Err(why.clone()), false),
            (Ok(_), Ok(blocks)) if blocks.is_empty() => {
                (Err("no model provider is configured".to_owned()), false)
            }
            (Ok(binary), Ok(blocks)) => match Supervisor::start(BifrostConfig {
                binary: binary.clone(),
                app_dir: self.app_dir.clone(),
                providers: blocks.clone(),
                data_dir: self.options.data_dir.clone(),
                port: self.options.port,
            }) {
                Ok(supervisor) => (Ok(Upstream::Bifrost(supervisor)), false),
                Err(e) => (Err(format!("cannot start Bifrost: {e}")), true),
            },
        };
        *self.write_upstream() = started.unwrap_or_else(|why| {
            tracing::warn!("model gateway disabled: {why}");
            Upstream::NotConfigured(why)
        });
        *applied = if retry {
            Applied::Nothing
        } else {
            Applied::Plan(plan)
        };
    }

    /// Stops Bifrost for good; later applies do nothing.
    pub async fn shutdown(&self) {
        let mut applied = self.applied.lock().await;
        *applied = Applied::Frozen;
        let old = std::mem::replace(
            &mut *self.write_upstream(),
            Upstream::NotConfigured("GAVMO is shutting down".to_owned()),
        );
        old.shutdown().await;
    }
}

/// A supervisor's view of its Bifrost, tagged with the run it is about so a
/// late report from an earlier run can't overwrite a newer one.
struct RunStatus {
    run: u64,
    status: GatewayStatus,
}

type SharedStatus = Arc<Mutex<RunStatus>>;

fn set_status(shared: &SharedStatus, run: u64, status: GatewayStatus) {
    let mut current = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.run == run {
        current.status = status;
    }
}

/// A running Bifrost and the task that restarts it if it dies.
pub struct Supervisor {
    addr: SocketAddr,
    status: SharedStatus,
    task: tokio::task::JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Supervisor {
    /// Writes Bifrost's config and starts it in the background on
    /// [`BifrostConfig::port`] (or a free port) on [`BIND_ADDR`]. Returns once
    /// it is spawned, not once it is ready; requests before then fail as
    /// "starting".
    pub fn start(cfg: BifrostConfig) -> io::Result<Arc<Self>> {
        check_executable(&cfg.binary)?;
        let addr = available_addr(cfg.port)?;
        vm::create_private_dir_all(&cfg.app_dir).map_err(io::Error::other)?;
        let config = serde_json::to_vec_pretty(&render_config(&cfg)).map_err(io::Error::other)?;
        crate::atomic_file::write_atomically(&cfg.app_dir.join("config.json"), &config)?;
        let status = Arc::new(Mutex::new(RunStatus {
            run: 0,
            status: GatewayStatus::Starting,
        }));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(supervise(cfg, addr, Arc::clone(&status), stopped));
        Ok(Arc::new(Self {
            addr,
            status,
            task,
            stop,
        }))
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn status(&self) -> GatewayStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone()
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

/// `port` on [`BIND_ADDR`] if it is currently free, or any free port for
/// `None`. (Racy, but only another local process grabbing it in between can
/// break it; the restart loop retries.)
fn available_addr(port: Option<u16>) -> io::Result<SocketAddr> {
    let wanted = SocketAddr::from((BIND_ADDR, port.unwrap_or(0)));
    std::net::TcpListener::bind(wanted)
        .and_then(|listener| listener.local_addr())
        .map_err(|e| io::Error::new(e.kind(), format!("cannot listen on {wanted}: {e}")))
}

/// Runs Bifrost until told to stop, restarting it with backoff whenever it
/// exits, and keeps `status` current.
async fn supervise(
    cfg: BifrostConfig,
    addr: SocketAddr,
    status: SharedStatus,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = RESTART_BACKOFF.0;
    for run in 0.. {
        {
            let mut current = status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            current.run = run;
            // A failure stays visible while the next run is starting.
            if run == 0 {
                current.status = GatewayStatus::Starting;
            }
        }
        let started = std::time::Instant::now();
        match spawn(&cfg, addr) {
            Ok(mut child) => {
                tokio::spawn(watch_ready(addr, Arc::clone(&status), run, log_path(&cfg)));
                tokio::select! {
                    exit = child.wait() => {
                        let why = match exit {
                            Ok(exit) => format!("Bifrost exited ({exit})"),
                            Err(e) => format!("failed to wait for Bifrost: {e}"),
                        };
                        tracing::warn!(log = %log_path(&cfg).display(), "{why}");
                        set_status(&status, run, GatewayStatus::Failing {
                            reason: format!("{why}; restarting it. See {}", log_path(&cfg).display()),
                        });
                    },
                    () = stop_requested(&mut stopped) => {
                        if let Err(e) = child.kill().await {
                            tracing::warn!(error = %e, "failed to stop Bifrost");
                        }
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to start Bifrost");
                set_status(
                    &status,
                    run,
                    GatewayStatus::Failing {
                        reason: format!("cannot start Bifrost: {e}"),
                    },
                );
            }
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
        .envs(cfg.providers.iter().flat_map(|p| p.env.iter().cloned()))
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    vm::die_with_parent(&mut command);
    tracing::info!(binary = %cfg.binary.display(), %addr, "starting Bifrost");
    command.spawn()
}

/// Marks run `run` ready once Bifrost answers `/health`, or failing if it
/// never does.
async fn watch_ready(addr: SocketAddr, status: SharedStatus, run: u64, log: PathBuf) {
    let url = format!("http://{addr}/health");
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return;
    };
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .run
            != run
        {
            return;
        }
        if client
            .get(&url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            tracing::info!(%addr, "Bifrost is ready");
            set_status(&status, run, GatewayStatus::Ready);
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tracing::warn!(%addr, "Bifrost did not become ready within {READY_TIMEOUT:?}");
    set_status(
        &status,
        run,
        GatewayStatus::Failing {
            reason: format!(
                "Bifrost did not become ready within {} s; see {}",
                READY_TIMEOUT.as_secs(),
                log.display()
            ),
        },
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn vertex_block(dir: &Path) -> ProviderBlock {
        crate::providers::vertex::block("my-project-1", "global", &dir.join("adc.json"))
    }

    fn cfg(dir: &Path, binary: PathBuf) -> BifrostConfig {
        BifrostConfig {
            binary,
            app_dir: dir.join("bifrost"),
            providers: vec![vertex_block(dir)],
            data_dir: None,
            port: None,
        }
    }

    #[test]
    fn the_config_is_file_only_without_logging_and_lists_the_providers() {
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
        c.providers.push(ProviderBlock {
            key: "anthropic".to_owned(),
            config: json!({ "keys": [] }),
            env: Vec::new(),
        });
        let v = render_config(&c);
        assert_eq!(
            v["framework"]["pricing"]["pricing_url"],
            "file:///data/datasheet.json"
        );
        assert_eq!(
            v["framework"]["pricing"]["model_parameters_url"],
            "file:///data/model-parameters.json"
        );
        assert_eq!(v["providers"]["anthropic"], json!({ "keys": [] }));
        assert!(v["providers"].get("vertex").is_some());
    }

    #[test]
    fn upstream_reports_why_it_is_not_configured() {
        let upstream = Upstream::NotConfigured("set GAVMO_BIFROST".to_owned());
        assert_eq!(upstream.base_url(), Err("set GAVMO_BIFROST".to_owned()));
        assert_eq!(
            upstream.status(),
            GatewayStatus::NotConfigured {
                reason: "set GAVMO_BIFROST".to_owned()
            }
        );
        assert_eq!(
            Upstream::Fixed("u".to_owned()).status(),
            GatewayStatus::Ready
        );
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
    pub(crate) fn fake_binary(dir: &Path, script: &str) -> PathBuf {
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

    /// Bifrost is started with its config, bind address and the providers'
    /// environment, restarted when it exits (reported as failing meanwhile),
    /// and killed on shutdown.
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

        wait_for("a failure report", || {
            matches!(supervisor.status(), GatewayStatus::Failing { reason } if reason.contains("exited"))
        })
        .await;
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
            first.contains(&format!("GAC={}", tmp.path().join("adc.json").display())),
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

    /// Starting until `/health` answers, then ready. The fake Bifrost doesn't
    /// listen, so the test answers `/health` on its port instead.
    #[tokio::test]
    async fn bifrost_is_reported_ready_once_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind((BIND_ADDR, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let supervisor = Supervisor::start(BifrostConfig {
            port: Some(port),
            ..cfg(tmp.path(), fake_binary(tmp.path(), "exec sleep 600"))
        })
        .unwrap();
        assert_eq!(supervisor.status(), GatewayStatus::Starting);
        let health = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind((BIND_ADDR, port))
            .await
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, health).await.unwrap() });
        wait_for("ready", || supervisor.status() == GatewayStatus::Ready).await;
        supervisor.shutdown().await;
    }

    /// `--bifrost-port` pins the port (still on 127.0.0.2); one already in
    /// use is refused up front rather than left to fail in the background.
    #[tokio::test]
    async fn a_configured_port_is_used_or_refused_if_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = fake_binary(tmp.path(), "exec sleep 600");
        let port = std::net::TcpListener::bind((BIND_ADDR, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let supervisor = Supervisor::start(BifrostConfig {
            port: Some(port),
            ..cfg(tmp.path(), binary.clone())
        })
        .unwrap();
        assert_eq!(supervisor.base_url(), format!("http://127.0.0.2:{port}"));
        supervisor.shutdown().await;

        let taken = std::net::TcpListener::bind((BIND_ADDR, 0)).unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let Err(e) = Supervisor::start(BifrostConfig {
            port: Some(taken_port),
            ..cfg(tmp.path(), binary)
        }) else {
            panic!("started on a port in use");
        };
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
        assert!(
            e.to_string().contains(&format!("127.0.0.2:{taken_port}")),
            "{e}"
        );
    }

    fn not_configured(gateway: &ModelGateway) -> String {
        match gateway.status() {
            GatewayStatus::NotConfigured { reason } => reason,
            other => panic!("expected not configured, got {other:?}"),
        }
    }

    /// The gateway restarts Bifrost only when the plan changes, reusing a
    /// pinned port; it explains a missing binary or empty plan; a failed
    /// start is retried; nothing starts after shutdown.
    #[tokio::test]
    async fn the_gateway_applies_plans_and_restarts_only_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let binary = fake_binary(
            tmp.path(),
            &format!("echo run >> '{}'\nexec sleep 600", runs.display()),
        );
        let port = std::net::TcpListener::bind((BIND_ADDR, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let gateway = ModelGateway::new(
            GatewayOptions {
                binary: Ok(binary),
                data_dir: None,
                port: Some(port),
            },
            tmp.path().join("bifrost"),
        );
        assert!(not_configured(&gateway).contains("hasn't started"));
        let run_count = || std::fs::read_to_string(&runs).map_or(0, |r| r.lines().count());

        gateway.apply(Err("add a provider".to_owned())).await;
        assert_eq!(not_configured(&gateway), "add a provider");
        gateway.apply(Ok(Vec::new())).await;
        assert_eq!(not_configured(&gateway), "no model provider is configured");

        let plan = Ok(vec![vertex_block(tmp.path())]);
        gateway.apply(plan.clone()).await;
        assert_eq!(
            gateway.upstream().base_url(),
            Ok(format!("http://127.0.0.2:{port}"))
        );
        wait_for("the first run", || run_count() == 1).await;
        gateway.apply(plan).await;

        // A changed plan restarts it on the same port.
        let mut changed = vertex_block(tmp.path());
        changed.config["keys"][0]["vertex_key_config"]["region"] = json!("us-east5");
        gateway.apply(Ok(vec![changed.clone()])).await;
        wait_for("the second run", || run_count() == 2).await;
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join("bifrost/config.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["providers"]["vertex"]["keys"][0]["vertex_key_config"]["region"],
            "us-east5"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(run_count(), 2, "an unchanged plan must not restart Bifrost");

        // A start that fails (port taken) is reported, then retried.
        gateway.apply(Err("off".to_owned())).await;
        let holder = std::net::TcpListener::bind((BIND_ADDR, port)).unwrap();
        gateway.apply(Ok(vec![changed.clone()])).await;
        assert!(not_configured(&gateway).contains("cannot start Bifrost"));
        drop(holder);
        gateway.apply(Ok(vec![changed.clone()])).await;
        assert!(gateway.upstream().base_url().is_ok());

        gateway.shutdown().await;
        assert!(not_configured(&gateway).contains("shutting down"));
        gateway.apply(Ok(vec![vertex_block(tmp.path())])).await;
        assert!(not_configured(&gateway).contains("shutting down"));
    }

    #[tokio::test]
    async fn without_a_binary_the_gateway_says_why() {
        let gateway = ModelGateway::new(
            GatewayOptions::disabled("GAVMO_BIFROST is not set"),
            PathBuf::from("/nonexistent"),
        );
        gateway
            .apply(Ok(vec![vertex_block(Path::new("/nonexistent"))]))
            .await;
        assert_eq!(not_configured(&gateway), "GAVMO_BIFROST is not set");
    }
}
