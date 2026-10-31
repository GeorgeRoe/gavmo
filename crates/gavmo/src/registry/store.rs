//! The persisted workspace registry (`workspaces.json`).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// On-disk registry: the set of repos GAVMO knows about. Running state is
/// never persisted - a stopped VM simply isn't in the in-memory map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Registry {
    pub workspaces: Vec<WorkspaceRecord>,
}

/// A single known workspace, identified by the repo's absolute host path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WorkspaceRecord {
    pub path: String,

    /// The host port this workspace was last booted on, if any. Persisted so
    /// the browser origin stays stable across restarts, letting opencode's
    /// per-origin `localStorage` (last project/session) survive.
    /// `serde(default)` keeps older `workspaces.json` files loadable.
    #[serde(default)]
    pub host_port: Option<u16>,

    /// VM resource settings chosen in the panel; unset fields use the
    /// defaults. `serde(default)` keeps older files loadable.
    #[serde(default)]
    pub resources: crate::types::Resources,
}

/// Loads `~/.local/share/gavmo/workspaces.json` (or platform equivalent).
///
/// A missing file is an empty registry. A file that exists but isn't a valid
/// registry (e.g. truncated by a crash before writes were atomic, or
/// hand-edited) is moved aside to `workspaces.json.corrupt-<unix time>` and
/// replaced by an empty registry, so the daemon still starts; the workspaces
/// can be re-added (their VM disks are found again by path). Other read
/// errors (permissions, I/O) are returned: nothing is discarded on a guess.
pub(super) fn load_registry(path: &Path) -> io::Result<Registry> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(registry) => Ok(registry),
        Err(parse_error) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".corrupt-{stamp}"));
            std::fs::rename(path, &aside)?;
            tracing::error!(
                error = %parse_error,
                moved_to = %Path::new(&aside).display(),
                "workspace registry was unreadable; starting with an empty one \
                 (re-add your workspaces; their VM disks are kept)"
            );
            Ok(Registry::default())
        }
    }
}

/// Durably replaces the registry file: a uniquely named `0600` temp file in
/// the same dir is written and fsynced, renamed over the old file, and the
/// dir is fsynced - so a crash leaves the old or the new registry, never a
/// truncated one. Runs on the blocking pool; callers hold the registry lock
/// across it, which keeps concurrent saves ordered.
pub(super) async fn save_registry(path: &Path, registry: &Registry) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::atomic_file::write_atomically(&path, &bytes))
        .await
        .map_err(io::Error::other)?
}
