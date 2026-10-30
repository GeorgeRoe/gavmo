//! Per-workspace host paths, the runtime-dir choice, and workspace path policy.

use std::io;
use std::path::{Path, PathBuf};

use super::AppState;

impl AppState {
    /// Where the guest writes its build output (`<repo>/.gavmo/vm`): rebuild
    /// logs and the failure marker. Guest-controlled; only ever read with
    /// [`read_guest_output_file`]. Derived from `id` because a `BuildFailed` slot
    /// outlives its `VmConfig`.
    pub(super) fn guest_output_dir_for(id: &str) -> PathBuf {
        PathBuf::from(id).join(".gavmo").join("vm")
    }

    /// Host-only persistent VM state (disk, console log):
    /// `~/.local/share/gavmo/vms/<sha256 of id>`.
    pub(super) fn host_state_dir_for(&self, id: &str) -> PathBuf {
        self.inner.vms_base_dir.join(workspace_hash(id))
    }

    /// Host-only VM sockets: `<runtime base>/<first 16 hex of sha256 of id>`,
    /// short enough for the unix socket path limit.
    pub(super) fn runtime_dir_for(&self, id: &str) -> PathBuf {
        let hash = workspace_hash(id);
        self.inner
            .runtime_base_dir
            .join(hash.get(..16).unwrap_or(&hash))
    }

    /// This workspace's MCP token share dir, under GAVMO's app-data dir and
    /// outside the repo so the token can't leak into the user's project.
    pub(super) fn mcp_dir_for(&self, id: &str) -> PathBuf {
        self.inner.mcp_base_dir.join(workspace_hash(id))
    }
}

/// Deterministic, filesystem-safe, fixed-length name for a workspace's
/// per-workspace host dirs (MCP token share, VM state, VM runtime): the hex
/// SHA-256 of the workspace path.
///
/// SHA-256 rather than the hex-encoded path itself: hex doubles the length, so
/// any path over 127 bytes exceeded the 255-byte filename limit and that
/// workspace could never boot. The digest is always 64 chars.
pub(super) fn workspace_hash(id: &str) -> String {
    use sha2::Digest as _;

    format!("{:x}", sha2::Sha256::digest(id.as_bytes()))
}

/// Where VM sockets live: `$XDG_RUNTIME_DIR/gavmo` (private to the user,
/// short, and cleared on reboot, so stale sockets disappear), else
/// `/tmp/gavmo-<uid>`. Either way it is created `0700`, and an existing dir
/// is refused unless it is a real directory owned by us
/// ([`vm::create_private_dir_all`]) - in `/tmp` another user could have
/// created it first.
pub(super) fn default_runtime_base() -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join("gavmo"));
    }
    let uid = std::fs::metadata("/proc/self")?.uid();
    Ok(std::env::temp_dir().join(format!("gavmo-{uid}")))
}

/// Logs a one-time hint if a workspace still has VM files from GAVMO versions
/// that kept them in the repo. They are no longer used (and the disk may have
/// been modified by the guest, so it is deliberately not migrated). Only
/// `symlink_metadata` is used: nothing guest-controlled is followed.
pub(super) fn warn_about_legacy_vm_state(id: &str) {
    let legacy = AppState::guest_output_dir_for(id).join("disk.qcow2");
    if let Ok(meta) = std::fs::symlink_metadata(&legacy)
        && meta.is_file()
    {
        tracing::info!(
            "old VM disk at {} is no longer used (VM state now lives in \
             ~/.local/share/gavmo/vms); it is safe to delete and frees {}",
            legacy.display(),
            human_bytes(meta.len())
        );
    }
}

/// `1536` -> `"1.5 KiB"`, for log messages.
pub(super) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS.get(unit).unwrap_or(&"B"))
    }
}

/// Refuses workspace paths whose read-write mount would hand the guest more
/// than a project: `/`, the user's home or any ancestor of it, and anything
/// overlapping a protected dir in either direction (equal to it, inside it,
/// or containing it). All paths must be canonical. Component-wise, so
/// `/home/u/.local/share/gavmo-other` doesn't count as inside `.../gavmo`.
pub(super) fn check_workspace_path(
    canonical: &Path,
    protected: &[PathBuf],
    home: Option<&Path>,
) -> Result<(), String> {
    if canonical.parent().is_none() {
        return Err("the filesystem root cannot be a workspace".to_owned());
    }
    if let Some(home) = home
        && home.starts_with(canonical)
    {
        return Err(format!(
            "{} would expose your entire home directory to the VM; choose a project directory",
            canonical.display()
        ));
    }
    for dir in protected {
        if canonical.starts_with(dir) || dir.starts_with(canonical) {
            return Err(format!(
                "{} overlaps {}, which holds GAVMO's own secrets or credentials",
                canonical.display(),
                dir.display()
            ));
        }
    }
    Ok(())
}

/// `path` canonicalized if it exists; otherwise as given (a protected dir
/// that doesn't exist yet can't be reached through a symlink either).
pub(super) fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
