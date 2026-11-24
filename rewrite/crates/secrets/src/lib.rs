//! Host-side secret storage: [`SecretValue`]s by [`SecretKey`].
//!
//! The real store, [`VaultStore`], is one XChaCha20-Poly1305-encrypted file
//! in a [`PrivateDir`](private_fs::PrivateDir), whose key is kept by a
//! [`Keystore`](keystore::Keystore). It fails closed: if the keystore is
//! unreachable, every operation errors; there is no plaintext fallback.
//! [`LazyStore`] opens it on first use, so the daemon starts without a
//! working keystore.
//!
//! Secret material (the data key, decrypted vault contents, values) is
//! wiped from memory when dropped. This is best effort - allocator copies
//! can't be tracked - but no copy the vault makes lingers.

mod lazy_store;
#[cfg(any(test, feature = "test-util"))]
mod memory_store;
mod open_error;
mod secret_key;
mod secret_store;
mod secret_value;
mod store_error;
mod vault;
mod write_error;

pub use lazy_store::LazyStore;
#[cfg(any(test, feature = "test-util"))]
pub use memory_store::MemoryStore;
pub use open_error::OpenError;
pub use secret_key::{KeyName, KeyNameChars, KeyNameLabel, SecretKey};
pub use secret_store::SecretStore;
pub use secret_value::SecretValue;
pub use store_error::StoreError;
pub use vault::{FormatError, VAULT_FILE, VaultStore};
pub use write_error::WriteError;
