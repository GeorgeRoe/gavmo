//! [`StoreError`]: why a [`SecretStore`](crate::SecretStore) call failed.

use crate::{OpenError, WriteError};

/// There is no "not found": a missing secret is normal, so `get` returns
/// `Ok(None)`.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store couldn't be opened (for a [`LazyStore`](crate::LazyStore),
    /// on this call's attempt).
    #[error(transparent)]
    Open(#[from] OpenError),
    /// A change couldn't be saved; the store still holds the old contents.
    #[error(transparent)]
    Write(#[from] WriteError),
}
