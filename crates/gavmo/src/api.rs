//! The server-function API: the control panel's entire interface to the
//! daemon. Bodies only compile on the server, where they use the registry.

use crate::types::{
    AppError, ConsoleChunk, DirListing, ToolList, VmStats, WorkspaceInfo, WorkspaceStatus,
};
use leptos::prelude::*;

#[server]
pub async fn list_workspaces() -> Result<Vec<WorkspaceInfo>, AppError> {
    let state = app_state()?;
    Ok(state.list().await)
}

#[server]
pub async fn poll_status(id: String) -> Result<WorkspaceStatus, AppError> {
    let state = app_state()?;
    Ok(state.status(&id).await)
}

/// Fetches the full `nixos-rebuild` output for a
/// [`WorkspaceStatus::BuildFailed`] workspace. `log` must be the filename that
/// status reported; the server validates it strictly before reading.
#[server]
pub async fn read_build_log(id: String, log: String) -> Result<String, AppError> {
    let state = app_state()?;
    state
        .read_build_log(&id, &log)
        .await
        .map_err(AppError::from)
}

/// Fetches new boot console output (kernel + systemd) after `offset` (the
/// previous chunk's `next_offset`, 0 at first), sanitized for display. The
/// log lives at a fixed path, so there is no client-controlled filename.
#[server]
pub async fn read_console_log(id: String, offset: u64) -> Result<ConsoleChunk, AppError> {
    let state = app_state()?;
    state
        .read_console_log(&id, offset)
        .await
        .map_err(AppError::from)
}

/// Fetches resource usage for a workspace's VM. `Ok(None)` means nothing to
/// show right now (not `Running`, or qemu just exited); `Err` means the
/// request failed.
#[server]
pub async fn poll_vm_stats(id: String) -> Result<Option<VmStats>, AppError> {
    let state = app_state()?;
    Ok(state.vm_stats(&id).await)
}

#[server]
pub async fn add_workspace(path: String) -> Result<(), AppError> {
    let state = app_state()?;
    state.add_workspace(path).await.map_err(AppError::from)
}

/// Starts booting the workspace's VM (no-op if already booting/running) and
/// returns immediately; the client follows progress via the polled list so
/// the UI never blocks for the whole boot.
#[server]
pub async fn open_workspace(id: String) -> Result<(), AppError> {
    let state = app_state()?;
    state.open_workspace(&id).await.map_err(AppError::from)
}

#[server]
pub async fn stop_workspace(id: String) -> Result<(), AppError> {
    let state = app_state()?;
    state.stop_workspace(&id).await.map_err(AppError::from)
}

/// Stops (if needed) and wipes a workspace's VM state, e.g. to recover from
/// an overlay built on a stale base image. **Destructive**: the client must
/// confirm with the user first; this function does not.
#[server]
pub async fn reset_workspace(id: String) -> Result<(), AppError> {
    let state = app_state()?;
    state.reset_workspace(&id).await.map_err(AppError::from)
}

/// Stops (if needed) and drops a workspace from the registry. Its VM disk
/// and `.gavmo/` are kept, so re-adding the path restores its state. The
/// client must confirm with the user first.
#[server]
pub async fn remove_workspace(id: String) -> Result<(), AppError> {
    let state = app_state()?;
    state.remove_workspace(&id).await.map_err(AppError::from)
}

#[server]
pub async fn browse_dir(path: Option<String>) -> Result<DirListing, AppError> {
    let dir = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            dirs::home_dir().ok_or_else(|| AppError::Internal("no home directory".to_owned()))?
        }
    };
    tokio::task::spawn_blocking(move || browse::list_dirs(&dir))
        .await
        .map_err(|e| AppError::Internal(format!("directory listing task failed: {e}")))?
}

/// Lists the tools a workspace's `gavmo.yaml` requests, with their configured
/// status. Read live, so config/secret changes show without a restart.
#[server]
pub async fn list_tool_configs(id: String) -> Result<ToolList, AppError> {
    let state = app_state()?;
    Ok(state.list_tool_configs(&id).await?)
}

