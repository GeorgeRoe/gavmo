//! Async wrappers over the blocking secret store.
//!
//! `SecretStore` calls are synchronous and can be slow (a locked keyring may
//! prompt), so they always run via `spawn_blocking`. Store failures are
//! returned, never folded into "not set": a locked keyring must show up as
//! "secret store unavailable", not as every tool being unconfigured.

use std::collections::HashMap;
use std::sync::Arc;

use super::AppState;
use super::error::Result;

impl AppState {
    /// Fetches one secret value (`None` if absent).
    pub(super) async fn secret_get(&self, key: String) -> Result<Option<String>> {
        let store = Arc::clone(&self.inner.secret_store);
        Ok(tokio::task::spawn_blocking(move || store.get(&key)).await??)
    }

    /// Stores one secret value.
    pub(super) async fn secret_set(&self, key: String, value: String) -> Result<()> {
        let store = Arc::clone(&self.inner.secret_store);
        Ok(tokio::task::spawn_blocking(move || store.set(&key, &value)).await??)
    }

    /// Deletes one secret value.
    pub(super) async fn secret_delete(&self, key: String) -> Result<()> {
        let store = Arc::clone(&self.inner.secret_store);
        Ok(tokio::task::spawn_blocking(move || store.delete(&key)).await??)
    }

    /// Fetches the values for `keys` in one blocking batch; absent keys are
    /// omitted. Fails as a whole if the store can't be read.
    pub(super) async fn secrets_fetch_many(
        &self,
        keys: Vec<String>,
    ) -> Result<HashMap<String, String>> {
        let store = Arc::clone(&self.inner.secret_store);
        let fetched = tokio::task::spawn_blocking(move || {
            let mut found = HashMap::new();
            for key in keys {
                if let Some(value) = store.get(&key)? {
                    found.insert(key, value);
                }
            }
            Ok::<_, secrets::SecretError>(found)
        })
        .await??;
        Ok(fetched)
    }
}
