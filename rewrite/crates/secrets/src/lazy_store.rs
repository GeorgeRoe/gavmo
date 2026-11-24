//! [`LazyStore`]: a store opened on first use.

use std::sync::{Mutex, PoisonError};

use crate::{OpenError, SecretKey, SecretStore, SecretValue, StoreError};

/// Opens its store with `open` on first use, so the daemon can start on a
/// host without a working keystore (only secret operations then fail,
/// closed). A failed open is not remembered: the next call tries again, so
/// unlocking the keyring or enabling interop recovers without a restart.
pub struct LazyStore<S, F> {
    open: F,
    opened: Mutex<Option<S>>,
}

impl<S, F> LazyStore<S, F>
where
    S: SecretStore,
    F: Fn() -> Result<S, OpenError> + Send + Sync,
{
    /// Does nothing until first use.
    pub fn new(open: F) -> Self {
        Self {
            open,
            opened: Mutex::new(None),
        }
    }

    /// Runs `f` on the store, opening it first if needed. The lock is held
    /// throughout, so concurrent first calls open it once.
    fn with_store<T>(&self, f: impl FnOnce(&S) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let mut opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        let store = match &mut *opened {
            Some(store) => store,
            empty @ None => empty.insert((self.open)()?),
        };
        f(store)
    }
}

impl<S, F> SecretStore for LazyStore<S, F>
where
    S: SecretStore,
    F: Fn() -> Result<S, OpenError> + Send + Sync,
{
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>, StoreError> {
        self.with_store(|store| store.get(key))
    }

    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<(), StoreError> {
        self.with_store(|store| store.set(key, value))
    }

    fn delete(&self, key: &SecretKey) -> Result<(), StoreError> {
        self.with_store(|store| store.delete(key))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use keystore::MemoryKeystore;
    use private_fs::PrivateDir;

    use super::*;
    use crate::{KeyName, VaultStore};

    fn key() -> SecretKey {
        SecretKey::new("/ws", &KeyName::new("k".to_owned()).unwrap())
    }

    #[test]
    fn opens_on_first_use_only_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        let keys = MemoryKeystore::new();
        let opens = AtomicUsize::new(0);
        let store = LazyStore::new(|| {
            opens.fetch_add(1, Ordering::Relaxed);
            VaultStore::open(&dir, &keys)
        });
        assert_eq!(opens.load(Ordering::Relaxed), 0, "nothing opened yet");

        store.set(&key(), &SecretValue::new("v")).unwrap();
        assert_eq!(store.get(&key()).unwrap().unwrap().expose(), "v");
        store.delete(&key()).unwrap();
        assert!(store.get(&key()).unwrap().is_none());
        assert_eq!(opens.load(Ordering::Relaxed), 1);
    }

    /// Fails closed while the keystore is down, and recovers once it is up,
    /// without being recreated.
    #[test]
    fn a_failed_open_is_retried_on_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        let up = std::sync::atomic::AtomicBool::new(false);
        let store = LazyStore::new(|| {
            let keys = if up.load(Ordering::Relaxed) {
                MemoryKeystore::holding(&[1; 32])
            } else {
                MemoryKeystore::unavailable()
            };
            VaultStore::open(&dir, &keys)
        });
        // An error, not `Ok(None)`: fail closed.
        assert!(matches!(store.get(&key()), Err(StoreError::Open(_))));
        assert!(store.set(&key(), &SecretValue::new("v")).is_err());

        up.store(true, Ordering::Relaxed);
        store.set(&key(), &SecretValue::new("v")).unwrap();
        assert_eq!(store.get(&key()).unwrap().unwrap().expose(), "v");
    }

    #[test]
    fn works_behind_a_shared_trait_object() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        let store: Arc<dyn SecretStore> = Arc::new(LazyStore::new(move || {
            VaultStore::open(&dir, &MemoryKeystore::holding(&[4; 32]))
        }));
        store.set(&key(), &SecretValue::new("v")).unwrap();
        assert_eq!(store.get(&key()).unwrap().unwrap().expose(), "v");
    }
}
