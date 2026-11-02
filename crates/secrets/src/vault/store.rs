//! [`VaultStore`]: all secrets in one encrypted file, held open (and locked)
//! by one process; and [`LazyVaultStore`], which opens it on first use.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::format::{self, SecretMap};
use crate::{DataKey, KeyProvider, KeyringKeyProvider, SecretError, SecretStore};

/// All secrets in one encrypted file, decrypted into memory on open and
/// rewritten wholesale on each mutation. The map is tiny, so whole-file
/// rewrites are simpler than per-entry updates.
///
/// The vault is locked (an OS file lock on `<vault>.lock`) for as long as the
/// store is open, so two processes can't both hold a copy and silently drop
/// each other's writes.
pub struct VaultStore {
    path: PathBuf,
    key: DataKey,
    /// Source of truth, mirrored to `path` on every mutation.
    map: Mutex<SecretMap>,
    /// Held for the store's lifetime; the lock is released on drop.
    _lock: File,
}

impl VaultStore {
    /// Opens the vault at `path`.
    ///
    /// - Errors if another process has it open, the key store is
    ///   unavailable, or the file is unreadable, foreign or tampered.
    /// - A missing vault starts empty, creating a key only if none exists.
    /// - An existing vault whose key is missing is [`SecretError::KeyMissing`]
    ///   rather than a silently minted new key.
    /// - A v1 vault is upgraded in place to v2 (header authenticated); the
    ///   original is kept as `<vault>.v1.bak`.
    pub fn open(
        path: impl Into<PathBuf>,
        key_provider: &dyn KeyProvider,
    ) -> Result<Self, SecretError> {
        let path = path.into();
        let dir = parent_dir(&path)?;
        create_private_dir_all(dir)?;
        let lock = lock_vault(&path)?;

        let (key, map, version) = match std::fs::read(&path) {
            Ok(bytes) => {
                let key = key_provider
                    .load_key()?
                    .ok_or_else(|| SecretError::KeyMissing { path: path.clone() })?;
                let (map, version) = format::decrypt(&key, &bytes)?;
                if version != format::VERSION {
                    write_new(&with_suffix(&path, ".v1.bak"), &bytes)?;
                }
                (key, map, version)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = match key_provider.load_key()? {
                    Some(key) => key,
                    None => key_provider.create_key()?,
                };
                (key, SecretMap::default(), format::VERSION)
            }
            Err(e) => return Err(SecretError::io("reading vault", &path)(e)),
        };

        let store = Self {
            path,
            key,
            map: Mutex::new(map),
            _lock: lock,
        };
        if version != format::VERSION {
            let map = store.lock().clone();
            store.flush(&map)?;
        }
        Ok(store)
    }

    /// [`Self::open`] with a [`KeyringKeyProvider`].
    pub fn open_keyring_backed(path: impl Into<PathBuf>) -> Result<Self, SecretError> {
        Self::open(path, &KeyringKeyProvider)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SecretMap> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Durably replaces the vault file with the encrypted `map`, so a crash
    /// leaves either the old or the new vault, never a truncated one: a
    /// uniquely named `0600` temp file in the same dir is written and
    /// fsynced, renamed over the vault, and the dir is fsynced. On any error
    /// the temp file is removed (by `NamedTempFile`'s drop).
    fn flush(&self, map: &SecretMap) -> Result<(), SecretError> {
        use std::io::Write as _;

        let payload = format::encrypt(&self.key, map)?;
        let dir = parent_dir(&self.path)?;
        create_private_dir_all(dir)?;
        let mut tmp = private_tempfile_builder()
            .tempfile_in(dir)
            .map_err(SecretError::io("creating temp file for", &self.path))?;
        tmp.write_all(&payload)
            .map_err(SecretError::io("writing temp file for", &self.path))?;
        tmp.as_file()
            .sync_all()
            .map_err(SecretError::io("syncing temp file for", &self.path))?;
        tmp.persist(&self.path)
            .map_err(|e| SecretError::io("replacing", &self.path)(e.error))?;
        sync_dir(dir)
    }

    /// Applies `mutate` to a copy of the map, persists the copy, and only then
    /// publishes it in memory. A failed write therefore leaves memory and disk
    /// agreeing on the old contents, instead of `get` serving a value that
    /// would vanish on restart.
    fn update(&self, mutate: impl FnOnce(&mut SecretMap) -> bool) -> Result<(), SecretError> {
        let mut map = self.lock();
        let mut next = map.clone();
        if !mutate(&mut next) {
            // Nothing changed, so skip the rewrite.
            return Ok(());
        }
        self.flush(&next)?;
        *map = next;
        Ok(())
    }
}

impl SecretStore for VaultStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.lock().0.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.update(|map| {
            map.0.insert(key.to_owned(), value.to_owned());
            true
        })
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.update(|map| {
            map.0.remove(key).is_some_and(|mut old| {
                zeroize::Zeroize::zeroize(&mut old);
                true
            })
        })
    }
}

