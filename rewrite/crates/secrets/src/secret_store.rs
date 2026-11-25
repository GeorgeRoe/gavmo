//! [`SecretStore`]: secrets by key.

use crate::{SecretKey, SecretValue, StoreError};

/// Stores and retrieves [`SecretValue`]s by [`SecretKey`]. Structured
/// credentials are JSON-encoded into the value by the caller. Takes `&self`
/// because stores are shared behind an `Arc` and synchronize internally.
/// Calls block, possibly for seconds (a keyring prompting to unlock).
pub trait SecretStore: Send + Sync {
    /// The stored value, or `Ok(None)` if nothing is stored under `key`.
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>, StoreError>;

    /// Stores `value` under `key`, replacing any existing value.
    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<(), StoreError>;

    /// Removes any value stored under `key`; deleting an absent key succeeds.
    fn delete(&self, key: &SecretKey) -> Result<(), StoreError>;
}
