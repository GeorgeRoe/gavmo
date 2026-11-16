//! Owner-only files and directories, written atomically. Shared by the vault
//! and the key files that sit next to it.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::SecretError;

pub(crate) fn parent_dir(path: &Path) -> Result<&Path, SecretError> {
    path.parent()
        .ok_or_else(|| SecretError::Backend(format!("path {} has no parent", path.display())))
}

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether `path` exists; an error (e.g. a permission problem) is reported
/// rather than read as "absent", so callers can fail closed.
pub(crate) fn exists(path: &Path) -> Result<bool, SecretError> {
    path.try_exists().map_err(SecretError::io("checking", path))
}

/// Durably writes `bytes` to `path`, so a crash leaves either the old file or
/// the new one, never a truncated one: a uniquely named `0600` temp file in
/// the same dir is written and fsynced, renamed into place, and the dir is
/// fsynced. On any error the temp file is removed (by `NamedTempFile`'s drop).
///
/// With `overwrite` false, an existing file at `path` is an error and is left
/// untouched.
pub(crate) fn write_private_atomic(
    path: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<(), SecretError> {
    use std::io::Write as _;

    let dir = parent_dir(path)?;
    create_private_dir_all(dir)?;
    let prefix = format!(
        ".{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut tmp = private_tempfile_builder(&prefix)
        .tempfile_in(dir)
        .map_err(SecretError::io("creating temp file for", path))?;
    tmp.write_all(bytes)
        .map_err(SecretError::io("writing temp file for", path))?;
    tmp.as_file()
        .sync_all()
        .map_err(SecretError::io("syncing temp file for", path))?;
    if overwrite {
        tmp.persist(path)
            .map_err(|e| SecretError::io("replacing", path)(e.error))?;
    } else {
        tmp.persist_noclobber(path)
            .map_err(|e| SecretError::io("creating", path)(e.error))?;
    }
    sync_dir(dir)
}

/// Writes `bytes` to a new `0600` file at `path`; an existing file is kept
/// (it is an earlier backup).
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Ok(mut file) => file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(SecretError::io("writing backup", path)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(SecretError::io("creating backup", path)(e)),
    }
}

/// A temp-file builder for private writes: `0600` on Unix, applied at
/// creation (also `tempfile`'s default, but this must never silently
/// regress). Elsewhere the platform default applies (`tempfile` rejects
/// explicit permissions there); Windows inherits ACLs from the per-user
/// profile dir.
fn private_tempfile_builder(prefix: &str) -> tempfile::Builder<'_, 'static> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix).suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    builder
}

/// Creates `dir` (and parents) with mode `0700` on Unix where they don't yet
/// exist. Existing directories are left as they are.
pub(crate) fn create_private_dir_all(dir: &Path) -> Result<(), SecretError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(SecretError::io("creating", dir))
}

/// Fsyncs a directory so a rename inside it is durable. Unix only: Windows
/// can't open directories this way and `MoveFileEx` semantics differ.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), SecretError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(SecretError::io("syncing", dir))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), SecretError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_atomically_and_refuses_to_clobber_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("key");
        write_private_atomic(&path, b"one", false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");

        let err = write_private_atomic(&path, b"two", false).unwrap_err();
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "creating",
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"one", "left untouched");

        write_private_atomic(&path, b"three", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"three");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["key"], "no temp files left behind");
    }

    #[cfg(unix)]
    #[test]
    fn written_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new").join("key");
        write_private_atomic(&path, b"k", false).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn exists_distinguishes_absent_from_present() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!exists(&dir.path().join("nope")).unwrap());
        assert!(exists(dir.path()).unwrap());
    }
}