/// Opens its [`VaultStore`] on first use, so a host without a working keystore
/// (e.g. stock WSL) can still start the daemon. Failed opens aren't cached, so
/// unlocking the keyring and retrying recovers without a restart.
pub struct LazyVaultStore {
    path: PathBuf,
    key_provider: Box<dyn KeyProvider>,
    opened: Mutex<Option<VaultStore>>,
}

impl LazyVaultStore {
    /// Does no I/O and doesn't touch the keystore until first use.
    pub fn new(path: impl Into<PathBuf>, key_provider: Box<dyn KeyProvider>) -> Self {
        Self {
            path: path.into(),
            key_provider,
            opened: Mutex::new(None),
        }
    }

    /// [`Self::new`] with a [`KeyringKeyProvider`].
    pub fn keyring_backed(path: impl Into<PathBuf>) -> Self {
        Self::new(path, Box::new(KeyringKeyProvider))
    }

    fn with_vault<T>(
        &self,
        f: impl FnOnce(&VaultStore) -> Result<T, SecretError>,
    ) -> Result<T, SecretError> {
        let mut guard = self
            .opened
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let vault = match &mut *guard {
            Some(vault) => vault,
            empty @ None => empty.insert(VaultStore::open(
                self.path.clone(),
                self.key_provider.as_ref(),
            )?),
        };
        f(vault)
    }
}

impl SecretStore for LazyVaultStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        self.with_vault(|v| v.get(key))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.with_vault(|v| v.set(key, value))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.with_vault(|v| v.delete(key))
    }
}

fn parent_dir(path: &Path) -> Result<&Path, SecretError> {
    path.parent()
        .ok_or_else(|| SecretError::Backend(format!("vault path {} has no parent", path.display())))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// Takes the vault's exclusive lock (non-blocking).
fn lock_vault(path: &Path) -> Result<File, SecretError> {
    let lock_path = with_suffix(path, ".lock");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(&lock_path)
        .map_err(SecretError::io("opening lock file", &lock_path))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(SecretError::InUse {
            path: path.to_path_buf(),
        }),
        Err(std::fs::TryLockError::Error(e)) => Err(SecretError::io("locking", &lock_path)(e)),
    }
}

/// Writes `bytes` to a new `0600` file at `path`; an existing file is kept
/// (it is an earlier backup).
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Ok(mut file) => file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(SecretError::io("writing backup", path)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(SecretError::io("creating backup", path)(e)),
    }
}

/// A temp-file builder for vault writes: `0600` on Unix, applied at creation
/// (also `tempfile`'s default, but this must never silently regress).
/// Elsewhere the platform default applies (`tempfile` rejects explicit
/// permissions there); Windows inherits ACLs from the per-user profile dir.
fn private_tempfile_builder() -> tempfile::Builder<'static, 'static> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".secrets.vault.").suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    builder
}

/// Creates `dir` (and parents) with mode `0700` on Unix where they don't yet
/// exist. Existing directories are left as they are.
fn create_private_dir_all(dir: &Path) -> Result<(), SecretError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(SecretError::io("creating", dir))
}

