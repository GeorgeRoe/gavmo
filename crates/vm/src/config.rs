use std::path::PathBuf;

use crate::share::ShareSpec;

/// The fixed guest-side port `opencode web` listens on. Baked into the base
/// image; only the host-side forwarded port (`VmConfig::host_port`) varies.
pub const GUEST_OPENCODE_PORT: u16 = 7080;

/// Configuration for a single VM instance.
///
/// Owned by the daemon; this crate does not allocate ports, resolve the
/// base image, or decide workspace identity.
///
/// # Trust boundaries
///
/// The repo is shared **read-write** into the guest, so anything under it is
/// guest-controlled. Files that host processes open, execute or write -
/// the disk image (whose header names other files for qemu to open), the
/// console log (opened by qemu, following symlinks) and the control sockets -
/// must therefore live outside the repo and outside every other share:
/// [`Self::state_dir`] and [`Self::runtime_dir`]. Only files the guest is
/// *meant* to write live in [`Self::guest_output_dir`].
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Stable identifier for the workspace this VM belongs to. Used only
    /// for logging and error messages; isolation comes from the directories
    /// below and the allocated `host_port`, not from this id.
    pub workspace_id: String,

    /// Host path to the repository, shared into the guest as `/workspace`.
    pub repo_dir: PathBuf,

    /// Host-only, persistent per-VM state: the overlay disk and the console
    /// log. Must not be inside `repo_dir` or any other share (see above).
    /// Created owner-only (`0700`) if missing.
    pub state_dir: PathBuf,

    /// Host-only, ephemeral per-VM runtime files: the QMP and virtiofsd
    /// sockets. Kept short (unix socket paths are limited to ~107 bytes) and
    /// outside every share. Created owner-only (`0700`) if missing.
    pub runtime_dir: PathBuf,

    /// Guest-written build output, conventionally `<repo>/.gavmo/vm`: the
    /// `gavmo-rebuild` failure marker and logs. Only ever read by the host,
    /// and treated as untrusted.
    pub guest_output_dir: PathBuf,

    /// Read-only base NixOS qcow2 image. Never written to; the per-VM
    /// overlay disk is what persists state across restarts.
    pub base_image: PathBuf,

    /// Host path to the gcloud ADC config directory, shared read-only into
    /// the guest so `opencode` can authenticate to Vertex AI.
    pub gcloud_dir: PathBuf,

    /// Host path to this workspace's MCP token dir, holding a `token.env` the
    /// guest loads as `GAVMO_MCP_TOKEN`. Shared read-only at `/etc/gavmo/mcp`.
    /// A dedicated share outside `repo_dir` keeps the capability token out
    /// of the user's repo.
    pub mcp_dir: PathBuf,

    /// Virtual size of the overlay disk, in GiB. A new overlay is created at
    /// this size and an existing smaller one is grown to it on boot; a larger
    /// one is never shrunk.
    pub disk_size_gib: u32,

    /// Guest memory, in MiB.
    pub memory_mib: u32,

    /// Guest vCPU count.
    pub vcpus: u32,

    /// Host port that forwards to the guest's `GUEST_OPENCODE_PORT`.
    /// Allocated by the daemon; must be unique per concurrently-running VM.
    pub host_port: u16,
}

impl VmConfig {
    /// The guest's virtiofs shares: the repo read-write, the gcloud and MCP
    /// dirs read-only. Tags must match the mounts in
    /// `nix/base/configuration.nix`.
    pub(crate) fn shares(&self) -> Vec<ShareSpec> {
        vec![
            ShareSpec {
                host_path: self.repo_dir.clone(),
                mount_tag: "workspace",
                read_only: false,
            },
            ShareSpec {
                host_path: self.gcloud_dir.clone(),
                mount_tag: "gcloud",
                read_only: true,
            },
            ShareSpec {
                host_path: self.mcp_dir.clone(),
                mount_tag: "mcp",
                read_only: true,
            },
        ]
    }

    /// Path to this VM's persistent, copy-on-write overlay disk.
    pub fn disk_path(&self) -> PathBuf {
        self.state_dir.join("disk.qcow2")
    }

    /// Path to the file the guest's serial console is redirected to.
    pub fn console_log_path(&self) -> PathBuf {
        self.state_dir.join("console.log")
    }

    /// Path to qemu's own stdout/stderr, kept so a boot that dies early can
    /// report why.
    pub fn qemu_log_path(&self) -> PathBuf {
        self.state_dir.join("qemu.log")
    }

    /// Path to the QMP control-channel unix socket.
    pub fn qmp_socket_path(&self) -> PathBuf {
        self.runtime_dir.join("qmp.sock")
    }

    /// Path to the `gavmo-rebuild` failure marker, written by the guest (via
    /// the `/workspace` share) only when `gavmo.nix` fails to build. Its
    /// contents are the filename, under `guest_output_dir`, of the full
    /// rebuild log.
    pub fn rebuild_failed_marker_path(&self) -> PathBuf {
        self.guest_output_dir.join("rebuild-failed")
    }
}
