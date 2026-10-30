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

/// Creates `dir` (and any missing parents) with mode `0700`, so other local
/// users can't read VM disks or connect to its sockets. An existing `dir`
/// must be a real directory (not a symlink) owned by us; its mode is then
/// narrowed to `0700`.
pub fn create_private_dir_all(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let create_err = |source| Error::CreateDir {
        path: dir.to_path_buf(),
        source,
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(create_err)?;

    let meta = std::fs::symlink_metadata(dir).map_err(create_err)?;
    if !meta.is_dir() {
        return Err(Error::UnsafeDir {
            path: dir.to_path_buf(),
            reason: "is not a directory (or is a symlink)",
        });
    }
    if let Some(uid) = process_uid()
        && meta.uid() != uid
    {
        return Err(Error::UnsafeDir {
            path: dir.to_path_buf(),
            reason: "is owned by another user",
        });
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(create_err)?;
    }
    Ok(())
}

/// The effective uid of this process: the owner of `/proc/self`.
fn process_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// Longest usable unix socket path: `sun_path` is 108 bytes including NUL.
const MAX_SOCKET_PATH: usize = 107;

/// Fails early, with a clear error, if `path` is too long to bind as a unix
/// socket (otherwise qemu/virtiofsd fail later with an opaque message).
pub(crate) fn check_socket_path(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if path.as_os_str().as_bytes().len() > MAX_SOCKET_PATH {
        return Err(Error::SocketPathTooLong {
            path: path.to_path_buf(),
            max: MAX_SOCKET_PATH,
        });
    }
    Ok(())
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

    #[test]
    fn create_private_dir_all_makes_owner_only_dirs_and_narrows_existing() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("b");
        create_private_dir_all(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&tmp.path().join("a")), 0o700);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn create_private_dir_all_refuses_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            create_private_dir_all(&link),
            Err(Error::UnsafeDir { .. })
        ));
    }

    #[test]
    fn check_socket_path_enforces_the_sun_path_limit() {
        check_socket_path(Path::new(&format!("/{}", "a".repeat(106)))).unwrap();
        assert!(matches!(
            check_socket_path(Path::new(&format!("/{}", "a".repeat(107)))),
            Err(Error::SocketPathTooLong { .. })
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
