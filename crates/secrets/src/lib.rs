//! Host-side secret storage for GAVMO's tool gateway: opaque strings by key.
//!
//! The main backend, [`VaultStore`], is one XChaCha20-Poly1305-encrypted file
//! whose data key is the single GAVMO entry in the OS keystore. It fails closed:
//! if the keystore is unreachable, every operation errors; there is no
//! plaintext fallback.
//!
//! Secret material (the data key, decrypted vault contents, values) is
//! zeroized when dropped. This is best effort - values returned to callers
//! are plain `String`s, and allocator copies can't be tracked - but it keeps
//! the vault's own copies from lingering.

mod error;
mod key;
#[cfg(any(test, feature = "test-util"))]
mod memory;
mod vault;

pub use error::SecretError;
pub use key::{
    DataKey, KEY_LEN, KEYRING_SERVICE, KeyProvider, KeyringKeyProvider, VAULT_KEY_ENTRY,
};
#[cfg(any(test, feature = "test-util"))]
pub use memory::MemoryStore;
pub use vault::{LazyVaultStore, VaultStore};

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

/// Composes a storage key from a workspace id (its absolute host path) and a
/// secret name, binding each secret to one workspace: `<id>\u{1f}<name>`.
///
/// Unambiguous for any inputs: `%` and `\u{1f}` in `name` are
/// percent-encoded, so the name never contains the separator and the key
/// splits uniquely at its *last* `\u{1f}` (a path may contain one). Names
/// allowed by `gavmo.yaml` contain neither character, so their keys are
/// unchanged from earlier versions.
pub fn secret_key(workspace_id: &str, name: &str) -> String {
    let name = name.replace('%', "%25").replace('\u{1f}', "%1F");
    format!("{workspace_id}\u{1f}{name}")
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
        // A separator in either part can't forge another pair's key.
        assert_ne!(secret_key("/a", "b\u{1f}c"), secret_key("/a\u{1f}b", "c"));
        assert_ne!(secret_key("/a", "b%1Fc"), secret_key("/a", "b\u{1f}c"));
    }

    #[test]
    fn store_is_object_safe_behind_arc() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore::new());
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap(), Some("v".to_owned()));
    }
}
