//! [`MemoryStore`]: an in-process store for tests (behind the `test-util`
//! feature).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::{SecretKey, SecretStore, SecretValue, StoreError};

/// Not persisted and not encrypted; for tests only.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<SecretKey, SecretValue>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Recovers from poisoning: the map has no cross-entry invariant.
    fn lock(&self) -> MutexGuard<'_, HashMap<SecretKey, SecretValue>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Lists keys only; values are redacted anyway.
impl fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStore")
            .field("keys", &self.lock().keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>, StoreError> {
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<(), StoreError> {
        self.lock().insert(key.clone(), value.clone());
        Ok(())
    }

    fn delete(&self, key: &SecretKey) -> Result<(), StoreError> {
        self.lock().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyName;

    fn key() -> SecretKey {
        SecretKey::new("/ws", &KeyName::new("name".to_owned()).unwrap())
    }

    #[test]
    fn round_trips_overwrites_and_deletes_idempotently() {
        let store = MemoryStore::new();
        assert!(store.get(&key()).unwrap().is_none());
        store.set(&key(), &SecretValue::new("v1")).unwrap();
        store.set(&key(), &SecretValue::new("v2")).unwrap();
        assert_eq!(store.get(&key()).unwrap().unwrap().expose(), "v2");
        store.delete(&key()).unwrap();
        assert!(store.get(&key()).unwrap().is_none());
        store.delete(&key()).unwrap();
    }

    #[test]
    fn debug_does_not_print_values() {
        let store = MemoryStore::new();
        store.set(&key(), &SecretValue::new("hunter2")).unwrap();
        let debug = format!("{store:?}");
        assert!(debug.contains("name"), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
    }
}
