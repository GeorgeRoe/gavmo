use std::time::{Duration, Instant};

use tokio::process::Child;

use crate::config::VmConfig;
use crate::disk::ensure_overlay;
use crate::error::Result;
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
        // isn't mistaken for this one's. NotFound is the common case.
        let _ = tokio::fs::remove_file(cfg.rebuild_failed_marker_path()).await;

        let (qemu, helpers) = spawn_qemu(&cfg, &shares).await?;
        let host_port = cfg.host_port;
        let marker_path = cfg.rebuild_failed_marker_path();
        let vm = RunningVm {
            cfg,
            qemu,
            helpers,
            started: Instant::now(),
        };

        // On `Err`, `vm` is dropped and `kill_on_drop` tears down qemu; the
        // rebuild log is already on disk in `state_dir`.
        wait_for_ready(host_port, &marker_path, BOOT_HEALTH_TIMEOUT).await?;

        // Best-effort: without balloon stats the VM runs fine, it just shows
        // no live memory usage. Not worth failing a healthy boot over.
        let _ =
            qmp::set_balloon_stats_interval(&vm.cfg.qmp_socket_path(), BALLOON_STATS_INTERVAL_SECS)
                .await;

        Ok(vm)
    }

    /// Requests a graceful shutdown via QMP (ACPI powerdown), falling back
    /// to a forced `quit` and then SIGKILL if the guest doesn't exit in
    /// time. The overlay disk is always preserved.
    pub async fn stop(mut self) -> Result<()> {
        let qmp_sock = self.cfg.qmp_socket_path();

        let mut shut_down_gracefully = false;
        if qmp_sock.exists() && qmp::system_powerdown(&qmp_sock).await.is_ok() {
            shut_down_gracefully = tokio::time::timeout(SHUTDOWN_TIMEOUT, self.qemu.wait())
                .await
                .is_ok();
        }

        if !shut_down_gracefully {
            let _ = qmp::quit(&qmp_sock).await;
            let _ = tokio::time::timeout(QUIT_TIMEOUT, self.qemu.wait()).await;
            let _ = self.qemu.kill().await;
        }

        for mut helper in self.helpers.drain(..) {
            let _ = helper.kill().await;
        }

        Ok(())
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
    pub fn qmp_socket_path(&self) -> std::path::PathBuf {
        self.cfg.qmp_socket_path()
    }

    /// Path to this VM's persistent overlay disk.
    pub fn disk_path(&self) -> std::path::PathBuf {
        self.cfg.disk_path()
    }
}
