//! In-process backend for tests (behind the `test-util` feature).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{SecretError, SecretStore};

/// Not persisted and not encrypted; for tests only.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

/// Redacting: lists keys only, never values, so `{:?}` can't leak secrets.
impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("keys", &self.lock().keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Recovers from poisoning: the map has no cross-entry invariant to protect.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.lock().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.lock().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_overwrites_and_deletes_idempotently() {
        let store = MemoryStore::new();
        assert_eq!(store.get("k").unwrap(), None);
        store.set("k", "v1").unwrap();
        store.set("k", "v2").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v2".to_owned()));
        store.delete("k").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
        store.delete("k").unwrap();
    }

    #[test]
    fn debug_does_not_print_values() {
        let store = MemoryStore::new();
        store.set("name", "hunter2").unwrap();
        let debug = format!("{store:?}");
        assert!(debug.contains("name"), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
    }
}
