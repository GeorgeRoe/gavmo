//! Reading files a guest VM can write.
//!
//! The repo is shared read-write into the guest, so anything under it -
//! `gavmo.yaml` and the rebuild logs in `.gavmo/vm/` - is attacker-controlled from the daemon's point of view. A guest could
//! replace such a file with a symlink to a host secret (which the daemon would
//! then show in the browser), a FIFO (hanging the reader forever), or a huge
//! file (exhausting memory).
//!
//! [`read_guest_file`] refuses all three: the final component is opened with
//! `O_NOFOLLOW`, non-blocking, and must be a regular file per `fstat` on the
//! opened descriptor; listed parent directories must not be symlinks; and at
//! most `max_bytes` are read.
//!
//! The parent check is best effort (a guest could race it). Files host
//! processes open or write (disk, console log, sockets) are therefore kept
//! out of the shared tree altogether; see `vm::VmConfig`.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// What [`read_guest_file`] read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GuestFile {
    pub(crate) bytes: Vec<u8>,
    /// The file was larger than the limit and only part of it was read.
    pub(crate) truncated: bool,
    /// The file's length when it was opened.
    pub(crate) len: u64,
}

/// Which part to keep when a file exceeds the limit.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Keep {
    /// The first `max_bytes` (config files: then rejected by the caller).
    Head,
    /// The last `max_bytes` (logs: the newest output matters most).
    Tail,
    /// Up to `max_bytes` starting at this offset (following a growing log).
    /// An offset past the end reads nothing.
    From(u64),
}

/// Reads at most `max_bytes` of the regular file `path`, refusing symlinks,
/// FIFOs, devices and directories. `Ok(None)` if it doesn't exist.
///
/// `no_symlink_parents` are directories between a trusted root and `path`
/// (e.g. `<repo>/.gavmo` and `<repo>/.gavmo/vm`) that must not be symlinks.
pub(crate) async fn read_guest_file(
    path: PathBuf,
    no_symlink_parents: Vec<PathBuf>,
    max_bytes: u64,
    keep: Keep,
) -> io::Result<Option<GuestFile>> {
    tokio::task::spawn_blocking(move || {
        read_guest_file_blocking(&path, &no_symlink_parents, max_bytes, keep)
    })
    .await
    .map_err(io::Error::other)?
}

fn read_guest_file_blocking(
    path: &Path,
    no_symlink_parents: &[PathBuf],
    max_bytes: u64,
    keep: Keep,
) -> io::Result<Option<GuestFile>> {
    for dir in no_symlink_parents {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(refused(dir, "is a symlink"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        // ELOOP: the final component is a symlink.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(refused(path, "is a symlink"));
        }
        Err(e) => return Err(e),
    };

    // fstat on the opened descriptor, so nothing can be swapped in between.
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(refused(path, "is not a regular file"));
    }

    let len = meta.len();
    let (start, truncated) = match keep {
        Keep::Head => (0, len > max_bytes),
        Keep::Tail => (len.saturating_sub(max_bytes), len > max_bytes),
        Keep::From(offset) => {
            let start = offset.min(len);
            (start, len - start > max_bytes)
        }
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    // `take` also bounds a file that grows while being read.
    file.take(max_bytes).read_to_end(&mut bytes)?;
    Ok(Some(GuestFile {
        bytes,
        truncated,
        len,
    }))
}

fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("refusing to read {}: {why}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(
        path: &Path,
        parents: &[&Path],
        max: u64,
        keep: Keep,
    ) -> io::Result<Option<GuestFile>> {
        read_guest_file(
            path.to_path_buf(),
            parents.iter().map(|p| p.to_path_buf()).collect(),
            max,
            keep,
        )
        .await
    }

    #[tokio::test]
    async fn reads_a_small_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"hello").unwrap();
        let got = read(&path, &[], 100, Keep::Head).await.unwrap().unwrap();
        assert_eq!(
            got,
            GuestFile {
                bytes: b"hello".to_vec(),
                truncated: false,
                len: 5,
            }
        );
    }

    #[tokio::test]
    async fn missing_file_or_parent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read(&dir.path().join("nope"), &[], 10, Keep::Head)
                .await
                .unwrap()
                .is_none()
        );
        let parent = dir.path().join("absent-dir");
        assert!(
            read(&parent.join("f"), &[&parent], 10, Keep::Head)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn keeps_the_head_or_the_tail_of_an_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"0123456789").unwrap();
        let head = read(&path, &[], 4, Keep::Head).await.unwrap().unwrap();
        assert_eq!(
            head,
            GuestFile {
                bytes: b"0123".to_vec(),
                truncated: true,
                len: 10,
            }
        );
        let tail = read(&path, &[], 4, Keep::Tail).await.unwrap().unwrap();
        assert_eq!(
            tail,
            GuestFile {
                bytes: b"6789".to_vec(),
                truncated: true,
                len: 10,
            }
        );
    }

    #[tokio::test]
    async fn reads_from_an_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"0123456789").unwrap();
        let got = read(&path, &[], 4, Keep::From(3)).await.unwrap().unwrap();
        assert_eq!(got.bytes, b"3456");
        assert!(got.truncated);
        let got = read(&path, &[], 100, Keep::From(8)).await.unwrap().unwrap();
        assert_eq!((got.bytes.as_slice(), got.truncated), (&b"89"[..], false));
        let past = read(&path, &[], 100, Keep::From(50))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(past.bytes, b"");
        assert_eq!(past.len, 10);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_symlinked_file() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, b"host secret").unwrap();
        let link = dir.path().join("console.log");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let err = read(&link, &[], 100, Keep::Tail).await.unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_symlinked_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("console.log"), b"x").unwrap();
        let link = dir.path().join("vm");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = read(&link.join("console.log"), &[&link], 100, Keep::Tail)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    /// Opening a FIFO for reading would block forever without `O_NONBLOCK`.
    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_fifo_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("console.log");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read(&fifo, &[], 100, Keep::Tail),
        )
        .await
        .expect("reading a FIFO must not hang");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[tokio::test]
    async fn refuses_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path(), &[], 100, Keep::Head).await.is_err());
    }
}
