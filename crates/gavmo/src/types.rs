//! View types shared by the server and the browser (serialized across the
//! server-function boundary), and the error type of every server function.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

/// Errors returned by every server function, shared by server and browser so
/// the UI can react to the kind of failure (e.g. go back to login), not just
/// show a string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum AppError {
    /// The browser session is missing or expired (HTTP 401 from the auth
    /// layer, which encodes this variant as its body).
    #[error("not logged in")]
    Unauthorized,
    #[error("unknown workspace: {0}")]
    UnknownWorkspace(String),
    /// Another lifecycle operation on the workspace is in progress.
    #[error("this workspace is busy (stopping, resetting or being removed); try again in a moment")]
    Busy,
    #[error("GAVMO is shutting down")]
    ShuttingDown,
    /// The request itself was invalid (bad path, unknown tool, missing field).
    #[error("{0}")]
    InvalidInput(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The host secret store (keyring/vault) could not be used.
    #[error("secret store unavailable: {0}")]
    SecretStore(String),
    /// The operation ran but did not succeed (e.g. a tool test got a non-2xx).
    #[error("{0}")]
    Failed(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// Transport/encoding failures from the server-function machinery.
    #[error("request failed: {0}")]
    ServerFn(ServerFnErrorErr),
}

impl FromServerFnError for AppError {
    type Encoder = server_fn::codec::JsonEncoding;

    fn from_server_fn_error(value: ServerFnErrorErr) -> Self {
        Self::ServerFn(value)
    }
}

/// A known workspace plus its current VM status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    pub status: WorkspaceStatus,
}

/// Browser-facing mirror of `vm::VmStatus`, plus `BuildFailed` and
/// `BaseImageChanged` cases (from `vm::Error::RebuildFailed` and
/// `vm::Error::BaseImageChanged`) that `vm::VmStatus` doesn't have.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceStatus {
    Stopped,
    Booting,
    /// A stop, reset or remove is in progress.
    Stopping,
    Running {
        url: String,
    },
    Failed {
        reason: String,
    },
    /// The repo's `gavmo.nix` failed to build (detected via the guest's
    /// failure marker, not a boot timeout). `log` is the failed run's log
    /// filename, fetchable via [`read_build_log`].
    BuildFailed {
        log: String,
    },
    /// The VM's disk was made from a different base image than the one GAVMO
    /// now uses (e.g. after an update), so it was not booted. A reset
    /// rebuilds the disk from the current image. `reason` is the detail.
    BaseImageChanged {
        reason: String,
    },
}

/// Resource usage for a `Running` workspace's VM, shown in the detail modal's
/// stats panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmStats {
    /// Top-style CPU usage: `0` idle, `100` per fully-busy core, up to
    /// `vcpus * 100` if every core is saturated.
    pub cpu_percent: f32,
    /// Configured guest vCPU count (the scale for `cpu_percent`).
    pub vcpus: u32,
    /// Guest-reported used memory in bytes (`total - MemAvailable`, via the
    /// virtio-balloon driver). Not qemu's RSS: with virtiofs the shared memfd
    /// keeps reclaimable page cache resident, so RSS reads near 100%.
    /// `None` until the balloon driver first reports; show a placeholder.
    pub mem_used_bytes: Option<u64>,
    /// Guest-reported total RAM in bytes; `None` together with `mem_used_bytes`.
    pub mem_total_bytes: Option<u64>,
    /// Seconds since the qemu process was spawned (not since it became healthy).
    pub uptime_secs: u64,
    /// Size of the persistent overlay disk (`~/.local/share/gavmo/vms/<hash>/disk.qcow2`), in bytes.
    pub disk_bytes: u64,
    /// The host port `opencode web` is forwarded to.
    pub host_port: u16,
}

/// A directory listing for the folder picker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirListing {
    /// Absolute path currently being browsed.
    pub path: String,
    /// Parent directory, if any (for an ".." entry).
    pub parent: Option<String>,
    /// Immediate sub-directories, sorted by name.
    pub entries: Vec<DirEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
}

/// The tools panel's contents: the requested tools, plus why the repo's
/// `gavmo.yaml` was ignored if it was (then `tools` is empty).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolList {
    pub tools: Vec<ToolConfig>,
    pub config_error: Option<String>,
}

/// A chunk of the boot console, from [`crate::api::read_console_log`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleChunk {
    /// Sanitized text.
    pub text: String,
    /// `true` if `text` replaces what the client has (first read, the log
    /// was truncated/restarted, or the client fell too far behind) rather
    /// than following it.
    pub replace: bool,
    /// Pass back as `offset` on the next call.
    pub next_offset: u64,
}

/// A tool requested by a workspace's `gavmo.yaml`, for the Tools panel.
/// `auth` is a plain string (not the `ssr`-only `toolspec` enum) so this type
/// also builds for hydrate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolConfig {
    /// Tool id (also the name the agent calls it by).
    pub id: String,
    /// Human-facing description from the config (or a generated fallback).
    pub description: String,
    /// The integration's base URL, shown so the user can verify where their
    /// credential will be sent.
    pub base_url: String,
    /// `"basic"`, `"bearer"`, or `"header"`; selects the configure form's fields.
    pub auth: String,
    /// The secret reference name declared in the config.
    pub secret_name: String,
    /// Optional setup help from the config.
    pub instructions: Option<String>,
    /// A credential is stored *and* bound to the current `base_url` (usable).
    pub configured: bool,
    /// A credential is stored but was approved for a different URL; the tool
    /// stays disabled until the user re-confirms it.
    pub url_changed: bool,
    /// The URL the stored credential was approved for, shown in the mismatch
    /// warning. `None` if the stored value carries no binding.
    pub bound_url: Option<String>,
    /// How this tool sends the credential, e.g. "the X-API-Key header".
    pub delivery: String,
    /// `Some` when a credential is stored for this destination but was
    /// approved to be sent differently (description of the approved way); the
    /// tool stays disabled until the user re-confirms it.
    pub approved_delivery: Option<String>,
}

/// A workspace's VM resource settings, as saved. `None` means "use the
/// default", so a changed default reaches workspaces that never chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,
}

/// Concrete resource values (defaults, limits, or a resolved setting).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceValues {
    pub disk_gib: u32,
    pub memory_gib: u32,
    pub vcpus: u32,
}

impl Resources {
    /// These settings with `defaults` filled in.
    pub fn resolve(self, defaults: ResourceValues) -> ResourceValues {
        ResourceValues {
            disk_gib: self.disk_gib.unwrap_or(defaults.disk_gib),
            memory_gib: self.memory_gib.unwrap_or(defaults.memory_gib),
            vcpus: self.vcpus.unwrap_or(defaults.vcpus),
        }
    }
}

/// The Resources panel's view: what is saved, what an unset field means,
/// the allowed range, and the disk's current size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcesView {
    pub saved: Resources,
    pub defaults: ResourceValues,
    pub min: ResourceValues,
    pub max: ResourceValues,
    /// Virtual size of the existing overlay disk, rounded up to whole GiB;
    /// `None` before the first boot (or after a reset). The disk can only
    /// be set to this or more.
    pub disk_current_gib: Option<u32>,
}
