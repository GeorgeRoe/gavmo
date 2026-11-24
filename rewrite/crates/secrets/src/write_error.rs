//! [`WriteError`]: why a change to a [`VaultStore`](crate::VaultStore)
//! wasn't saved.

use crate::FormatError;

/// The vault, in memory and on disk, still holds the old contents.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error(transparent)]
    File(#[from] private_fs::Error),
}
