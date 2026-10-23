use std::path::PathBuf;

/// The fixed guest-side port `opencode web` listens on. This is baked into
/// the base NixOS image's bootstrap service and is not configurable per-VM
/// — only the host-side forwarded port (`VmConfig::host_port`) varies.
pub const GUEST_OPENCODE_PORT: u16 = 7080;

/// Configuration for a single VM instance.
///
/// Constructed and owned by the daemon. This crate treats it as an opaque
/// set of inputs: it does not allocate ports, resolve the base image, or
/// decide workspace identity — see `plan.md` for the responsibility split.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Stable identifier for the workspace this VM belongs to. Used only
    /// for logging and error messages; isolation comes from `state_dir`
    /// and the allocated `host_port`, not from this id.
    pub workspace_id: String,

    /// Host path to the repository, shared into the guest as `/workspace`.
    pub repo_dir: PathBuf,

    /// Host path to this VM's persistent state (conventionally
    /// `<repo>/.gavmo/vm`). Holds the disk overlay, the QMP socket, and the
    /// console log.
    pub state_dir: PathBuf,

    /// Read-only base NixOS qcow2 image. Never written to; the per-VM
    /// overlay disk is what persists state across restarts.
    pub base_image: PathBuf,

    /// Host path to the gcloud ADC config directory, shared read-only into
    /// the guest so `opencode` can authenticate to Vertex AI.
    pub gcloud_dir: PathBuf,

    /// Host path to this workspace's MCP token share — a small daemon-owned
    /// directory (**not** under `repo_dir`/`state_dir`; see
    /// `crates/gavmo/src/registry.rs`'s `mcp_dir_for`) containing a
    /// `token.env` the guest loads as `GAVMO_MCP_TOKEN` (via systemd's
    /// `EnvironmentFile`, see `nix/base/configuration.nix`). Shared
    /// read-only into the guest at `/etc/gavmo/mcp`, parallel to
    /// `gcloud_dir` — see `crates/mcp/plan.md` for the full design (why a
    /// dedicated share rather than reusing `state_dir`: keeps the
    /// capability token out of the user's repo, which `state_dir`, being
    /// under `repo_dir/.gavmo/vm`, is not).
    pub mcp_dir: PathBuf,

    /// Guest memory, in MiB.
    pub memory_mib: u32,

    /// Guest vCPU count.
    pub vcpus: u32,

    /// Host port that forwards to the guest's `GUEST_OPENCODE_PORT`.
    /// Allocated by the daemon; must be unique per concurrently-running VM.
    pub host_port: u16,
}

impl VmConfig {
    /// Path to this VM's persistent, copy-on-write overlay disk.
    pub fn disk_path(&self) -> PathBuf {
        self.state_dir.join("disk.qcow2")
    }

    /// Path to the QMP control-channel unix socket.
    pub fn qmp_socket_path(&self) -> PathBuf {
        self.state_dir.join("qmp.sock")
    }

    /// Path to the file the guest's serial console is redirected to.
    pub fn console_log_path(&self) -> PathBuf {
        self.state_dir.join("console.log")
    }

    /// Path to the M2 `gavmo-rebuild` failure marker (see
    /// `nix/base/configuration.nix`). Written by the guest, inside
    /// `state_dir`, only when a repo's `gavmo.nix` fails to build — its
    /// *contents* are the filename (also under `state_dir`) of the full
    /// `nixos-rebuild` log. Host-visible because `state_dir` is conventionally
    /// `<repo>/.gavmo/vm`, which is `/workspace/.gavmo/vm` from the guest's
    /// side of the virtiofs share.
    pub fn rebuild_failed_marker_path(&self) -> PathBuf {
        self.state_dir.join("rebuild-failed")
    }
}
