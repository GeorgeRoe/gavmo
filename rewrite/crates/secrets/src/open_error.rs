//! [`OpenError`]: why a [`VaultStore`](crate::VaultStore) didn't open.

use std::path::PathBuf;

use crate::FormatError;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Another process (e.g. a second daemon) has the vault open.
    #[error("the vault {path} is in use by another GAVMO process")]
    InUse { path: PathBuf },
    /// Locking or reading the vault failed.
    #[error(transparent)]
    File(private_fs::Error),
    /// The keystore holding the vault's key failed.
    #[error(transparent)]
    Keystore(#[from] keystore::Error),
    /// A vault exists but its key is gone from the keystore. A new key is
    /// deliberately *not* created: that would make the existing secrets
    /// unrecoverable without saying so.
    #[error(
        "the vault {path} exists but its key is missing from {keystore}; \
         restore the key, or move the vault aside to start over"
    )]
    KeyMissing {
        path: PathBuf,
        keystore: &'static str,
    },
    #[error("the vault key in {keystore} is {len} bytes, expected 32")]
    WrongKeyLength { keystore: &'static str, len: usize },
    #[error(transparent)]
    Format(#[from] FormatError),
}

impl From<private_fs::Error> for OpenError {
    fn from(e: private_fs::Error) -> Self {
        match e {
            private_fs::Error::Locked { path } => Self::InUse { path },
            other => Self::File(other),
        }
    }
}
