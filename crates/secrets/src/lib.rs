//! Host-side secret storage for GAVMO's tool gateway: opaque strings by key.
//!
//! The main backend, [`VaultStore`], is one XChaCha20-Poly1305-encrypted file
//! whose data key is the single GAVMO entry in the OS keystore. It fails closed:
//! if the keystore is unreachable, every operation errors; there is no
//! plaintext fallback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

/// Errors from a secret backend. There is no `NotFound`: a missing secret is
/// normal, so `get` returns `Ok(None)`.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// Keystore or vault failure. The message is for display, not control flow.
    #[error("secret store backend error: {0}")]
    Backend(String),
}

/// Store and retrieve opaque secret strings by an opaque key. Structured
/// credentials are JSON-encoded into the value by the caller. Takes `&self`
/// because stores are shared behind an `Arc` and synchronize internally.
pub trait SecretStore: Send + Sync {
    /// Returns the stored value, or `Ok(None)` if nothing is stored under `key`.
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;

    /// Stores `value` under `key`, overwriting any existing value.
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;

    /// Removes any value stored under `key`; deleting an absent key succeeds.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// The `service` name all GAVMO entries live under in the OS keystore.
pub const KEYRING_SERVICE: &str = "gavmo";

/// Composes a storage key from a workspace id (its absolute host path) and a
/// secret name, binding each secret to one workspace. The `\u{1f}` (Unit
/// Separator) join can't appear in typed input, so keys can't collide.
pub fn secret_key(workspace_id: &str, name: &str) -> String {
    format!("{workspace_id}\u{1f}{name}")
}

/// One OS keystore entry per secret. Superseded by [`VaultStore`] in the daemon;
/// prior entries are not migrated because `keyring` has no list API.
#[derive(Debug, Default, Clone)]
pub struct KeyringStore;

impl KeyringStore {
    /// Does not touch the keystore; failures surface on first use.
    pub fn new() -> Self {
        Self
    }

    fn entry(key: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(KEYRING_SERVICE, key).map_err(|e| SecretError::Backend(e.to_string()))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        Self::entry(key)?
            .set_password(value)
            .map_err(|e| SecretError::Backend(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }
}

/// In-process backend for tests. Not persisted and not encrypted.
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
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.lock().insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.lock().remove(key);
        Ok(())
    }
}

impl MemoryStore {
    /// Recovers from poisoning: the map has no cross-entry invariant to protect.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// --- VaultStore ---

/// XChaCha20-Poly1305 uses a 256-bit key.
const KEY_LEN: usize = 32;

/// Rejects foreign files up front with a clear error instead of an AEAD failure.
const VAULT_MAGIC: [u8; 2] = *b"GV";

/// On-disk format version; bump only on an incompatible layout change.
const VAULT_VERSION: u8 = 1;

/// Keyring entry holding the vault's base64-encoded data key.
pub const VAULT_KEY_ENTRY: &str = "vault-data-key";

/// Supplies the vault's data key; abstracted so the crypto path is testable
/// without an OS keystore. Returning `Err` is what makes [`VaultStore`] fail
/// closed.
pub trait KeyProvider: Send + Sync {
    /// Returns the data key, creating and persisting one on first use. `Err`
    /// means the backing store is unavailable, not that no key exists yet.
    fn load_or_create_key(&self) -> Result<[u8; KEY_LEN], SecretError>;
}

/// Production [`KeyProvider`]: the key lives in the [`VAULT_KEY_ENTRY`] keyring
/// entry.
#[derive(Debug, Default, Clone)]
pub struct KeyringKeyProvider;

impl KeyringKeyProvider {
    pub fn new() -> Self {
        Self
    }
}

impl KeyProvider for KeyringKeyProvider {
    fn load_or_create_key(&self) -> Result<[u8; KEY_LEN], SecretError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, VAULT_KEY_ENTRY)
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(encoded) => decode_key(&encoded),
            // First run: mint a key. Any other error propagates so the vault
            // fails closed.
            Err(keyring::Error::NoEntry) => {
                let key = Key::generate();
                let encoded = base64::engine::general_purpose::STANDARD.encode(key.as_slice());
                entry
                    .set_password(&encoded)
                    .map_err(|e| SecretError::Backend(e.to_string()))?;
                Ok(key.into())
            }
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }
}

