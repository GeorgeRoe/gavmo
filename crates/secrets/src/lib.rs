//! Host-side secret storage for GAVMO's tool gateway (see
//! `gavmo-tools-config-plan.md`).
//!
//! Pure mechanism, same spirit as `vm`/`mcp`: it knows how to put and fetch a
//! secret string under a key, but has no idea what a "workspace", an
//! "integration", or a credential *means* — the daemon (`crates/gavmo`) owns
//! that, composes the keys (see [`secret_key`]), and decides what JSON to stuff
//! in the value.
//!
//! ## Backends
//!
//! The daemon's real backend is [`VaultStore`] — the "Option B" hybrid from
//! `gavmo-tools-config-plan.md` §3: **one** AEAD-encrypted file
//! (`secrets.vault`) holds *every* secret, encrypted with a single 32-byte
//! data-key that is itself the **one** entry GAVMO keeps in the platform
//! credential store (Windows Credential Manager / macOS Keychain / Linux Secret
//! Service) via the `keyring` crate. Versus the older one-keyring-entry-per-secret
//! [`KeyringStore`], this gives: a single file to back up / inspect / reset; one
//! keyring touch instead of N (more robust where the Linux Secret Service is
//! flaky); and full control of the on-disk format (a seam for future per-secret
//! metadata / URL-binding). The AEAD is XChaCha20-Poly1305 (pure-Rust, 24-byte
//! random nonce per write).
//!
//! **Fail-closed, never a plaintext fallback.** The data-key lives *only* in the
//! OS keystore. If the keystore is unreachable (no/locked Secret Service, no
//! D-Bus session bus — the common stock-WSL case), [`VaultStore`] refuses every
//! operation with [`SecretError::Backend`] rather than degrading to an
//! unencrypted store. At-rest encryption is thus a hard guarantee, not a
//! best-effort one. (Making that failure *legible* in the control-panel UI is a
//! separate, deliberately-unshipped concern — a broken distro keystore surfaces
//! as the raw backend error.)
//!
//! [`KeyringStore`] (the original one-entry-per-secret backend) is kept as a
//! valid [`SecretStore`] impl but is no longer what the daemon wires up; there is
//! **no migration** from its entries to the vault (the `keyring` crate has no
//! list API, so enumerating arbitrary prior entries isn't even possible) — a user
//! re-enters each secret once via the Tools panel.
//!
//! [`MemoryStore`] is an in-process backend for tests (and for any environment
//! with no usable keystore), so the rest of the system is testable in the
//! sidecar without a live Secret Service. Likewise [`VaultStore`] is generic over
//! a [`KeyProvider`], so its whole encrypt/decrypt path is unit-tested against an
//! in-memory key with zero keyring dependency.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

/// Everything that can go wrong talking to a secret backend. `NotFound` is
/// *not* in here on purpose: "no such secret" is a normal, expected outcome
/// (the user simply hasn't configured it yet), so `get` returns
/// `Ok(None)` rather than an error — callers branch on presence, not on a
/// caught error.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The underlying OS keystore failed (locked keyring, no Secret Service,
    /// platform error, etc.). The wrapped message is for logging/surfacing to
    /// the user ("couldn't reach your system keychain"), not for control flow.
    #[error("secret store backend error: {0}")]
    Backend(String),
}

/// Store and retrieve opaque secret strings by an opaque key.
///
/// Deliberately tiny and string-in/string-out: structure (e.g. a Jira
/// `email` + `api_token` pair) is the caller's concern — it JSON-encodes into
/// the value. Keeping the trait this small is what lets the v1 [`KeyringStore`]
/// and the future encrypted vault be interchangeable.
///
/// `&self` (not `&mut self`): backends are shared across the daemon's async
/// tasks behind an `Arc`, and both impls here are internally synchronized (the
/// OS keystore is its own source of truth; [`MemoryStore`] uses a `Mutex`).
pub trait SecretStore: Send + Sync {
    /// Returns the stored value, or `Ok(None)` if nothing is stored under
    /// `key` (the normal "not configured yet" case — see [`SecretError`]).
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;

