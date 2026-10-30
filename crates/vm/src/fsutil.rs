//! Small filesystem helpers shared across the crate.

use std::io;
use std::path::Path;

use crate::error::{Error, Result};

/// Removes `path`, treating "already absent" as success. Any other failure
/// (permissions, `EISDIR`, I/O) is returned rather than silently ignored.
pub(crate) fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => not_found_ok(path, e),
    }
}

/// Async counterpart of [`remove_if_exists`].
pub(crate) async fn remove_if_exists_async(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) => not_found_ok(path, e),
    }
}

fn not_found_ok(path: &Path, e: io::Error) -> Result<()> {
    if e.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(Error::RemoveStale {
            path: path.to_path_buf(),
            source: e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        remove_if_exists(&dir.path().join("absent")).unwrap();
    }

    #[test]
    fn existing_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        remove_if_exists(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn other_errors_are_reported() {
        // A directory can't be removed with `remove_file`.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            remove_if_exists(dir.path()),
            Err(Error::RemoveStale { .. })
        ));
    }

    #[tokio::test]
    async fn async_variant_matches() {
        let dir = tempfile::tempdir().unwrap();
        remove_if_exists_async(&dir.path().join("absent"))
            .await
            .unwrap();
        assert!(matches!(
            remove_if_exists_async(dir.path()).await,
            Err(Error::RemoveStale { .. })
        ));
    }
}
