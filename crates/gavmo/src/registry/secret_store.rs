//! Async wrappers over the blocking secret store.

use std::collections::HashMap;
use std::sync::Arc;

use super::AppState;

impl AppState {
    // --- Secret store helpers ---
    //
    // `SecretStore` calls are synchronous and can be slow (a locked keyring
    // may prompt), so they always run via `spawn_blocking`.

    /// Fetches one secret value (`None` if absent or on backend error).
    pub(super) async fn secret_get(&self, key: String) -> Option<String> {
        let store = Arc::clone(&self.inner.secret_store);
        match tokio::task::spawn_blocking(move || store.get(&key)).await {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "secret store read failed; treating as unset");
                None
            }
            Err(e) => {
                tracing::error!(error = %e, "secret store task failed");
                None
            }
        }
    }

    /// Stores one secret value.
    pub(super) async fn secret_set(&self, key: String, value: String) -> Result<(), String> {
        let store = Arc::clone(&self.inner.secret_store);
        tokio::task::spawn_blocking(move || store.set(&key, &value))
            .await
            .map_err(|e| format!("secret store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Deletes one secret value.
    pub(super) async fn secret_delete(&self, key: String) -> Result<(), String> {
        let store = Arc::clone(&self.inner.secret_store);
        tokio::task::spawn_blocking(move || store.delete(&key))
            .await
            .map_err(|e| format!("secret store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Fetches the values for `keys` in one blocking batch; absent or errored
    /// keys are omitted.
    pub(super) async fn secrets_fetch_many(&self, keys: Vec<String>) -> HashMap<String, String> {
        let store = Arc::clone(&self.inner.secret_store);
        let fetched = tokio::task::spawn_blocking(move || {
            keys.into_iter()
                .filter_map(|key| match store.get(&key) {
                    Ok(value) => value.map(|v| (key, v)),
                    Err(e) => {
                        tracing::warn!(error = %e, "secret store read failed; treating as unset");
                        None
                    }
                })
                .collect()
        })
        .await;
        fetched.unwrap_or_else(|e| {
            tracing::error!(error = %e, "secret store task failed");
            HashMap::new()
        })
    }
}
