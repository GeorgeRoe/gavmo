use std::time::Duration;

use tokio::process::Child;

use crate::config::VmConfig;
use crate::disk::ensure_overlay;
use crate::error::Result;
use crate::health::{probe_once, wait_for_http};
use crate::qemu::spawn_qemu;
use crate::qmp;
use crate::share::{NinePShare, ShareBackend, ShareSpec};

/// How long to wait for `opencode web` to become reachable during boot.
const BOOT_HEALTH_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a graceful ACPI shutdown before force-killing.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait after a forced `quit` before giving up and sending
/// SIGKILL directly.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The guest-visible mount tag for the repository share. Must match the
/// base NixOS image's fstab.
const MOUNT_TAG_WORKSPACE: &str = "workspace";

/// The guest-visible mount tag for the read-only gcloud credentials share.
/// Must match the base NixOS image's fstab.
const MOUNT_TAG_GCLOUD: &str = "gcloud";

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

/// A booted VM: owns the qemu process (and any share-backend helper
/// processes, currently none for 9p) plus the config it was booted with.
///
/// Dropping a `RunningVm` without calling [`RunningVm::stop`] will still
/// kill the qemu child (via `kill_on_drop`), but won't attempt a graceful
/// shutdown or clean up helper processes — prefer `stop()`.
pub struct RunningVm {
    cfg: VmConfig,
    qemu: Child,
    helpers: Vec<Child>,
}

impl RunningVm {
    /// Boots a VM per `cfg`, sharing `/workspace` (read-write) and the
    /// gcloud credentials directory (read-only) via 9p, and waits for
    /// `opencode web` to answer on the forwarded host port before
    /// returning.
    pub async fn boot(cfg: VmConfig) -> Result<Self> {
        ensure_overlay(&cfg.base_image, &cfg.disk_path()).await?;

        let nine_p = NinePShare;
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
        let shares: Vec<(&dyn ShareBackend, ShareSpec)> =
            vec![(&nine_p, workspace_share), (&nine_p, gcloud_share)];

        let (qemu, helpers) = spawn_qemu(&cfg, &shares).await?;
        let host_port = cfg.host_port;
        let vm = RunningVm { cfg, qemu, helpers };

        wait_for_http(host_port, BOOT_HEALTH_TIMEOUT).await?;

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
            Ok(None) => {} // still running, continue checking
            Err(e) => {
                return VmStatus::Failed {
                    reason: format!("failed to check qemu process: {e}"),
                };
            }
        }

        // The qemu process is alive. Check whether QEMU itself reports the
        // guest as running (vs. still in early boot) before trusting an
        // OpenCode health probe — a missing/unresponsive QMP socket just
        // means we're early in boot, not a failure.
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
}
