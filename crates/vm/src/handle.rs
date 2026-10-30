use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::process::Child;

use crate::config::VmConfig;
use crate::disk::ensure_overlay;
use crate::error::{Error, Result};
use crate::fsutil::{create_private_dir_all, remove_if_exists_async};
use crate::health::{probe_once, wait_for_ready};
use crate::qemu::spawn_qemu;
use crate::qmp;
use crate::share::{ShareBackend, ShareSpec, VirtiofsShare};

/// How long to wait for `opencode web` to become reachable during boot.
/// Also covers the in-guest `gavmo-rebuild` of the repo's `gavmo.nix`,
/// which runs before `opencode web` starts.
const BOOT_HEALTH_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a graceful ACPI shutdown before force-killing.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait after a forced `quit` before giving up and sending
/// SIGKILL directly.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the guest's balloon driver reports memory stats to qemu; the
/// control panel's Memory stat is only as fresh as this.
const BALLOON_STATS_INTERVAL_SECS: u32 = 2;

// Guest-visible virtiofs mount tags. Must match the mounts in
// `nix/base/configuration.nix`.
const MOUNT_TAG_WORKSPACE: &str = "workspace";
const MOUNT_TAG_GCLOUD: &str = "gcloud";
const MOUNT_TAG_MCP: &str = "mcp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmStatus {
    /// Not running (either never booted, or cleanly stopped).
    Stopped,
    /// The qemu process is alive but `opencode web` isn't answering yet.
    Booting,
    /// `opencode web` is reachable at `url`.
    Running { url: String },
    /// The qemu process exited unexpectedly or a health check failed hard.
    Failed { reason: String },
}

/// A booted VM: owns the qemu process, its `virtiofsd` helpers, and the
/// config it was booted with.
///
/// Dropping without [`RunningVm::stop`] still kills qemu (`kill_on_drop`),
/// but skips graceful shutdown and helper cleanup - prefer `stop()`.
pub struct RunningVm {
    cfg: VmConfig,
    qemu: Child,
    helpers: Vec<Child>,
    /// When qemu was spawned, not when `opencode web` first answered (which
    /// can be minutes later), so uptime matches what the boot console shows.
    started: Instant,
}

impl RunningVm {
    /// Boots a VM per `cfg` and waits for `opencode web` to answer on the
    /// forwarded host port.
    pub async fn boot(cfg: VmConfig) -> Result<Self> {
        // Owner-only, so other local users can't read the disk or reach the
        // sockets. (`ensure_overlay` does the same for `state_dir`.)
        create_private_dir_all(&cfg.runtime_dir)?;
        ensure_overlay(&cfg.base_image, &cfg.disk_path()).await?;

        let virtiofs = VirtiofsShare;
        let workspace_share = ShareSpec {
            host_path: cfg.repo_dir.clone(),
            mount_tag: MOUNT_TAG_WORKSPACE,
            read_only: false,
        };
        let gcloud_share = ShareSpec {
            host_path: cfg.gcloud_dir.clone(),
            mount_tag: MOUNT_TAG_GCLOUD,
            read_only: true,
        };
        let mcp_share = ShareSpec {
            host_path: cfg.mcp_dir.clone(),
            mount_tag: MOUNT_TAG_MCP,
            read_only: true,
        };
        let shares: Vec<(&dyn ShareBackend, ShareSpec)> = vec![
            (&virtiofs, workspace_share),
            (&virtiofs, gcloud_share),
            (&virtiofs, mcp_share),
        ];

        // Clear a stale rebuild-failure marker so a previous boot's result
        // isn't mistaken for this one's. NotFound is the common case; any
        // other failure must abort, or the stale marker would immediately
        // (and wrongly) fail this boot.
        remove_if_exists_async(&cfg.rebuild_failed_marker_path()).await?;

        let (qemu, helpers) = spawn_qemu(&cfg, &shares).await?;
        let host_port = cfg.host_port;
        let marker_path = cfg.rebuild_failed_marker_path();
        let mut vm = RunningVm {
            cfg,
            qemu,
            helpers,
            started: Instant::now(),
        };

        // On `Err`, `vm` is dropped and `kill_on_drop` tears down qemu; the
        // rebuild log is already on disk in `guest_output_dir`.
        wait_ready_or_exit(
            &mut vm.qemu,
            &vm.cfg.qemu_log_path(),
            wait_for_ready(host_port, &marker_path, BOOT_HEALTH_TIMEOUT),
        )
        .await?;

        // Best-effort: without balloon stats the VM runs fine, it just shows
        // no live memory usage. Not worth failing a healthy boot over.
        if let Err(e) =
            qmp::set_balloon_stats_interval(&vm.cfg.qmp_socket_path(), BALLOON_STATS_INTERVAL_SECS)
                .await
        {
            tracing::warn!(
                workspace = %vm.cfg.workspace_id,
                error = %e,
                "could not enable balloon stats; guest memory usage will be unavailable"
            );
        }

        Ok(vm)
    }

