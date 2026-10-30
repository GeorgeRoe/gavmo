//! The encrypted vault: on-disk format ([`format`]) and the stores built on
//! it.

mod format;
mod store;

pub use store::{LazyVaultStore, VaultStore};
