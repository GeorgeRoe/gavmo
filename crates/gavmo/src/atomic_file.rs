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