    /// Stores `value` under `key`, overwriting any existing value.
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;

    /// Removes any value stored under `key`. Deleting a key that isn't present
    /// is a no-op success (idempotent) — callers shouldn't have to care whether
    /// a secret they're clearing was ever set.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// The `service` name all GAVMO secrets live under in the OS keystore. A single
/// constant namespace keeps GAVMO's entries grouped and distinguishable from
/// other apps' credentials in e.g. Windows Credential Manager.
pub const KEYRING_SERVICE: &str = "gavmo";

/// Composes a storage key from a workspace identity and a secret name.
///
/// This is the one shared opinion the daemon and the store agree on, lifted
/// here so both sides format keys identically. `workspace_id` is the workspace's
/// identity (its absolute host path); `name` is the secret reference from the
/// config (e.g. `"jira-cred"`). Keying by workspace binds a secret to the one
/// repo that requested it.
///
/// The separator is `\u{1f}` (ASCII Unit Separator): it can't appear in a
/// human-typed workspace path or secret name, so the two can never be confused
/// for one another the way a `:` or `/` join could be ambiguous.
pub fn secret_key(workspace_id: &str, name: &str) -> String {
    format!("{workspace_id}\u{1f}{name}")
}

/// The real backend: one entry per secret in the OS credential store.
#[derive(Debug, Default, Clone)]
pub struct KeyringStore;

impl KeyringStore {
    /// Creates a keyring-backed store. Construction never touches the OS — the
    /// keystore is only contacted on the first `get`/`set`/`delete`, so a
    /// missing/locked keyring surfaces as a [`SecretError::Backend`] at the
    /// point of use (where it can be reported in context), not at startup.
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
            // "No such entry" is the expected not-configured case -> None.
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
            // Idempotent: clearing an absent secret is a success.
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }
}

/// In-process backend for tests and keystore-less environments. Not persisted
/// and not encrypted — never use it as the daemon's real store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
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
    /// A poisoned lock here only means a prior test panicked mid-mutation;
    /// recovering the guard keeps unrelated tests from cascading-failing on an
    /// unwrap, and there's no invariant across the map worth preserving.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------
// VaultStore: one AEAD-encrypted file, data-key in the OS keystore (Option B).
// ---------------------------------------------------------------------------

/// How many bytes the AEAD data-key is. XChaCha20-Poly1305 uses a 256-bit key.
const KEY_LEN: usize = 32;

/// First two bytes of every vault file: a fixed magic so a wrong/garbage file is
/// rejected up front with a clear error rather than failing deep inside AEAD
/// decryption. `b"GV"` for GAVMO Vault.
const VAULT_MAGIC: [u8; 2] = *b"GV";

/// On-disk format version (the byte after the magic). Bump only on an
/// incompatible layout change; `open` rejects anything it doesn't recognize.
const VAULT_VERSION: u8 = 1;

/// The `keyring` entry name under which the vault's single data-key is stored
/// (base64). One entry total, versus [`KeyringStore`]'s one-per-secret.
pub const VAULT_KEY_ENTRY: &str = "vault-data-key";

/// Supplies the vault's 32-byte AEAD data-key, abstracted so the encrypt/decrypt
/// path is testable without a real OS keystore.
///
/// The production impl ([`KeyringKeyProvider`]) loads the key from the single
/// [`VAULT_KEY_ENTRY`] keyring entry, generating + storing a fresh one on first
/// run. A keystore that can't be reached returns `Err` here — which is what makes
/// [`VaultStore`] fail *closed* (refuse to operate) rather than silently fall
/// back to an unencrypted store.
pub trait KeyProvider: Send + Sync {
    /// Returns the 32-byte data-key, creating and persisting one on first use.
    /// `Err` means the key backing store is unavailable (not that "there's no
    /// key yet" — first-run key creation is this method's own job).
    fn load_or_create_key(&self) -> Result<[u8; KEY_LEN], SecretError>;
}

