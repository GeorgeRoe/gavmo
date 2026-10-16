//! Host-side secret storage for GAVMO's tool gateway (v1 — see
//! `gavmo-tools-config-plan.md` §13a).
//!
//! Pure mechanism, same spirit as `vm`/`mcp`: it knows how to put and fetch a
//! secret string under a key, but has no idea what a "workspace", an
//! "integration", or a credential *means* — the daemon (`crates/gavmo`) owns
//! that, composes the keys (see [`secret_key`]), and decides what JSON to stuff
//! in the value.
//!
//! ## v1 posture (security deferred)
//!
//! The only backend that touches the real OS is [`KeyringStore`], which stores
//! one entry per secret in the platform credential store (Windows Credential
//! Manager / macOS Keychain / Linux Secret Service) via the `keyring` crate.
//! That gives at-rest protection tied to the user's login *for free*, with no
//! crypto code of our own. The encrypted "Option B" vault (one file, data-key in
//! the keystore, URL-bound keys) from the plan lands later as **another
//! [`SecretStore`] impl behind this same trait** — callers won't change.
//!
//! [`MemoryStore`] is an in-process backend for tests (and for any environment
//! with no usable keystore), so the rest of the system is testable in the
//! sidecar without a live Secret Service.

use std::collections::HashMap;
use std::sync::Mutex;

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

/// Composes a storage key from a scope and a secret name.
///
/// This is the one shared opinion the daemon and the store agree on, lifted
/// here so both sides format keys identically. `scope` is something like
/// `"team:<team-spec-id>"` or `"project:<workspace-id>"`; `name` is the
/// secret reference from the config (e.g. `"jira-cred"`). The exact scope
/// strings are the daemon's business — this only guarantees a stable,
/// collision-resistant join.
///
/// The separator is `\u{1f}` (ASCII Unit Separator): it can't appear in a
/// human-typed scope or secret name, so `scope`/`name` can never be confused
/// for one another the way a `:` or `/` join could be ambiguous.
pub fn secret_key(scope: &str, name: &str) -> String {
    format!("{scope}\u{1f}{name}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_joins_scope_and_name_with_a_non_typeable_separator() {
        let key = secret_key("team:dafni", "jira-cred");
        assert_eq!(key, "team:dafni\u{1f}jira-cred");
    }

    #[test]
    fn secret_key_cannot_collide_across_a_scope_name_boundary() {
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
}