fn decode_key(encoded: &str) -> Result<[u8; KEY_LEN], SecretError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| SecretError::Backend(format!("vault data-key is not valid base64: {e}")))?;
    let out: [u8; KEY_LEN] = bytes.try_into().map_err(|v: Vec<u8>| {
        SecretError::Backend(format!(
            "vault data-key is {} bytes, expected {KEY_LEN}",
            v.len()
        ))
    })?;
    Ok(out)
}

/// All secrets in one encrypted file, decrypted into memory on open and
/// rewritten wholesale on each mutation. The map is tiny, so whole-file
/// rewrites are simpler than per-entry updates.
pub struct VaultStore {
    path: PathBuf,
    cipher: XChaCha20Poly1305,
    /// Source of truth, mirrored to `path` on every mutation.
    map: Mutex<HashMap<String, String>>,
}

impl VaultStore {
    /// Opens the vault at `path`. Errors if the key is unavailable or the file
    /// is unreadable or tampered; only a missing file starts an empty vault.
    pub fn open(
        path: impl Into<PathBuf>,
        key_provider: &dyn KeyProvider,
    ) -> Result<Self, SecretError> {
        let path = path.into();
        let key_bytes = key_provider.load_or_create_key()?;
        let cipher = XChaCha20Poly1305::new(&Key::from(key_bytes));

        let map = match std::fs::read(&path) {
            Ok(bytes) => decrypt_vault(&cipher, &bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                return Err(SecretError::Backend(format!(
                    "reading vault {}: {e}",
                    path.display()
                )));
            }
        };

        Ok(Self {
            path,
            cipher,
            map: Mutex::new(map),
        })
    }

    /// [`Self::open`] with a [`KeyringKeyProvider`].
    pub fn open_keyring_backed(path: impl Into<PathBuf>) -> Result<Self, SecretError> {
        Self::open(path, &KeyringKeyProvider::new())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Durably replaces the vault file with the encrypted `map`, so a crash
    /// leaves either the old or the new vault, never a truncated one:
    ///
    /// 1. a uniquely named temp file is created `O_EXCL` with mode `0600`
    ///    from the start (never briefly world-readable, never follows a
    ///    planted symlink) in the *same* dir, so the rename stays on one
    ///    filesystem;
    /// 2. its contents are fsynced before the rename;
    /// 3. the directory is fsynced after it, so the rename itself survives.
    ///
    /// On any error the temp file is removed (by `NamedTempFile`'s drop).
    fn flush(&self, map: &HashMap<String, String>) -> Result<(), SecretError> {
        use std::io::Write as _;

        let payload = encrypt_vault(&self.cipher, map)?;

        let dir = self.path.parent().ok_or_else(|| {
            SecretError::Backend(format!("vault path {} has no parent", self.path.display()))
        })?;
        create_private_dir_all(dir)?;

        let io_err = |what: &str, e: std::io::Error| {
            SecretError::Backend(format!("{what} {}: {e}", self.path.display()))
        };
        let mut tmp = private_tempfile_builder()
            .tempfile_in(dir)
            .map_err(|e| io_err("creating temp file for", e))?;
        tmp.write_all(&payload)
            .map_err(|e| io_err("writing temp file for", e))?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| io_err("syncing temp file for", e))?;
        tmp.persist(&self.path)
            .map_err(|e| io_err("replacing", e.error))?;
        sync_dir(dir)
    }