/// Production [`KeyProvider`]: the data-key lives as the single [`VAULT_KEY_ENTRY`]
/// entry in the OS keystore, base64-encoded (the keystore stores strings).
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
            // First run: no key yet -> mint one and persist it. Any *other*
            // error (locked keyring, no Secret Service, no session bus) is a
            // real backend failure and propagates, so the vault fails closed.
            Err(keyring::Error::NoEntry) => {
                let key = Key::generate();
                let encoded = base64::engine::general_purpose::STANDARD.encode(key.as_slice());
                entry
                    .set_password(&encoded)
                    .map_err(|e| SecretError::Backend(e.to_string()))?;
                let mut out = [0u8; KEY_LEN];
                out.copy_from_slice(key.as_slice());
                Ok(out)
            }
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }
}

/// Decodes a base64 data-key string into exactly [`KEY_LEN`] bytes, erroring on
/// malformed input or the wrong length (a corrupt/foreign keyring entry).
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

/// The daemon's real backend: every secret lives in one AEAD-encrypted file,
/// decrypted into an in-memory map on open and re-encrypted wholesale (atomic
/// temp-file + rename) on each mutation.
///
/// Whole-file rewrite is deliberate: a workspace has only a handful of
/// integrations, so the map is tiny and the simplicity (no per-entry crypto,
/// no partial-write reconciliation) is worth far more than incremental updates
/// would save.
pub struct VaultStore {
    path: PathBuf,
    cipher: XChaCha20Poly1305,
    /// In-memory source of truth, mirrored to `path` on every mutation.
    map: Mutex<HashMap<String, String>>,
}

impl VaultStore {
    /// Opens (or, if the file is absent, initializes empty) the vault at `path`,
    /// using `key_provider` for the AEAD data-key.
    ///
    /// Fails closed: if the key can't be obtained (keystore unreachable) or an
    /// existing file is unreadable/undecryptable/tampered, this returns `Err`
    /// rather than proceeding with an empty or unencrypted store. A *missing*
    /// file is the one benign case — it's a fresh install, so the vault starts
    /// empty and is written on the first `set`.
    pub fn open(path: impl Into<PathBuf>, key_provider: &dyn KeyProvider) -> Result<Self, SecretError> {
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

    /// Convenience constructor for the production ([`KeyringKeyProvider`]) path.
    pub fn open_keyring_backed(path: impl Into<PathBuf>) -> Result<Self, SecretError> {
        Self::open(path, &KeyringKeyProvider::new())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Encrypts the current map and writes it to `path` atomically: a sibling
    /// temp file is fully written (and, on Unix, `chmod 0600`'d) before an atomic
    /// rename replaces the real file, so a crash mid-write can never leave a
    /// truncated/garbage vault — only the old or the new whole file.
    fn flush(&self, map: &HashMap<String, String>) -> Result<(), SecretError> {
        let payload = encrypt_vault(&self.cipher, map)?;

        let dir = self.path.parent().ok_or_else(|| {
            SecretError::Backend(format!("vault path {} has no parent", self.path.display()))
        })?;
        std::fs::create_dir_all(dir)
            .map_err(|e| SecretError::Backend(format!("creating {}: {e}", dir.display())))?;

        // Temp file in the *same* dir so the rename stays on one filesystem.
        let tmp = self.path.with_extension("vault.tmp");
        std::fs::write(&tmp, &payload)
            .map_err(|e| SecretError::Backend(format!("writing {}: {e}", tmp.display())))?;
        set_owner_only_perms(&tmp)?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            // Best-effort cleanup so a failed rename doesn't litter a temp file.
            let _ = std::fs::remove_file(&tmp);
            SecretError::Backend(format!("replacing {}: {e}", self.path.display()))
        })?;
        Ok(())
    }
}