/// Stores (or replaces) a tool's credential on the host. `username` is only used
/// for Basic auth; Bearer/Header pass just `token`. The credential never leaves
/// the host.
#[server]
pub async fn set_tool_secret(
    id: String,
    tool_id: String,
    username: Option<String>,
    token: String,
) -> Result<(), AppError> {
    let state = app_state()?;
    state
        .set_tool_secret(&id, &tool_id, username, token)
        .await
        .map_err(AppError::from)
}

/// Removes a tool's stored credential (disabling it on the workspace's next open).
#[server]
pub async fn clear_tool_secret(id: String, tool_id: String) -> Result<(), AppError> {
    let state = app_state()?;
    state
        .clear_tool_secret(&id, &tool_id)
        .await
        .map_err(AppError::from)
}

/// Live connectivity/credential test for a configured tool: forwards a `GET /`
/// with the stored credential and returns the status + body. `Err` is returned
/// for a non-2xx (or transport failure) so the UI can show a red result.
#[server]
pub async fn test_tool(id: String, tool_id: String) -> Result<String, AppError> {
    let state = app_state()?;
    state.test_tool(&id, &tool_id).await.map_err(AppError::from)
}

/// Fetches the injected [`registry::AppState`] from Leptos context.
#[cfg(feature = "ssr")]
fn app_state() -> Result<crate::registry::AppState, AppError> {
    use_context::<crate::registry::AppState>()
        .ok_or_else(|| AppError::Internal("app state not available".to_owned()))
}

/// The folder picker's directory listing (blocking; run off the runtime).
#[cfg(feature = "ssr")]
mod browse {
    use std::path::Path;

    use crate::types::{AppError, DirEntry, DirListing};

    /// Lists the non-hidden subdirectories of `dir` (canonicalized), sorted
    /// by name. Symlinks to directories count as directories. Entries whose
    /// names aren't valid UTF-8 are skipped: a lossily-converted path would
    /// not exist, so they couldn't be opened anyway.
    pub(super) fn list_dirs(dir: &Path) -> Result<DirListing, AppError> {
        let invalid = |e: std::io::Error| AppError::InvalidInput(format!("{}: {e}", dir.display()));
        let dir = std::fs::canonicalize(dir).map_err(invalid)?;
        let utf8 = |p: &Path| {
            p.to_str().map(str::to_owned).ok_or_else(|| {
                AppError::InvalidInput(format!("{} is not valid UTF-8", p.display()))
            })
        };

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(invalid)?.flatten() {
            let path = entry.path();
            // `metadata` follows symlinks (unlike `DirEntry::file_type`).
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let (Some(name), Some(path)) = (
                entry.file_name().to_str().map(str::to_owned),
                path.to_str().map(str::to_owned),
            ) else {
                continue;
            };
            if name.starts_with('.') {
                continue; // hide dotfiles/dirs from the picker
            }
            entries.push(DirEntry { name, path });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(DirListing {
            path: utf8(&dir)?,
            parent: dir.parent().map(utf8).transpose()?,
            entries,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn lists_visible_subdirs_sorted_including_symlinked_ones() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            for d in ["b", "a", ".hidden"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            std::fs::write(root.join("file"), b"").unwrap();
            let target = tmp.path().join("elsewhere");
            std::fs::create_dir(&target).unwrap();
            std::os::unix::fs::symlink(&target, root.join("c-link")).unwrap();

            let listing = list_dirs(&root).unwrap();
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["a", "b", "c-link"]);
            assert_eq!(
                listing.path,
                std::fs::canonicalize(&root).unwrap().to_str().unwrap()
            );
            assert!(listing.parent.is_some());
        }

        #[test]
        fn skips_non_utf8_names() {
            use std::os::unix::ffi::OsStrExt;
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir(tmp.path().join(std::ffi::OsStr::from_bytes(b"bad\xff"))).unwrap();
            std::fs::create_dir(tmp.path().join("good")).unwrap();
            let listing = list_dirs(tmp.path()).unwrap();
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["good"]);
        }

        #[test]
        fn a_missing_dir_is_invalid_input() {
            assert!(matches!(
                list_dirs(Path::new("/nonexistent/gavmo-browse")),
                Err(AppError::InvalidInput(_))
            ));
        }
    }
}
