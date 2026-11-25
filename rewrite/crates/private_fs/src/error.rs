//! [`Error`]: what can go wrong with a [`PrivateDir`](crate::PrivateDir).

use std::fmt;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A relative path would depend on the working directory.
    #[error("{path} is not an absolute path")]
    NotAbsolute { path: PathBuf },
    /// The directory exists but can't be trusted to keep files private.
    #[error("{path} {problem}")]
    Unsafe { path: PathBuf, problem: Unsafe },
    /// [`PrivateDir::create_new`](crate::PrivateDir::create_new) found a
    /// file already there; it is left untouched.
    #[error("{path} already exists")]
    AlreadyExists { path: PathBuf },
    /// Another process holds the lock.
    #[error("{path} is locked by another process")]
    Locked { path: PathBuf },
    #[error("{op} {path}: {source}")]
    Io {
        op: Op,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(op: Op, path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { op, path, source }
    }
}

/// Why an existing directory isn't private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsafe {
    /// A file, or a symlink (which could be repointed).
    NotADirectory,
    OwnedByAnotherUser,
}

impl fmt::Display for Unsafe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotADirectory => "is not a directory (or is a symlink)",
            Self::OwnedByAnotherUser => "is owned by another user",
        })
    }
}

/// The step that failed, for [`Error::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    CreateDir,
    Inspect,
    Narrow,
    FindUser,
    Read,
    CreateTemp,
    Write,
    Sync,
    Rename,
    OpenLock,
    Lock,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CreateDir => "creating directory",
            Self::Inspect => "inspecting",
            Self::Narrow => "restricting permissions of",
            Self::FindUser => "finding this process's user from",
            Self::Read => "reading",
            Self::CreateTemp => "creating a temp file in",
            Self::Write => "writing a temp file for",
            Self::Sync => "syncing",
            Self::Rename => "moving the new file to",
            Self::OpenLock => "opening lock file",
            Self::Lock => "locking",
        })
    }
}