impl SecretStore for VaultStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let mut map = self.lock();
        map.insert(key.to_string(), value.to_string());
        self.flush(&map)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let mut map = self.lock();
        // Idempotent: deleting an absent key is a no-op success — and skip the
        // (re-encrypt + write) work entirely when nothing actually changed.
        if map.remove(key).is_none() {
            return Ok(());
        }
        self.flush(&map)
    }
}

/// A [`SecretStore`] that opens its backing [`VaultStore`] *lazily*, on the first
/// `get`/`set`/`delete`, rather than at construction.
///
/// This exists to match the deferred-contact posture the daemon relies on: the
/// keystore (and thus the data-key, and thus the whole vault) must not be touched
/// at daemon startup, only when a secret is actually accessed. Otherwise a host
/// with a broken/absent Secret Service (the common stock-WSL case) couldn't even
/// start the daemon — blocking VM management, which has nothing to do with tool
/// secrets. With lazy open, such a host runs fine and only tool-secret operations
/// fail (loudly, fail-closed) when attempted.
///
/// A failed open is **not** cached: the error propagates and the next call
/// retries, so transiently unlocking the keyring (then retrying from the UI)
/// recovers without a daemon restart.
pub struct LazyVaultStore {
    path: PathBuf,
    key_provider: Box<dyn KeyProvider>,
    /// `None` until the first successful open; `Some` thereafter.
    opened: Mutex<Option<VaultStore>>,
}

impl LazyVaultStore {
    /// Creates a lazily-opened vault at `path`, using `key_provider` for its
    /// data-key. Does no I/O and never touches the keystore until first use.
    pub fn new(path: impl Into<PathBuf>, key_provider: Box<dyn KeyProvider>) -> Self {
        Self {
            path: path.into(),
            key_provider,
            opened: Mutex::new(None),
        }
    }

    /// Convenience constructor for the production ([`KeyringKeyProvider`]) path.
    pub fn keyring_backed(path: impl Into<PathBuf>) -> Self {
        Self::new(path, Box::new(KeyringKeyProvider::new()))
    }

