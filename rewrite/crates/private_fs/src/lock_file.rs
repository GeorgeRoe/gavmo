//! [`LockFile`]: an exclusive lock held for as long as the value lives.

use std::fs::File;
use std::path::PathBuf;

/// Proof that this process holds an exclusive OS lock on a file in a
/// [`PrivateDir`](crate::PrivateDir) (see
/// [`PrivateDir::lock`](crate::PrivateDir::lock)). Dropping it releases the
/// lock; so does the process exiting, however it exits.
#[derive(Debug)]
#[must_use = "the lock is released when this is dropped"]
pub struct LockFile {
    path: PathBuf,
    _file: File,
}

impl LockFile {
    pub(crate) fn new(path: PathBuf, file: File) -> Self {
        Self { path, _file: file }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}