    /// Requests a graceful shutdown via QMP (ACPI powerdown), falling back
    /// to a forced `quit` and then SIGKILL if the guest doesn't exit in
    /// time. The overlay disk is always preserved.
    ///
    /// Every helper is torn down even if an earlier step fails; the first
    /// failure to actually terminate a process is returned.
    pub async fn stop(mut self) -> Result<()> {
        let workspace = self.cfg.workspace_id.clone();
        let qmp_sock = self.cfg.qmp_socket_path();

        let mut shut_down_gracefully = false;
        if qmp_sock.exists() {
            match qmp::system_powerdown(&qmp_sock).await {
                Ok(()) => {
                    shut_down_gracefully = matches!(
                        tokio::time::timeout(SHUTDOWN_TIMEOUT, self.qemu.wait()).await,
                        Ok(Ok(_))
                    );
                    if !shut_down_gracefully {
                        tracing::warn!(%workspace, "guest did not power off in time; forcing quit");
                    }
                }
                Err(e) => {
                    tracing::warn!(%workspace, error = %e, "ACPI powerdown failed; forcing quit");
                }
            }
        }

        let mut result = Ok(());
        if !shut_down_gracefully {
            if let Err(e) = qmp::quit(&qmp_sock).await {
                tracing::warn!(%workspace, error = %e, "QMP quit failed; will SIGKILL qemu");
            }
            if tokio::time::timeout(QUIT_TIMEOUT, self.qemu.wait())
                .await
                .is_err()
            {
                tracing::warn!(%workspace, "qemu ignored quit; sending SIGKILL");
            }
            result = kill_and_reap(&mut self.qemu, "qemu-system-x86_64").await;
        }

        for mut helper in self.helpers.drain(..) {
            let helper_result = kill_and_reap(&mut helper, "virtiofsd").await;
            if let Err(e) = &helper_result {
                tracing::warn!(%workspace, error = %e, "failed to stop virtiofsd helper");
            }
            result = result.and(helper_result);
        }

        result
    }

    /// Checks whether the VM process is alive, and if so whether
    /// `opencode web` is currently answering.
    pub async fn status(&mut self) -> VmStatus {
        match self.qemu.try_wait() {
            Ok(Some(exit)) => {
                return VmStatus::Failed {
                    reason: format!("qemu exited: {exit:?}"),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return VmStatus::Failed {
                    reason: format!("failed to check qemu process: {e}"),
                };
            }
        }

        // A missing or unresponsive QMP socket just means early boot, not
        // a failure.
        let qmp_sock = self.cfg.qmp_socket_path();
        if qmp_sock.exists()
            && let Ok(info) = qmp::query_status(&qmp_sock).await
            && !info.running
        {
            return VmStatus::Booting;
        }

        if probe_once(self.cfg.host_port).await {
            VmStatus::Running {
                url: format!("http://127.0.0.1:{}", self.cfg.host_port),
            }
        } else {
            VmStatus::Booting
        }
    }

    /// The host port `opencode web` is forwarded to.
    pub fn host_port(&self) -> u16 {
        self.cfg.host_port
    }

    /// The workspace this VM belongs to.
    pub fn workspace_id(&self) -> &str {
        &self.cfg.workspace_id
    }

    /// How long ago this VM's qemu process was spawned (not when it first
    /// became healthy).
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// The host pid of the qemu process, for host-side resource stats.
    /// `None` if the process has already exited.
    pub fn qemu_pid(&self) -> Option<u32> {
        self.qemu.id()
    }

    /// Configured guest vCPU count.
    pub fn vcpus(&self) -> u32 {
        self.cfg.vcpus
    }

    /// Path to this VM's QMP control socket, for
    /// [`crate::qmp::guest_memory`].
    pub fn qmp_socket_path(&self) -> PathBuf {
        self.cfg.qmp_socket_path()
    }

    /// Path to this VM's persistent overlay disk.
    pub fn disk_path(&self) -> PathBuf {
        self.cfg.disk_path()
    }
}

/// Runs the readiness wait, but fails as soon as qemu exits - bad arguments,
/// a missing `/dev/kvm`, a locked or unopenable disk - with the tail of its
/// log, instead of waiting out the whole boot timeout.
async fn wait_ready_or_exit(
    qemu: &mut Child,
    log_path: &std::path::Path,
    ready: impl Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        result = ready => result,
        exit = qemu.wait() => {
            let status = exit.map_err(|source| Error::Wait {
                program: "qemu-system-x86_64",
                source,
            })?;
            Err(Error::QemuExited {
                status,
                stderr_tail: crate::qemu::qemu_log_tail(log_path).await,
            })
        }
    }
}

