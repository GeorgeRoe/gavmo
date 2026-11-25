//! [`VaultStore`]: all secrets in one encrypted file, held open (and locked)
//! by one process.

use std::sync::{Mutex, MutexGuard, PoisonError};

use keystore::Keystore;
use private_fs::{FileName, LockFile, PrivateDir};

use super::data_key::DataKey;
use super::format;
use super::secret_map::SecretMap;
use crate::{OpenError, SecretKey, SecretStore, SecretValue, StoreError, WriteError};

/// The vault's file name in its directory.
pub const VAULT_FILE: FileName = FileName::new("secrets.vault");

/// Locked for as long as a [`VaultStore`] is open.
const LOCK_FILE: FileName = FileName::new("secrets.vault.lock");

/// All secrets in one encrypted file, decrypted into memory on open and
/// rewritten whole on each change. The map is tiny, so whole-file rewrites
/// are simpler than per-entry updates.
///
/// The vault is locked (an OS file lock on [`LOCK_FILE`]) for as long as
/// the store is open, so two processes can't both hold a copy and silently
/// drop each other's writes.
pub struct VaultStore {
    dir: PrivateDir,
    key: DataKey,
    /// Source of truth, mirrored to the file on every change.
    map: Mutex<SecretMap>,
    _lock: LockFile,
}

impl VaultStore {
    /// Opens the vault [`VAULT_FILE`] in `dir`, whose key `keystore` holds.
    ///
    /// - Errors if another process has it open, the keystore is
    ///   unavailable, or the file is unreadable, foreign or tampered.
    /// - A missing vault starts empty, with a new key only if the keystore
    ///   holds none.
    /// - An existing vault whose key is missing is
    ///   [`OpenError::KeyMissing`], never a silently created new key.
    pub fn open(dir: &PrivateDir, keystore: &dyn Keystore) -> Result<Self, OpenError> {
        let lock = dir.lock(LOCK_FILE)?;
        let (key, map) = match dir.read(VAULT_FILE)? {
            Some(bytes) => {
                let key = load_key(keystore)?.ok_or_else(|| OpenError::KeyMissing {
                    path: dir.file(VAULT_FILE),
                    keystore: keystore.name(),
                })?;
                let map = format::decrypt(&key, &bytes)?;
                (key, map)
            }
            None => (key_for_new_vault(keystore)?, SecretMap::default()),
        };
        Ok(Self {
            dir: dir.clone(),
            key,
            map: Mutex::new(map),
            _lock: lock,
        })
    }

    /// Recovers from poisoning: an update publishes a new map in one
    /// assignment, so the map is never half-changed.
    fn lock(&self) -> MutexGuard<'_, SecretMap> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies `change` to a copy of the map, saves the copy, and only then
    /// publishes it in memory. A failed save leaves memory and disk agreeing
    /// on the old contents, instead of `get` serving a value that would
    /// vanish on restart. `change` returns whether it changed anything; if
    /// not, nothing is written.
    fn update(&self, change: impl FnOnce(&mut SecretMap) -> bool) -> Result<(), WriteError> {
        let mut map = self.lock();
        let mut next = map.clone();
        if !change(&mut next) {
            return Ok(());
        }
        let file = format::encrypt(&self.key, &next)?;
        self.dir.replace(VAULT_FILE, &file)?;
        *map = next;
        Ok(())
    }
}

/// The key `keystore` holds, checked to be a vault key.
fn load_key(keystore: &dyn Keystore) -> Result<Option<DataKey>, OpenError> {
    keystore
        .load()?
        .map(|bytes| {
            DataKey::from_stored(&bytes).ok_or(OpenError::WrongKeyLength {
                keystore: keystore.name(),
                len: bytes.len(),
            })
        })
        .transpose()
}

/// The key for a vault that doesn't exist yet: the one `keystore` holds
/// (an empty vault is only written on its first change), else a new one.
fn key_for_new_vault(keystore: &dyn Keystore) -> Result<DataKey, OpenError> {
    if let Some(key) = load_key(keystore)? {
        return Ok(key);
    }
    let key = DataKey::generate();
    keystore.create(key.as_bytes())?;
    Ok(key)
}

