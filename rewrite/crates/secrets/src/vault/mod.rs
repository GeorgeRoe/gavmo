//! The encrypted vault: its key ([`data_key`]), file format ([`format`]),
//! decrypted contents ([`secret_map`]) and the store built on them.

mod data_key;
mod format;
mod format_error;
mod secret_map;
mod vault_store;

pub use format_error::FormatError;
pub use vault_store::{VAULT_FILE, VaultStore};