/// SIGKILLs and reaps `child`, unless it has already exited (tokio's
/// `kill` errors on an already-reaped child, which is not a failure here).
async fn kill_and_reap(child: &mut Child, program: &'static str) -> Result<()> {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }
    child
        .kill()
        .await
        .map_err(|source| Error::Kill { program, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("600")
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn kill_and_reap_kills_a_live_child() {
        let mut child = sleeper();
        kill_and_reap(&mut child, "sleep").await.unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }

    /// Killing a child that `wait()` already reaped must not be an error.
    #[tokio::test]
    async fn kill_and_reap_tolerates_an_already_reaped_child() {
        let mut child = Command::new("true").spawn().unwrap();
        child.wait().await.unwrap();
        kill_and_reap(&mut child, "true").await.unwrap();
    }

    /// A qemu that dies during boot fails the boot at once, with its stderr.
    #[tokio::test]
    async fn boot_wait_fails_fast_with_the_log_tail_when_qemu_exits() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("qemu.log");
        let file = std::fs::File::create(&log).unwrap();
        let mut child = Command::new("sh")
            .args(["-c", "echo 'qemu: could not open disk' >&2; exit 3"])
            .stderr(file)
            .spawn()
            .unwrap();

        let never_ready = std::future::pending::<Result<()>>();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_ready_or_exit(&mut child, &log, never_ready),
        )
        .await
        .expect("must not wait for readiness once qemu has exited");

        match result {
            Err(Error::QemuExited {
                status,
                stderr_tail,
            }) => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr_tail, "qemu: could not open disk");
            }
            other => panic!("expected QemuExited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn boot_wait_returns_readiness_while_qemu_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = sleeper();
        let result =
            wait_ready_or_exit(&mut child, &dir.path().join("qemu.log"), async { Ok(()) }).await;
        assert!(result.is_ok());
        kill_and_reap(&mut child, "sleep").await.unwrap();
    }

    /// Real qemu + virtiofsd: an unopenable base image makes qemu exit, and
    /// `boot` reports qemu's own message within seconds.
    #[tokio::test]
    #[ignore = "needs qemu, virtiofsd and /dev/kvm"]
    async fn real_qemu_boot_failure_is_reported_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = VmConfig {
            workspace_id: "t".to_owned(),
            repo_dir: tmp.path().join("repo"),
            state_dir: tmp.path().join("state"),
            runtime_dir: tmp.path().join("run"),
            guest_output_dir: tmp.path().join("repo/.gavmo/vm"),
            base_image: tmp.path().join("missing-base.qcow2"),
            gcloud_dir: tmp.path().join("gcloud"),
            mcp_dir: tmp.path().join("mcp"),
            memory_mib: 256,
            vcpus: 1,
            host_port: 1,
        };
        for dir in [&cfg.repo_dir, &cfg.gcloud_dir, &cfg.mcp_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        create_private_dir_all(&cfg.state_dir).unwrap();
        // A pre-existing overlay, so only qemu (not qemu-img) sees the
        // missing base.
        let status = std::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(cfg.disk_path())
            .arg("64M")
            .status()
            .unwrap();
        assert!(status.success());

        let started = Instant::now();
        let Err(err) = RunningVm::boot(cfg).await else {
            panic!("boot must fail");
        };
        assert!(started.elapsed() < Duration::from_secs(30), "{err}");
        assert!(matches!(err, Error::QemuExited { .. }), "{err}");
        assert!(err.to_string().contains("missing-base.qcow2"), "{err}");
    }
}
