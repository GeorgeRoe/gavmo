//! Small shared helpers (locks, owner-only writes, idempotent deletes).

use std::io;
use std::path::Path;

/// Recovers a poisoned `std` mutex. Every map guarded this way (MCP tokens,
/// resolved integrations) is updated with single, self-contained operations,
/// so a panic elsewhere can't leave it half-modified; serving it is safer
/// than turning one panic into a panic on every later request.
pub(super) fn lock_unpoisoned<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Writes `contents` to `path`, truncating any existing file, with mode
/// `0600` applied at creation on Unix - never briefly readable by other users
/// as with write-then-chmod. An existing file's mode is forced back to `0600`
/// too, since `mode` only applies when the file is created.
pub(super) async fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    file.write_all(contents).await?;
    file.flush().await
}

/// Deletes a directory tree, treating "already gone" as success.
pub(super) async fn remove_dir_all_idempotent(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