impl SecretStore for VaultStore {
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>, StoreError> {
        Ok(self.lock().get(key.as_str()).map(SecretValue::new))
    }

    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<(), StoreError> {
        self.update(|map| {
            map.insert(key.as_str(), value.expose());
            true
        })
        .map_err(StoreError::from)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), StoreError> {
        self.update(|map| map.remove(key.as_str()))
            .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use base64::Engine as _;
    use keystore::MemoryKeystore;

    use super::*;
    use crate::KeyName;
    use crate::vault::data_key::KEY_LEN;

    fn key(name: &str) -> SecretKey {
        SecretKey::new("/ws", &KeyName::new(name.to_owned()).unwrap())
    }

    fn value(text: &str) -> SecretValue {
        SecretValue::new(text)
    }

    fn get(store: &VaultStore, name: &str) -> Option<String> {
        store
            .get(&key(name))
            .unwrap()
            .map(|v| v.expose().to_owned())
    }

    fn fixed(byte: u8) -> MemoryKeystore {
        MemoryKeystore::holding(&[byte; KEY_LEN])
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        dir: PrivateDir,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        Fixture { _tmp: tmp, dir }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn round_trips_overwrites_and_deletes() {
        let fx = fixture();
        let store = VaultStore::open(&fx.dir, &fixed(1)).unwrap();
        assert_eq!(get(&store, "k"), None);
        store.set(&key("k"), &value("v1")).unwrap();
        store.set(&key("k"), &value("v2")).unwrap();
        assert_eq!(get(&store, "k").as_deref(), Some("v2"));
        store.delete(&key("k")).unwrap();
        assert_eq!(get(&store, "k"), None);
        store.delete(&key("k")).unwrap();
    }

    #[test]
    fn persists_across_reopen_with_the_same_key() {
        let fx = fixture();
        {
            let store = VaultStore::open(&fx.dir, &fixed(7)).unwrap();
            store
                .set(&key("jira"), &value(r#"{"token":"abc"}"#))
                .unwrap();
        }
        let reopened = VaultStore::open(&fx.dir, &fixed(7)).unwrap();
        assert_eq!(
            get(&reopened, "jira").as_deref(),
            Some(r#"{"token":"abc"}"#)
        );
    }

    /// The vault file the previous implementation wrote, with its key as
    /// the OS keyring held it (base64), opens with every secret intact.
    #[test]
    fn opens_a_vault_and_key_from_the_previous_implementation() {
        let fx = fixture();
        let vault = base64::engine::general_purpose::STANDARD
            .decode("R1YCKM5CPoCpMebfPeEYFM4uqlGXyg0tiyqfFxhRqN3/YnPBUwBBHrFDT0uJbUZ4r6/5OIqByc3enEYTj2Vr8D6lSffh5GofUPUsT1Sk3qPJI7ijC5W6fT2Ji0w1UE02sn6B+0B5RYQIbiFCCmYd0L6wxwpDLUKe6MGYL0Ok3TLVPPGqedxohSQudHtKY2SGSg==")
            .unwrap();
        fx.dir.replace(VAULT_FILE, &vault).unwrap();
        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode("QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI=")
            .unwrap();

        let store = VaultStore::open(&fx.dir, &MemoryKeystore::holding(&key_bytes)).unwrap();
        let tool = SecretKey::new("/home/me/repo", &KeyName::new("jira-cred".into()).unwrap());
        let provider = SecretKey::new(
            "gavmo:providers",
            &KeyName::new("vertex/api_key".into()).unwrap(),
        );
        assert_eq!(
            store.get(&tool).unwrap().unwrap().expose(),
            r#"{"token":"abc"}"#
        );
        assert_eq!(store.get(&provider).unwrap().unwrap().expose(), "sk-test");
    }

    #[test]
    fn a_wrong_key_fails_closed() {
        let fx = fixture();
        VaultStore::open(&fx.dir, &fixed(1))
            .unwrap()
            .set(&key("k"), &value("v"))
            .unwrap();
        assert!(matches!(
            VaultStore::open(&fx.dir, &fixed(2)),
            Err(OpenError::Format(crate::FormatError::Decrypt))
        ));
    }

    /// The first open creates a key; a later open with no key next to an
    /// existing vault must not create a new one.
    #[test]
    fn a_key_is_only_created_when_no_vault_exists() {
        let fx = fixture();
        let keys = MemoryKeystore::new();
        VaultStore::open(&fx.dir, &keys)
            .unwrap()
            .set(&key("k"), &value("v"))
            .unwrap();
        assert!(keys.held().is_some(), "first open created a key");
        assert_eq!(
            get(&VaultStore::open(&fx.dir, &keys).unwrap(), "k").as_deref(),
            Some("v")
        );

        // Key lost: a clear error, and still no new key.
        let lost = MemoryKeystore::new();
        let err = VaultStore::open(&fx.dir, &lost).err().unwrap();
        assert!(matches!(err, OpenError::KeyMissing { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("missing from the in-memory keystore"),
            "{err}"
        );
        assert!(lost.held().is_none());
    }

    /// Opening an empty dir twice reuses the key the first open created
    /// (the vault file is only written on the first change).
    #[test]
    fn an_empty_vault_reuses_its_key() {
        let fx = fixture();
        let keys = MemoryKeystore::new();
        drop(VaultStore::open(&fx.dir, &keys).unwrap());
        let first = keys.held().unwrap();
        drop(VaultStore::open(&fx.dir, &keys).unwrap());
        assert_eq!(keys.held().unwrap(), first);
    }

    #[test]
    fn a_stored_key_of_the_wrong_length_is_refused() {
        let fx = fixture();
        let err = VaultStore::open(&fx.dir, &MemoryKeystore::holding(&[1; 16]))
            .err()
            .unwrap();
        assert!(
            matches!(err, OpenError::WrongKeyLength { len: 16, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_second_open_while_one_is_held_is_refused() {
        let fx = fixture();
        let first = VaultStore::open(&fx.dir, &fixed(1)).unwrap();
        assert!(matches!(
            VaultStore::open(&fx.dir, &fixed(1)),
            Err(OpenError::InUse { .. })
        ));
        drop(first);
        VaultStore::open(&fx.dir, &fixed(1)).unwrap();
    }

    #[test]
    fn files_are_owner_only_and_no_temp_files_are_left() {
        use std::os::unix::fs::PermissionsExt as _;
        let fx = fixture();
        let store = VaultStore::open(&fx.dir, &fixed(1)).unwrap();
        store.set(&key("a"), &value("1")).unwrap();
        store.set(&key("b"), &value("2")).unwrap();
        store.delete(&key("a")).unwrap();
        let mode = |name| {
            std::fs::metadata(fx.dir.file(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(VAULT_FILE), 0o600);
        assert_eq!(mode(LOCK_FILE), 0o600);
        assert_eq!(
            entries(fx.dir.path()),
            ["secrets.vault", "secrets.vault.lock"]
        );
    }

    /// A failed write must not leave memory ahead of disk: `get` would serve
    /// a value that silently disappears on restart.
    #[test]
    fn a_failed_write_leaves_memory_unchanged() {
        let fx = fixture();
        let store = VaultStore::open(&fx.dir, &fixed(1)).unwrap();
        store.set(&key("kept"), &value("v")).unwrap();
        // Renaming a file over a non-empty directory fails.
        std::fs::remove_file(fx.dir.file(VAULT_FILE)).unwrap();
        std::fs::create_dir(fx.dir.file(VAULT_FILE)).unwrap();
        std::fs::write(fx.dir.file(VAULT_FILE).join("inside"), b"").unwrap();

        assert!(matches!(
            store.set(&key("k"), &value("v")),
            Err(StoreError::Write(WriteError::File(_)))
        ));
        assert_eq!(get(&store, "k"), None);
        assert!(store.delete(&key("kept")).is_err());
        assert_eq!(get(&store, "kept").as_deref(), Some("v"));
    }

    #[test]
    fn deleting_an_absent_key_writes_nothing() {
        let fx = fixture();
        let store = VaultStore::open(&fx.dir, &fixed(1)).unwrap();
        store.delete(&key("absent")).unwrap();
        assert!(!fx.dir.exists(VAULT_FILE).unwrap());
    }

    #[test]
    fn a_keystore_that_cannot_store_a_key_fails_the_open() {
        let fx = fixture();
        let err = VaultStore::open(&fx.dir, &MemoryKeystore::refusing_create())
            .err()
            .unwrap();
        assert!(
            matches!(
                err,
                OpenError::Keystore(keystore::Error::Unavailable { .. })
            ),
            "{err:?}"
        );
        assert!(!fx.dir.exists(VAULT_FILE).unwrap());
    }

    #[test]
    fn an_existing_vault_with_an_unreachable_keystore_fails_closed() {
        let fx = fixture();
        VaultStore::open(&fx.dir, &fixed(1))
            .unwrap()
            .set(&key("k"), &value("v"))
            .unwrap();
        assert!(matches!(
            VaultStore::open(&fx.dir, &MemoryKeystore::unavailable()),
            Err(OpenError::Keystore(keystore::Error::Unavailable { .. }))
        ));
    }

    /// Anything but "not found" is an error, never an empty vault that the
    /// next write would replace.
    #[test]
    fn an_unreadable_vault_is_an_error_not_an_empty_vault() {
        let fx = fixture();
        std::fs::create_dir(fx.dir.file(VAULT_FILE)).unwrap();
        assert!(matches!(
            VaultStore::open(&fx.dir, &fixed(1)),
            Err(OpenError::File(private_fs::Error::Io {
                op: private_fs::Op::Read,
                ..
            }))
        ));
    }
}
