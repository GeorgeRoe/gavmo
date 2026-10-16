use std::time::{Duration, Instant};

use tokio::process::Child;

use crate::config::VmConfig;
use crate::disk::ensure_overlay;
use crate::error::Result;
use crate::health::{probe_once, wait_for_ready};
use crate::qemu::spawn_qemu;
use crate::qmp;
use crate::share::{NinePShare, ShareBackend, ShareSpec};

/// How long to wait for `opencode web` to become reachable during boot.
///
/// Also covers M2's `gavmo-rebuild` step (in-guest `nixos-rebuild test`,
/// which runs before `opencode web` starts if the repo has a `gavmo.nix` —
/// see `nix/base/configuration.nix` and `nix/spike-m2.md`). The M2 spike's
/// recorded timings (`nix/spike-m2.md` §9) found this adds only seconds to
/// low tens-of-seconds even when pulling in a whole new service (Docker),
/// comfortably inside the existing budget below — no widening needed based
/// on that evidence. Revisit if a real repo's `gavmo.nix` proves much
/// heavier (e.g. a large toolchain fetch on a slow link).
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

/// The guest-visible mount tag for the read-only MCP token share (see
/// `VmConfig::mcp_dir`). Must match the base NixOS image's fstab.
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
    /// When the qemu process was spawned — *not* when `opencode web`
    /// first answered (which can be minutes later on a first boot, or a
    /// `gavmo.nix` rebuild). Used for [`Self::uptime`], so "uptime" means
    /// "how long has this VM existed", matching what a user watching the
    /// live boot console would expect, not "how long has it been healthy".
    started: Instant,
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
        let mcp_share = ShareSpec {
            host_path: cfg.mcp_dir.clone(),
            mount_tag: MOUNT_TAG_MCP,
            read_only: true,
        };
        let shares: Vec<(&dyn ShareBackend, ShareSpec)> = vec![
            (&nine_p, workspace_share),
            (&nine_p, gcloud_share),
            (&nine_p, mcp_share),
        ];

        // Clear a stale M2 failure marker from a previous boot attempt
        // before this one starts, so `wait_for_ready` below can't mistake
        // leftover state for this boot's own result. `ensure_overlay` above
        // already created `state_dir`, so this is just a best-effort remove
        // (ignore "doesn't exist", which is the common case).
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

        // Races opencode's port answering against the guest's M2
        // rebuild-failure marker appearing — see `health::wait_for_ready`
        // and `nix/base/configuration.nix`'s `gavmo-rebuild` service. On
        // `Err`, `vm` is dropped here without calling `stop()`; its `Child`
        // has `kill_on_drop` set (see `qemu::spawn_qemu`), so the qemu
        // process is still torn down — the failure log on the host's
        // `state_dir` is unaffected since it's written to disk, not held by
        // the dying guest process.
        wait_for_ready(host_port, &marker_path, BOOT_HEALTH_TIMEOUT).await?;

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

    /// How long ago this VM's qemu process was spawned (see `started`'s
    /// doc comment for why this is measured from spawn, not from first
    /// becoming healthy).
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// The host OS pid of the qemu process, for host-side resource stats
    /// (see `crate::stats`). `None` only if the process has already exited
    /// (per `tokio::process::Child::id`'s own doc comment) — in practice
    /// this doesn't happen for a `RunningVm` still held by the caller
    /// without `stop()` having been called, but isn't ruled out by the
    /// type system either.
    pub fn qemu_pid(&self) -> Option<u32> {
        self.qemu.id()
    }

    /// Configured guest memory, in MiB — used by the stats panel to show
    /// resident memory against the configured cap.
    pub fn memory_mib(&self) -> u32 {
        self.cfg.memory_mib
    }

    /// Configured guest vCPU count — used by the stats panel to show CPU
    /// usage against the configured core count.
    pub fn vcpus(&self) -> u32 {
        self.cfg.vcpus
    }

    /// Path to this VM's persistent overlay disk, for the stats panel's
    /// disk-size reading (see `crate::stats::file_size_or_zero`).
    pub fn disk_path(&self) -> std::path::PathBuf {
        self.cfg.disk_path()
    }
}
