//! Crash-safe replacement of small host-only state files (registry,
//! sessions).

use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Durably replaces `path` with `bytes`: a uniquely named `0600` temp file in
/// the same dir is written and fsynced, renamed over `path`, and the dir is
/// fsynced. A crash leaves the old or the new contents, never a truncated
/// file, and the file is never readable by other users. Blocking.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".gavmo-")
        .suffix(".tmp")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Names in `dir`, sorted, to check nothing but the target is left.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn creates_an_owner_only_file_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_atomically(&path, b"{}").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(entries(dir.path()), ["state.json"]);
    }

    #[test]
    fn replaces_existing_contents_and_tightens_a_looser_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"old contents, longer than the new").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomically(&path, b"new").unwrap();

        // Not truncated-and-rewritten in place: the old bytes are all gone.
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(entries(dir.path()), ["state.json"]);
    }

    #[test]
    fn replaces_a_symlink_rather_than_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"untouched").unwrap();
        let path = dir.path().join("state.json");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        write_atomically(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn a_path_without_a_parent_is_invalid_input() {
        let err = write_atomically(Path::new("/"), b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("state.json");

        let err = write_atomically(&path, b"x").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(entries(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty directory can't be renamed over.
        let path = dir.path().join("state.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), b"").unwrap();

        write_atomically(&path, b"x").unwrap_err();

        assert_eq!(entries(dir.path()), ["state.json"]);
        assert!(path.is_dir());
    }
}
