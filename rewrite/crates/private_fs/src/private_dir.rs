//! [`PrivateDir`]: a directory checked to be ours alone, and the private,
//! atomic file operations inside it.

use std::fs::{File, OpenOptions, Permissions};
use std::io::Write as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::{Error, FileName, LockFile, Op, Unsafe};

/// Owner-only, for directories and files.
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// An absolute path to a directory that, when this value was made, was a
/// real directory (not a symlink), owned by this process's user, with mode
/// `0700`. The only way to get one is [`PrivateDir::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// Creates `path` and any missing parents with mode `0700`. If it already
    /// exists it must be a real directory owned by us; group and other
    /// permissions are then removed.
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(Error::NotAbsolute { path });
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&path)
            .map_err(Error::io(Op::CreateDir, &path))?;

        let meta = std::fs::symlink_metadata(&path).map_err(Error::io(Op::Inspect, &path))?;
        if !meta.is_dir() {
            return Err(Error::Unsafe {
                path,
                problem: Unsafe::NotADirectory,
            });
        }
        if meta.uid() != process_uid()? {
            return Err(Error::Unsafe {
                path,
                problem: Unsafe::OwnedByAnotherUser,
            });
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(&path, Permissions::from_mode(DIR_MODE))
                .map_err(Error::io(Op::Narrow, &path))?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The full path of `name` in this directory.
    pub fn file(&self, name: FileName) -> PathBuf {
        self.path.join(name)
    }

    /// Whether `name` exists. A failure to tell (e.g. permissions) is an
    /// error, not "absent", so callers can fail closed.
    pub fn exists(&self, name: FileName) -> Result<bool, Error> {
        let path = self.file(name);
        path.try_exists().map_err(Error::io(Op::Inspect, &path))
    }

    /// The contents of `name`, or `None` if there is no such file. Any other
    /// failure is an error, never an empty result.
    pub fn read(&self, name: FileName) -> Result<Option<Vec<u8>>, Error> {
        let path = self.file(name);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(Op::Read, path)(e)),
        }
    }

    /// Durably replaces `name` with `bytes`, which are never readable by
    /// other users. A crash leaves the old contents or the new, never a
    /// truncated file. A symlink at `name` is replaced, not written through.
    pub fn replace(&self, name: FileName, bytes: &[u8]) -> Result<(), Error> {
        let path = self.file(name);
        self.write_temp(name, bytes)?
            .persist(&path)
            .map_err(|e| Error::io(Op::Rename, &path)(e.error))?;
        self.sync()
    }

    /// Like [`Self::replace`], but only if `name` doesn't exist yet: an
    /// existing file is [`Error::AlreadyExists`] and is left untouched.
    pub fn create_new(&self, name: FileName, bytes: &[u8]) -> Result<(), Error> {
        let path = self.file(name);
        match self.write_temp(name, bytes)?.persist_noclobber(&path) {
            Ok(_) => self.sync(),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(Error::AlreadyExists { path })
            }
            Err(e) => Err(Error::io(Op::Rename, path)(e.error)),
        }
    }

    /// Takes an exclusive lock on `name` (created empty, `0600`, if missing)
    /// without waiting: if another process holds it, [`Error::Locked`].
    pub fn lock(&self, name: FileName) -> Result<LockFile, Error> {
        let path = self.file(name);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(FILE_MODE)
            .open(&path)
            .map_err(Error::io(Op::OpenLock, &path))?;
        match file.try_lock() {
            Ok(()) => Ok(LockFile::new(path, file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::Locked { path }),
            Err(std::fs::TryLockError::Error(e)) => Err(Error::io(Op::Lock, path)(e)),
        }
    }

    /// A fsynced `0600` temp file next to `name`, holding `bytes`. Dropping
    /// it without persisting (e.g. on an error) removes it.
    fn write_temp(&self, name: FileName, bytes: &[u8]) -> Result<NamedTempFile, Error> {
        let path = self.file(name);
        let mut tmp = tempfile::Builder::new()
            .prefix(&format!(".{name}."))
            .suffix(".tmp")
            .permissions(Permissions::from_mode(FILE_MODE))
            .tempfile_in(&self.path)
            .map_err(Error::io(Op::CreateTemp, &self.path))?;
        tmp.write_all(bytes).map_err(Error::io(Op::Write, &path))?;
        tmp.as_file()
            .sync_all()
            .map_err(Error::io(Op::Sync, tmp.path()))?;
        Ok(tmp)
    }

    /// Fsyncs the directory, so a rename inside it survives a crash.
    fn sync(&self) -> Result<(), Error> {
        File::open(&self.path)
            .and_then(|dir| dir.sync_all())
            .map_err(Error::io(Op::Sync, &self.path))
    }
}