    /// Runs `f` against the opened vault, opening it first if needed. A failed
    /// open leaves `opened` as `None` so the next call retries.
    fn with_vault<T>(
        &self,
        f: impl FnOnce(&VaultStore) -> Result<T, SecretError>,
    ) -> Result<T, SecretError> {
        let mut guard = self.opened.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(VaultStore::open(self.path.clone(), self.key_provider.as_ref())?);
        }
        f(guard.as_ref().expect("just opened"))
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

/// Serializes + encrypts the whole secret map into the on-disk payload:
/// `MAGIC ‖ VERSION ‖ 24-byte nonce ‖ AEAD(ciphertext+tag)`. A fresh random
/// nonce per write means re-encrypting the same map twice yields different
/// bytes — exactly what keeps XChaCha20-Poly1305 safe across repeated whole-file
/// rewrites.
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

/// Inverse of [`encrypt_vault`]: validates the header, splits off the nonce, and
/// AEAD-decrypts. Any failure (bad magic/version, short file, wrong key, or a
/// flipped ciphertext byte — the auth tag catches tampering) is a
/// [`SecretError::Backend`], so a corrupt or tampered vault fails loudly instead
/// of silently losing secrets.
fn decrypt_vault(
    cipher: &XChaCha20Poly1305,
    bytes: &[u8],
) -> Result<HashMap<String, String>, SecretError> {
    const HEADER: usize = 3; // magic(2) + version(1)
    const NONCE_LEN: usize = 24;
    if bytes.len() < HEADER + NONCE_LEN {
        return Err(SecretError::Backend("vault file is truncated".to_string()));
    }
    if bytes[0..2] != VAULT_MAGIC {
        return Err(SecretError::Backend(
            "not a GAVMO vault file (bad magic)".to_string(),
        ));
    }
    if bytes[2] != VAULT_VERSION {
        return Err(SecretError::Backend(format!(
            "unsupported vault version {} (this build understands {VAULT_VERSION})",
            bytes[2]
        )));
    }

    let nonce = XNonce::try_from(&bytes[HEADER..HEADER + NONCE_LEN])
        .map_err(|_| SecretError::Backend("vault nonce is malformed".to_string()))?;
    let ciphertext = &bytes[HEADER + NONCE_LEN..];
    let plaintext = cipher.decrypt(&nonce, ciphertext).map_err(|_| {
        // Deliberately generic: a decryption failure means wrong key OR
        // tampering, and we can't (and shouldn't) distinguish them.
        SecretError::Backend(
            "vault could not be decrypted (wrong key or the file was tampered with/corrupted)"
                .to_string(),
        )
    })?;
    serde_json::from_slice(&plaintext)
        .map_err(|e| SecretError::Backend(format!("vault contents are not valid JSON: {e}")))
}

/// Restricts a just-written vault/temp file to owner-only (`0600`) on Unix, so a
/// multi-user host can't let another account read the encrypted blob. A no-op on
/// non-Unix (Windows ACLs are inherited from the user's profile dir).
#[cfg(unix)]
fn set_owner_only_perms(path: &Path) -> Result<(), SecretError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| SecretError::Backend(format!("chmod {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn set_owner_only_perms(_path: &Path) -> Result<(), SecretError> {
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
        // Without the Unit Separator these could both be "a" + "bc" vs "ab" + "c".
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
        // Deleting again is a no-op success, not an error.
        store.delete("k").unwrap();
    }

    /// The daemon will hold the store as `Arc<dyn SecretStore>`; make sure the
    /// trait is object-safe and usable that way.
    #[test]
    fn store_is_object_safe_behind_arc() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore::new());
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_string()));
    }

    // --- VaultStore -------------------------------------------------------
    //
    // All vault tests drive the AEAD + file path through a test `KeyProvider`
    // so there's zero dependency on a live OS keystore — exactly the seam
    // `KeyProvider` exists for.

    /// A fixed in-memory key, so a reopened vault decrypts with the "same" key.
    struct FixedKey([u8; KEY_LEN]);
    impl KeyProvider for FixedKey {
        fn load_or_create_key(&self) -> Result<[u8; KEY_LEN], SecretError> {
            Ok(self.0)
        }
    }

    /// A provider that always fails — stands in for an unreachable keystore.
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
        store.delete("k").unwrap(); // no-op success
    }

    #[test]
    fn vault_persists_across_reopen_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = vault_path(&dir);
        {
            let store = VaultStore::open(&path, &fixed(7)).unwrap();
            store.set("jira", r#"{"token":"abc"}"#).unwrap();
        }
        // A brand-new VaultStore over the same file + key must see the secret.
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
        // The header is visible, but the secret bytes must not appear in cleartext.
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
        // A *different* key must not silently read an empty vault — it must error.
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
        // Flip a byte deep in the ciphertext (past header + nonce) — the AEAD
        // tag must reject it on the next open.
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
        // Never written: opening must succeed with an empty map, not error.
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
        // Rewriting the *same* logical content must still change the bytes
        // (random nonce), which is what keeps repeated whole-file rewrites safe.
        store.set("k", "v").unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_ne!(first, second, "nonce (and thus ciphertext) must change per write");
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

    #[test]
    fn lazy_vault_defers_open_and_fails_closed_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        // Construction does no I/O / keystore access…
        let store = LazyVaultStore::new(vault_path(&dir), Box::new(UnavailableKey));
        // …but the first real access surfaces the key-unavailable error
        // (fail-closed), rather than pretending the secret is simply absent.
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