/// Fsyncs a directory so a rename inside it is durable. Unix only: Windows
/// can't open directories this way and `MoveFileEx` semantics differ.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), SecretError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(SecretError::io("syncing", dir))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), SecretError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KEY_LEN;

    /// A key provider over one in-memory slot.
    struct FakeKeys(Mutex<Option<[u8; KEY_LEN]>>);
    impl KeyProvider for FakeKeys {
        fn load_key(&self) -> Result<Option<DataKey>, SecretError> {
            Ok(self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .map(DataKey::from_bytes))
        }
        fn create_key(&self) -> Result<DataKey, SecretError> {
            let key = DataKey::generate();
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(*key.as_bytes());
            Ok(key)
        }
    }

    fn fixed(byte: u8) -> FakeKeys {
        FakeKeys(Mutex::new(Some([byte; KEY_LEN])))
    }

    fn no_key() -> FakeKeys {
        FakeKeys(Mutex::new(None))
    }

    /// Simulates an unreachable keystore: entirely, or (`reads`) only for
    /// storing a new key.
    struct UnavailableKey {
        reads: bool,
    }
    impl KeyProvider for UnavailableKey {
        fn load_key(&self) -> Result<Option<DataKey>, SecretError> {
            if self.reads {
                Ok(None)
            } else {
                Err(SecretError::Backend("no keystore".to_owned()))
            }
        }
        fn create_key(&self) -> Result<DataKey, SecretError> {
            Err(SecretError::Backend("no keystore".to_owned()))
        }
    }

    fn vault_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("secrets.vault")
    }

    #[test]
    fn round_trips_overwrites_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        assert_eq!(store.get("k").unwrap(), None);
        store.set("k", "v1").unwrap();
        store.set("k", "v2").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v2".to_owned()));
        store.delete("k").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
        store.delete("k").unwrap();
    }

    #[test]
    fn persists_across_reopen_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        {
            let store = VaultStore::open(&path, &fixed(7)).unwrap();
            store.set("jira", r#"{"token":"abc"}"#).unwrap();
        }
        let reopened = VaultStore::open(&path, &fixed(7)).unwrap();
        assert_eq!(
            reopened.get("jira").unwrap().as_deref(),
            Some(r#"{"token":"abc"}"#)
        );
    }

    #[test]
    fn wrong_key_or_tampering_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        {
            let store = VaultStore::open(&path, &fixed(1)).unwrap();
            store.set("k", "v").unwrap();
        }
        assert!(matches!(
            VaultStore::open(&path, &fixed(2)),
            Err(SecretError::Decrypt)
        ));
    }

    /// The first open creates a key; a later open with no key next to an
    /// existing vault must not mint a new one.
    #[test]
    fn a_key_is_only_created_when_no_vault_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let keys = no_key();
        {
            let store = VaultStore::open(&path, &keys).unwrap();
            store.set("k", "v").unwrap();
        }
        assert!(keys.0.lock().unwrap().is_some(), "first open created a key");
        // Same keys: reopens fine.
        assert_eq!(
            VaultStore::open(&path, &keys)
                .unwrap()
                .get("k")
                .unwrap()
                .as_deref(),
            Some("v")
        );
        // Key lost: a clear error, and still no new key.
        let lost = no_key();
        assert!(matches!(
            VaultStore::open(&path, &lost),
            Err(SecretError::KeyMissing { .. })
        ));
        assert!(lost.0.lock().unwrap().is_none());
    }

    #[test]
    fn a_second_open_while_one_is_held_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let first = VaultStore::open(&path, &fixed(1)).unwrap();
        assert!(matches!(
            VaultStore::open(&path, &fixed(1)),
            Err(SecretError::InUse { .. })
        ));
        drop(first);
        VaultStore::open(&path, &fixed(1)).unwrap();
    }

    /// v1 vaults are upgraded to v2 on open, keeping a backup.
    #[test]
    fn a_v1_vault_is_migrated_and_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let key = DataKey::from_bytes([3; KEY_LEN]);
        let v1 = format::tests::encrypt_v1(
            &key,
            &SecretMap([("k".to_owned(), "v".to_owned())].into_iter().collect()),
        );
        std::fs::write(&path, &v1).unwrap();

        let store = VaultStore::open(&path, &fixed(3)).unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some("v"));
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw[2], format::VERSION, "rewritten as v2");
        assert_eq!(
            std::fs::read(dir.path().join("secrets.vault.v1.bak")).unwrap(),
            v1
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_and_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("new-dir");
        let path = sub.join("secrets.vault");
        let store = VaultStore::open(&path, &fixed(1)).unwrap();
        store.set("k", "v").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&sub), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&sub.join("secrets.vault.lock")), 0o600);
    }

    /// A failed write must not leave memory ahead of disk: `get` would serve a
    /// value that silently disappears on restart.
    #[test]
    fn a_failed_write_leaves_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let store = VaultStore::open(sub.join("secrets.vault"), &fixed(1)).unwrap();
        store.set("kept", "v").unwrap();
        // A *file* where the vault's directory should be makes every flush fail.
        std::fs::remove_dir_all(&sub).unwrap();
        std::fs::write(&sub, b"").unwrap();

        assert!(store.set("k", "v").is_err());
        assert_eq!(store.get("k").unwrap(), None);
        assert!(store.delete("kept").is_err());
        assert_eq!(store.get("kept").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn writes_leave_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        store.set("a", "1").unwrap();
        store.set("b", "2").unwrap();
        store.delete("a").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["secrets.vault", "secrets.vault.lock"]);
    }

    #[test]
    fn lazy_vault_defers_open_and_fails_closed_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LazyVaultStore::new(vault_path(&dir), Box::new(UnavailableKey { reads: false }));
        // An error, not `Ok(None)`: fail closed.
        assert!(store.get("k").is_err());
        assert!(store.set("k", "v").is_err());
    }

    #[test]
    fn lazy_vault_opens_on_first_use_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store: std::sync::Arc<dyn SecretStore> =
            std::sync::Arc::new(LazyVaultStore::new(vault_path(&dir), Box::new(fixed(4))));
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some("v"));
    }
    #[test]
    fn lazy_vault_deletes_through_the_opened_vault() {
        let dir = tempfile::tempdir().unwrap();
        let store = LazyVaultStore::new(vault_path(&dir), Box::new(fixed(4)));
        store.set("k", "v").unwrap();
        store.delete("k").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
    }

    /// A keystore that can be read but refuses a new key fails the open,
    /// rather than leaving a vault that can't be decrypted later.
    #[test]
    fn a_key_that_cannot_be_stored_fails_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let err = VaultStore::open(vault_path(&dir), &UnavailableKey { reads: true })
            .err()
            .unwrap();
        assert!(matches!(err, SecretError::Backend(_)), "{err:?}");
        assert!(!vault_path(&dir).exists());
    }

    #[test]
    fn an_existing_vault_with_an_unreachable_keystore_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        VaultStore::open(vault_path(&dir), &fixed(1))
            .unwrap()
            .set("k", "v")
            .unwrap();
        let err = VaultStore::open(vault_path(&dir), &UnavailableKey { reads: false })
            .err()
            .unwrap();
        assert!(matches!(err, SecretError::Backend(_)), "{err:?}");
    }

    /// Anything but "not found" is an error, never an empty vault that the
    /// next write would replace.
    #[test]
    fn an_unreadable_vault_is_an_error_not_an_empty_vault() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(vault_path(&dir)).unwrap();
        let err = VaultStore::open(vault_path(&dir), &fixed(1)).err().unwrap();
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "reading vault",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_vault_path_without_a_usable_parent_is_refused() {
        let err = VaultStore::open("", &fixed(1)).err().unwrap();
        assert!(matches!(err, SecretError::Backend(_)), "{err:?}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let err = VaultStore::open(file.join("secrets.vault"), &fixed(1))
            .err()
            .unwrap();
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "creating",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// Runs `f` with `dir` read-only, restoring it after so the temp dir can
    /// be cleaned up.
    #[cfg(unix)]
    fn with_read_only_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = f();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        result
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_file_that_cannot_be_created_fails_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let err = with_read_only_dir(dir.path(), || {
            VaultStore::open(vault_path(&dir), &fixed(1)).err().unwrap()
        });
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "opening lock file",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    fn write_v1_vault(path: &Path) -> Vec<u8> {
        let key = DataKey::from_bytes([3; KEY_LEN]);
        let v1 = format::tests::encrypt_v1(
            &key,
            &SecretMap([("k".to_owned(), "v".to_owned())].into_iter().collect()),
        );
        std::fs::write(path, &v1).unwrap();
        v1
    }

    /// An existing backup is from an earlier migration attempt: it is kept.
    #[test]
    fn an_existing_v1_backup_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        write_v1_vault(&path);
        let backup = dir.path().join("secrets.vault.v1.bak");
        std::fs::write(&backup, b"earlier").unwrap();

        let store = VaultStore::open(&path, &fixed(3)).unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some("v"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"earlier");
    }

    /// A migration that can't write its backup, or the upgraded vault, fails
    /// and leaves the v1 file as it was.
    #[cfg(unix)]
    #[test]
    fn a_migration_that_cannot_write_fails_and_keeps_the_v1_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let v1 = write_v1_vault(&path);
        // Created up front: the lock file must open in the read-only dir.
        drop(VaultStore::open(dir.path().join("other.vault"), &fixed(1)));
        std::fs::write(with_suffix(&path, ".lock"), b"").unwrap();

        let err = with_read_only_dir(dir.path(), || {
            VaultStore::open(&path, &fixed(3)).err().unwrap()
        });
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "creating backup",
                    ..
                }
            ),
            "{err:?}"
        );

        std::fs::write(with_suffix(&path, ".v1.bak"), &v1).unwrap();
        let err = with_read_only_dir(dir.path(), || {
            VaultStore::open(&path, &fixed(3)).err().unwrap()
        });
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "creating temp file for",
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), v1);
    }

    #[test]
    fn a_vault_that_cannot_be_replaced_keeps_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let store = VaultStore::open(&path, &fixed(1)).unwrap();
        // Renaming a file over a directory fails.
        std::fs::create_dir(&path).unwrap();
        let err = store.set("k", "v").unwrap_err();
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "replacing",
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(store.get("k").unwrap(), None);
    }

    /// Set in the child process of the keyring test below.
    const NO_BUS_CHILD: &str = "GAVMO_SECRETS_TEST_NO_BUS_CHILD";

    /// Without a reachable Secret Service, the keyring-backed vaults fail
    /// closed. The real work runs in a child process whose session bus
    /// points at nothing, so the keyring of whoever runs the tests is never
    /// touched (the environment can't be changed safely in-process).
    #[test]
    fn keyring_backed_vaults_fail_closed_without_a_secret_service() {
        let dir = tempfile::tempdir().unwrap();
        if std::env::var_os(NO_BUS_CHILD).is_some() {
            let unavailable =
                |r: Result<_, SecretError>| matches!(r, Err(SecretError::KeyUnavailable(_)));
            assert!(unavailable(KeyringKeyProvider.load_key().map(drop)));
            assert!(unavailable(KeyringKeyProvider.create_key().map(drop)));
            assert!(unavailable(
                VaultStore::open_keyring_backed(vault_path(&dir)).map(drop)
            ));
            let lazy = LazyVaultStore::keyring_backed(vault_path(&dir));
            assert!(unavailable(lazy.get("k").map(drop)));
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "vault::store::tests::keyring_backed_vaults_fail_closed_without_a_secret_service",
                "--nocapture",
            ])
            .env(NO_BUS_CHILD, "1")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", dir.path().join("no-bus").display()),
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