/// This process's effective uid: the owner of `/proc/self`. Linux-only, like
/// the daemon; no `unsafe` call to `geteuid` needed.
fn process_uid() -> Result<u32, Error> {
    const PROC_SELF: &str = "/proc/self";
    std::fs::metadata(PROC_SELF)
        .map(|meta| meta.uid())
        .map_err(Error::io(Op::FindUser, PROC_SELF))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: FileName = FileName::new("state.json");

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
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

    fn private(tmp: &tempfile::TempDir) -> PrivateDir {
        PrivateDir::create(tmp.path().join("private")).unwrap()
    }

    #[test]
    fn create_makes_owner_only_dirs_and_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create(tmp.path().join("a").join("b")).unwrap();
        assert_eq!(dir.path(), tmp.path().join("a").join("b"));
        assert_eq!(mode(dir.path()), 0o700);
        assert_eq!(mode(&tmp.path().join("a")), 0o700);
    }

    #[test]
    fn create_narrows_an_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("open");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
        PrivateDir::create(&path).unwrap();
        assert_eq!(mode(&path), 0o700);
    }

    #[test]
    fn create_refuses_a_relative_path() {
        assert!(matches!(
            PrivateDir::create("relative/dir"),
            Err(Error::NotAbsolute { .. })
        ));
    }

    #[test]
    fn create_refuses_a_symlink_or_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();

        for path in [link, file] {
            let err = PrivateDir::create(&path).unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::Unsafe {
                        problem: Unsafe::NotADirectory,
                        ..
                    } | Error::Io {
                        op: Op::CreateDir,
                        ..
                    }
                ),
                "{path:?}: {err:?}"
            );
        }
    }

    #[test]
    fn a_dir_under_a_file_cannot_be_created() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(matches!(
            PrivateDir::create(file.join("dir")),
            Err(Error::Io {
                op: Op::CreateDir,
                ..
            })
        ));
    }

    #[test]
    fn replace_creates_an_owner_only_file_and_leaves_no_temp_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        dir.replace(NAME, b"{}").unwrap();
        assert_eq!(std::fs::read(dir.file(NAME)).unwrap(), b"{}");
        assert_eq!(mode(&dir.file(NAME)), 0o600);
        assert_eq!(entries(dir.path()), ["state.json"]);
    }

    #[test]
    fn replace_replaces_contents_and_tightens_a_looser_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        std::fs::write(dir.file(NAME), b"old contents, longer than the new").unwrap();
        std::fs::set_permissions(dir.file(NAME), Permissions::from_mode(0o644)).unwrap();

        dir.replace(NAME, b"new").unwrap();

        // Not truncated-and-rewritten in place: the old bytes are all gone.
        assert_eq!(std::fs::read(dir.file(NAME)).unwrap(), b"new");
        assert_eq!(mode(&dir.file(NAME)), 0o600);
        assert_eq!(entries(dir.path()), ["state.json"]);
    }

    #[test]
    fn replace_replaces_a_symlink_rather_than_writing_through_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        let target = tmp.path().join("elsewhere");
        std::fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, dir.file(NAME)).unwrap();

        dir.replace(NAME, b"new").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert!(
            !std::fs::symlink_metadata(dir.file(NAME))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(std::fs::read(dir.file(NAME)).unwrap(), b"new");
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        // A non-empty directory can't be renamed over.
        std::fs::create_dir(dir.file(NAME)).unwrap();
        std::fs::write(dir.file(NAME).join("inside"), b"").unwrap();

        let err = dir.replace(NAME, b"x").unwrap_err();

        assert!(matches!(err, Error::Io { op: Op::Rename, .. }), "{err:?}");
        assert_eq!(entries(dir.path()), ["state.json"]);
    }

    #[test]
    fn a_dir_that_is_gone_fails_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        std::fs::remove_dir(dir.path()).unwrap();
        assert!(matches!(
            dir.replace(NAME, b"x"),
            Err(Error::Io {
                op: Op::CreateTemp,
                ..
            })
        ));
    }

    #[test]
    fn create_new_refuses_to_clobber() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        dir.create_new(NAME, b"one").unwrap();
        assert_eq!(mode(&dir.file(NAME)), 0o600);

        let err = dir.create_new(NAME, b"two").unwrap_err();
        assert!(matches!(err, Error::AlreadyExists { .. }), "{err:?}");
        assert_eq!(std::fs::read(dir.file(NAME)).unwrap(), b"one", "untouched");
        assert_eq!(entries(dir.path()), ["state.json"], "no temp file left");
    }

    #[test]
    fn read_and_exists_tell_absent_from_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        assert_eq!(dir.read(NAME).unwrap(), None);
        assert!(!dir.exists(NAME).unwrap());
        dir.replace(NAME, b"x").unwrap();
        assert_eq!(dir.read(NAME).unwrap().as_deref(), Some(&b"x"[..]));
        assert!(dir.exists(NAME).unwrap());
    }

    /// Anything but "not found" is an error, never an empty file.
    #[test]
    fn an_unreadable_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        std::fs::create_dir(dir.file(NAME)).unwrap();
        assert!(matches!(
            dir.read(NAME),
            Err(Error::Io { op: Op::Read, .. })
        ));
    }

    #[test]
    fn a_second_lock_is_refused_until_the_first_is_dropped() {
        const LOCK: FileName = FileName::new("state.lock");
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);

        let first = dir.lock(LOCK).unwrap();
        assert_eq!(first.path(), dir.file(LOCK));
        assert_eq!(mode(&dir.file(LOCK)), 0o600);
        assert!(matches!(dir.lock(LOCK), Err(Error::Locked { .. })));
        drop(first);
        let _again = dir.lock(LOCK).unwrap();
    }

    #[test]
    fn a_lock_file_that_cannot_be_opened_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = private(&tmp);
        std::fs::create_dir(dir.file(NAME)).unwrap();
        assert!(matches!(
            dir.lock(NAME),
            Err(Error::Io {
                op: Op::OpenLock,
                ..
            })
        ));
    }

    #[test]
    fn errors_say_what_failed_where() {
        let err = Error::Unsafe {
            path: "/d".into(),
            problem: Unsafe::OwnedByAnotherUser,
        };
        assert_eq!(err.to_string(), "/d is owned by another user");
        let err = Error::io(Op::Rename, "/d/f")(std::io::Error::other("boom"));
        assert_eq!(err.to_string(), "moving the new file to /d/f: boom");
    }
}