    /// Applies `mutate` to a copy of the map, persists the copy, and only then
    /// publishes it in memory. A failed write therefore leaves memory and disk
    /// agreeing on the old contents, instead of `get` serving a value that
    /// would vanish on restart.
    fn update(
        &self,
        mutate: impl FnOnce(&mut HashMap<String, String>) -> bool,
    ) -> Result<(), SecretError> {
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
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.update(|map| {
            map.insert(key.to_owned(), value.to_owned());
            true
        })
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.update(|map| map.remove(key).is_some())
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
        Self::new(path, Box::new(KeyringKeyProvider::new()))
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

/// On-disk layout: `MAGIC | VERSION | 24-byte nonce | ciphertext+tag`. The
/// nonce is fresh per write, which keeps repeated rewrites safe.
fn encrypt_vault(
    cipher: &XChaCha20Poly1305,
    map: &HashMap<String, String>,
) -> Result<Vec<u8>, SecretError> {
    let plaintext = serde_json::to_vec(map)
        .map_err(|e| SecretError::Backend(format!("serializing vault: {e}")))?;
    let nonce = XNonce::generate();
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_ref())
        .map_err(|e| SecretError::Backend(format!("encrypting vault: {e}")))?;

    let mut out = Vec::with_capacity(3 + nonce.len() + ciphertext.len());
    out.extend_from_slice(&VAULT_MAGIC);
    out.push(VAULT_VERSION);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Inverse of [`encrypt_vault`]. Corrupt, foreign or tampered files are errors
/// rather than an empty map, so secrets are never silently lost.
fn decrypt_vault(
    cipher: &XChaCha20Poly1305,
    bytes: &[u8],
) -> Result<HashMap<String, String>, SecretError> {
    const NONCE_LEN: usize = 24;
    let truncated = || SecretError::Backend("vault file is truncated".to_owned());

    let (magic, rest) = bytes.split_first_chunk::<2>().ok_or_else(truncated)?;
    if *magic != VAULT_MAGIC {
        return Err(SecretError::Backend(
            "not a GAVMO vault file (bad magic)".to_owned(),
        ));
    }
    let (&version, rest) = rest.split_first().ok_or_else(truncated)?;
    if version != VAULT_VERSION {
        return Err(SecretError::Backend(format!(
            "unsupported vault version {version} (this build understands {VAULT_VERSION})"
        )));
    }
    let (nonce, ciphertext) = rest
        .split_first_chunk::<NONCE_LEN>()
        .ok_or_else(truncated)?;
    let nonce = XNonce::from(*nonce);

    // `aead::Error` is deliberately opaque: wrong key and tampering are
    // indistinguishable, so there is no detail to keep.
    let plaintext =
        cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|chacha20poly1305::aead::Error| {
                SecretError::Backend(
                "vault could not be decrypted (wrong key or the file was tampered with/corrupted)"
                    .to_owned(),
            )
            })?;
    serde_json::from_slice(&plaintext)
        .map_err(|e| SecretError::Backend(format!("vault contents are not valid JSON: {e}")))
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
        .map_err(|e| SecretError::Backend(format!("creating {}: {e}", dir.display())))
}

/// Fsyncs a directory so a rename inside it is durable. Unix only: Windows
/// can't open directories this way and `MoveFileEx` semantics differ.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), SecretError> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| SecretError::Backend(format!("syncing {}: {e}", dir.display())))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), SecretError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_joins_workspace_and_name_with_a_non_typeable_separator() {
        let key = secret_key("/home/me/repo", "jira-cred");
        assert_eq!(key, "/home/me/repo\u{1f}jira-cred");
    }

    #[test]
    fn secret_key_cannot_collide_across_a_workspace_name_boundary() {
        assert_ne!(secret_key("a", "bc"), secret_key("ab", "c"));
    }

    #[test]
    fn memory_store_round_trips_set_get() {
        let store = MemoryStore::new();
        assert_eq!(store.get("k").unwrap(), None);
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn memory_store_set_overwrites() {
        let store = MemoryStore::new();
        store.set("k", "v1").unwrap();
        store.set("k", "v2").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn memory_store_delete_removes_and_is_idempotent() {
        let store = MemoryStore::new();
        store.set("k", "v").unwrap();
        store.delete("k").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
        store.delete("k").unwrap();
    }

    #[test]
    fn store_is_object_safe_behind_arc() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore::new());
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_string()));
    }

    // --- VaultStore ---

    struct FixedKey([u8; KEY_LEN]);
    impl KeyProvider for FixedKey {
        fn load_or_create_key(&self) -> Result<[u8; KEY_LEN], SecretError> {
            Ok(self.0)
        }
    }

    /// Simulates an unreachable keystore.
    struct UnavailableKey;
    impl KeyProvider for UnavailableKey {
        fn load_or_create_key(&self) -> Result<[u8; KEY_LEN], SecretError> {
            Err(SecretError::Backend("no keystore".to_string()))
        }
    }

    fn fixed(byte: u8) -> FixedKey {
        FixedKey([byte; KEY_LEN])
    }

    fn vault_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("secrets.vault")
    }

    #[test]
    fn vault_round_trips_set_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        assert_eq!(store.get("k").unwrap(), None);
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn vault_set_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        store.set("k", "v1").unwrap();
        store.set("k", "v2").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn vault_delete_removes_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        store.set("k", "v").unwrap();
        store.delete("k").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
        store.delete("k").unwrap();
    }

    #[test]
    fn vault_persists_across_reopen_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        {
            let store = VaultStore::open(&path, &fixed(7)).unwrap();
            store.set("jira", r#"{"token":"abc"}"#).unwrap();
        }
        let reopened = VaultStore::open(&path, &fixed(7)).unwrap();
        assert_eq!(
            reopened.get("jira").unwrap(),
            Some(r#"{"token":"abc"}"#.to_string())
        );
    }

    #[test]
    fn vault_file_is_encrypted_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let store = VaultStore::open(&path, &fixed(3)).unwrap();
        store.set("k", "super-secret-value").unwrap();

        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[0..2], &VAULT_MAGIC);
        assert!(
            !raw.windows(b"super-secret-value".len())
                .any(|w| w == b"super-secret-value"),
            "secret value leaked into the on-disk vault in cleartext"
        );
    }

    #[test]
    fn vault_wrong_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        {
            let store = VaultStore::open(&path, &fixed(1)).unwrap();
            store.set("k", "v").unwrap();
        }
        // Must error, not silently read an empty vault.
        let err = VaultStore::open(&path, &fixed(2));
        assert!(matches!(err, Err(SecretError::Backend(_))));
    }

    #[test]
    fn vault_tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        {
            let store = VaultStore::open(&path, &fixed(5)).unwrap();
            store.set("k", "v").unwrap();
        }
        let mut raw = std::fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        std::fs::write(&path, &raw).unwrap();

        let err = VaultStore::open(&path, &fixed(5));
        assert!(matches!(err, Err(SecretError::Backend(_))));
    }

    #[test]
    fn vault_rejects_a_file_with_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        std::fs::write(&path, b"not-a-vault-file-at-all....................").unwrap();
        let err = VaultStore::open(&path, &fixed(1));
        assert!(matches!(err, Err(SecretError::Backend(_))));
    }

    #[test]
    fn vault_missing_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        assert_eq!(store.get("anything").unwrap(), None);
    }

    #[test]
    fn vault_reencrypt_uses_a_fresh_nonce_each_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let store = VaultStore::open(&path, &fixed(9)).unwrap();
        store.set("k", "v").unwrap();
        let first = std::fs::read(&path).unwrap();
        store.set("k", "v").unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_ne!(
            first, second,
            "nonce (and thus ciphertext) must change per write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn vault_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let store = VaultStore::open(&path, &fixed(1)).unwrap();
        store.set("k", "v").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "vault must be chmod 0600");
    }

    /// A failed write must not leave memory ahead of disk: `get` would serve a
    /// value that silently disappears on restart.
    #[test]
    fn vault_failed_write_leaves_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let store = VaultStore::open(sub.join("secrets.vault"), &fixed(1)).unwrap();
        // A *file* where the vault's directory should be makes every flush fail.
        std::fs::write(&sub, b"").unwrap();

        assert!(store.set("k", "v").is_err());
        assert_eq!(store.get("k").unwrap(), None);
    }

    #[test]
    fn vault_failed_delete_keeps_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let store = VaultStore::open(sub.join("secrets.vault"), &fixed(1)).unwrap();
        store.set("k", "v").unwrap();
        std::fs::remove_dir_all(&sub).unwrap();
        std::fs::write(&sub, b"").unwrap();

        assert!(store.delete("k").is_err());
        assert_eq!(store.get("k").unwrap(), Some("v".to_owned()));
    }

    #[test]
    fn vault_write_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = VaultStore::open(vault_path(&dir), &fixed(1)).unwrap();
        store.set("a", "1").unwrap();
        store.set("b", "2").unwrap();
        store.delete("a").unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["secrets.vault"]);
    }

    #[test]
    fn vault_rejects_truncated_files_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        for len in 0..=(3 + 24) {
            let mut bytes = vec![0u8; len];
            if len >= 2 {
                bytes[..2].copy_from_slice(&VAULT_MAGIC);
            }
            if len >= 3 {
                bytes[2] = VAULT_VERSION;
            }
            std::fs::write(&path, &bytes).unwrap();
            assert!(
                VaultStore::open(&path, &fixed(1)).is_err(),
                "a {len}-byte vault must be rejected"
            );
        }
    }

    #[test]
    fn vault_rejects_an_unknown_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        let mut bytes = vec![0u8; 64];
        bytes[..2].copy_from_slice(&VAULT_MAGIC);
        bytes[2] = VAULT_VERSION + 1;
        std::fs::write(&path, &bytes).unwrap();
        let Err(SecretError::Backend(msg)) = VaultStore::open(&path, &fixed(1)) else {
            panic!("expected an error");
        };
        assert!(msg.contains("unsupported vault version"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn vault_creates_its_directory_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("new-dir");
        let store = VaultStore::open(sub.join("secrets.vault"), &fixed(1)).unwrap();
        store.set("k", "v").unwrap();
        let mode = std::fs::metadata(&sub).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn memory_store_debug_does_not_print_values() {
        let store = MemoryStore::new();
        store.set("name", "hunter2").unwrap();
        let debug = format!("{store:?}");
        assert!(debug.contains("name"), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
    }

    #[test]
    fn lazy_vault_defers_open_and_fails_closed_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = LazyVaultStore::new(vault_path(&dir), Box::new(UnavailableKey));
        // An error, not `Ok(None)`: fail closed.
        assert!(matches!(store.get("k"), Err(SecretError::Backend(_))));
        assert!(matches!(store.set("k", "v"), Err(SecretError::Backend(_))));
    }

    #[test]
    fn lazy_vault_opens_on_first_use_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = LazyVaultStore::new(vault_path(&dir), Box::new(fixed(4)));
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn lazy_vault_is_object_safe_behind_arc() {
        let dir = tempfile::tempdir().unwrap();
        let store: std::sync::Arc<dyn SecretStore> =
            std::sync::Arc::new(LazyVaultStore::new(vault_path(&dir), Box::new(fixed(1))));
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn decode_key_rejects_wrong_length() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(matches!(decode_key(&short), Err(SecretError::Backend(_))));
        let ok = base64::engine::general_purpose::STANDARD.encode([0u8; KEY_LEN]);
        assert!(decode_key(&ok).is_ok());
    }
}
